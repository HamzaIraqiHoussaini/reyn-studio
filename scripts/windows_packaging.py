"""Deterministic Windows portable-package staging and validation helpers."""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import stat
import subprocess
import zipfile
from pathlib import Path


DEFAULT_SOURCE_DATE_EPOCH = 315532800
ENGINE_RESOURCES = (
    "model_bundle.py",
    "n5_inspector.py",
    "n5_overlap.py",
    "pinned_model_trust.py",
    "reyn_engine.py",
)
RESEARCH_RESOURCES = (
    "dataset.py",
    "dataset_3d.py",
    "flow_contract.py",
    "flow_quantities.py",
    "models_3d.py",
    "occupancy_geometry_3d.py",
    "obstacle_dataset.py",
    "obstacle_solver.py",
    "obstacle_solver_3d.py",
    "pressure_channel_contract_3d.py",
    "pressure_model_contract_3d.py",
    "spectral_solver.py",
    "spectral_solver_3d.py",
    "time_moe_operator.py",
)
DOCUMENTATION_RESOURCES = (
    ("PRD.md", "PRD.md"),
    ("docs/MODEL_BUNDLE_PROVENANCE.md", "MODEL_BUNDLE_PROVENANCE.md"),
)
PREVIEW_MODEL_ROOT = Path("packaging/models/yc-preview-h64")
PREVIEW_MODEL_NAME = "reyn-h64-tail-brinkman-seed0-v1.reynmodel"
PREVIEW_MODEL_FILES = (
    PREVIEW_MODEL_NAME,
    f"{PREVIEW_MODEL_NAME}.sig",
)
PREVIEW_MODEL_EVIDENCE = (
    "h64_v3_tail_brinkman_factorial_summary.json",
    "h64_v3_tail_brinkman_combined_replication_summary.json",
)
FLOW3D_RESEARCH_MODEL_ROOT = Path("packaging/models/flow3d-obstacle-32")
FLOW3D_RESEARCH_MODEL_NAME = "reyn-flow3d-obstacle-32-v1.reynmodel"
FLOW3D_RESEARCH_MODEL_FILES = (
    FLOW3D_RESEARCH_MODEL_NAME,
    f"{FLOW3D_RESEARCH_MODEL_NAME}.sig",
)
FLOW3D_RESEARCH_MODEL_MANIFEST = "flow3d-obstacle-32-model-release-manifest.json"
WINDOWS_REPARSE_POINT = getattr(stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0x400)


def normalize_package_name(name: str) -> str:
    return name.lower().replace("_", "-").replace(".", "-")


def normalize_architecture(value: str) -> str:
    normalized = value.strip().lower()
    if normalized in {"amd64", "x64", "x86_64"}:
        return "x86_64"
    if normalized in {"aarch64", "arm64"}:
        return "arm64"
    raise ValueError(f"unknown runtime architecture: {value}")


def is_windows_reparse_point(
    path: Path,
    *,
    lstat: object = os.lstat,
) -> bool:
    metadata = lstat(path)
    attributes = int(getattr(metadata, "st_file_attributes", 0))
    return bool(attributes & WINDOWS_REPARSE_POINT)


def safe_files(
    root: Path,
    *,
    reparse_checker: object = is_windows_reparse_point,
) -> list[Path]:
    root = root.absolute()
    if not root.is_dir():
        raise ValueError(f"safe tree root is not a directory: {root}")
    if root.is_symlink() or reparse_checker(root):
        raise ValueError(f"safe tree root is a link or reparse point: {root}")
    canonical_root = root.resolve(strict=True)
    files: list[Path] = []

    def visit(directory: Path) -> None:
        with os.scandir(directory) as entries:
            for entry in sorted(entries, key=lambda item: item.name):
                path = Path(entry.path)
                if entry.is_symlink() or reparse_checker(path):
                    raise ValueError(
                        f"package tree contains a link or reparse point: {path}"
                    )
                resolved = path.resolve(strict=True)
                if not resolved.is_relative_to(canonical_root):
                    raise ValueError(f"package path escapes its root: {path}")
                if entry.is_dir(follow_symlinks=False):
                    visit(path)
                elif entry.is_file(follow_symlinks=False):
                    files.append(path)
                else:
                    raise ValueError(f"package tree contains a special file: {path}")

    visit(root)
    return files


def assert_safe_file(
    path: Path,
    root: Path,
    *,
    reparse_checker: object = is_windows_reparse_point,
) -> None:
    root = root.absolute()
    path = path.absolute()
    try:
        relative = path.relative_to(root)
    except ValueError as error:
        raise ValueError(f"source file escapes its declared root: {path}") from error
    if root.is_symlink() or reparse_checker(root):
        raise ValueError(f"source root is a link or reparse point: {root}")
    current = root
    for component in relative.parts:
        current = current / component
        if current.is_symlink() or reparse_checker(current):
            raise ValueError(f"source path contains a link or reparse point: {current}")
    if not path.is_file():
        raise ValueError(f"source is not a regular file: {path}")
    if not path.resolve(strict=True).is_relative_to(root.resolve(strict=True)):
        raise ValueError(f"source file resolves outside its declared root: {path}")


def safe_copy_file(
    source: Path,
    source_root: Path,
    destination: Path,
    *,
    reparse_checker: object = is_windows_reparse_point,
) -> None:
    assert_safe_file(source, source_root, reparse_checker=reparse_checker)
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source, destination)


def safe_copy_tree(
    source: Path,
    destination: Path,
    *,
    reparse_checker: object = is_windows_reparse_point,
) -> None:
    files = safe_files(source, reparse_checker=reparse_checker)
    destination.mkdir(parents=True, exist_ok=False)
    for path in files:
        relative = path.relative_to(source.absolute())
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(path, target)


def assert_safe_output_file(path: Path) -> None:
    parent = path.parent.absolute()
    if parent.is_symlink() or is_windows_reparse_point(parent):
        raise ValueError(f"output directory is a link or reparse point: {parent}")
    if path.is_symlink() or (path.exists() and is_windows_reparse_point(path)):
        raise ValueError(f"output file is a link or reparse point: {path}")


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def write_json(path: Path, value: object) -> None:
    path.write_text(
        json.dumps(value, indent=2, sort_keys=True, ensure_ascii=False) + "\n",
        encoding="utf-8",
    )


def copy_resources(root: Path, research_source: Path, stage: Path) -> None:
    resources = stage / "resources"
    for name in ENGINE_RESOURCES:
        destination = resources / "engine" / name
        safe_copy_file(root / "engine" / name, root, destination)
    for name in RESEARCH_RESOURCES:
        destination = resources / "research" / name
        safe_copy_file(research_source / name, research_source, destination)
    for source, destination in DOCUMENTATION_RESOURCES:
        safe_copy_file(root / source, root, resources / "docs" / destination)
    model_source = root / PREVIEW_MODEL_ROOT
    for name in PREVIEW_MODEL_FILES:
        safe_copy_file(
            model_source / name,
            root,
            resources / "research" / name,
        )
    safe_copy_tree(
        model_source / f"{PREVIEW_MODEL_NAME}.tuf",
        resources / "research" / f"{PREVIEW_MODEL_NAME}.tuf",
    )
    safe_copy_file(
        model_source / "model-release-manifest.json",
        root,
        resources / "docs/models/model-release-manifest.json",
    )
    for name in PREVIEW_MODEL_EVIDENCE:
        safe_copy_file(
            model_source / "evidence" / name,
            root,
            resources / "docs/models" / name,
        )
    flow3d_source = root / FLOW3D_RESEARCH_MODEL_ROOT
    for name in FLOW3D_RESEARCH_MODEL_FILES:
        destination = resources / "research" / name
        safe_copy_file(flow3d_source / name, root, destination)
        destination.chmod(destination.stat().st_mode | 0o644)
    safe_copy_tree(
        flow3d_source / f"{FLOW3D_RESEARCH_MODEL_NAME}.tuf",
        resources / "research" / f"{FLOW3D_RESEARCH_MODEL_NAME}.tuf",
    )
    safe_copy_file(
        flow3d_source / "model-release-manifest.json",
        root,
        resources / "docs/models" / FLOW3D_RESEARCH_MODEL_MANIFEST,
    )


def inventory(
    stage: Path,
    excluded: set[str] | None = None,
    *,
    reparse_checker: object = is_windows_reparse_point,
) -> list[dict[str, object]]:
    excluded = excluded or set()
    rows: list[dict[str, object]] = []
    for path in safe_files(stage, reparse_checker=reparse_checker):
        relative = path.relative_to(stage).as_posix()
        if relative in excluded:
            continue
        rows.append(
            {
                "path": relative,
                "bytes": path.stat().st_size,
                "sha256": sha256_file(path),
            }
        )
    return rows


def runtime_probe(runtime: Path) -> dict[str, str]:
    python = runtime / "python.exe"
    if not python.is_file():
        raise ValueError(f"bundled runtime has no python.exe: {python}")
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
    if result["platform"] != "win32":
        raise ValueError(f"runtime reports platform {result['platform']}, expected win32")
    architecture = normalize_architecture(str(result["machine"]))
    if architecture != "x86_64":
        raise ValueError(
            f"runtime reports machine {result['machine']}, expected Windows x86_64"
        )
    if result["cuda"]:
        raise ValueError("Windows v1 runtime must be CPU-only; CUDA was detected")
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

    python = stage / "ReynPython/python.exe"
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
    expected = {
        "bundle_schema": "com.reyn.inference-model-bundle/1",
        "bundled_model_authenticity": "verified",
        "bundled_model_dimension": 2,
        "bundled_model_id": "reyn-h64-tail-brinkman-seed0-v1.reynmodel",
        "bundled_model_max_steps": 64,
        "bundled_model_sha256": "1282395279cbbe8dea50524bb5844938edb5df44e4f15c6a8b4cb1bf5fd0e022",
        "bundled_model_status": "clean",
        "import_ok": False,
        "loader_origin": "engine",
        "model_card_status": "invalid",
        "production_tuf_root_pinned": True,
    }
    for key, value in expected.items():
        if result.get(key) != value:
            raise ValueError(f"loader probe reports {key}={result.get(key)!r}, expected {value!r}")
    if not str(result.get("loader_error", "")).strip():
        raise ValueError("loader probe did not report a fail-closed validation error")
    return result


def research_closure_sha256(stage: Path) -> str:
    files: list[dict[str, object]] = []
    for prefix, names in (
        ("engine", ENGINE_RESOURCES),
        ("research", RESEARCH_RESOURCES),
    ):
        for name in names:
            source = stage / "resources" / prefix / name
            files.append(
                {
                    "path": f"{prefix}/{name}",
                    "sha256": sha256_file(source),
                    "size": source.stat().st_size,
                }
            )
    payload = json.dumps(
        sorted(files, key=lambda row: str(row["path"])),
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")
    return hashlib.sha256(payload).hexdigest()


def locked_python_packages(lock_path: Path) -> dict[str, str]:
    import re

    packages: dict[str, str] = {}
    for line in lock_path.read_text(encoding="utf-8").splitlines():
        match = re.match(r"^([A-Za-z0-9_.-]+)==([^ \\\\]+)", line)
        if match:
            packages[normalize_package_name(match.group(1))] = match.group(2)
    if not packages:
        raise ValueError(f"Python runtime lock contains no packages: {lock_path}")
    return packages


def normalize_python_dependency_metadata(
    rows: object,
) -> list[dict[str, str]]:
    if not isinstance(rows, list):
        raise ValueError("Python distribution metadata is not a list")
    normalized: list[dict[str, str]] = []
    for row in rows:
        if not isinstance(row, dict):
            raise ValueError(f"Python distribution metadata row is malformed: {row}")
        name = str(row.get("name", "")).strip()
        version = str(row.get("version", "")).strip()
        license_name = str(row.get("license", "")).strip()
        source = str(row.get("source", "")).strip()
        if not name or not version:
            raise ValueError(f"Python distribution has incomplete metadata: {row}")
        if not license_name or license_name.upper() in {"UNKNOWN", "NOASSERTION"}:
            raise ValueError(f"Python distribution {name} {version} has no license metadata")
        if not source or source.upper() in {"UNKNOWN", "NOASSERTION", "NONE", "NULL"}:
            raise ValueError(f"Python distribution {name} {version} has no source metadata")
        normalized.append(
            {
                "ecosystem": "python",
                "name": name,
                "normalized_name": normalize_package_name(name),
                "version": version,
                "license": license_name,
                "source": source,
            }
        )
    return sorted(
        normalized,
        key=lambda row: (row["normalized_name"], row["version"], row["source"]),
    )


def python_dependency_metadata(runtime: Path) -> list[dict[str, str]]:
    python = runtime / "python.exe"
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
            "x86_64-pc-windows-msvc",
            "--manifest-path",
            str(root / "Cargo.toml"),
        ],
        cwd=root,
        check=True,
        capture_output=True,
        text=True,
        timeout=120,
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
        "target": "windows-x86_64",
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
            "name": "Reyn-Studio-Windows-dependency-closure",
            "documentNamespace": (
                "https://reynflow.com/sbom/windows/"
                + hashlib.sha256(
                    json.dumps(closure, sort_keys=True, separators=(",", ":")).encode(
                        "utf-8"
                    )
                ).hexdigest()
            ),
            "creationInfo": {
                "created": "1980-01-01T00:00:00Z",
                "creators": ["Tool: Reyn deterministic Windows packager"],
            },
            "packages": spdx_packages,
        },
    )
    notice_lines = [
        "# Reyn Studio Windows third-party notices",
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
        "platform": "windows",
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
    *,
    reparse_checker: object = is_windows_reparse_point,
) -> list[str]:
    errors: list[str] = []
    try:
        staged_files = {
            path.relative_to(stage.absolute()).as_posix()
            for path in safe_files(stage, reparse_checker=reparse_checker)
        }
    except (OSError, ValueError) as error:
        return [f"unsafe package tree: {error}"]
    required = (
        "Reyn Studio.exe",
        "ReynStudio.ico",
        "LICENSE",
        "NOTICE",
        "ReynPython/python.exe",
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
    if errors:
        return errors

    manifest = json.loads((stage / "release-manifest.json").read_text(encoding="utf-8"))
    if manifest.get("platform") != "windows" or manifest.get("architecture") != "x86_64":
        errors.append("release manifest must declare windows/x86_64")
    if manifest.get("cuda_supported") is not False:
        errors.append("release manifest must state cuda_supported=false")
    if manifest.get("windows_verified") is not False:
        errors.append("local packaging must not claim Windows verification")
    expected_access = {
        "schema": "com.reyn.studio.preview-access/1",
        "required": True,
        "endpoint": "https://reynflow.com/api/yc-access/v1/session",
        "terms_version": "1.0",
        "privacy_version": "1.0",
    }
    if manifest.get("preview_access") != expected_access:
        errors.append("release manifest must record the exact YC preview access contract")
    expected_loader_probe = {
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
    recorded_loader_probe = manifest.get("model_loader_probe")
    if not isinstance(recorded_loader_probe, dict):
        errors.append("release manifest must record the staged model-loader probe")
    else:
        for key, value in expected_loader_probe.items():
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
        if bundled_model.get("bundle_sha256") != expected_loader_probe[
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
        reparse_checker=reparse_checker,
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


def deterministic_zip(
    stage: Path,
    destination: Path,
    source_date_epoch: int,
    *,
    reparse_checker: object = is_windows_reparse_point,
) -> None:
    import datetime

    files = safe_files(stage, reparse_checker=reparse_checker)
    timestamp = datetime.datetime.fromtimestamp(
        max(source_date_epoch, DEFAULT_SOURCE_DATE_EPOCH),
        tz=datetime.timezone.utc,
    )
    date_time = (
        timestamp.year,
        timestamp.month,
        timestamp.day,
        timestamp.hour,
        timestamp.minute,
        timestamp.second,
    )
    destination.parent.mkdir(parents=True, exist_ok=True)
    assert_safe_output_file(destination)
    with zipfile.ZipFile(destination, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
        for path in files:
            relative = (Path(stage.name) / path.relative_to(stage)).as_posix()
            info = zipfile.ZipInfo(relative, date_time=date_time)
            info.compress_type = zipfile.ZIP_DEFLATED
            info.create_system = 0
            info.external_attr = 0o100644 << 16
            archive.writestr(info, path.read_bytes())


def write_sha256sums(archives: list[Path], destination: Path) -> None:
    assert_safe_output_file(destination)
    lines = [f"{sha256_file(path)}  {path.name}" for path in sorted(archives)]
    destination.write_text("\n".join(lines) + "\n", encoding="utf-8")


def authenticode_sign(
    executable: Path,
    pfx_path: Path | None,
    password_env: str,
    timestamp_url: str,
) -> None:
    if pfx_path is None:
        return
    password = os.environ.get(password_env)
    if not password:
        raise RuntimeError(
            f"Authenticode signing requested but {password_env} is not set"
        )
    signtool = shutil.which("signtool")
    if signtool is None:
        raise RuntimeError("Authenticode signing requested but signtool is unavailable")
    subprocess.run(
        [
            signtool,
            "sign",
            "/fd",
            "SHA256",
            "/td",
            "SHA256",
            "/tr",
            timestamp_url,
            "/f",
            str(pfx_path),
            "/p",
            password,
            str(executable),
        ],
        check=True,
    )
    subprocess.run(
        [signtool, "verify", "/pa", "/all", str(executable)],
        check=True,
    )
