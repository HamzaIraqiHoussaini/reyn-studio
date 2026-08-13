import base64
import sys
import tempfile
import unittest
from datetime import datetime, timedelta, timezone
from pathlib import Path
from unittest.mock import patch

import numpy as np
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from engine.reyn_engine import (
    Engine,
    analyze_checkpoint_provenance,
    benchmark_cell_evidence,
    classify_benchmark_seed,
    divergence_rms,
    engineering_surface_loads,
    radial_energy_spectrum,
)

try:
    import torch
except ImportError:  # The lightweight system-Python suite remains usable.
    torch = None


def utc_text(value):
    return value.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def signing_context(root, key_id="engine-release-key"):
    private_key = Ed25519PrivateKey.generate()
    private_path = root / f"{key_id}.pem"
    private_path.write_bytes(
        private_key.private_bytes(
            serialization.Encoding.PEM,
            serialization.PrivateFormat.PKCS8,
            serialization.NoEncryption(),
        )
    )
    private_path.chmod(0o600)
    public_key = private_key.public_key().public_bytes(
        serialization.Encoding.Raw,
        serialization.PublicFormat.Raw,
    )
    entry = {
        "key_id": key_id,
        "algorithm": "ed25519",
        "public_key": base64.b64encode(public_key).decode("ascii"),
        "revoked_at": None,
        "minimum_release_sequence": 0,
        "maximum_release_sequence": 2**63 - 1,
    }
    return private_path, entry


def sign_bundle(path, private_path, key_id="engine-release-key"):
    from model_bundle import sign_model_bundle

    now = datetime.now(timezone.utc).replace(microsecond=0)
    return sign_model_bundle(
        path,
        private_key_path=private_path,
        key_id=key_id,
        release_sequence=1,
        issued_at=utc_text(now - timedelta(minutes=1)),
        expires_at=utc_text(now + timedelta(days=30)),
    )


class ProvenanceTests(unittest.TestCase):
    def test_reserved_streams_are_distinct_and_flagged(self):
        checkpoint = {
            "epoch": 40,
            "checkpoint_role": "fixed_final",
            "best_metric": "val_multi_horizon_wake_rel_l2",
            "source_fingerprint": {"digest": "source-a"},
            "train_args": {"seed": 7, "dataset": "mixed", "epochs": 40},
        }
        result = analyze_checkpoint_provenance(
            checkpoint, [7, 10007, 50007, 70000]
        )

        self.assertEqual(result["verdict"], "flagged")
        self.assertEqual(result["overlap_count"], 3)
        self.assertEqual(
            [record["stream"] for record in result["benchmark_seeds"]],
            ["training", "mixed_fork", "validation_selection", "fresh_test"],
        )
        self.assertEqual(result["selection_stream"], "fixed_epoch")
        self.assertEqual(result["final_epoch_status"], "fixed_final")

    def test_fresh_fixed_final_checkpoint_is_clean(self):
        checkpoint = {
            "epoch": 20,
            "checkpoint_role": "fixed_final",
            "best_metric": "val_multi_horizon_rel_l2",
            "source_fingerprint": {"digest": "source-b"},
            "train_args": {"seed": 0, "dataset": "standard", "epochs": 20},
        }
        result = analyze_checkpoint_provenance(
            checkpoint, [70000, 70001, 70002]
        )

        self.assertEqual(result["verdict"], "clean")
        self.assertEqual(result["overlap_count"], 0)
        self.assertTrue(all(
            record["stream"] == "fresh_test"
            for record in result["benchmark_seeds"]
        ))
        self.assertFalse(result["mixed_fork_used"])

    def test_legacy_metadata_remains_unknown(self):
        checkpoint = {
            "epoch": 30,
            "best_metric": "val_multi_horizon_rel_l2",
            "train_args": {"seed": 0, "dataset": "standard", "epochs": 30},
        }
        result = analyze_checkpoint_provenance(checkpoint, [70000])

        self.assertEqual(result["verdict"], "unknown")
        self.assertIn("checkpoint role absent", result["legacy_unknown"])
        self.assertIn("source fingerprint absent", result["legacy_unknown"])
        self.assertEqual(
            classify_benchmark_seed(50000, 0), "validation_selection"
        )


class SpectralEvidenceTests(unittest.TestCase):
    def test_radial_spectrum_finds_known_mode(self):
        n = 32
        x = 2.0 * np.pi * np.arange(n) / n
        field = np.zeros((2, n, n), dtype=np.float32)
        field[0] = np.sin(3.0 * x)[:, None]

        k, energy = radial_energy_spectrum(field)

        self.assertEqual(float(k[np.argmax(energy)]), 3.0)
        self.assertGreater(float(energy.max()), 0.0)

    def test_divergence_measure_distinguishes_solenoidal_field(self):
        n = 32
        x = 2.0 * np.pi * np.arange(n) / n
        divergence_free = np.zeros((2, n, n), dtype=np.float32)
        divergence_free[0] = np.sin(x)[:, None]
        divergence_free[1] = -np.sin(x)[None, :]
        divergent = np.zeros_like(divergence_free)
        divergent[0] = np.sin(x)[None, :]

        self.assertLess(divergence_rms(divergence_free), 1e-6)
        self.assertGreater(divergence_rms(divergent), 0.5)

    def test_cell_evidence_matches_scaled_prediction(self):
        n = 32
        x = 2.0 * np.pi * np.arange(n) / n
        truth = np.zeros((2, n, n), dtype=np.float32)
        truth[0] = np.sin(2.0 * x)[None, :]
        truth[1] = -np.sin(2.0 * x)[:, None]
        prediction = 0.5 * truth
        initial = np.zeros_like(truth)

        evidence = benchmark_cell_evidence(prediction, truth, initial)

        self.assertAlmostEqual(evidence["rel_l2"], 0.5, places=6)
        self.assertAlmostEqual(evidence["persist_rel_l2"], 1.0, places=6)
        self.assertAlmostEqual(evidence["improvement_ratio"], 2.0, places=6)
        self.assertAlmostEqual(evidence["spectrum_rel_l2"], 0.75, places=6)
        self.assertEqual(evidence["error_map"].shape, (n, n))
        self.assertTrue(np.all(np.isfinite(evidence["error_map"])))


class EngineeringSurfaceLoadTests(unittest.TestCase):
    def synthetic_cube(self, n=32):
        mask = np.zeros((n, n, n), dtype=np.float32)
        mask[n // 3: 2 * n // 3, n // 3: 2 * n // 3, n // 3: 2 * n // 3] = 1.0
        velocity = np.zeros((3, n, n, n), dtype=np.float32)
        return velocity, mask

    def test_constant_pressure_closed_surface_has_negligible_net_force(self):
        velocity, mask = self.synthetic_cube()
        pressure = np.ones_like(mask) * 0.3
        result = engineering_surface_loads(
            velocity,
            pressure,
            mask,
            reynolds=150.0,
            char_len_solver=0.6,
            reference_length_m=0.1,
            velocity_mps=12.0,
            density_kg_m3=1.225,
            reference_pressure_pa=101325.0,
        )
        self.assertLess(np.linalg.norm(result["force_coefficients"]), 1e-5)
        self.assertTrue(np.allclose(result["cp"][mask > 0], 0.6))
        expected_pressure = 101325.0 + 0.3 * 1.225 * 12.0**2
        self.assertTrue(
            np.allclose(result["pressure_pa"][mask > 0], expected_pressure)
        )
        self.assertGreater(result["surface_area_m2"], 0.0)
        self.assertEqual(result["method"], "diffuse_interface_traction.v1")
        self.assertGreaterEqual(result["wake_deficit_peak"], 0.0)
        self.assertGreaterEqual(result["wake_deficit_mean"], 0.0)

    def test_pressure_gradient_produces_drag_and_physical_scaling(self):
        velocity, mask = self.synthetic_cube()
        x = np.linspace(0.5, -0.5, mask.shape[0], dtype=np.float32)
        pressure = np.broadcast_to(x[:, None, None], mask.shape).copy()
        result = engineering_surface_loads(
            velocity,
            pressure,
            mask,
            reynolds=150.0,
            char_len_solver=0.6,
            reference_length_m=0.2,
            velocity_mps=20.0,
            density_kg_m3=1.0,
            reference_pressure_pa=100000.0,
        )
        self.assertGreater(abs(result["force_coefficients"][0]), 1e-3)
        expected = (
            result["force_coefficients"][0]
            * 0.5
            * 1.0
            * 20.0**2
            * 0.2**2
        )
        self.assertAlmostEqual(result["force_newtons"][0], expected, places=5)
        self.assertEqual(result["traction_pa"].shape, velocity.shape)
        self.assertEqual(result["moment_origin_mode"], "diffuse_surface_centroid")
        self.assertEqual(
            result["coefficient_reference"],
            "q_inf * L_ref^2 ; q_inf * L_ref^3",
        )

    def test_source_frame_moment_origin_and_reference_area_override(self):
        velocity, mask = self.synthetic_cube()
        x = np.linspace(0.5, -0.5, mask.shape[0], dtype=np.float32)
        pressure = np.broadcast_to(x[:, None, None], mask.shape).copy()
        baseline = engineering_surface_loads(
            velocity,
            pressure,
            mask,
            reynolds=150.0,
            char_len_solver=0.6,
            reference_length_m=0.2,
            velocity_mps=20.0,
            density_kg_m3=1.0,
            reference_pressure_pa=100000.0,
        )
        pinned = engineering_surface_loads(
            velocity,
            pressure,
            mask,
            reynolds=150.0,
            char_len_solver=0.6,
            reference_length_m=0.2,
            velocity_mps=20.0,
            density_kg_m3=1.0,
            reference_pressure_pa=100000.0,
            moment_origin_mode="source_frame_point",
            moment_origin_solver=[0.0, 0.0, 0.0],
            reference_area_m2=0.05,
        )
        self.assertEqual(pinned["moment_origin_mode"], "source_frame_point")
        self.assertEqual(pinned["moment_origin_solver"], [0.0, 0.0, 0.0])
        self.assertEqual(pinned["reference_area_m2"], 0.05)
        self.assertEqual(
            pinned["coefficient_reference"],
            "q_inf * A_ref ; q_inf * A_ref * L_ref",
        )
        np.testing.assert_allclose(
            pinned["force_newtons"], baseline["force_newtons"], rtol=1e-6, atol=1e-9
        )
        q = 0.5 * 1.0 * 20.0**2
        expected_cx = pinned["force_newtons"][0] / (q * 0.05)
        self.assertAlmostEqual(pinned["force_coefficients"][0], expected_cx, places=5)
        self.assertFalse(
            np.allclose(
                pinned["moment_newton_meters"],
                baseline["moment_newton_meters"],
            )
        )

    def test_malformed_or_nonphysical_contract_is_rejected(self):
        velocity, mask = self.synthetic_cube(16)
        with self.assertRaisesRegex(ValueError, "positive"):
            engineering_surface_loads(
                velocity,
                np.zeros_like(mask),
                mask,
                reynolds=0.0,
                char_len_solver=0.6,
                reference_length_m=1.0,
                velocity_mps=1.0,
                density_kg_m3=1.0,
                reference_pressure_pa=0.0,
            )
        invalid_pressure = np.zeros_like(mask)
        invalid_pressure[0, 0, 0] = np.nan
        with self.assertRaisesRegex(ValueError, "finite"):
            engineering_surface_loads(
                velocity,
                invalid_pressure,
                mask,
                reynolds=150.0,
                char_len_solver=0.6,
                reference_length_m=1.0,
                velocity_mps=1.0,
                density_kg_m3=1.0,
                reference_pressure_pa=101325.0,
            )


@unittest.skipIf(torch is None, "torch is available in the Reyn research environment")
class ModelLibraryTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)
        self.research_dir = Path(__file__).resolve().parents[2] / "reyn-research"
        sys.path.insert(0, str(self.research_dir))
        import model_bundle
        import test_model_bundle as bundle_test_support

        self.model_bundle = model_bundle
        self.bundle_test_support = bundle_test_support
        self.original_tuf_root = model_bundle.PINNED_TUF_ROOT_JSON
        self.private_key, self.signature_entry = signing_context(self.root)
        self.tuf_signers = bundle_test_support.tuf_signers()
        self.engine = Engine.__new__(Engine)
        self.engine.research_dir = str(self.root)
        self.engine._managed_model_dir = self.root / "reyn_models"
        self.engine.torch = torch
        self.engine.device = torch.device("cpu")
        self.engine.cache = {}
        self.engine.traj2d = {}
        self.engine.cad_cache = {}
        self.engine._probe_checkpoint_compatibility = lambda _path: None

    def tearDown(self):
        self.model_bundle.PINNED_TUF_ROOT_JSON = self.original_tuf_root
        self.directory.cleanup()

    def test_windows_tuf_root_pointer_is_a_regular_file(self):
        metadata = self.root / "portable-tuf"
        history = metadata / "root_history"
        history.mkdir(parents=True)
        expected = b'{"signed":{"version":1}}'
        (history / "1.root.json").write_bytes(expected)
        updater = object.__new__(self.model_bundle._PortableOfflineUpdater)
        updater._dir = str(metadata)
        updater._trusted_set = type(
            "TrustedSet",
            (),
            {"root": type("Root", (), {"version": 1})()},
        )()

        updater._update_root_file()

        pointer = metadata / "root.json"
        self.assertTrue(pointer.is_file())
        self.assertFalse(pointer.is_symlink())
        self.assertEqual(pointer.read_bytes(), expected)

    def test_windows_trusted_state_does_not_fsync_directories(self):
        with patch.object(
            self.model_bundle.os,
            "open",
            side_effect=AssertionError("Windows must not open directories for fsync"),
        ):
            self.model_bundle._fsync_directory(
                self.root,
                platform_name="nt",
            )

    def test_windows_trusted_state_fsyncs_files_with_a_writable_handle(self):
        candidate = self.root / "trusted-metadata.json"
        expected = b'{"version":1}'
        candidate.write_bytes(expected)

        self.model_bundle._fsync_regular_file(
            candidate,
            platform_name="nt",
        )

        self.assertEqual(candidate.read_bytes(), expected)

    def checkpoint(
        self,
        path,
        *,
        source=True,
        in_channels=3,
        out_channels=2,
        param_dim=0,
    ):
        from model_bundle import write_model_bundle
        from time_moe_operator import DirectFlowMap

        config = {
            "in_channels": in_channels,
            "out_channels": out_channels,
            "width": 8,
            "trunk_depth": 1,
            "time_dim": 8,
            "dt_scale": 0.01,
            "param_dim": param_dim,
        }
        model = DirectFlowMap(**config)
        checkpoint = {
            "model_config": config,
            "model_state_dict": model.state_dict(),
            "train_args": {
                "dataset": "engine-fixture",
                "seed": 0,
                "grid_size": 128,
                "max_steps": 64,
                "epochs": 40,
                "dt": 0.01,
                "stride": 4,
                "warmup_steps": 8,
                "nu": 0.01,
                "scenario": "obstacle",
            },
            "epoch": 40,
            "checkpoint_role": "fixed_final",
            "limitations": ["Static 2D geometry only"],
            "benchmark_reports": [{"sha256": "a" * 64}],
        }
        if source:
            checkpoint["source_fingerprint"] = {
                "algorithm": "sha256",
                "digest": "b" * 64,
            }
        write_model_bundle(
            checkpoint,
            path,
            model_id="engine-fixture",
            model_version="1.0.0",
        )
        sign_bundle(path, self.private_key)
        repository = self.bundle_test_support.write_tuf_repository(
            Path(path),
            self.signature_entry,
            signers=self.tuf_signers,
        )
        self.model_bundle.PINNED_TUF_ROOT_JSON = repository["bootstrap_root"]

    def test_import_validates_copies_and_deletes_only_managed_models(self):
        source = self.root / "outside.reynmodel"
        self.checkpoint(source)

        result = self.engine.import_model(source)
        imported = result["imported"]
        cards = result["models"]

        self.assertTrue(result["ok"])
        self.assertTrue(result["validation"]["accepted"])
        self.assertEqual(imported["status"], "clean")
        self.assertTrue(imported["managed"])
        self.assertTrue(imported["id"].startswith("reyn_models/"))
        self.assertEqual(len(imported["checkpoint_sha256"]), 64)
        self.assertEqual(imported["authenticity_status"], "verified")
        self.assertEqual(imported["publisher_key_id"], "engine-release-key")
        self.assertEqual(imported["limitations"], ["Static 2D geometry only"])
        self.assertEqual(imported["benchmark_report_hashes"], ["a" * 64])
        self.assertEqual(len(cards), 2)
        imported_path = self.root / imported["id"]
        self.assertTrue(imported_path.with_name(imported_path.name + ".sig").is_file())
        self.assertTrue(imported_path.with_name(imported_path.name + ".tuf").is_dir())
        remaining = self.engine.delete_model(imported["id"])
        self.assertEqual([card["id"] for card in remaining], ["outside.reynmodel"])
        self.assertFalse(imported_path.with_name(imported_path.name + ".sig").exists())
        self.assertFalse(imported_path.with_name(imported_path.name + ".tuf").exists())
        with self.assertRaisesRegex(ValueError, "only model bundles imported"):
            self.engine.delete_model("outside.reynmodel")

    def test_invalid_checkpoint_is_rejected_with_a_clear_reason(self):
        source = self.root / "notes.reynmodel"
        torch.save({"not": "a model"}, source)

        card = self.engine.checkpoint_card(source)

        self.assertEqual(card["status"], "invalid")
        self.assertTrue(card["status_detail"])
        result = self.engine.import_model(source)
        self.assertFalse(result["ok"])
        self.assertFalse(result["validation"]["accepted"])
        self.assertTrue(result["validation"]["issues"])
        self.assertFalse(self.engine.managed_model_dir.exists())

    def test_unsigned_bundle_is_rejected_and_never_copied(self):
        source = self.root / "unsigned.reynmodel"
        self.checkpoint(source)
        source.with_name(source.name + ".sig").unlink()

        result = self.engine.import_model(source)

        self.assertFalse(result["ok"])
        self.assertIn(
            "signature.missing",
            {issue["code"] for issue in result["validation"]["issues"]},
        )
        self.assertFalse(self.engine.managed_model_dir.exists())
        self.assertEqual(self.engine.cache, {})

    def test_pickle_checkpoint_returns_structured_fail_closed_rejection(self):
        source = self.root / "unsupported.pth"
        torch.save({"model_state_dict": {}}, source)

        result = self.engine.import_model(source)

        self.assertFalse(result["ok"])
        self.assertIn(
            "bundle.invalid_extension",
            {issue["code"] for issue in result["validation"]["issues"]},
        )
        self.assertFalse(self.engine.managed_model_dir.exists())

    def test_state_dictionary_load_failure_is_structured_and_not_copied(self):
        source = self.root / "shape-mismatch.reynmodel"
        self.checkpoint(source)

        def fail_probe(_path):
            raise RuntimeError("state dictionary shape mismatch")

        self.engine._probe_checkpoint_compatibility = fail_probe
        result = self.engine.import_model(source)

        self.assertFalse(result["ok"])
        self.assertIn(
            "checkpoint.load_incompatible",
            {issue["code"] for issue in result["validation"]["issues"]},
        )
        self.assertFalse(
            any(self.engine.managed_model_dir.glob("*.reynmodel"))
        )
        self.assertTrue(self.engine.model_trust_state_dir.is_dir())

    def test_legacy_checkpoint_is_never_deserialized_by_runtime(self):
        source = self.root / "legacy.pth"
        torch.save({"model_state_dict": {}}, source)

        with patch.object(
            torch,
            "load",
            side_effect=AssertionError("runtime must never call torch.load"),
        ):
            card = self.engine.checkpoint_card(source)

        self.assertEqual(card["status"], "invalid")
        self.assertIn("pickle-backed checkpoints are disabled", card["status_detail"])
        self.assertIn(
            "checkpoint.unsafe_pickle_disabled",
            {issue["code"] for issue in card["validation_issues"]},
        )


@unittest.skipIf(torch is None, "torch is available in the Reyn research environment")
class ModelContractTests(unittest.TestCase):
    def test_fixed_body_v2_packs_sponge_and_viscosity(self):
        research_dir = Path(__file__).resolve().parents[2] / "reyn-research"
        sys.path.insert(0, str(research_dir))
        from flow_contract import fixed_body_v2_metadata

        class RecordingModel:
            def __init__(self):
                self.packed = None
                self.params = None

            def __call__(self, packed, dt, params=None):
                self.packed = packed
                self.params = params
                return packed[:, :2]

        model = RecordingModel()
        engine = Engine.__new__(Engine)
        engine.torch = torch
        engine.device = torch.device("cpu")
        n = 8
        state = torch.randn(1, 2, n, n)
        mask = torch.zeros(1, 1, n, n)
        mask[:, :, 2:4, 2:4] = 1.0
        sponge = torch.full((1, 1, n, n), 2.5)
        context = {
            "nu": torch.tensor([[0.004]]),
            "sponge": sponge,
            "u_inf": torch.tensor([[1.0, 0.0]]),
            "eta": torch.tensor([[1e-3]]),
        }
        info = {
            "model": model,
            "cfg": {
                "in_channels": 4,
                "out_channels": 2,
                "param_dim": 1,
            },
            "ta": {"dt": 0.01, "stride": 4},
            "physics_spec": fixed_body_v2_metadata(),
        }

        result = engine._run_model_2d(info, state, mask, context, horizon=3)

        self.assertEqual(tuple(result.shape), (1, 2, n, n))
        self.assertEqual(tuple(model.packed.shape), (1, 4, n, n))
        self.assertTrue(torch.equal(model.packed[:, 2:3], mask))
        self.assertTrue(torch.allclose(model.packed[:, 3:4], sponge / 5.0))
        self.assertEqual(tuple(model.params.shape), (1, 1))
        self.assertTrue(torch.isfinite(model.params).all())

    def test_physics_checkpoint_benchmark_round_trip(self):
        research_dir = Path(__file__).resolve().parents[2] / "reyn-research"
        sys.path.insert(0, str(research_dir))
        from flow_contract import fixed_body_v2_metadata
        from time_moe_operator import DirectFlowMap

        config = {
            "in_channels": 4,
            "out_channels": 2,
            "width": 8,
            "trunk_depth": 1,
            "time_dim": 8,
            "dt_scale": 0.01,
            "param_dim": 1,
        }
        model = DirectFlowMap(**config)
        train_args = {
            "dataset": "standard",
            "epochs": 1,
            "seed": 0,
            "grid_size": 16,
            "dt": 0.01,
            "stride": 1,
            "warmup_steps": 1,
            "max_steps": 2,
        }
        checkpoint = {
            "model_config": config,
            "model_state_dict": model.state_dict(),
            "train_args": train_args,
            "epoch": 1,
            "checkpoint_role": "fixed_final",
            "source_fingerprint": {
                "algorithm": "sha256",
                "digest": "c" * 64,
            },
            "physics_spec": fixed_body_v2_metadata(),
        }
        import model_bundle
        import test_model_bundle as bundle_test_support
        from model_bundle import write_model_bundle

        engine = Engine.__new__(Engine)
        engine.research_dir = str(research_dir)
        engine._managed_model_dir = research_dir / "reyn_models"
        engine.torch = torch
        engine.device = torch.device("cpu")
        engine.cache = {}
        engine.traj2d = {}
        engine.cad_cache = {}

        with tempfile.TemporaryDirectory() as directory:
            engine.research_dir = directory
            engine._managed_model_dir = Path(directory) / "reyn_models"
            path = str(Path(directory) / "physics.reynmodel")
            private_key, signature_entry = signing_context(Path(directory))
            write_model_bundle(
                checkpoint,
                path,
                model_id="physics-fixture",
                model_version="1.0.0",
            )
            sign_bundle(path, private_key)
            repository = bundle_test_support.write_tuf_repository(
                Path(path),
                signature_entry,
            )
            with patch.object(
                model_bundle,
                "PINNED_TUF_ROOT_JSON",
                repository["bootstrap_root"],
            ):
                suite = engine.run_benchmark({
                    "model": path,
                    "seeds": [70000],
                    "horizons": [1],
                })
                cell, meta = engine.inspect_benchmark_cell({
                    "model": path,
                    "seed": 70000,
                    "horizon": 1,
                })

        self.assertEqual(suite["provenance"]["verdict"], "clean")
        self.assertEqual(np.asarray(cell).shape, (3 * 16 * 16,))
        self.assertTrue(np.isfinite(cell).all())
        self.assertTrue(np.isfinite(meta["rel_l2"]))


if __name__ == "__main__":
    unittest.main()
