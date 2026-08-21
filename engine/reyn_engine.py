"""Reyn Studio Python engine (sidecar).

Runs under the reyn-research venv (has torch + the research modules). The native
Rust app spawns this, reads `READY {port}` from stdout, connects over a localhost
TCP socket, and exchanges length-prefixed frames:

    frame  = u32 total_len, then `total_len` bytes = [u32 json_len][json][payload]

Requests carry only JSON (no payload); field responses carry the raw f32 field
in `payload` and its shape in the JSON. (Shared-memory transport is the planned
optimization; loopback TCP is the correct, robust first cut — a 32^3 field is ~400 KB.)
"""
import argparse
import hashlib
import json
import math
import os
import shutil
import socket
import struct
import sys
import traceback
from pathlib import Path

os.environ.setdefault("PYTORCH_ENABLE_MPS_FALLBACK", "1")  # CPU-fallback any MPS gap

import numpy as np

try:
    from .n5_inspector import INSPECTOR_SCHEMA, inspector_payload
except ImportError:  # Running this file directly as the sidecar entry point.
    from n5_inspector import INSPECTOR_SCHEMA, inspector_payload


# -- framing -----------------------------------------------------------------
def _recvn(conn, n):
    buf = bytearray()
    while len(buf) < n:
        chunk = conn.recv(n - len(buf))
        if not chunk:
            raise ConnectionError("socket closed")
        buf += chunk
    return bytes(buf)


def recv(conn):
    (total,) = struct.unpack("<I", _recvn(conn, 4))
    body = _recvn(conn, total)
    (jl,) = struct.unpack("<I", body[:4])
    obj = json.loads(body[4:4 + jl].decode("utf-8"))
    return obj, body[4 + jl:]


def send(conn, obj, payload=b""):
    j = json.dumps(obj).encode("utf-8")
    body = struct.pack("<I", len(j)) + j + payload
    conn.sendall(struct.pack("<I", len(body)) + body)


# -- pressure recovery (F-RecoverySettings) ----------------------------------
def _fd_poisson(rhs, dx, periodic, tol, max_iter):
    """Solve ∇²p = rhs on the FD 5-point Laplacian by conjugate gradient (A=−∇²
    is SPD, up to the constant null space for periodic BCs). Stops at relative
    residual `tol` or `max_iter`; returns (p, achieved_residual, iters)."""
    import torch
    dx2 = dx * dx

    def lap(p):
        if periodic:
            return (torch.roll(p, 1, 0) + torch.roll(p, -1, 0)
                    + torch.roll(p, 1, 1) + torch.roll(p, -1, 1) - 4.0 * p) / dx2
        pp = torch.nn.functional.pad(p[None, None], (1, 1, 1, 1))[0, 0]  # Dirichlet: p=0 outside
        return (pp[:-2, 1:-1] + pp[2:, 1:-1] + pp[1:-1, :-2] + pp[1:-1, 2:] - 4.0 * p) / dx2

    b = -rhs  # solve A p = b, A = −lap
    if periodic:
        b = b - b.mean()  # compatible RHS (remove the constant null direction)
    p = torch.zeros_like(rhs)
    r = b - (-lap(p))
    d = r.clone()
    rs = (r * r).sum()
    bn = b.norm() + 1e-12
    iters = 0
    for it in range(1, max_iter + 1):
        iters = it
        ad = -lap(d)
        denom = (d * ad).sum()
        if denom.abs() < 1e-30:
            break
        alpha = rs / denom
        p = p + alpha * d
        r = r - alpha * ad
        if periodic:
            p = p - p.mean()
        rs_new = (r * r).sum()
        if (rs_new.sqrt() / bn) < tol:
            break
        d = r + (rs_new / rs) * d
        rs = rs_new
    resid = float(((lap(p) - rhs).norm() / (rhs.norm() + 1e-12)).item())
    return p, resid, iters


SURFACE_LOAD_METHOD = "diffuse_interface_traction.v1"


def derive_model_qualification_class(version, limitations, status):
    """Mirror Studio's fail-closed release class for model cards."""
    version_text = str(version or "").strip().lower()
    limitation_blob = "\n".join(str(item).lower() for item in (limitations or []))
    if "yc-preview" in version_text or "preview" in version_text:
        return "preview"
    if "research" in version_text:
        return "research"
    if (
        "yc research preview" in limitation_blob
        or "not production-qualified" in limitation_blob
    ):
        return "preview"
    if "research" in limitation_blob or "not production" in limitation_blob:
        return "research"
    if (
        status == "clean"
        and "incomplete" not in limitation_blob
        and "preview" not in limitation_blob
    ):
        return "production"
    return "unknown"


def engineering_surface_loads(
    velocity,
    recovered_pressure,
    mask,
    *,
    reynolds,
    char_len_solver,
    reference_length_m,
    velocity_mps,
    density_kg_m3,
    reference_pressure_pa,
    moment_origin_mode="diffuse_surface_centroid",
    moment_origin_solver=None,
    reference_area_m2=None,
):
    """Convert a nondimensional fixed-body result into versioned fluid loads.

    The smoothed immersed mask is a diffuse surface. Its gradient supplies the
    fluid-to-solid normal and area measure. Pressure and Newtonian viscous
    traction are integrated in solver coordinates, then nondimensionalized by
    q∞Lref² (force) and q∞Lref³ (moment). These are fluid loads, not structural
    stress, and the recovered pressure remains labeled as model-derived.
    """
    velocity = np.asarray(velocity, dtype=np.float64)
    pressure = np.asarray(recovered_pressure, dtype=np.float64)
    mask = np.asarray(mask, dtype=np.float64)
    if velocity.ndim != 4 or velocity.shape[0] != 3:
        raise ValueError("engineering loads require velocity shaped [3,N,N,N]")
    if pressure.shape != mask.shape or pressure.shape != velocity.shape[1:]:
        raise ValueError("pressure, mask, and velocity grids must match")
    if (
        not np.all(np.isfinite(velocity))
        or not np.all(np.isfinite(pressure))
        or not np.all(np.isfinite(mask))
    ):
        raise ValueError("engineering load fields must contain only finite values")
    reference_values = (
        reynolds,
        char_len_solver,
        reference_length_m,
        velocity_mps,
        density_kg_m3,
        reference_pressure_pa,
    )
    if any(not math.isfinite(float(value)) for value in reference_values):
        raise ValueError("engineering reference quantities must be finite")
    if min(
        reynolds,
        char_len_solver,
        reference_length_m,
        velocity_mps,
        density_kg_m3,
    ) <= 0.0:
        raise ValueError("engineering reference quantities must be positive")

    n = mask.shape[0]
    dx = 2.0 * math.pi / n

    def derivative(field, axis):
        return (np.roll(field, -1, axis=axis) - np.roll(field, 1, axis=axis)) / (
            2.0 * dx
        )

    grad_mask = np.stack([derivative(mask, axis) for axis in range(3)], axis=0)
    surface_density = np.linalg.norm(grad_mask, axis=0)
    normals = grad_mask / np.maximum(surface_density[None], 1e-12)
    grad_velocity = np.empty((3, 3, n, n, n), dtype=np.float64)
    for component in range(3):
        for axis in range(3):
            grad_velocity[component, axis] = derivative(
                velocity[component], axis
            )
    strain = grad_velocity + np.swapaxes(grad_velocity, 0, 1)
    nu_solver = char_len_solver / reynolds
    pressure_traction = pressure[None] * normals
    viscous_traction = -nu_solver * np.einsum(
        "abijk,bijk->aijk", strain, normals
    )
    traction_normalized = pressure_traction + viscous_traction
    area_weight = surface_density * dx**3

    force_pressure = np.sum(
        pressure_traction * area_weight[None], axis=(1, 2, 3)
    )
    force_viscous = np.sum(
        viscous_traction * area_weight[None], axis=(1, 2, 3)
    )
    force = force_pressure + force_viscous
    grid = (np.arange(n, dtype=np.float64) + 0.5) * dx
    coordinates = np.stack(
        np.meshgrid(grid, grid, grid, indexing="ij"), axis=0
    )
    area_sum = max(float(area_weight.sum()), 1e-12)
    surface_centroid = np.sum(
        coordinates * area_weight[None], axis=(1, 2, 3)
    ) / area_sum
    mode = str(moment_origin_mode or "diffuse_surface_centroid").strip().lower()
    if mode in ("", "diffuse_surface_centroid", "diffuse-surface_centroid"):
        mode = "diffuse_surface_centroid"
        moment_origin = surface_centroid.copy()
        moment_warning = (
            "Reported moments use the diffuse-surface area centroid as the reference origin."
        )
    elif mode in ("source_frame_point", "source-frame_point"):
        mode = "source_frame_point"
        if moment_origin_solver is None:
            raise ValueError(
                "source_frame_point moment origin requires moment_origin_solver"
            )
        moment_origin = np.asarray(moment_origin_solver, dtype=np.float64).reshape(3)
        if not np.all(np.isfinite(moment_origin)):
            raise ValueError("moment_origin_solver must be finite")
        moment_warning = (
            "Reported moments use the operator-declared source-frame point "
            "(CG / moment origin) mapped into solver coordinates."
        )
    else:
        raise ValueError(f"unsupported moment_origin_mode: {moment_origin_mode!r}")
    arm = coordinates - moment_origin[:, None, None, None]
    moment_density = np.cross(
        np.moveaxis(arm, 0, -1),
        np.moveaxis(traction_normalized, 0, -1),
    )
    moment = np.sum(
        np.moveaxis(moment_density, -1, 0) * area_weight[None],
        axis=(1, 2, 3),
    )
    # Dimensional loads always scale with L_ref from the nondimensional solver
    # integrals. Coefficient reporting may optionally use an aero reference area.
    force_scale = force / (0.5 * char_len_solver**2)
    moment_scale = moment / (0.5 * char_len_solver**3)

    dynamic_pressure = 0.5 * density_kg_m3 * velocity_mps**2
    pressure_delta_pa = pressure * density_kg_m3 * velocity_mps**2
    cp = pressure_delta_pa / dynamic_pressure
    pressure_pa = (
        reference_pressure_pa
        + pressure_delta_pa
    )
    traction_pa = traction_normalized * density_kg_m3 * velocity_mps**2
    force_newtons = force_scale * dynamic_pressure * reference_length_m**2
    moment_newton_meters = moment_scale * dynamic_pressure * reference_length_m**3
    surface_area_m2 = (
        area_sum * (reference_length_m / char_len_solver) ** 2
    )
    aero_area = None
    if reference_area_m2 is not None:
        aero_area = float(reference_area_m2)
        if not math.isfinite(aero_area) or aero_area <= 0.0:
            raise ValueError("reference_area_m2 must be finite and positive when set")
        force_coefficients = force_newtons / (dynamic_pressure * aero_area)
        moment_coefficients = moment_newton_meters / (
            dynamic_pressure * aero_area * reference_length_m
        )
        coefficient_reference = "q_inf * A_ref ; q_inf * A_ref * L_ref"
    else:
        force_coefficients = force_scale
        moment_coefficients = moment_scale
        coefficient_reference = "q_inf * L_ref^2 ; q_inf * L_ref^3"

    surface = surface_density > max(float(surface_density.max()) * 0.05, 1e-8)
    if not np.any(surface):
        raise ValueError("diffuse mask contains no resolvable surface")
    surface_indices = np.argwhere(surface)
    surface_cp = cp[surface]
    load_hotspot = surface_indices[int(np.argmax(surface_cp))]
    suction_hotspot = surface_indices[int(np.argmin(surface_cp))]
    to_physical = reference_length_m / char_len_solver
    load_hotspot_m = (
        (load_hotspot.astype(np.float64) + 0.5) * dx - moment_origin
    ) * to_physical
    suction_hotspot_m = (
        (suction_hotspot.astype(np.float64) + 0.5) * dx - moment_origin
    ) * to_physical
    divergence = sum(
        derivative(velocity[axis], axis) for axis in range(3)
    )
    divergence_rms = float(np.sqrt(np.mean(divergence**2)))
    speed = np.linalg.norm(velocity, axis=0)
    # Wake detection stays geometric (surface centroid), not CG-relative.
    body_x = float(surface_centroid[0])
    wake_region = (coordinates[0] > body_x + 0.5 * char_len_solver) & (mask < 0.1)
    wake_deficit = np.maximum(0.0, 1.0 - speed)
    if np.any(wake_region):
        wake_deficit_peak = float(wake_deficit[wake_region].max())
        wake_deficit_mean = float(wake_deficit[wake_region].mean())
    else:
        wake_deficit_peak = 0.0
        wake_deficit_mean = 0.0
    pressure_norm = float(np.linalg.norm(force_pressure))
    total_component_norm = pressure_norm + float(np.linalg.norm(force_viscous))
    pressure_force_fraction = (
        pressure_norm / total_component_norm if total_component_norm > 0.0 else 0.0
    )
    warnings = [
        "Pressure is recovered from the predicted velocity field; it is model-derived, not a solver reference.",
        "Diffuse-interface tractions are fluid loads for downstream FEA mapping, not structural stress.",
        moment_warning,
    ]
    return {
        "method": SURFACE_LOAD_METHOD,
        "pressure_pa": pressure_pa.astype(np.float32),
        "cp": cp.astype(np.float32),
        "traction_pa": traction_pa.astype(np.float32),
        "force_coefficients": force_coefficients.astype(float).tolist(),
        "moment_coefficients": moment_coefficients.astype(float).tolist(),
        "force_newtons": force_newtons.astype(float).tolist(),
        "moment_newton_meters": moment_newton_meters.astype(float).tolist(),
        "surface_area_m2": float(surface_area_m2),
        "pressure_force_fraction": pressure_force_fraction,
        "load_hotspot": load_hotspot_m.astype(float).tolist(),
        "suction_hotspot": suction_hotspot_m.astype(float).tolist(),
        "divergence_rms": divergence_rms,
        "wake_deficit_peak": wake_deficit_peak,
        "wake_deficit_mean": wake_deficit_mean,
        "cp_min": float(surface_cp.min()),
        "cp_max": float(surface_cp.max()),
        "moment_origin_mode": mode,
        "moment_origin_solver": moment_origin.astype(float).tolist(),
        "surface_centroid_solver": surface_centroid.astype(float).tolist(),
        "coefficient_reference": coefficient_reference,
        "reference_area_m2": aero_area,
        "warnings": warnings,
    }


# -- model / field ops -------------------------------------------------------
def list_checkpoints(research_dir):
    out = []
    for name in sorted(os.listdir(research_dir)):
        if name.endswith(".reynmodel"):
            out.append(name)
    return out


def _optional_int(value):
    try:
        return int(value) if value is not None else None
    except (TypeError, ValueError):
        return None


def classify_benchmark_seed(seed, training_seed):
    """Classify an exact dataset RNG seed against the checkpoint's reserved streams."""
    seed = int(seed)
    if training_seed is None:
        return "unknown"
    if seed == training_seed:
        return "training"
    if seed == training_seed + 10000:
        return "mixed_fork"
    if seed == training_seed + 50000:
        return "validation_selection"
    return "fresh_test"


def analyze_checkpoint_provenance(checkpoint, benchmark_seeds):
    """Build an evidence-only leak/provenance verdict from checkpoint metadata.

    This deliberately checks exact RNG-stream collisions. It does not claim an
    expensive field-space nearest-neighbour comparison that was not performed.
    """
    train_args = checkpoint.get("train_args")
    train_args = train_args if isinstance(train_args, dict) else {}
    training_seed = _optional_int(train_args.get("seed"))
    if training_seed is not None and training_seed < 0:
        training_seed = None
    dataset = str(train_args.get("dataset", "legacy/unknown"))
    mixed_fork_used = dataset in ("mixed", "viscosity-fork")
    mixed_fork_seed = training_seed + 10000 if training_seed is not None else None
    validation_seed = training_seed + 50000 if training_seed is not None else None

    seed_records = []
    for seed in benchmark_seeds:
        stream = classify_benchmark_seed(seed, training_seed)
        seed_records.append({
            "seed": int(seed),
            "stream": stream,
            "overlap": stream in ("training", "mixed_fork", "validation_selection"),
        })

    epoch = _optional_int(checkpoint.get("epoch"))
    declared_epochs = _optional_int(train_args.get("epochs"))
    role_value = checkpoint.get("checkpoint_role")
    checkpoint_role = (
        str(role_value) if role_value not in (None, "") else "legacy/unknown"
    )
    if epoch is None or declared_epochs is None:
        final_epoch_status = "unknown"
    elif epoch == declared_epochs:
        if checkpoint_role == "fixed_final":
            final_epoch_status = "fixed_final"
        elif checkpoint_role == "best_validation":
            final_epoch_status = "validation_selected_final_epoch"
        else:
            final_epoch_status = "final_epoch_role_unknown"
    elif checkpoint_role == "best_validation":
        final_epoch_status = "validation_selected_nonfinal"
    elif epoch < declared_epochs:
        final_epoch_status = "nonfinal_epoch"
    else:
        final_epoch_status = "epoch_out_of_contract"

    source = checkpoint.get("source_fingerprint")
    source_digest = source.get("digest") if isinstance(source, dict) else None
    source_present = isinstance(source_digest, str) and bool(source_digest)
    best_metric_value = checkpoint.get("best_metric")
    best_metric = (
        str(best_metric_value)
        if best_metric_value not in (None, "")
        else (
            "not_applicable_fixed_epoch"
            if checkpoint_role == "fixed_final"
            else "legacy/unknown"
        )
    )
    metric_lower = best_metric.lower()
    if checkpoint_role == "fixed_final":
        selection_stream = "fixed_epoch"
    elif checkpoint_role == "best_validation":
        selection_stream = "validation"
    elif "val" in metric_lower:
        selection_stream = "validation"
    elif "train" in metric_lower:
        selection_stream = "training"
    else:
        selection_stream = "unknown"

    unknown = []
    if training_seed is None:
        unknown.append("training seed absent")
    if checkpoint_role == "legacy/unknown":
        unknown.append("checkpoint role absent")
    if not source_present:
        unknown.append("source fingerprint absent")
    if (
        best_metric == "legacy/unknown"
        and checkpoint_role != "fixed_final"
    ):
        unknown.append("checkpoint selection metric absent")
    if epoch is None or declared_epochs is None:
        unknown.append("final epoch status unavailable")

    flags = []
    overlaps = [record for record in seed_records if record["overlap"]]
    if overlaps:
        streams = ", ".join(
            f"{record['seed']}={record['stream']}" for record in overlaps
        )
        flags.append(f"benchmark seeds overlap reserved streams: {streams}")
    if len({record["seed"] for record in seed_records}) != len(seed_records):
        flags.append("benchmark seeds contain duplicates")
    if (
        checkpoint_role == "fixed_final"
        and declared_epochs is not None
        and epoch != declared_epochs
    ):
        flags.append("fixed_final role conflicts with checkpoint epoch")
    if final_epoch_status == "epoch_out_of_contract":
        flags.append("checkpoint epoch exceeds declared training epochs")

    verdict = "flagged" if flags else ("unknown" if unknown else "clean")
    overlap_pct = 100.0 * len(overlaps) / max(1, len(seed_records))
    return {
        "verdict": verdict,
        "training_seed": training_seed,
        "mixed_fork_seed": mixed_fork_seed,
        "mixed_fork_used": mixed_fork_used,
        "validation_seed": validation_seed,
        "dataset": dataset,
        "benchmark_seeds": seed_records,
        "overlap_count": len(overlaps),
        "overlap_pct": overlap_pct,
        "epoch": epoch,
        "declared_epochs": declared_epochs,
        "checkpoint_role": checkpoint_role,
        "final_epoch_status": final_epoch_status,
        "selection_metric": best_metric,
        "selection_stream": selection_stream,
        "source_fingerprint_present": source_present,
        "source_fingerprint_digest": source_digest if source_present else None,
        "legacy_unknown": unknown,
        "flags": flags,
    }


def radial_energy_spectrum(field):
    """Radially averaged kinetic-energy spectrum for a `[2,N,N]` velocity field."""
    field = np.asarray(field, dtype=np.float64)
    if field.ndim != 3 or field.shape[0] < 2 or field.shape[1] != field.shape[2]:
        raise ValueError("energy spectrum requires a [2,N,N] velocity field")
    n = field.shape[-1]
    u_hat = np.fft.fft2(field[0])
    v_hat = np.fft.fft2(field[1])
    energy = (np.abs(u_hat) ** 2 + np.abs(v_hat) ** 2) / (n * n)
    kfreq = np.fft.fftfreq(n) * n
    kx, ky = np.meshgrid(kfreq, kfreq)
    kmag = np.sqrt(kx ** 2 + ky ** 2)
    kbins = np.arange(0.5, n // 2 + 1, 1.0)
    kvals = 0.5 * (kbins[1:] + kbins[:-1])
    shell_energy, _ = np.histogram(kmag, bins=kbins, weights=energy)
    counts, _ = np.histogram(kmag, bins=kbins)
    spectrum = shell_energy / np.maximum(counts, 1)
    return kvals.astype(np.float32), spectrum.astype(np.float32)


def divergence_rms(field):
    """Spectral RMS divergence for a periodic `[2,N,N]` velocity field."""
    field = np.asarray(field, dtype=np.float64)
    if field.ndim != 3 or field.shape[0] < 2 or field.shape[1] != field.shape[2]:
        raise ValueError("divergence requires a [2,N,N] velocity field")
    n = field.shape[-1]
    modes = np.fft.fftfreq(n, d=1.0 / n)
    kx = modes.reshape(1, n)
    ky = modes.reshape(n, 1)
    div_hat = 1j * kx * np.fft.fft2(field[0])
    div_hat += 1j * ky * np.fft.fft2(field[1])
    divergence = np.fft.ifft2(div_hat).real
    return float(np.sqrt(np.mean(divergence ** 2)))


def benchmark_cell_evidence(prediction, truth, initial):
    """Compute the scalar maps, spectra, and calibrated metrics for one suite cell."""
    prediction = np.asarray(prediction, dtype=np.float32)
    truth = np.asarray(truth, dtype=np.float32)
    initial = np.asarray(initial, dtype=np.float32)
    if prediction.shape != truth.shape or prediction.shape != initial.shape:
        raise ValueError("prediction, truth, and initial fields must share a shape")
    if prediction.ndim != 3 or prediction.shape[0] != 2:
        raise ValueError("benchmark evidence requires [2,N,N] velocity fields")

    truth_norm = max(float(np.linalg.norm(truth)), 1e-12)
    rel_l2 = float(np.linalg.norm(prediction - truth) / truth_norm)
    persist_rel_l2 = float(np.linalg.norm(initial - truth) / truth_norm)
    error_map = np.sqrt(np.sum((prediction - truth) ** 2, axis=0))
    model_speed = np.sqrt(np.sum(prediction ** 2, axis=0))
    truth_speed = np.sqrt(np.sum(truth ** 2, axis=0))
    spectrum_k, spectrum_model = radial_energy_spectrum(prediction)
    _, spectrum_truth = radial_energy_spectrum(truth)
    spectrum_norm = max(float(np.linalg.norm(spectrum_truth)), 1e-20)
    spectrum_rel_l2 = float(
        np.linalg.norm(spectrum_model - spectrum_truth) / spectrum_norm
    )
    return {
        "rel_l2": rel_l2,
        "persist_rel_l2": persist_rel_l2,
        "improvement_ratio": persist_rel_l2 / max(rel_l2, 1e-12),
        "mean_abs_error": float(error_map.mean()),
        "p95_abs_error": float(np.quantile(error_map, 0.95)),
        "max_abs_error": float(error_map.max()),
        "divergence_model_rms": divergence_rms(prediction),
        "divergence_truth_rms": divergence_rms(truth),
        "divergence_error_rms": divergence_rms(prediction - truth),
        "spectrum_rel_l2": spectrum_rel_l2,
        "spectrum_k": spectrum_k,
        "spectrum_model": spectrum_model,
        "spectrum_truth": spectrum_truth,
        "model_speed": model_speed.astype(np.float32),
        "truth_speed": truth_speed.astype(np.float32),
        "error_map": error_map.astype(np.float32),
    }


class Engine:
    def __init__(self, research_dir, requested_device="auto", managed_model_dir=None):
        self.research_dir = str(Path(research_dir).expanduser().resolve())
        self._managed_model_dir = (
            Path(managed_model_dir).expanduser().resolve()
            if managed_model_dir
            else Path(self.research_dir) / "reyn_models"
        )
        engine_dir = str(Path(__file__).resolve().parent)
        sys.path[:] = [
            engine_dir,
            self.research_dir,
            *[
                entry
                for entry in sys.path
                if entry not in {engine_dir, self.research_dir}
            ],
        ]
        os.chdir(research_dir)
        import torch  # noqa: imported lazily so startup errors are reported cleanly
        self.torch = torch
        requested_device = str(requested_device).lower()
        if requested_device == "auto":
            if torch.cuda.is_available():
                selected_device = "cuda"
            elif torch.backends.mps.is_available():
                selected_device = "mps"
            else:
                selected_device = "cpu"
        elif requested_device == "mps":
            if not torch.backends.mps.is_available():
                raise RuntimeError("MPS was requested but is not available on this Mac")
            selected_device = "mps"
        elif requested_device == "cpu":
            selected_device = "cpu"
        else:
            raise ValueError(
                f"unsupported compute device {requested_device!r}; use auto, mps, or cpu"
            )
        self.device = torch.device(selected_device)
        self.cache = {}
        self.traj2d = {}  # (model, seed) -> (y0, mask, trajectory, physics context)
        self.cad_cache = {}  # (model, mask_digest) -> developed field (post-warmup)

    @property
    def managed_model_dir(self):
        return self._managed_model_dir

    @property
    def model_trust_state_dir(self):
        return self.managed_model_dir / ".tuf-trusted-state"

    def _model_id(self, path):
        path = Path(path).resolve()
        try:
            return path.relative_to(Path(self.research_dir).resolve()).as_posix()
        except ValueError:
            try:
                return (
                    Path("reyn_models")
                    / path.relative_to(self.managed_model_dir.resolve())
                ).as_posix()
            except ValueError:
                return str(path)

    @staticmethod
    def _checkpoint_sha256(path):
        digest = hashlib.sha256()
        with Path(path).open("rb") as checkpoint_file:
            for chunk in iter(lambda: checkpoint_file.read(1024 * 1024), b""):
                digest.update(chunk)
        return digest.hexdigest()

    @staticmethod
    def _validation(card):
        issues = list(card.get("validation_issues") or [])
        accepted = card.get("status") != "invalid"
        return {
            "accepted": accepted,
            "status": "accepted" if accepted else "rejected",
            "summary": card.get("status_detail") or "checkpoint validation completed",
            "issues": issues,
            "candidate": {
                "name": card.get("name"),
                "checkpoint_sha256": card.get("checkpoint_sha256"),
            },
        }

    def _bundle_model_card(self, path, *, managed=False):
        """Verify a non-pickle inference bundle and return its model card."""
        path = Path(path).expanduser().resolve()
        stat = path.stat()
        bundle_sha256 = (
            self._checkpoint_sha256(path)
            if stat.st_size <= 2 * 1024**3
            else None
        )
        card = {
            "id": self._model_id(path),
            "name": path.name,
            "managed": bool(managed),
            "size_bytes": int(stat.st_size),
            "modified_unix": int(stat.st_mtime),
            "checkpoint_sha256": bundle_sha256,
            "status": "invalid",
            "status_detail": "",
            "dimension": 0,
            "grid": 0,
            "in_channels": 0,
            "out_channels": 0,
            "max_steps": 0,
            "epoch": 0,
            "declared_epochs": 0,
            "checkpoint_role": "unknown",
            "scenario": "unknown",
            "source_digest": None,
            "physics_contract": "unknown",
            "authenticity_status": "unverified",
            "publisher_key_id": None,
            "publisher_key_sha256": None,
            "release_sequence": None,
            "tuf_target_path": None,
            "tuf_metadata_versions": None,
            "support": [],
            "limitations": [],
            "benchmark_report_hashes": [],
            "unknown_fields": [],
            "fact_sources": {},
            "validation_issues": [],
        }
        try:
            from model_bundle import ModelBundleError, load_model_bundle

            loaded = load_model_bundle(
                path,
                trusted_state_dir=self.model_trust_state_dir,
                development_allow_unsigned=os.environ.get(
                    "REYN_MODEL_DEVELOPMENT_UNSIGNED", ""
                ).strip().lower()
                in {"1", "true", "yes"},
            )
            manifest = loaded.manifest
            authenticity = loaded.authenticity
        except Exception as exc:
            if "ModelBundleError" in locals() and isinstance(exc, ModelBundleError):
                code, field, message = exc.code, exc.field, exc.message
            else:
                code, field, message = (
                    "bundle.validation_failed",
                    "bundle",
                    f"bundle validation failed: {exc}",
                )
            card["validation_issues"].append(
                {
                    "code": code,
                    "field": field,
                    "message": message,
                    "severity": "error",
                }
            )
            card["status_detail"] = message
            return card

        architecture = manifest["architecture"]
        config = architecture["config"]
        support_envelope = manifest["support_envelope"]
        source = manifest["source_training"]
        conditioning = manifest["conditioning"]
        dimension = int(support_envelope["dimension"])
        grid = int(support_envelope["grid_size"])
        max_steps = int(support_envelope["horizon_steps"]["max"])
        role = source["checkpoint_role"]
        epoch = int(source["epoch"])
        declared_epochs = int(source["declared_epochs"])
        fixed_final = (
            role in ("fixed_final", "fixed_final_raw", "fixed_final_ema")
            and epoch == declared_epochs
        )
        support_lines = [
            f"{dimension}D · {grid}^{dimension} grid",
            f"{config['in_channels']} input → {config['out_channels']} output channels",
            f"declared horizon 1–{max_steps} steps",
            f"declared regime: {support_envelope['scenario']}",
            f"physics contract: {conditioning['physics_id']}",
        ]
        if fixed_final:
            status = "clean"
            status_detail = (
                "authenticated TUF target + Ed25519 publisher · verified "
                "non-pickle bundle · strict tensor contract · fixed final source identity"
            )
            unknown_fields = []
        else:
            status = "review"
            status_detail = (
                "authenticated TUF target + Ed25519 publisher · verified "
                "non-pickle bundle · strict tensor contract · final checkpoint "
                "role/epoch unverified"
            )
            unknown_fields = ["fixed_final_source_status"]
        card.update(
            {
                "status": status,
                "status_detail": status_detail,
                "dimension": dimension,
                "grid": grid,
                "in_channels": int(config["in_channels"]),
                "out_channels": int(config["out_channels"]),
                "max_steps": max_steps,
                "epoch": epoch,
                "declared_epochs": declared_epochs,
                "checkpoint_role": role,
                "scenario": support_envelope["scenario"],
                "source_digest": source["source_fingerprint"]["digest"],
                "physics_contract": conditioning["physics_id"],
                "authenticity_status": authenticity["status"],
                "publisher_key_id": authenticity["key_id"],
                "publisher_key_sha256": authenticity["public_key_sha256"],
                "release_sequence": authenticity["release_sequence"],
                "tuf_target_path": authenticity["tuf_target_path"],
                "tuf_metadata_versions": authenticity["tuf_metadata_versions"],
                "support": support_lines,
                "limitations": list(manifest["limitations"]),
                "benchmark_report_hashes": list(manifest["benchmark_reports"]),
                "unknown_fields": unknown_fields,
                "version": str(manifest.get("model", {}).get("version", "")),
                "qualification_class": derive_model_qualification_class(
                    manifest.get("model", {}).get("version", ""),
                    manifest.get("limitations", []),
                    status,
                ),
                "fact_sources": {
                    "dimension": "verified_bundle_manifest_and_tensor_schema",
                    "grid": "verified_bundle_manifest",
                    "channels": "verified_bundle_manifest_and_tensor_schema",
                    "horizon": "verified_bundle_manifest",
                    "scenario": "verified_bundle_manifest",
                    "integrity": "verified_sha256",
                    "authenticity": "verified_tuf_target_and_ed25519_signature",
                },
            }
        )
        return card

    def checkpoint_card(self, path, *, managed=False):
        """Verify safe bundles; reject pickle-backed checkpoints without opening."""
        path = Path(path).expanduser().resolve()
        if path.suffix.lower() == ".reynmodel":
            return self._bundle_model_card(path, managed=managed)
        stat = path.stat()
        card = {
            "id": self._model_id(path),
            "name": path.name,
            "managed": bool(managed),
            "size_bytes": int(stat.st_size),
            "modified_unix": int(stat.st_mtime),
            "checkpoint_sha256": None,
            "status": "invalid",
            "status_detail": "",
            "dimension": 0,
            "grid": 0,
            "in_channels": 0,
            "out_channels": 0,
            "max_steps": 0,
            "epoch": 0,
            "declared_epochs": 0,
            "checkpoint_role": "unknown",
            "scenario": "unknown",
            "source_digest": None,
            "physics_contract": "unknown",
            "support": [],
            "limitations": [],
            "benchmark_report_hashes": [],
            "unknown_fields": [],
            "fact_sources": {},
            "validation_issues": [],
        }
        issues = card["validation_issues"]

        def issue(code, field, message, severity="error"):
            issues.append(
                {
                    "code": code,
                    "field": field,
                    "message": message,
                    "severity": severity,
                }
            )

        def reject():
            card["status"] = "invalid"
            card["status_detail"] = next(
                (
                    validation_issue["message"]
                    for validation_issue in issues
                    if validation_issue["severity"] == "error"
                ),
                "checkpoint is incompatible",
            )
            return card

        issue(
            "checkpoint.unsafe_pickle_disabled",
            "checkpoint",
            "pickle-backed checkpoints are disabled in the Reyn Studio runtime; "
            "convert a trusted checkpoint offline with convert_model_bundle.py",
        )
        return reject()

        if not isinstance(checkpoint, dict):
            issue(
                "checkpoint.invalid_root",
                "checkpoint",
                "checkpoint root must be a mapping",
            )
            return reject()

        config = checkpoint.get("model_config")
        state = checkpoint.get("model_state_dict")
        train_args = checkpoint.get("train_args")
        missing = [
            name
            for name, value in (
                ("model_config", config),
                ("model_state_dict", state),
                ("train_args", train_args),
            )
            if not isinstance(value, dict)
        ]
        if missing:
            for field in missing:
                issue(
                    "checkpoint.missing_field",
                    field,
                    f"missing required {field}",
                )
            return reject()
        tensors = [value for value in state.values() if hasattr(value, "dim")]
        if not tensors:
            issue(
                "checkpoint.empty_state",
                "model_state_dict",
                "model_state_dict contains no tensors",
            )
            return reject()

        is3d = any(value.dim() == 5 for value in tensors)
        dimension = 3 if is3d else 2
        in_channels = _optional_int(config.get("in_channels"))
        out_channels = _optional_int(config.get("out_channels")) or in_channels
        param_dim = _optional_int(config.get("param_dim")) or 0
        grid = _optional_int(train_args.get("grid_size"))
        max_steps = _optional_int(train_args.get("max_steps"))
        scenario_value = train_args.get("scenario")
        scenario = (
            str(scenario_value)
            if scenario_value not in (None, "")
            else "unknown"
        )
        inferred_scenario = (
            scenario
            if scenario != "unknown"
            else (
                "obstacle"
                if in_channels is not None
                and out_channels is not None
                and in_channels > out_channels
                else "free"
            )
        )

        for field, value in (
            ("model_config.in_channels", in_channels),
            ("model_config.out_channels", out_channels),
            ("train_args.grid_size", grid),
            ("train_args.max_steps", max_steps),
        ):
            if value is None or value <= 0:
                issue(
                    "contract.missing_positive_value",
                    field,
                    f"{field} must be a positive integer",
                )
        for field in ("dt", "stride", "warmup_steps"):
            if train_args.get(field) is None:
                issue(
                    "runtime.missing_setting",
                    f"train_args.{field}",
                    f"train_args.{field} is required for local execution",
                )
        if inferred_scenario not in ("free", "obstacle"):
            issue(
                "contract.unsupported_scenario",
                "train_args.scenario",
                f"unsupported scenario {inferred_scenario!r}",
            )

        physics_spec = checkpoint.get("physics_spec")
        physics_id = (
            str(physics_spec.get("physics_id"))
            if isinstance(physics_spec, dict) and physics_spec.get("physics_id")
            else "legacy/unknown"
        )
        if is3d:
            supported_contract = (
                (in_channels, out_channels, param_dim)
                in ((3, 3, 0), (4, 3, 0), (5, 3, 0))
                and (
                    (inferred_scenario == "free" and in_channels == 3)
                    or (inferred_scenario == "obstacle" and in_channels in (4, 5))
                )
            )
            if train_args.get("nu") is None:
                issue(
                    "runtime.missing_setting",
                    "train_args.nu",
                    "train_args.nu is required for 3D local execution",
                )
        else:
            supported_contract = (in_channels, out_channels, param_dim) in (
                (2, 2, 0),
                (3, 2, 0),
                (4, 2, 1),
            )
            if (in_channels, out_channels, param_dim) == (4, 2, 1):
                support = (
                    physics_spec.get("support")
                    if isinstance(physics_spec, dict)
                    else None
                )
                nu_bounds = support.get("nu") if isinstance(support, dict) else None
                if (
                    physics_id != "fixed_body_brinkman.v2"
                    or not isinstance(nu_bounds, (list, tuple))
                    or len(nu_bounds) != 2
                    or float(support.get("sponge_strength", 0.0)) <= 0.0
                ):
                    issue(
                        "contract.missing_physics_spec",
                        "physics_spec",
                        "parameter-conditioned checkpoint requires fixed_body_v2 support metadata",
                    )
        if not supported_contract:
            issue(
                "contract.unsupported_channels",
                "model_config",
                "unsupported checkpoint contract: "
                f"{dimension}D in={in_channels}, out={out_channels}, params={param_dim}",
            )

        source = checkpoint.get("source_fingerprint")
        source_digest = source.get("digest") if isinstance(source, dict) else None
        role = str(checkpoint.get("checkpoint_role") or "legacy/unknown")
        epoch = _optional_int(checkpoint.get("epoch")) or 0
        declared_epochs = _optional_int(train_args.get("epochs")) or 0
        fixed_final = (
            role == "fixed_final"
            and declared_epochs > 0
            and epoch == declared_epochs
        )
        source_present = isinstance(source_digest, str) and bool(source_digest)
        limitations_value = checkpoint.get("limitations")
        limitations = (
            [str(value) for value in limitations_value if str(value).strip()]
            if isinstance(limitations_value, (list, tuple))
            else []
        )
        reports_value = checkpoint.get("benchmark_reports")
        reports_value = (
            reports_value if isinstance(reports_value, (list, tuple)) else []
        )
        report_hashes = []
        for report in reports_value:
            digest = report.get("sha256") if isinstance(report, dict) else report
            digest = str(digest).lower() if digest is not None else ""
            if len(digest) == 64 and all(char in "0123456789abcdef" for char in digest):
                report_hashes.append(digest)
            else:
                issue(
                    "report.invalid_hash",
                    "benchmark_reports",
                    "ignored benchmark report without a canonical SHA-256 hash",
                    severity="warning",
                )

        unknown_fields = []
        if not source_present:
            unknown_fields.append("source_fingerprint")
        if role == "legacy/unknown":
            unknown_fields.append("checkpoint_role")
        if scenario == "unknown":
            unknown_fields.append("scenario")
        if not limitations:
            unknown_fields.append("limitations")
        if not report_hashes:
            unknown_fields.append("benchmark_reports")

        support_lines = []
        if grid and dimension:
            support_lines.append(f"{dimension}D · {grid}^{dimension} grid")
        if in_channels and out_channels:
            support_lines.append(f"{in_channels} input → {out_channels} output channels")
        if max_steps:
            support_lines.append(f"declared horizon 1–{max_steps} steps")
        if scenario != "unknown":
            support_lines.append(f"declared regime: {scenario}")
        if physics_id != "legacy/unknown":
            support_lines.append(f"physics contract: {physics_id}")

        errors = [value for value in issues if value["severity"] == "error"]
        if errors:
            status = "invalid"
            status_detail = errors[0]["message"]
        elif fixed_final and source_present:
            status = "clean"
            status_detail = "compatible contract · fixed final epoch · source fingerprint present"
        else:
            gaps = []
            if not fixed_final:
                gaps.append("final checkpoint role/epoch unverified")
            if not source_present:
                gaps.append("source fingerprint absent")
            status = "review"
            status_detail = "compatible contract · " + " · ".join(gaps)

        card.update(
            {
                "status": status,
                "status_detail": status_detail,
                "dimension": dimension,
                "grid": int(grid or 0),
                "in_channels": int(in_channels or 0),
                "out_channels": int(out_channels or 0),
                "max_steps": int(max_steps or 0),
                "epoch": epoch,
                "declared_epochs": declared_epochs,
                "checkpoint_role": role,
                "scenario": scenario,
                "source_digest": source_digest if source_present else None,
                "physics_contract": physics_id,
                "support": support_lines,
                "limitations": limitations,
                "benchmark_report_hashes": report_hashes,
                "unknown_fields": unknown_fields,
                "qualification_class": derive_model_qualification_class(
                    checkpoint.get("model_version") or path.name,
                    limitations,
                    status,
                ),
                "fact_sources": {
                    "dimension": "inspected_state_dict",
                    "grid": "checkpoint_metadata" if grid else "unknown",
                    "channels": "checkpoint_metadata" if in_channels else "unknown",
                    "horizon": "checkpoint_metadata" if max_steps else "unknown",
                    "scenario": (
                        "checkpoint_metadata" if scenario != "unknown" else "unknown"
                    ),
                },
            }
        )
        return card

    def list_model_cards(self):
        root = Path(self.research_dir)
        managed = self.managed_model_dir
        paths = [(path, False) for path in sorted(root.glob("*.reynmodel"))]
        if managed.is_dir():
            paths.extend(
                (path, True) for path in sorted(managed.glob("*.reynmodel"))
            )
        return [
            self.checkpoint_card(path, managed=is_managed)
            for path, is_managed in paths
        ]

    def _probe_checkpoint_compatibility(self, path):
        """Instantiate and load weights without changing the active model/cache."""
        cache_key = str(Path(path).expanduser().resolve())
        try:
            self._load(cache_key)
        finally:
            self.cache.pop(cache_key, None)

    def import_model(self, source):
        source = Path(source).expanduser().resolve()
        if source.suffix.lower() != ".reynmodel":
            issue = {
                "code": "bundle.invalid_extension",
                "field": "path",
                "message": (
                    "production model import requires a .reynmodel bundle; "
                    "convert trusted .pth checkpoints offline with "
                    "convert_model_bundle.py"
                ),
                "severity": "error",
            }
            return {
                "ok": False,
                "error": issue["message"],
                "validation": {
                    "accepted": False,
                    "status": "rejected",
                    "summary": issue["message"],
                    "issues": [issue],
                    "candidate": {"name": source.name, "checkpoint_sha256": None},
                },
            }
        if not source.is_file():
            issue = {
                "code": "checkpoint.not_found",
                "field": "path",
                "message": f"checkpoint does not exist: {source}",
                "severity": "error",
            }
            return {
                "ok": False,
                "error": issue["message"],
                "validation": {
                    "accepted": False,
                    "status": "rejected",
                    "summary": issue["message"],
                    "issues": [issue],
                    "candidate": {"name": source.name, "checkpoint_sha256": None},
                },
            }
        candidate = self.checkpoint_card(source)
        if candidate["status"] == "invalid":
            validation = self._validation(candidate)
            return {
                "ok": False,
                "error": f"checkpoint rejected: {candidate['status_detail']}",
                "validation": validation,
            }
        try:
            self._probe_checkpoint_compatibility(source)
        except Exception as exc:
            issue = {
                "code": "checkpoint.load_incompatible",
                "field": "model_state_dict",
                "message": f"checkpoint cannot load into a supported Reyn model: {exc}",
                "severity": "error",
            }
            candidate["status"] = "invalid"
            candidate["status_detail"] = issue["message"]
            candidate["validation_issues"].append(issue)
            return {
                "ok": False,
                "error": f"checkpoint rejected: {issue['message']}",
                "validation": self._validation(candidate),
            }

        target_dir = self.managed_model_dir
        target_dir.mkdir(parents=True, exist_ok=True)
        target = target_dir / source.name
        target_signature = target.with_name(target.name + ".sig")
        target_tuf = target.with_name(target.name + ".tuf")
        if target.exists() or target_signature.exists() or target_tuf.exists():
            issue = {
                "code": "bundle.import_collision",
                "field": "path",
                "message": (
                    "authenticated model filenames are immutable; remove or select "
                    f"the existing managed target before importing {target.name}"
                ),
                "severity": "error",
            }
            candidate["status"] = "invalid"
            candidate["status_detail"] = issue["message"]
            candidate["validation_issues"].append(issue)
            return {
                "ok": False,
                "error": f"checkpoint rejected: {issue['message']}",
                "validation": self._validation(candidate),
            }
        source_signature = source.with_name(source.name + ".sig")
        source_tuf = source.with_name(source.name + ".tuf")
        temporary_bundle = target.with_name(f".{target.name}.importing")
        temporary_signature = target_signature.with_name(
            f".{target_signature.name}.importing"
        )
        temporary_tuf = target_tuf.with_name(f".{target_tuf.name}.importing")
        try:
            shutil.copy2(source, temporary_bundle)
            shutil.copy2(source_signature, temporary_signature)
            shutil.copytree(source_tuf, temporary_tuf)
            os.replace(temporary_tuf, target_tuf)
            os.replace(temporary_signature, target_signature)
            os.replace(temporary_bundle, target)
        except Exception:
            target.unlink(missing_ok=True)
            target_signature.unlink(missing_ok=True)
            shutil.rmtree(target_tuf, ignore_errors=True)
            raise
        finally:
            temporary_bundle.unlink(missing_ok=True)
            temporary_signature.unlink(missing_ok=True)
            shutil.rmtree(temporary_tuf, ignore_errors=True)
        imported = self.checkpoint_card(target, managed=True)
        if imported["status"] == "invalid":
            target.unlink(missing_ok=True)
            target_signature.unlink(missing_ok=True)
            shutil.rmtree(target_tuf, ignore_errors=True)
            return {
                "ok": False,
                "error": f"checkpoint rejected after import: {imported['status_detail']}",
                "validation": self._validation(imported),
            }
        return {
            "ok": True,
            "validation": self._validation(imported),
            "imported": imported,
            "models": self.list_model_cards(),
        }

    def delete_model(self, model_id):
        managed = self.managed_model_dir.resolve()
        requested = Path(model_id)
        target = (
            managed / requested.name
            if requested.parts[:1] == ("reyn_models",)
            else Path(self.research_dir) / requested
        ).resolve()
        if target.parent != managed or target.suffix.lower() != ".reynmodel":
            raise ValueError("only model bundles imported into Reyn Studio can be deleted")
        if not target.is_file():
            raise ValueError(f"managed checkpoint not found: {model_id}")
        target.unlink()
        target.with_name(target.name + ".sig").unlink(missing_ok=True)
        shutil.rmtree(target.with_name(target.name + ".tuf"), ignore_errors=True)
        target_id = self._model_id(target)
        self.cache = {
            key: value
            for key, value in self.cache.items()
            if self._model_id(Path(key)) != target_id
        }
        self.traj2d = {
            key: value for key, value in self.traj2d.items() if key[0] != model_id
        }
        self.cad_cache = {
            key: value for key, value in self.cad_cache.items() if key[0] != model_id
        }
        return self.list_model_cards()

    def _load(self, path):
        if path in self.cache:
            return self.cache[path]
        bundle_path = Path(path).expanduser().resolve()
        if bundle_path.suffix.lower() != ".reynmodel":
            raise ValueError(
                "production inference requires a verified .reynmodel bundle; "
                "pickle checkpoints are never opened by the Reyn Studio runtime"
            )
        from model_bundle import load_model_bundle

        loaded = load_model_bundle(
            bundle_path,
            trusted_state_dir=self.model_trust_state_dir,
            development_allow_unsigned=os.environ.get(
                "REYN_MODEL_DEVELOPMENT_UNSIGNED", ""
            ).strip().lower()
            in {"1", "true", "yes"},
        )
        manifest = loaded.manifest
        m = loaded.model
        cfg = dict(manifest["architecture"]["config"])
        support = manifest["support_envelope"]
        source = manifest["source_training"]
        conditioning = manifest["conditioning"]
        integration = support["time_integration"]
        is3d = support["dimension"] == 3
        ta = {
            "grid_size": support["grid_size"],
            "max_steps": support["horizon_steps"]["max"],
            "dt": integration["solver_dt"],
            "stride": integration["stride"],
            "warmup_steps": integration["warmup_steps"],
            "scenario": support["scenario"],
            "seed": source["training_seed"],
            "dataset": source["dataset"],
            "epochs": source["declared_epochs"],
        }
        if is3d:
            ta["nu"] = support["physics"]["kinematic_viscosity"]
        physics_spec = {
            "physics_id": conditioning["physics_id"],
            "state_channels": manifest["io_schema"]["dynamic_inputs"],
            "spatial_conditions": manifest["io_schema"]["spatial_conditions"],
            "global_conditions": manifest["io_schema"]["global_conditions"],
            "output_channels": manifest["io_schema"]["outputs"],
            "support": dict(support["physics"]),
        }
        m.eval()
        m.to(self.device)
        checkpoint_meta = {
            "train_args": dict(ta),
            "epoch": source["epoch"],
            "checkpoint_role": source["checkpoint_role"],
            "best_metric": None,
            "source_fingerprint": dict(source["source_fingerprint"]),
        }
        info = {"model": m, "cfg": cfg, "ta": ta, "is3d": is3d,
                "epoch": source["epoch"], "checkpoint_meta": checkpoint_meta,
                "physics_spec": physics_spec,
                "scenario": support["scenario"]}
        self.cache[path] = info
        return info

    def _normalized_log_viscosity_channel(self, nu, bounds, like):
        torch = self.torch
        low, high = float(bounds[0]), float(bounds[1])
        value = float(nu)
        if not 0.0 < low < high:
            raise ValueError("invalid viscosity normalization bounds")
        if not math.isfinite(value) or value <= 0.0:
            raise ValueError("viscosity must be finite and positive")
        if value < low or value > high:
            raise ValueError(
                f"viscosity {value:g} leaves model bounds [{low:g}, {high:g}]"
            )
        mapped = 2.0 * (math.log(value) - math.log(low)) / (
            math.log(high) - math.log(low)
        ) - 1.0
        return torch.full(
            (like.shape[0], 1, like.shape[2], like.shape[3], like.shape[4]),
            mapped,
            dtype=like.dtype,
            device=like.device,
        )

    def _signed_log_viscosity_bounds(self, info):
        support = ((info.get("physics_spec") or {}).get("support") or {})
        nu = support.get("nu")
        if isinstance(nu, (list, tuple)) and len(nu) == 2:
            lo, hi = float(nu[0]), float(nu[1])
            if hi > lo > 0.0:
                return (lo, hi)
        raise ValueError(
            "5-channel 3D packing requires signed support_envelope.physics.nu bounds"
        )

    def _pack_obstacle_3d(self, info, velocity, mask, nu=None):
        torch = self.torch
        in_channels = int(info["cfg"].get("in_channels", 4))
        if mask.ndim == 4:
            mask = mask.unsqueeze(1) if mask.shape[1] != 1 else mask
        if in_channels == 4:
            return torch.cat([velocity, mask], dim=1)
        if in_channels == 5:
            if nu is None:
                nu = info["ta"].get("nu")
            log_nu = self._normalized_log_viscosity_channel(
                nu, self._signed_log_viscosity_bounds(info), velocity
            )
            packed = torch.cat([velocity, mask, log_nu], dim=1)
            if packed.size(1) != 5:
                raise ValueError("five-channel 3D packing failed")
            return packed
        raise ValueError(
            f"unsupported 3D obstacle packing {in_channels}→"
            f"{info['cfg'].get('out_channels')}"
        )

    def predict_field(self, req):
        torch = self.torch
        path = req["model"]
        info = self._load(path)
        m, ta, is3d = info["model"], info["ta"], info["is3d"]
        scenario = info["scenario"]
        dt_frame = ta["dt"] * ta["stride"]
        horizon = int(req.get("steps", ta["max_steps"]))
        seed = int(req.get("seed", 1))

        if is3d:
            from dataset_3d import FlowDataset3D
            N = ta["grid_size"]
            ds = FlowDataset3D(scenario=scenario, num_trajectories=1,
                               trajectory_length=horizon + 1, N=N, solver_dt=ta["dt"],
                               nu=ta["nu"], warmup_steps=ta["warmup_steps"],
                               seq_len=horizon + 1, stride=ta["stride"], seed=seed + 50000)
            y0 = ds.trajectories[0][0:1]
            mask = ds.masks[0].unsqueeze(0)
            model_in = (
                self._pack_obstacle_3d(info, y0, mask, nu=ta.get("nu"))
                if scenario == "obstacle"
                else y0
            )
            with torch.no_grad():
                pred = m(model_in.to(self.device), torch.tensor([[horizon * dt_frame]], device=self.device))
            field = pred[0].cpu().contiguous().numpy().astype(np.float32)  # [3, N, N, N]
        else:
            # 2D → return a [3, N, N] field (w-channel zero) so the client is uniform.
            y0, mask, _, context = self._traj2d(path, seed, horizon + 1)
            with torch.no_grad():
                pred = self._run_model_2d(
                    info, y0, mask, context, horizon
                )
            N = ta["grid_size"]
            uv = pred[0].cpu().numpy().astype(np.float32)  # [2, N, N]
            field = np.concatenate([uv, np.zeros((1, N, N), np.float32)], 0)

        meta = {"ok": True, "shape": list(field.shape), "scenario": scenario,
                "dims": field.ndim - 1, "horizon": horizon}
        return field, meta

    def _traj2d(self, model, seed, need_len, seed_offset=50000):
        """Cached solver trajectory for a 2D model, so TimeJump only re-runs the
        (fast) model forward pass per horizon instead of re-solving from scratch.

        Interactive TimeJump defaults to the checkpoint-selection stream
        (`seed_offset=50000`). Benchmark callers pass zero and supply exact,
        independently classified test seeds.
        """
        effective_seed = int(seed) + int(seed_offset)
        key = (model, effective_seed)
        cached = self.traj2d.get(key)
        if cached is not None and cached[2].shape[0] >= need_len:
            return cached
        info = self._load(model)
        ta = info["ta"]
        N = ta["grid_size"]
        from obstacle_dataset import ObstacleFlowDataset
        length = max(need_len, 33)  # cover a full scrub after the first (slow) gen
        ds = ObstacleFlowDataset(num_trajectories=1, trajectory_length=length, N=N,
                                 solver_dt=ta["dt"], warmup_steps=ta["warmup_steps"],
                                 seq_len=length, stride=ta["stride"], seed=effective_seed,
                                 return_context=True)
        trajectory, mask, context = ds[0]
        physics_context = {
            key: context[key].unsqueeze(0)
            for key in ("nu", "sponge", "u_inf", "eta")
        }
        out = (
            trajectory[0:1],
            mask.unsqueeze(0),
            trajectory,
            physics_context,
        )
        self.traj2d[key] = out
        return out

    def _run_model_2d(self, info, state, mask, context, horizon):
        """Run legacy or fixed-body-v2 checkpoints through their exact contract."""
        torch = self.torch
        cfg = info["cfg"]
        device = self.device
        state = state.to(device)
        mask = mask.to(device)
        dt_frame = info["ta"]["dt"] * info["ta"]["stride"]
        dt = torch.full(
            (state.shape[0], 1),
            float(horizon) * dt_frame,
            device=device,
            dtype=state.dtype,
        )
        in_channels = int(cfg.get("in_channels", state.shape[1]))
        out_value = cfg.get("out_channels")
        out_channels = state.shape[1] if out_value is None else int(out_value)
        param_dim = int(cfg.get("param_dim") or 0)

        if in_channels == state.shape[1] and param_dim == 0:
            packed, params = state, None
        elif (
            in_channels == state.shape[1] + 1
            and out_channels == state.shape[1]
            and param_dim == 0
        ):
            packed, params = torch.cat([state, mask], dim=1), None
        elif (
            in_channels == state.shape[1] + 2
            and out_channels == state.shape[1]
            and param_dim == 1
        ):
            from flow_contract import (
                FIXED_BODY_V2,
                FixedBodyContext2D,
                FlowRequest2D,
                pack_fixed_body_v2,
            )
            physics_spec = info.get("physics_spec")
            if (
                not isinstance(physics_spec, dict)
                or physics_spec.get("physics_id") != FIXED_BODY_V2
            ):
                raise ValueError(
                    "parameter-conditioned checkpoint lacks a fixed-body-v2 physics spec"
                )
            support = physics_spec.get("support")
            support = support if isinstance(support, dict) else {}
            nu_bounds = support.get("nu")
            if (
                not isinstance(nu_bounds, (list, tuple))
                or len(nu_bounds) != 2
            ):
                raise ValueError("fixed-body-v2 checkpoint lacks viscosity support bounds")
            sponge_scale = float(support.get("sponge_strength", 0.0))
            fixed_body = FixedBodyContext2D(
                solid_fraction=mask,
                sponge_coefficient=context["sponge"].to(device),
                nu=context["nu"].to(device),
                u_inf=context["u_inf"].to(device),
                eta=context["eta"].to(device),
            )
            packed, params = pack_fixed_body_v2(
                FlowRequest2D(
                    velocity=state,
                    dt=dt,
                    physics_id=FIXED_BODY_V2,
                    fixed_body=fixed_body,
                ),
                nu_bounds=(float(nu_bounds[0]), float(nu_bounds[1])),
                sponge_scale=sponge_scale,
            )
        else:
            raise ValueError(
                "unsupported 2D checkpoint contract: "
                f"in={in_channels}, out={out_channels}, params={param_dim}"
            )
        return info["model"](packed, dt, params=params)

    def recover_pressure(self, field, method, tol, max_iter, periodic):
        """Recover pressure from a velocity field and report the Poisson-solve
        recovery error (relative residual). Spectral is an exact FFT inversion
        (residual ~ float32 eps); FD is an iterative CG solve to `tol`."""
        torch = self.torch
        from flow_quantities import _wavenumbers, pressure_from_velocity
        N = field.shape[-1]
        dx = 2.0 * math.pi / N
        kx, ky = _wavenumbers(N, field.device, field.dtype)
        u, v = field[:, 0:1], field[:, 1:2]
        uh, vh = torch.fft.fft2(u), torch.fft.fft2(v)
        dudx = torch.fft.ifft2(1j * kx * uh).real
        dudy = torch.fft.ifft2(1j * ky * uh).real
        dvdx = torch.fft.ifft2(1j * kx * vh).real
        dvdy = torch.fft.ifft2(1j * ky * vh).real
        adv_u = u * dudx + v * dudy
        adv_v = u * dvdx + v * dvdy
        div_adv = torch.fft.ifft2(1j * kx * torch.fft.fft2(adv_u)
                                  + 1j * ky * torch.fft.fft2(adv_v)).real
        rhs = (-div_adv)[0, 0]  # ∇²p = −div((u·∇)u)
        if method == "fd":
            return _fd_poisson(rhs, dx, periodic, float(tol), int(max_iter))
        p = pressure_from_velocity(field)  # spectral, exact
        lap = torch.fft.ifft2(-(kx ** 2 + ky ** 2) * torch.fft.fft2(p[None, None])).real[0, 0]
        resid = float((torch.norm(lap - rhs) / (torch.norm(rhs) + 1e-12)).item())
        return p, resid, 0

    def predict2d(self, req):
        """2D field for the pressure-recovery view: model velocity plus recovered
        pressure as `[3,N,N]` (u,v,p), a semigroup self-consistency number, the
        pressure-recovery residual, and—when legacy protocol flag ``want_truth``
        is set—the solver reference `[3,N,N]` plus RelL2/persistence."""
        torch = self.torch
        info = self._load(req["model"])
        if info["is3d"]:
            raise ValueError("predict2d requires a 2D checkpoint")
        ta = info["ta"]
        dt_frame = ta["dt"] * ta["stride"]
        horizon = int(req.get("steps", ta["max_steps"]))
        seed = int(req.get("seed", 1))
        want_truth = bool(req.get("want_truth", False))
        method = req.get("method", "spectral")
        tol = float(req.get("tolerance", 1e-5))
        max_iter = int(req.get("max_iter", 400))
        periodic = req.get("boundary", "periodic") != "dirichlet"
        from flow_quantities import pressure_from_velocity

        y0, mask, traj, context = self._traj2d(
            req["model"], seed, horizon + 1
        )

        def run(state, h):  # forward pass on the GPU
            return self._run_model_2d(info, state, mask, context, h)

        with torch.no_grad():
            pred_d = run(y0, horizon)  # [1,2,N,N] on device
            semi = None
            if horizon >= 2 and horizon % 2 == 0:  # semigroup: h vs (h/2 ∘ h/2)
                half = horizon // 2
                comp = run(run(y0, half), half)
                semi = float((torch.norm(comp - pred_d) / (torch.norm(pred_d) + 1e-9)).item())
            pred = pred_d.cpu()
            p_ai, p_resid, p_iters = self.recover_pressure(pred, method, tol, max_iter, periodic)

        N = pred.shape[-1]
        ai = torch.cat([pred[0], p_ai.unsqueeze(0)], 0).numpy().astype(np.float32)  # [3,N,N]
        meta = {"ok": True, "shape": [3, N, N], "scenario": info["scenario"], "dims": 2,
                "horizon": horizon, "dt_frame": dt_frame,
                "peak_p": float(p_ai.max()), "low_p": float(p_ai.min()), "semigroup": semi,
                "p_residual": p_resid, "p_iters": p_iters, "method": method}

        if want_truth and horizon < traj.shape[0]:
            truth = traj[horizon:horizon + 1]  # [1,2,N,N]
            with torch.no_grad():
                p_t = pressure_from_velocity(truth)
            truth3 = torch.cat([truth[0], p_t.unsqueeze(0)], 0).numpy().astype(np.float32)
            meta["rel_l2"] = float((torch.norm(pred - truth) / (torch.norm(truth) + 1e-9)).item())
            meta["persist"] = float((torch.norm(y0 - truth) / (torch.norm(truth) + 1e-9)).item())
            meta["has_truth"] = True
            return np.concatenate([ai, truth3], 0), meta  # [6,N,N]
        meta["has_truth"] = False
        return ai, meta

    def pressure_3d(self, field):
        """Spectral pressure recovery in 3D: solve ∇²p = −∇·((u·∇)u) in Fourier
        space on the periodic box (the 3D mirror of flow_quantities). `field` is
        `[1,3,N,N,N]` on CPU; returns `[N,N,N]`."""
        torch = self.torch
        N = field.shape[-1]
        dx = 2.0 * math.pi / N
        k1 = 2.0 * math.pi * torch.fft.fftfreq(N, d=dx)
        if N % 2 == 0:
            k1[N // 2] = 0.0
        kx = k1.reshape(N, 1, 1)
        ky = k1.reshape(1, N, 1)
        kz = k1.reshape(1, 1, N)
        u = field[0]  # [3,N,N,N]
        uh = torch.fft.fftn(u, dim=(-3, -2, -1))
        grads = []
        for kdir in (kx, ky, kz):
            grads.append(torch.fft.ifftn(1j * kdir * uh, dim=(-3, -2, -1)).real)  # [3,N,N,N]
        adv = sum(u[a] * grads[a] for a in range(3))  # (u·∇)u -> [3,N,N,N]
        advh = torch.fft.fftn(adv, dim=(-3, -2, -1))
        div_adv = 1j * kx * advh[0] + 1j * ky * advh[1] + 1j * kz * advh[2]
        k2 = kx ** 2 + ky ** 2 + kz ** 2
        k2 = torch.where(k2 == 0, torch.ones_like(k2), k2)
        p_hat = div_adv / k2
        p_hat[0, 0, 0] = 0.0
        return torch.fft.ifftn(p_hat, dim=(-3, -2, -1)).real

    def predict_cad(self, req, payload, progress=None):
        """CAD flow analysis: a voxelized STL mask (`[N³]` f32 payload) becomes a
        wind-tunnel case — smooth the mask (training masks are tanh-smoothed),
        develop the flow with the real Brinkman solver (cached per mask), then
        one model pass at the requested horizon. Returns `[4,N,N,N]`: velocity +
        spectrally-recovered pressure, for the surface-load view.

        Optional `progress(dict)` emits stage frames (`progress: true`) before the
        final response. Fractions are stage-based, not wall-clock percentages.
        """
        import hashlib
        torch = self.torch
        request_id = str(req.get("request_id", ""))
        stage_count = 4

        def report(stage, stage_index, detail="", local=0.0):
            if progress is None:
                return
            local = max(0.0, min(1.0, float(local)))
            fraction = (stage_index - 1 + local) / stage_count
            progress({
                "ok": True,
                "progress": True,
                "request_id": request_id,
                "stage": stage,
                "stage_index": int(stage_index),
                "stage_count": stage_count,
                "detail": detail,
                "fraction": float(fraction),
            })

        report("preparing", 1, "Validating mask and smoothing edges", 0.0)
        info = self._load(req["model"])
        if not info["is3d"] or info["cfg"]["in_channels"] <= info["cfg"]["out_channels"]:
            raise ValueError("predict_cad needs a geometry-conditioned 3D checkpoint")
        m, ta = info["model"], info["ta"]
        N = ta["grid_size"]
        if len(payload) != N ** 3 * 4:
            raise ValueError(f"mask payload is {len(payload)} bytes; model grid {N} needs {N ** 3 * 4}")
        mask_array = np.frombuffer(payload, np.float32).reshape(N, N, N).copy()
        if not np.all(np.isfinite(mask_array)):
            raise ValueError("CAD mask must contain only finite values")
        if float(mask_array.min()) < 0.0 or float(mask_array.max()) > 1.0:
            raise ValueError("CAD mask values must lie in [0, 1]")
        if not np.any(mask_array > 0.5):
            raise ValueError("CAD mask contains no solid voxels")
        mask = torch.from_numpy(mask_array)
        # ~1.5-cell smoothing (matches make_obstacle_mask_3d's tanh edges)
        sm = mask.reshape(1, 1, N, N, N)
        kernel = torch.ones(1, 1, 3, 3, 3) / 27.0
        import torch.nn.functional as tF
        for _ in range(2):
            sm = tF.conv3d(tF.pad(sm, (1,) * 6, mode="replicate"), kernel)
        mask_s = sm.reshape(N, N, N).clamp(0.0, 1.0)
        report("preparing", 1, "Mask validated", 1.0)

        char_len = float(req.get("char_len", 0.6))
        reynolds = float(req.get("reynolds", 150.0))
        reference_length_m = float(req.get("reference_length_m", 1.0))
        velocity_mps = float(req.get("velocity_mps", 1.0))
        density_kg_m3 = float(req.get("density_kg_m3", 1.225))
        reference_pressure_pa = float(req.get("reference_pressure_pa", 101325.0))
        moment_origin_mode = str(
            req.get("moment_origin_mode", "diffuse_surface_centroid")
        )
        moment_origin_solver = req.get("moment_origin_solver")
        reference_area_m2 = req.get("reference_area_m2")
        if reference_area_m2 is not None:
            reference_area_m2 = float(reference_area_m2)
        engineering_values = {
            "Reynolds number": reynolds,
            "reference length": reference_length_m,
            "free-stream speed": velocity_mps,
            "density": density_kg_m3,
            "reference pressure": reference_pressure_pa,
        }
        if any(not math.isfinite(value) for value in engineering_values.values()):
            raise ValueError("engineering reference quantities must be finite")
        if min(
            reynolds,
            reference_length_m,
            velocity_mps,
            density_kg_m3,
        ) <= 0.0:
            raise ValueError(
                "Reynolds number, reference length, speed, and density must be positive"
            )
        if not 60.0 <= reynolds <= 400.0:
            raise ValueError(
                f"Reynolds number {reynolds:g} lies outside the qualified 60–400 envelope"
            )
        nu = 1.0 * char_len / reynolds
        horizon = int(req.get("steps", ta["max_steps"]))
        if horizon < 1 or horizon > int(ta["max_steps"]):
            raise ValueError(
                f"requested horizon {horizon} lies outside model support "
                f"1–{int(ta['max_steps'])}"
            )
        dt_frame = ta["dt"] * ta["stride"]

        digest = (req["model"], hashlib.sha1(payload).hexdigest(), reynolds)
        developed = self.cad_cache.get(digest)
        if developed is None:
            from obstacle_solver_3d import ObstacleFlowSolver3D
            solver = ObstacleFlowSolver3D(N=N, dt=ta["dt"], nu=nu, mask=mask_s)
            field = solver.initial_field(seed=int(req.get("seed", 7)))
            warmup_steps = int(ta["warmup_steps"])
            report("developing", 2, f"Brinkman warmup 0/{warmup_steps}", 0.0)
            with torch.no_grad():
                # Emit at most ~10 progress ticks so the UI stays live without
                # flooding the loopback socket during long warmups.
                tick_every = max(1, warmup_steps // 10) if warmup_steps else 1
                for step in range(warmup_steps):
                    field = solver.step(field)
                    if step + 1 == warmup_steps or (step + 1) % tick_every == 0:
                        report(
                            "developing",
                            2,
                            f"Brinkman warmup {step + 1}/{warmup_steps}",
                            (step + 1) / max(warmup_steps, 1),
                        )
            developed = field.detach().clone()
            self.cad_cache[digest] = developed
        else:
            report("developing", 2, "Reusing cached developed flow for this mask", 1.0)

        report("predicting", 3, f"Model horizon step {horizon}", 0.0)
        device = self.device
        with torch.no_grad():
            # Engineering CAD path: one forward only. Semigroup self-consistency
            # stays in the 2D research sandbox — not on the customer hot path.
            model_in = self._pack_obstacle_3d(
                info,
                developed,
                mask_s.reshape(1, 1, N, N, N),
                nu=nu,
            ).to(device)
            pred = m(model_in, torch.tensor([[horizon * dt_frame]], device=device)).cpu()
            report("predicting", 3, f"Model horizon step {horizon} complete", 1.0)
            report("recovering", 4, "Recovering pressure and surface loads", 0.0)
            p = self.pressure_3d(pred)

        velocity = pred[0].numpy()
        mask_bytes = mask_s.numpy().astype(np.float32)
        loads = engineering_surface_loads(
            velocity,
            p.numpy(),
            mask_bytes,
            reynolds=reynolds,
            char_len_solver=char_len,
            reference_length_m=reference_length_m,
            velocity_mps=velocity_mps,
            density_kg_m3=density_kg_m3,
            reference_pressure_pa=reference_pressure_pa,
            moment_origin_mode=moment_origin_mode,
            moment_origin_solver=moment_origin_solver,
            reference_area_m2=reference_area_m2,
        )
        report("recovering", 4, "Surface loads integrated", 1.0)
        out = np.concatenate(
            [
                velocity,
                loads["pressure_pa"][None],
                mask_bytes[None],
                loads["cp"][None],
                loads["traction_pa"],
            ],
            axis=0,
        ).astype(np.float32)
        meta = {"ok": True, "request_id": str(req.get("request_id", "")),
                "shape": [9, N, N, N], "scenario": "cad", "dims": 3,
                "horizon": horizon, "dt_frame": dt_frame, "reynolds": reynolds,
                "char_len": char_len, "solver_dt": float(ta["dt"]),
                "solver_stride": int(ta["stride"]),
                "warmup_steps": int(ta["warmup_steps"]),
                "has_pressure": True,
                "peak_p": float(loads["pressure_pa"].max()),
                "low_p": float(loads["pressure_pa"].min()),
                "force_coefficients": loads["force_coefficients"],
                "moment_coefficients": loads["moment_coefficients"],
                "force_newtons": loads["force_newtons"],
                "moment_newton_meters": loads["moment_newton_meters"],
                "surface_area_m2": loads["surface_area_m2"],
                "pressure_force_fraction": loads["pressure_force_fraction"],
                "load_hotspot": loads["load_hotspot"],
                "suction_hotspot": loads["suction_hotspot"],
                "divergence_rms": loads["divergence_rms"],
                "wake_deficit_peak": loads["wake_deficit_peak"],
                "wake_deficit_mean": loads["wake_deficit_mean"],
                "moment_origin_mode": loads["moment_origin_mode"],
                "moment_origin_solver": loads["moment_origin_solver"],
                "surface_centroid_solver": loads["surface_centroid_solver"],
                "coefficient_reference": loads["coefficient_reference"],
                "reference_area_m2": loads["reference_area_m2"],
                "load_method": loads["method"],
                "warnings": loads["warnings"]}
        return out.reshape(-1), meta

    def run_benchmark(self, req):
        """N5 — one 2D model × exact test seeds × horizons.

        Unlike the interactive validation view, benchmark seeds are not offset
        into the `train seed + 50000` checkpoint-selection stream.
        """
        torch = self.torch
        info = self._load(req["model"])
        if info["is3d"]:
            raise ValueError("run_benchmark currently drives 2D checkpoints")
        ta = info["ta"]
        dt_frame = ta["dt"] * ta["stride"]
        seeds = [int(s) for s in req.get("seeds", [70000, 70001, 70002])]
        horizons = [int(h) for h in req.get("horizons", [1, 4, 8, 16])]
        if not seeds or not horizons:
            raise ValueError("benchmark requires at least one seed and horizon")
        if any(seed < 0 for seed in seeds) or any(horizon < 1 for horizon in horizons):
            raise ValueError("benchmark seeds must be nonnegative and horizons positive")
        t0 = __import__("time").time()
        rel, persist = [], []
        for seed in seeds:
            y0, mask, traj, context = self._traj2d(
                req["model"], seed, max(horizons) + 1, seed_offset=0
            )
            row_r, row_p = [], []
            with torch.no_grad():
                for h in horizons:
                    target = traj[h:h + 1]
                    pred = self._run_model_2d(
                        info, y0, mask, context, h
                    ).cpu()
                    row_r.append(float((torch.norm(pred - target) / (torch.norm(target) + 1e-9)).item()))
                    row_p.append(float((torch.norm(y0 - target) / (torch.norm(target) + 1e-9)).item()))
            rel.append(row_r)
            persist.append(row_p)
        flat = [x for row in rel for x in row]
        return {"ok": True, "seeds": seeds, "horizons": horizons, "rel": rel,
                "persist": persist, "global_rel": sum(flat) / max(1, len(flat)),
                "runtime_s": __import__("time").time() - t0,
                "grid": ta["grid_size"], "epoch": info.get("epoch", 0),
                "dt_frame": dt_frame,
                "provenance": analyze_checkpoint_provenance(
                    info["checkpoint_meta"], seeds
                )}

    def inspect_benchmark_cell(self, req):
        """On-demand per-variable, spatial, and spectral evidence for one cell."""
        torch = self.torch
        info = self._load(req["model"])
        if info["is3d"]:
            raise ValueError("benchmark inspector currently drives 2D checkpoints")
        seed = int(req["seed"])
        horizon = int(req["horizon"])
        if seed < 0 or horizon < 1:
            raise ValueError("inspector seed must be nonnegative and horizon positive")
        ta = info["ta"]
        dt_frame = ta["dt"] * ta["stride"]
        y0, mask, traj, context = self._traj2d(
            req["model"], seed, horizon + 1, seed_offset=0
        )
        with torch.no_grad():
            pred = self._run_model_2d(
                info, y0, mask, context, horizon
            ).cpu()
        truth = traj[horizon:horizon + 1]
        evidence = benchmark_cell_evidence(
            pred[0].numpy(), truth[0].numpy(), y0[0].numpy()
        )
        provenance = analyze_checkpoint_provenance(
            info["checkpoint_meta"], [seed]
        )
        seed_record = provenance["benchmark_seeds"][0]
        n = int(pred.shape[-1])
        requested_schema = req.get("evidence_schema")
        if requested_schema in (None, ""):
            # Backward-compatible velocity-only payload for older native clients.
            payload = np.concatenate([
                evidence["model_speed"].reshape(-1),
                evidence["truth_speed"].reshape(-1),
                evidence["error_map"].reshape(-1),
            ]).astype(np.float32)
            map_meta = {"shape": [3, n, n]}
        elif requested_schema == INSPECTOR_SCHEMA:
            payload, map_meta = inspector_payload(
                pred[0].numpy(), truth[0].numpy()
            )
        else:
            raise ValueError(
                f"unsupported benchmark inspector evidence schema: {requested_schema}"
            )
        meta = {
            "ok": True,
            **map_meta,
            "seed": seed,
            "horizon": horizon,
            "dt_frame": dt_frame,
            "seed_stream": seed_record["stream"],
            "provenance_status": provenance["verdict"],
            "rel_l2": evidence["rel_l2"],
            "persist_rel_l2": evidence["persist_rel_l2"],
            "improvement_ratio": evidence["improvement_ratio"],
            "mean_abs_error": evidence["mean_abs_error"],
            "p95_abs_error": evidence["p95_abs_error"],
            "max_abs_error": evidence["max_abs_error"],
            "divergence_model_rms": evidence["divergence_model_rms"],
            "divergence_truth_rms": evidence["divergence_truth_rms"],
            "divergence_error_rms": evidence["divergence_error_rms"],
            "spectrum_rel_l2": evidence["spectrum_rel_l2"],
            "spectrum_k": evidence["spectrum_k"].tolist(),
            "spectrum_model": evidence["spectrum_model"].tolist(),
            "spectrum_truth": evidence["spectrum_truth"].tolist(),
        }
        return payload, meta

    def predict_ic(self, req, payload):
        """Flow Painter: advance a USER-painted `[2,N,N]` velocity IC (carried as
        the request payload, already divergence-free client-side) with a 2D
        model. Mask-conditioned models get a zero mask — use a checkpoint trained
        with --empty-fraction (the unified model) for in-distribution behaviour.
        Stateless: the IC rides with every request, so TimeJump re-scrubs are
        just new horizons over the same payload. A painted IC has no solver
        reference trajectory; the semigroup number reports self-consistency,
        not accuracy."""
        torch = self.torch
        info = self._load(req["model"])
        if info["is3d"]:
            raise ValueError("predict_ic requires a 2D checkpoint")
        m, ta = info["model"], info["ta"]
        N = ta["grid_size"]
        expect = 2 * N * N * 4
        if len(payload) != expect:
            raise ValueError(f"IC payload is {len(payload)} bytes; model grid {N} needs {expect}")
        ic = np.frombuffer(payload, dtype=np.float32).reshape(1, 2, N, N).copy()
        y0 = torch.from_numpy(ic)
        dt_frame = ta["dt"] * ta["stride"]
        horizon = int(req.get("steps", ta["max_steps"]))
        method = req.get("method", "spectral")
        tol = float(req.get("tolerance", 1e-5))
        max_iter = int(req.get("max_iter", 400))
        periodic = req.get("boundary", "periodic") != "dirichlet"

        conditioned = info["cfg"]["in_channels"] > info["cfg"]["out_channels"]
        device = self.device
        mask_d = torch.zeros(1, 1, N, N, device=device) if conditioned else None

        def run(state, h):
            s = state.to(device)
            model_in = torch.cat([s, mask_d], dim=1) if conditioned else s
            return m(model_in, torch.tensor([[h * dt_frame]], device=device))

        with torch.no_grad():
            pred_d = run(y0, horizon)
            semi = None
            if horizon >= 2 and horizon % 2 == 0:
                half = horizon // 2
                comp = run(run(y0, half), half)
                semi = float((torch.norm(comp - pred_d) / (torch.norm(pred_d) + 1e-9)).item())
            pred = pred_d.cpu()
            p_ai, p_resid, p_iters = self.recover_pressure(pred, method, tol, max_iter, periodic)

        ai = torch.cat([pred[0], p_ai.unsqueeze(0)], 0).numpy().astype(np.float32)
        meta = {"ok": True, "shape": [3, N, N], "scenario": "painted", "dims": 2,
                "horizon": horizon, "dt_frame": dt_frame,
                "peak_p": float(p_ai.max()), "low_p": float(p_ai.min()), "semigroup": semi,
                "p_residual": p_resid, "p_iters": p_iters, "method": method,
                "has_truth": False}
        return ai, meta


def serve(research_dir, device="auto", managed_model_dir=None):
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", 0))
    srv.listen(1)
    port = srv.getsockname()[1]
    try:
        engine = Engine(research_dir, device, managed_model_dir)
    except Exception as exc:
        print("READY " + json.dumps({"error": str(exc)}), flush=True)
        return
    print(
        "READY "
        + json.dumps(
            {"port": port, "device": str(engine.device), "research_dir": engine.research_dir}
        ),
        flush=True,
    )
    # After the handshake we talk only over the socket. Silence stdout so library
    # prints (e.g. dataset generation) don't hit the closed pipe and SIGPIPE us.
    sys.stdout.flush()
    sys.stdout = open(os.devnull, "w")

    conn, _ = srv.accept()
    while True:
        try:
            obj, payload = recv(conn)
        except (ConnectionError, OSError):
            break
        op = obj.get("op")
        try:
            if op == "ping":
                send(conn, {"ok": True})
            elif op == "list_models":
                send(conn, {"ok": True, "models": engine.list_model_cards()})
            elif op == "import_model":
                send(conn, engine.import_model(obj.get("path", "")))
            elif op == "delete_model":
                models = engine.delete_model(obj.get("model", ""))
                send(conn, {"ok": True, "deleted": obj.get("model"), "models": models})
            elif op == "predict_field":
                field, meta = engine.predict_field(obj)
                send(conn, meta, field.tobytes())
            elif op == "predict2d":
                field, meta = engine.predict2d(obj)
                send(conn, meta, field.tobytes())
            elif op == "predict_ic":
                field, meta = engine.predict_ic(obj, payload)
                send(conn, meta, field.tobytes())
            elif op == "predict_cad":
                def progress(frame):
                    send(conn, frame)

                field, meta = engine.predict_cad(obj, payload, progress=progress)
                send(conn, meta, field.tobytes())
            elif op == "run_benchmark":
                send(conn, engine.run_benchmark(obj))
            elif op == "inspect_benchmark_cell":
                field, meta = engine.inspect_benchmark_cell(obj)
                send(conn, meta, field.tobytes())
            else:
                send(conn, {"ok": False, "error": f"unknown op: {op}"})
        except Exception as exc:  # never die on a request
            send(conn, {"ok": False, "error": f"{exc}", "trace": traceback.format_exc()[-800:]})


if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("--research-dir", required=True)
    p.add_argument("--managed-model-dir")
    p.add_argument("--device", choices=("auto", "mps", "cpu"), default="auto")
    args = p.parse_args()
    serve(args.research_dir, args.device, args.managed_model_dir)
