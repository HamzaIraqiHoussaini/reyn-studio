"""Deterministic Linux portable / AppImage / .deb packaging helpers."""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import stat
import subprocess
import tarfile
import tempfile
from pathlib import Path

from windows_packaging import (
    DEFAULT_SOURCE_DATE_EPOCH,
    ENGINE_RESOURCES,
    PREVIEW_MODEL_NAME,
    RESEARCH_RESOURCES,
    copy_resources,
    inventory,
    locked_python_packages,
    normalize_architecture,
    normalize_package_name,
    normalize_python_dependency_metadata,
    research_closure_sha256,
    safe_copy_file,
    safe_copy_tree,
    safe_files,
    sha256_file,
    write_json,
    write_sha256sums,
)

__all__ = [
    "DEFAULT_SOURCE_DATE_EPOCH",
    "ENGINE_RESOURCES",
    "PREVIEW_MODEL_NAME",
    "RESEARCH_RESOURCES",
    "build_appimage",
    "build_deb",
    "copy_resources",
    "deterministic_tar_xz",
    "generate_supply_chain_artifacts",
    "inventory",
    "loader_probe",
    "prepare_runtime_manifest",
    "resolve_python_binary",
    "runtime_probe",
    "safe_copy_file",
    "safe_copy_tree",
    "safe_files",
    "sha256_file",
    "validate_stage",
    "write_json",
    "write_sha256sums",
]

EXPECTED_LOADER_PROBE = {
    "bundle_schema": "com.reyn.inference-model-bundle/1",
    "bundled_model_authenticity": "verified",
    "bundled_model_dimension": 2,
    "bundled_model_id": PREVIEW_MODEL_NAME,
    "bundled_model_max_steps": 64,
    "bundled_model_sha256": "1282395279cbbe8dea50524bb5844938edb5df44e4f15c6a8b4cb1bf5fd0e022",
    "bundled_model_status": "clean",
    "import_ok": False,
    "loader_origin": "engine",
    "model_card_status": "invalid",
    "production_tuf_root_pinned": True,
}

EXPECTED_ACCESS = {
    "schema": "com.reyn.studio.preview-access/1",
    "required": True,
    "endpoint": "https://reynflow.com/api/yc-access/v1/session",
    "terms_version": "1.0",
    "privacy_version": "1.0",
}


def resolve_python_binary(runtime: Path) -> Path:
    candidates = (
        runtime / "bin/python3.14",
        runtime / "bin/python3",
        runtime / "bin/python",
    )
    for candidate in candidates:
        if candidate.is_file():
            return candidate
    raise ValueError(f"bundled runtime has no python binary under {runtime}/bin")


def runtime_probe(runtime: Path) -> dict[str, str]:
    python = resolve_python_binary(runtime)
    completed = subprocess.run(
        [
            str(python),
            "-I",
            "-c",
            (
                "import importlib.metadata as m,json,platform,sys,numpy,torch;"
                "print(json.dumps({'python':platform.python_version(),"
                "'numpy':numpy.__version__.split('+',1)[0],"
                "'torch':torch.__version__.split('+',1)[0],"
                "'cryptography':m.version('cryptography'),"
                "'safetensors':m.version('safetensors'),"
                "'securesystemslib':m.version('securesystemslib'),"
                "'tuf':m.version('tuf'),"
                "'platform':sys.platform,'machine':platform.machine(),"
                "'cuda':bool(torch.cuda.is_available())},sort_keys=True))"
            ),
        ],
        check=True,
        capture_output=True,
        text=True,
        timeout=120,
    )
    result = json.loads(completed.stdout.strip().splitlines()[-1])
    if result["platform"] != "linux":
        raise ValueError(f"runtime reports platform {result['platform']}, expected linux")
    architecture = normalize_architecture(str(result["machine"]))
    if architecture != "x86_64":
        raise ValueError(
            f"runtime reports machine {result['machine']}, expected Linux x86_64"
        )
    if result["cuda"]:
        raise ValueError("Linux v1 runtime must be CPU-only; CUDA was detected")
    expected_versions = {
        "python": "3.14.6",
        "cryptography": "49.0.0",
        "numpy": "2.5.1",
        "safetensors": "0.8.0",
        "securesystemslib": "1.4.0",
        "torch": "2.13.0",
        "tuf": "6.0.0",
    }
    for package, expected in expected_versions.items():
        if result[package] != expected:
            raise ValueError(
                f"runtime reports {package} {result[package]}, expected {expected}"
            )
    normalized = {key: str(value) for key, value in result.items()}
    normalized["machine"] = architecture
    return normalized


def loader_probe(stage: Path) -> dict[str, object]:
    """Exercise the staged loader, engine model card, and import rejection path."""

    python = resolve_python_binary(stage / "ReynPython")
    engine = stage / "resources/engine"
    research = stage / "resources/research"
    script = r"""
import json
import os
import pathlib
import shutil
import sys
import tempfile

engine_dir, research_dir = map(pathlib.Path, sys.argv[1:3])
sys.path.insert(0, str(engine_dir))
import model_bundle
from reyn_engine import Engine

if pathlib.Path(model_bundle.__file__).resolve().parent != engine_dir.resolve():
    raise RuntimeError("model_bundle resolved outside the staged engine directory")
if model_bundle.PINNED_TUF_ROOT_JSON is None:
    raise RuntimeError("packaging probe expected the YC preview TUF root to be pinned")

with tempfile.TemporaryDirectory(prefix="reyn-loader-probe-") as temporary:
    candidate = pathlib.Path(temporary) / "malformed.reynmodel"
    candidate.write_bytes(b"not a model bundle")
    try:
        model_bundle.load_model_bundle(
            candidate,
            trusted_state_dir=pathlib.Path(temporary) / "trusted-state",
        )
    except model_bundle.ModelBundleError as error:
        loader_error = error.code
    else:
        raise RuntimeError("production loader accepted a malformed unsigned bundle")

    original_cwd = os.getcwd()
    try:
        runtime = Engine(temporary, requested_device="cpu")
        card = runtime.checkpoint_card(candidate)
        imported = runtime.import_model(candidate)
        if card.get("status") != "invalid":
            raise RuntimeError(f"model card accepted malformed bundle: {card!r}")
        if imported.get("ok") is not False:
            raise RuntimeError(f"model import accepted malformed bundle: {imported!r}")
    finally:
        os.chdir(original_cwd)

with tempfile.TemporaryDirectory(prefix="reyn-preview-model-probe-") as temporary:
    probe_research = pathlib.Path(temporary)
    model_name = "reyn-h64-tail-brinkman-seed0-v1.reynmodel"
    shutil.copy2(research_dir / model_name, probe_research / model_name)
    shutil.copy2(research_dir / f"{model_name}.sig", probe_research / f"{model_name}.sig")
    shutil.copytree(
        research_dir / f"{model_name}.tuf",
        probe_research / f"{model_name}.tuf",
    )
    sys.path.insert(0, str(research_dir))
    original_cwd = os.getcwd()
    try:
        runtime = Engine(str(probe_research), requested_device="cpu")
        models = runtime.list_model_cards()
        if len(models) != 1:
            raise RuntimeError(f"expected exactly one bundled model, found {models!r}")
        preview = models[0]
        if preview.get("name") != model_name:
            raise RuntimeError(f"unexpected bundled model: {preview!r}")
        if preview.get("status") != "clean":
            raise RuntimeError(f"bundled model did not validate cleanly: {preview!r}")
        if preview.get("authenticity_status") != "verified":
            raise RuntimeError(f"bundled model authenticity was not verified: {preview!r}")
        if preview.get("dimension") != 2 or preview.get("max_steps") != 64:
            raise RuntimeError(f"bundled model support envelope is wrong: {preview!r}")
    finally:
        os.chdir(original_cwd)

print(json.dumps({
    "bundle_schema": model_bundle.BUNDLE_SCHEMA,
    "bundled_model_authenticity": preview["authenticity_status"],
    "bundled_model_dimension": preview["dimension"],
    "bundled_model_id": preview["id"],
    "bundled_model_max_steps": preview["max_steps"],
    "bundled_model_sha256": preview["checkpoint_sha256"],
    "bundled_model_status": preview["status"],
    "loader_error": loader_error,
    "model_card_status": card["status"],
    "import_ok": imported["ok"],
    "loader_origin": "engine",
    "production_tuf_root_pinned": model_bundle.PINNED_TUF_ROOT_JSON is not None,
}, sort_keys=True))
"""
    try:
        completed = subprocess.run(
            [str(python), "-I", "-c", script, str(engine), str(research)],
            check=True,
            capture_output=True,
            text=True,
            timeout=120,
        )
    except subprocess.CalledProcessError as error:
        details = (error.stderr or error.stdout or "").strip()
        raise RuntimeError(f"staged model-loader probe failed: {details}") from error
    result = json.loads(completed.stdout.strip().splitlines()[-1])
    for key, value in EXPECTED_LOADER_PROBE.items():
        if result.get(key) != value:
            raise ValueError(f"loader probe reports {key}={result.get(key)!r}, expected {value!r}")
    if not str(result.get("loader_error", "")).strip():
        raise ValueError("loader probe did not report a fail-closed validation error")
    return result


def python_dependency_metadata(runtime: Path) -> list[dict[str, str]]:
    python = resolve_python_binary(runtime)
    script = r"""
import importlib.metadata as metadata
import json

rows = []
for distribution in metadata.distributions():
    package = distribution.metadata
    project_urls = package.get_all("Project-URL") or []
    source = ""
    for value in project_urls:
        label, separator, url = value.partition(",")
        if separator and label.strip().lower() in {"source", "homepage", "repository"}:
            source = url.strip()
            break
    if not source and project_urls:
        source = project_urls[0].partition(",")[2].strip()
    source = source or package.get("Home-page", "") or package.get("Download-URL", "")
    license_name = package.get("License-Expression", "") or package.get("License", "")
    if not license_name:
        classifiers = package.get_all("Classifier") or []
        license_name = " OR ".join(
            item.removeprefix("License :: ").strip()
            for item in classifiers
            if item.startswith("License :: ")
        )
    rows.append({
        "name": package.get("Name", ""),
        "version": distribution.version,
        "license": license_name.strip(),
        "source": source.strip(),
    })
print(json.dumps(rows, sort_keys=True))
"""
    completed = subprocess.run(
        [str(python), "-I", "-c", script],
        check=True,
        capture_output=True,
        text=True,
        timeout=120,
    )
    rows = json.loads(completed.stdout.strip().splitlines()[-1])
    return normalize_python_dependency_metadata(rows)


def rust_dependency_metadata(root: Path) -> list[dict[str, str]]:
    completed = subprocess.run(
        [
            "cargo",
            "metadata",
            "--locked",
            "--format-version",
            "1",
            "--filter-platform",
            "x86_64-unknown-linux-gnu",
            "--manifest-path",
            str(root / "Cargo.toml"),
        ],
        cwd=root,
        check=True,
        capture_output=True,
        text=True,
        timeout=300,
    )
    metadata = json.loads(completed.stdout)
    packages = {package["id"]: package for package in metadata["packages"]}
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    root_id = metadata["resolve"]["root"]
    pending = [root_id]
    resolved: set[str] = set()
    while pending:
        package_id = pending.pop()
        if package_id in resolved:
            continue
        resolved.add(package_id)
        pending.extend(dependency["pkg"] for dependency in nodes[package_id]["deps"])
    rows: list[dict[str, str]] = []
    for package_id in sorted(resolved):
        if package_id == root_id:
            continue
        package = packages[package_id]
        license_name = str(package.get("license") or "").strip()
        if not license_name:
            raise ValueError(
                f"Rust dependency {package['name']} {package['version']} has no license metadata"
            )
        source = str(package.get("source") or package.get("repository") or "").strip()
        if not source:
            raise ValueError(
                f"Rust dependency {package['name']} {package['version']} has no source metadata"
            )
        rows.append(
            {
                "ecosystem": "cargo",
                "name": package["name"],
                "normalized_name": normalize_package_name(package["name"]),
                "version": package["version"],
                "license": license_name,
                "source": source,
            }
        )
    return sorted(rows, key=lambda row: (row["normalized_name"], row["version"]))


def generate_supply_chain_artifacts(
    root: Path,
    runtime: Path,
    stage: Path,
    python_lock: Path,
) -> dict[str, object]:
    locked = locked_python_packages(python_lock)
    python_packages = python_dependency_metadata(runtime)
    installed = {
        row["normalized_name"]: row["version"] for row in python_packages
    }
    if installed != locked:
        raise ValueError(
            "staged Python distributions do not match python-runtime.lock; "
            f"installed={installed}, locked={locked}"
        )
    packages = rust_dependency_metadata(root) + [
        {
            "ecosystem": "python",
            "name": "CPython",
            "normalized_name": "cpython",
            "version": "3.14.6",
            "license": "PSF-2.0",
            "source": "https://www.python.org/",
        },
        *python_packages,
    ]
    packages.sort(
        key=lambda row: (
            str(row["ecosystem"]),
            str(row["normalized_name"]),
            str(row["version"]),
        )
    )
    closure = {
        "schema": "com.reyn.dependency-closure/1",
        "target": "linux-x86_64",
        "cargo_lock_sha256": sha256_file(root / "Cargo.lock"),
        "python_lock_sha256": sha256_file(python_lock),
        "packages": packages,
    }
    write_json(stage / "dependency-closure.json", closure)
    spdx_packages = []
    for index, package in enumerate(packages, start=1):
        ecosystem = str(package["ecosystem"])
        name = str(package["name"])
        version = str(package["version"])
        spdx_packages.append(
            {
                "SPDXID": f"SPDXRef-Package-{index:04d}",
                "name": name,
                "versionInfo": version,
                "downloadLocation": package["source"],
                "filesAnalyzed": False,
                "licenseConcluded": "NOASSERTION",
                "licenseDeclared": package["license"],
                "externalRefs": [
                    {
                        "referenceCategory": "PACKAGE-MANAGER",
                        "referenceType": "purl",
                        "referenceLocator": (
                            f"pkg:{ecosystem}/{normalize_package_name(name)}@{version}"
                        ),
                    }
                ],
            }
        )
    write_json(
        stage / "SBOM.spdx.json",
        {
            "spdxVersion": "SPDX-2.3",
            "dataLicense": "CC0-1.0",
            "SPDXID": "SPDXRef-DOCUMENT",
            "name": "Reyn-Studio-Linux-dependency-closure",
            "documentNamespace": (
                "https://reynflow.com/sbom/linux/"
                + hashlib.sha256(
                    json.dumps(closure, sort_keys=True, separators=(",", ":")).encode(
                        "utf-8"
                    )
                ).hexdigest()
            ),
            "creationInfo": {
                "created": "1980-01-01T00:00:00Z",
                "creators": ["Tool: Reyn deterministic Linux packager"],
            },
            "packages": spdx_packages,
        },
    )
    notice_lines = [
        "# Reyn Studio Linux third-party notices",
        "",
        "Generated from the locked Rust and Python dependency closure.",
        "",
    ]
    for package in packages:
        notice_lines.extend(
            [
                f"## {package['name']} {package['version']} ({package['ecosystem']})",
                "",
                f"License: {package['license']}",
                f"Source: {package['source']}",
                "",
            ]
        )
    (stage / "THIRD_PARTY_NOTICES.md").write_text(
        "\n".join(notice_lines), encoding="utf-8"
    )
    return closure


def prepare_runtime_manifest(
    stage: Path,
    source_revision: str,
    probe: dict[str, str],
    dependency_closure: dict[str, object],
) -> None:
    runtime = stage / "ReynPython"
    write_json(
        runtime / "runtime-sbom.cdx.json",
        {
            "bomFormat": "CycloneDX",
            "specVersion": "1.6",
            "version": 1,
            "metadata": {
                "component": {
                    "type": "application",
                    "name": "ReynPython",
                    "version": probe["python"],
                }
            },
            "components": [
                {
                    "type": (
                        "framework"
                        if package["normalized_name"] == "cpython"
                        else "library"
                    ),
                    "name": package["name"],
                    "version": package["version"],
                    "licenses": [{"expression": package["license"]}],
                    "externalReferences": (
                        []
                        if package["source"] == "NOASSERTION"
                        else [{"type": "website", "url": package["source"]}]
                    ),
                }
                for package in dependency_closure["packages"]
                if package["ecosystem"] == "python"
            ],
        },
    )
    notices = (stage / "THIRD_PARTY_NOTICES.md").read_text(encoding="utf-8")
    (runtime / "THIRD_PARTY_NOTICES.html").write_text(
        "<!doctype html><meta charset=\"utf-8\"><pre>"
        + notices.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")
        + "</pre>\n",
        encoding="utf-8",
    )
    files = []
    for path in safe_files(runtime):
        if not path.is_file() or path.name in {
            "runtime-manifest.cjson",
            "runtime-manifest.sig",
        }:
            continue
        files.append(
            {
                "path": path.relative_to(runtime).as_posix(),
                "sha256": sha256_file(path),
                "size": path.stat().st_size,
            }
        )
    manifest = {
        "schema": "com.reyn.runtime-manifest/1",
        "runtime_id": "",
        "platform": "linux",
        "architecture": "x86_64",
        "python": probe["python"],
        "torch": probe["torch"],
        "numpy": probe["numpy"],
        "engine_protocol": 1,
        "research_closure_sha256": research_closure_sha256(stage),
        "source_revision": source_revision,
        "build_epoch": 0,
        "files": files,
        "sbom_sha256": sha256_file(runtime / "runtime-sbom.cdx.json"),
        "notices_sha256": sha256_file(runtime / "THIRD_PARTY_NOTICES.html"),
    }
    identity = dict(manifest)
    identity.pop("runtime_id")
    identity_bytes = json.dumps(
        identity, sort_keys=True, separators=(",", ":")
    ).encode("utf-8")
    manifest["runtime_id"] = "sha256:" + hashlib.sha256(identity_bytes).hexdigest()
    (runtime / "runtime-manifest.cjson").write_text(
        json.dumps(manifest, sort_keys=True, separators=(",", ":")),
        encoding="utf-8",
    )


def validate_stage(
    stage: Path,
    run_runtime_probe: bool = False,
) -> list[str]:
    errors: list[str] = []
    try:
        staged_files = {
            path.relative_to(stage.absolute()).as_posix()
            for path in safe_files(stage)
        }
    except (OSError, ValueError) as error:
        return [f"unsafe package tree: {error}"]
    required = (
        "reyn-studio",
        "ReynStudio.png",
        "LICENSE",
        "NOTICE",
        "ReynPython/bin/python3.14",
        "ReynPython/runtime-manifest.cjson",
        "ReynPython/runtime-sbom.cdx.json",
        "ReynPython/THIRD_PARTY_NOTICES.html",
        "resources/engine/reyn_engine.py",
        "resources/engine/model_bundle.py",
        "resources/docs/PRD.md",
        "resources/docs/MODEL_BUNDLE_PROVENANCE.md",
        "THIRD_PARTY_NOTICES.md",
        "SBOM.spdx.json",
        "dependency-closure.json",
        "release-manifest.json",
        "resource-inventory.json",
    )
    for relative in required:
        if relative not in staged_files:
            errors.append(f"missing {relative}")
    for name in RESEARCH_RESOURCES:
        if not (stage / "resources/research" / name).is_file():
            errors.append(f"missing resources/research/{name}")
    binary = stage / "reyn-studio"
    if binary.is_file() and not os.access(binary, os.X_OK):
        errors.append("reyn-studio is not executable")
    if errors:
        return errors

    manifest = json.loads((stage / "release-manifest.json").read_text(encoding="utf-8"))
    if manifest.get("platform") != "linux" or manifest.get("architecture") != "x86_64":
        errors.append("release manifest must declare linux/x86_64")
    if manifest.get("cuda_supported") is not False:
        errors.append("release manifest must state cuda_supported=false")
    if manifest.get("linux_verified") is not False:
        errors.append("local packaging must not claim Linux verification")
    if "Linux preview pending verification" not in str(manifest.get("verification_boundary", "")):
        errors.append("release manifest must label Linux preview pending verification")
    if manifest.get("preview_access") != EXPECTED_ACCESS:
        errors.append("release manifest must record the exact YC preview access contract")
    recorded_loader_probe = manifest.get("model_loader_probe")
    if not isinstance(recorded_loader_probe, dict):
        errors.append("release manifest must record the staged model-loader probe")
    else:
        for key, value in EXPECTED_LOADER_PROBE.items():
            if recorded_loader_probe.get(key) != value:
                errors.append(
                    f"release manifest model_loader_probe.{key} must equal {value!r}"
                )
        if not str(recorded_loader_probe.get("loader_error", "")).strip():
            errors.append("release manifest model-loader probe must record rejection code")
    bundled_models = manifest.get("bundled_models")
    if not isinstance(bundled_models, list) or len(bundled_models) != 1:
        errors.append("release manifest must record exactly one bundled YC preview model")
    else:
        bundled_model = bundled_models[0]
        if bundled_model.get("bundle_sha256") != EXPECTED_LOADER_PROBE[
            "bundled_model_sha256"
        ]:
            errors.append("bundled model manifest SHA-256 does not match loader probe")
        if bundled_model.get("schema") != "com.reyn.yc-preview-model-release/1":
            errors.append("bundled model release manifest schema is invalid")
        if (
            bundled_model.get("qualification_boundary")
            != "Three-seed research replication passed; production scientific/runtime/distribution qualification remains incomplete."
        ):
            errors.append("bundled model qualification boundary is missing or altered")

    closure = json.loads(
        (stage / "dependency-closure.json").read_text(encoding="utf-8")
    )
    packages = closure.get("packages")
    if closure.get("schema") != "com.reyn.dependency-closure/1" or not isinstance(
        packages, list
    ):
        errors.append("dependency closure is malformed")
        packages = []
    if closure.get("target") != "linux-x86_64":
        errors.append("dependency closure target must be linux-x86_64")
    for field in ("cargo_lock_sha256", "python_lock_sha256"):
        if manifest.get(field) != closure.get(field):
            errors.append(f"release manifest {field} does not match dependency closure")
    invalid_metadata_values = {"", "UNKNOWN", "NOASSERTION", "NONE", "NULL"}
    for package in packages:
        name = str(package.get("name") or "").strip()
        version = str(package.get("version") or "").strip()
        license_name = str(package.get("license") or "").strip()
        source = str(package.get("source") or "").strip()
        identity = f"{name or '<unnamed>'} {version or '<unversioned>'}"
        if not name or not version:
            errors.append(f"dependency closure has incomplete package identity: {identity}")
        if license_name.upper() in invalid_metadata_values:
            errors.append(f"dependency closure package {identity} has no license metadata")
        if source.upper() in invalid_metadata_values:
            errors.append(f"dependency closure package {identity} has no source metadata")
    sbom = json.loads((stage / "SBOM.spdx.json").read_text(encoding="utf-8"))
    sbom_packages = sbom.get("packages")
    if sbom.get("spdxVersion") != "SPDX-2.3" or not isinstance(sbom_packages, list):
        errors.append("SBOM must be an SPDX-2.3 package inventory")
        sbom_packages = []
    closure_rows = sorted(
        (
            str(package.get("name")),
            str(package.get("version")),
            str(package.get("license")),
            str(package.get("source")),
        )
        for package in packages
    )
    sbom_rows = sorted(
        (
            str(package.get("name")),
            str(package.get("versionInfo")),
            str(package.get("licenseDeclared")),
            str(package.get("downloadLocation")),
        )
        for package in sbom_packages
    )
    if closure_rows != sbom_rows:
        errors.append("SBOM package closure does not match dependency-closure.json")
    notices = (stage / "THIRD_PARTY_NOTICES.md").read_text(encoding="utf-8")
    for package in packages:
        marker = f"## {package.get('name')} {package.get('version')} ({package.get('ecosystem')})"
        if marker not in notices:
            errors.append(f"third-party notices omit {marker}")
    runtime_sbom = json.loads(
        (stage / "ReynPython/runtime-sbom.cdx.json").read_text(encoding="utf-8")
    )
    runtime_rows = sorted(
        (
            str(component.get("name")),
            str(component.get("version")),
        )
        for component in runtime_sbom.get("components", [])
    )
    expected_runtime_rows = sorted(
        (str(package.get("name")), str(package.get("version")))
        for package in packages
        if package.get("ecosystem") == "python"
    )
    if runtime_rows != expected_runtime_rows:
        errors.append("runtime SBOM does not match the staged Python closure")

    expected = inventory(
        stage,
        excluded={"release-manifest.json", "resource-inventory.json"},
    )
    recorded = json.loads((stage / "resource-inventory.json").read_text(encoding="utf-8"))
    if expected != recorded:
        errors.append("resource inventory does not match staged files")

    if run_runtime_probe:
        try:
            runtime_probe(stage / "ReynPython")
            loader_probe(stage)
        except (OSError, ValueError, subprocess.SubprocessError, json.JSONDecodeError) as error:
            errors.append(f"runtime or model-loader probe failed: {error}")
    return errors


def deterministic_tar_xz(
    stage: Path,
    destination: Path,
    source_date_epoch: int,
) -> None:
    files = safe_files(stage)
    destination.parent.mkdir(parents=True, exist_ok=True)
    if destination.exists():
        destination.unlink()
    with tarfile.open(destination, "w:xz", preset=6) as archive:
        for path in files:
            info = archive.gettarinfo(path, arcname=path.relative_to(stage).as_posix())
            info.uid = 0
            info.gid = 0
            info.uname = ""
            info.gname = ""
            info.mtime = source_date_epoch
            mode = info.mode
            if path.is_dir():
                info.mode = 0o755
            elif os.access(path, os.X_OK):
                info.mode = 0o755
            else:
                info.mode = 0o644
            if path.is_file():
                with path.open("rb") as handle:
                    archive.addfile(info, handle)
            else:
                archive.addfile(info)
            _ = mode


def build_appimage(
    root: Path,
    stage: Path,
    destination: Path,
    version: str,
    *,
    appimagetool: Path | None = None,
) -> Path:
    with tempfile.TemporaryDirectory(prefix=".reyn-appdir-") as temporary:
        temporary_root = Path(temporary)
        appdir = temporary_root / "ReynStudio.AppDir"
        safe_copy_tree(stage, appdir)
        safe_copy_file(
            root / "packaging/linux/AppRun",
            root,
            appdir / "AppRun",
        )
        (appdir / "AppRun").chmod(
            (appdir / "AppRun").stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH
        )
        desktop = (root / "packaging/linux/ReynStudio.desktop").read_text(encoding="utf-8")
        desktop = desktop.replace(
            "X-AppImage-Version=preview",
            f"X-AppImage-Version={version}",
        )
        (appdir / "reyn-studio.desktop").write_text(desktop, encoding="utf-8")
        safe_copy_file(
            root / "packaging/linux/ReynStudio.png",
            root,
            appdir / "ReynStudio.png",
        )
        destination.parent.mkdir(parents=True, exist_ok=True)
        if destination.exists():
            destination.unlink()

        tool = appimagetool or Path(shutil.which("appimagetool") or "")
        env = os.environ.copy()
        env["ARCH"] = "x86_64"
        env["VERSION"] = version
        env["SOURCE_DATE_EPOCH"] = str(
            env.get("SOURCE_DATE_EPOCH", DEFAULT_SOURCE_DATE_EPOCH)
        )
        # GitHub-hosted runners often lack FUSE for nested AppImages.
        env.setdefault("APPIMAGE_EXTRACT_AND_RUN", "1")

        built = False
        if tool.is_file():
            try:
                subprocess.run(
                    [str(tool), "--no-appstream", str(appdir), str(destination)],
                    check=True,
                    env=env,
                )
                built = destination.is_file()
            except (OSError, subprocess.CalledProcessError) as error:
                # Rosetta/qemu containers often cannot exec the AppImage wrapper.
                print(f"appimagetool direct exec failed ({error}); using runtime+mksquashfs")

        if not built:
            _build_appimage_with_runtime(appdir, destination, temporary_root)

    if not destination.is_file():
        raise ValueError(f"AppImage was not created at {destination}")
    destination.chmod(destination.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    return destination


def _build_appimage_with_runtime(
    appdir: Path,
    destination: Path,
    temporary_root: Path,
) -> None:
    """Assemble a Type-2 AppImage without executing appimagetool."""

    if shutil.which("mksquashfs") is None:
        raise ValueError(
            "mksquashfs is required to build the AppImage when appimagetool cannot run; "
            "install squashfs-tools (see docs/LINUX_RELEASE.md)"
        )
    runtime = temporary_root / "runtime-x86_64"
    squashfs = temporary_root / "ReynStudio.squashfs"
    url = (
        "https://github.com/AppImage/type2-runtime/releases/download/"
        "continuous/runtime-x86_64"
    )
    subprocess.run(
        ["curl", "-fsSL", "-o", str(runtime), url],
        check=True,
    )
    runtime.chmod(runtime.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    subprocess.run(
        [
            "mksquashfs",
            str(appdir),
            str(squashfs),
            "-root-owned",
            "-noappend",
            "-comp",
            "xz",
        ],
        check=True,
        env={**os.environ, "SOURCE_DATE_EPOCH": str(os.environ.get("SOURCE_DATE_EPOCH", DEFAULT_SOURCE_DATE_EPOCH))},
    )
    with destination.open("wb") as out, runtime.open("rb") as left, squashfs.open("rb") as right:
        shutil.copyfileobj(left, out)
        shutil.copyfileobj(right, out)


def build_deb(
    root: Path,
    stage: Path,
    destination: Path,
    version: str,
) -> Path:
    with tempfile.TemporaryDirectory(prefix=".reyn-deb-") as temporary:
        package_root = Path(temporary) / "pkg"
        opt = package_root / "opt/reyn-studio"
        applications = package_root / "usr/share/applications"
        icons = package_root / "usr/share/icons/hicolor/256x256/apps"
        debian = package_root / "DEBIAN"
        for path in (applications, icons, debian):
            path.mkdir(parents=True, exist_ok=True)
        safe_copy_tree(stage, opt)
        desktop = (root / "packaging/linux/ReynStudio.desktop").read_text(encoding="utf-8")
        desktop = desktop.replace("Exec=reyn-studio", "Exec=/opt/reyn-studio/reyn-studio")
        desktop = desktop.replace(
            "X-AppImage-Version=preview",
            f"X-AppImage-Version={version}",
        )
        (applications / "reyn-studio.desktop").write_text(desktop, encoding="utf-8")
        safe_copy_file(
            root / "packaging/linux/ReynStudio.png",
            root,
            icons / "ReynStudio.png",
        )
        installed_size = sum(path.stat().st_size for path in safe_files(package_root) if path.is_file())
        control = "\n".join(
            [
                "Package: reyn-studio",
                f"Version: {version}",
                "Section: science",
                "Priority: optional",
                "Architecture: amd64",
                f"Installed-Size: {max(1, installed_size // 1024)}",
                "Maintainer: Reyn <support@reynflow.com>",
                "Depends: libsecret-1-0, mesa-vulkan-drivers | vulkan-icd",
                "Description: Reyn Studio Linux preview (pending verification)",
                " Native desktop studio for fluid-flow cases with a bundled CPU",
                " Python runtime. Labeled preview until the Ubuntu clean-machine",
                " matrix passes.",
                "",
            ]
        )
        (debian / "control").write_text(control, encoding="utf-8")
        destination.parent.mkdir(parents=True, exist_ok=True)
        if destination.exists():
            destination.unlink()
        subprocess.run(
            ["dpkg-deb", "--root-owner-group", "--build", str(package_root), str(destination)],
            check=True,
        )
    if not destination.is_file():
        raise ValueError(f"dpkg-deb did not create {destination}")
    return destination
