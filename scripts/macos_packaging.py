#!/usr/bin/env python3
"""Shared, dependency-free helpers for Reyn Studio's macOS packaging."""

from __future__ import annotations

import ast
import base64
import binascii
import hashlib
import json
import os
import platform
import plistlib
import re
import shutil
import stat
import subprocess
import tempfile
import tomllib
import zipfile
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Iterable


ENGINE_RESOURCES = (
    "model_bundle.py",
    "n5_inspector.py",
    "n5_overlap.py",
    "pinned_model_trust.py",
    "reyn_engine.py",
)
DOCUMENTATION_RESOURCES = ("PRD.md",)
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
    "physics_losses.py",
    "pressure_channel_contract_3d.py",
    "pressure_model_contract_3d.py",
    "spectral_solver.py",
    "spectral_solver_3d.py",
    "time_moe_operator.py",
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
SECURITY_RESOURCES = (
    "MODEL_TRUST_CONTRACT.json",
    "SBOM.spdx.json",
    "THIRD_PARTY_NOTICES.md",
)
RUNTIME_LOCK_PATH = (
    Path(__file__).resolve().parents[1] / "packaging/macos/python-runtime.lock.json"
)
_RUNTIME_LOCK = json.loads(RUNTIME_LOCK_PATH.read_text(encoding="utf-8"))
RUNTIME_DISTRIBUTIONS = tuple(_RUNTIME_LOCK["distributions"])
RUNTIME_DEPENDENCIES = {
    distribution["name"]: (distribution["version"], distribution["license"])
    for distribution in RUNTIME_DISTRIBUTIONS
}
LOCK_PACKAGE_NAMES = {
    "cryptography": "cryptography",
    "numpy": "NumPy",
    "safetensors": "safetensors",
    "securesystemslib": "securesystemslib",
    "torch": "PyTorch",
    "tuf": "python-tuf",
}
PROJECT_EXTENSIONS = ("reyn", "reynproj")
TEMPLATE_EXTENSIONS = ("reyntemplate",)
DEFAULT_SOURCE_DATE_EPOCH = 315532800  # 1980-01-01, the ZIP format floor.
ARCHITECTURE_ORDER = ("arm64", "x86_64")
RUST_TARGET_ARCHITECTURES = {
    "aarch64-apple-darwin": "arm64",
    "x86_64-apple-darwin": "x86_64",
}
PACKAGE_RUST_TARGETS = {
    "aarch64-apple-darwin": ("aarch64-apple-darwin",),
    "x86_64-apple-darwin": ("x86_64-apple-darwin",),
    "universal2": ("aarch64-apple-darwin", "x86_64-apple-darwin"),
}
TARGET_ARCHITECTURES = {
    target: tuple(RUST_TARGET_ARCHITECTURES[item] for item in rust_targets)
    for target, rust_targets in PACKAGE_RUST_TARGETS.items()
}
DEVELOPER_PATH_BYTE_PATTERNS = (
    re.compile(rb"file://(?:/|\./)[\x20-\x7e]{0,1024}?PRD\.md"),
    re.compile(rb"/Users/[^/\x00\s\"']+/"),
    re.compile(rb"/home/[^/\x00\s\"']+/"),
    re.compile(rb"[A-Za-z]:\\Users\\[^\\\x00\s\"']+\\"),
    re.compile(rb"/(?:private/)?var/folders/[^/\x00\s\"']+/"),
    re.compile(rb"/(?:private/)?tmp/[^/\x00\s\"']+/"),
    re.compile(rb"/(?:workspace|workspaces|build|builds)/"),
    re.compile(rb"/Volumes/[^/\x00\s\"']+/"),
    re.compile(rb"(?:^|/)\.cargo/(?:registry|git)/"),
    re.compile(rb"/(?:opt/)?cargo/(?:registry|git)/"),
    re.compile(rb"/(?:opt/)?rustup/toolchains/"),
)
TUF_METADATA_NAME = re.compile(
    r"(?:[1-9][0-9]*\.)?(?:root|snapshot|targets|models)\.json|timestamp\.json"
)
FORBIDDEN_SECRET_BYTE_PATTERNS = (
    re.compile(
        rb"-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----",
        re.IGNORECASE,
    ),
    re.compile(rb"""["'](?:private_key|secret_key|signing_key)["']\s*:""", re.IGNORECASE),
)
FORBIDDEN_MODEL_SUFFIXES = (".pth", ".pt", ".ckpt")


@dataclass(frozen=True)
class ReleaseConfig:
    root: Path
    package_name: str
    version: str
    executable: str
    bundle_identifier: str
    display_name: str
    category: str
    minimum_system_version: str


@dataclass(frozen=True)
class Check:
    level: str
    name: str
    detail: str


def run(
    command: list[str],
    *,
    cwd: Path,
    env: dict[str, str] | None = None,
    capture: bool = False,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        command,
        cwd=cwd,
        env=env,
        check=True,
        text=True,
        stdout=subprocess.PIPE if capture else None,
        stderr=subprocess.STDOUT if capture else None,
    )


def load_config(root: Path) -> ReleaseConfig:
    root = root.resolve()
    manifest = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
    package = manifest["package"]
    metadata = package.get("metadata", {}).get("reyn-macos", {})
    required = (
        "bundle-identifier",
        "display-name",
        "category",
        "minimum-system-version",
    )
    missing = [key for key in required if not metadata.get(key)]
    if missing:
        raise ValueError(f"missing [package.metadata.reyn-macos] keys: {', '.join(missing)}")
    return ReleaseConfig(
        root=root,
        package_name=str(package["name"]),
        version=str(package["version"]),
        executable=str(package["name"]),
        bundle_identifier=str(metadata["bundle-identifier"]),
        display_name=str(metadata["display-name"]),
        category=str(metadata["category"]),
        minimum_system_version=str(metadata["minimum-system-version"]),
    )


def validate_build_number(build_number: str) -> None:
    if not re.fullmatch(r"[0-9]+(?:\.[0-9]+){0,2}", build_number):
        raise ValueError("build number must contain one to three dot-separated integers")


def rustc_host_target(root: Path) -> str:
    result = run(["rustc", "-vV"], cwd=root, capture=True)
    for line in result.stdout.splitlines():
        if line.startswith("host: "):
            return line.removeprefix("host: ").strip()
    raise RuntimeError("rustc did not report a host target")


def installed_rust_targets(root: Path) -> tuple[str, ...]:
    result = run(["rustc", "--print", "sysroot"], cwd=root, capture=True)
    rustlib = Path(result.stdout.strip()) / "lib/rustlib"
    if not rustlib.is_dir():
        return ()
    return tuple(
        sorted(
            path.name
            for path in rustlib.iterdir()
            if path.is_dir() and (path / "lib").is_dir()
        )
    )


def require_packaging_toolchain(root: Path, target: str) -> None:
    if target not in PACKAGE_RUST_TARGETS:
        raise ValueError(f"unsupported macOS target: {target}")
    missing_tools = [
        name for name in ("cargo", "rustc", "lipo", "otool", "xcrun")
        if shutil.which(name) is None
    ]
    if missing_tools:
        raise RuntimeError(
            "required macOS packaging tools are unavailable: "
            f"{', '.join(missing_tools)}; install Rust and Xcode Command Line Tools"
        )
    try:
        sdk = run(["xcrun", "--sdk", "macosx", "--show-sdk-path"], cwd=root, capture=True)
    except subprocess.CalledProcessError as error:
        raise RuntimeError(
            "the macOS SDK is unavailable; run `xcode-select --install` or select a "
            "complete Xcode developer directory"
        ) from error
    if not sdk.stdout.strip():
        raise RuntimeError(
            "xcrun returned no macOS SDK path; select a complete Xcode developer directory"
        )
    installed = set(installed_rust_targets(root))
    missing_targets = [
        item for item in PACKAGE_RUST_TARGETS[target] if item not in installed
    ]
    if missing_targets:
        command = f"rustup target add {' '.join(missing_targets)}"
        raise RuntimeError(
            "Rust standard libraries are missing for "
            f"{', '.join(missing_targets)}; install them with `{command}`"
        )


def rosetta_status() -> str:
    if platform.system() != "Darwin" or platform.machine() not in {"arm64", "aarch64"}:
        return "not-required"
    result = subprocess.run(
        ["/usr/bin/arch", "-x86_64", "/usr/bin/uname", "-m"],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
    )
    return (
        "available"
        if result.returncode == 0 and result.stdout.strip() == "x86_64"
        else "unavailable"
    )


def require_local_architecture_runtime(
    architectures: tuple[str, ...],
) -> None:
    if "x86_64" in architectures and rosetta_status() == "unavailable":
        raise RuntimeError(
            "x86_64 runtime verification was requested but Rosetta is unavailable; "
            "install it with `softwareupdate --install-rosetta`"
        )


def info_plist(config: ReleaseConfig, build_number: str) -> dict[str, object]:
    validate_build_number(build_number)
    return {
        "CFBundleDevelopmentRegion": "en",
        "CFBundleDisplayName": config.display_name,
        "CFBundleExecutable": config.executable,
        "CFBundleIconFile": "ReynStudio",
        "CFBundleIdentifier": config.bundle_identifier,
        "CFBundleInfoDictionaryVersion": "6.0",
        "CFBundleName": config.display_name,
        "CFBundlePackageType": "APPL",
        "CFBundleShortVersionString": config.version,
        "CFBundleSupportedPlatforms": ["MacOSX"],
        "CFBundleVersion": build_number,
        "LSApplicationCategoryType": config.category,
        "LSMinimumSystemVersion": config.minimum_system_version,
        "NSHighResolutionCapable": True,
        "UTExportedTypeDeclarations": [
            {
                "UTTypeConformsTo": ["public.data", "public.content"],
                "UTTypeDescription": "Reyn Studio Project",
                "UTTypeIdentifier": f"{config.bundle_identifier}.project",
                "UTTypeTagSpecification": {
                    "public.filename-extension": list(PROJECT_EXTENSIONS),
                    "public.mime-type": "application/vnd.reyn.project+json",
                },
            },
            {
                "UTTypeConformsTo": ["public.data", "public.content"],
                "UTTypeDescription": "Reyn Studio Case Template",
                "UTTypeIdentifier": f"{config.bundle_identifier}.case-template",
                "UTTypeTagSpecification": {
                    "public.filename-extension": list(TEMPLATE_EXTENSIONS),
                    "public.mime-type": "application/vnd.reyn.case-template+json",
                },
            },
        ],
    }


def runtime_requirements() -> dict[str, object]:
    return {
        "schema": "com.reyn.studio.runtime-requirements.v3",
        "engine": {
            "bundled_modules": list(ENGINE_RESOURCES),
            "entrypoint": "engine/reyn_engine.py",
            "used_by_current_binary": True,
            "resolution": [
                "REYN_ENGINE_SCRIPT",
                "<REYN_RESOURCES_DIR>/engine/reyn_engine.py",
                "<current_exe_dir>/../Resources/engine/reyn_engine.py",
                "development ancestors/engine/reyn_engine.py (non-bundle only)",
            ],
        },
        "python": {
            "bundled": True,
            "architecture": "arm64",
            "minimum_macos": "14.0",
            "compute_supported_on": ["arm64"],
            "compute_unsupported_on": ["x86_64"],
            "resolution": [
                "REYN_PYTHON",
                "<current_exe_dir>/../Resources/ReynPython/bin/python3.14",
                "<managed-runtime-slot>/ReynPython/bin/python3.14",
                "Developer-mode custom Python only when explicitly configured",
            ],
            "required_imports": [
                "cryptography",
                "numpy",
                "safetensors",
                "securesystemslib",
                "torch",
                "tuf",
            ],
            "required_distributions": {
                name: version
                for name, (version, _license) in RUNTIME_DEPENDENCIES.items()
            },
        },
        "research_runtime": {
            "bundled": True,
            "bundled_modules": list(RESEARCH_RESOURCES),
            "entrypoint": "research",
            "environment": "REYN_RESEARCH_DIR",
            "external_checkout_required": False,
            "writable": False,
        },
        "checkpoints": {
            "bundled": True,
            "locations": [
                "<app-resources>/research/*.reynmodel",
                "<REYN_RESEARCH_DIR>/*.reynmodel",
                "<managed-model-dir>/*.reynmodel",
            ],
            "pickle_formats_permitted": False,
        },
        "model_trust": {
            "contract": "security/MODEL_TRUST_CONTRACT.json",
            "detached_signature_required": True,
            "offline_tuf_metadata_required": True,
            "production_root_pinned": True,
            "model_assets_bundled": True,
            "failure_mode": "fail-closed",
        },
        "supply_chain": {
            "sbom": "security/SBOM.spdx.json",
            "third_party_notices": "security/THIRD_PARTY_NOTICES.md",
        },
        "documentation": {
            "bundled": True,
            "entrypoint": "docs/PRD.md",
            "network_required": False,
        },
        "apple_distribution": {
            "developer_id_signing_performed": False,
            "notarization_performed": False,
        },
    }


def standalone_blockers(
    root: Path,
    *,
    developer_id_signed: bool = False,
    notarized: bool = False,
) -> list[str]:
    engine = (root / "src/engine.rs").read_text(encoding="utf-8")
    main = (root / "src/main.rs").read_text(encoding="utf-8")
    blockers: list[str] = []
    if 'concat!(env!("CARGO_MANIFEST_DIR"), "/engine/reyn_engine.py")' in engine:
        blockers.append(
            "Python sidecar lookup embeds the build machine's CARGO_MANIFEST_DIR "
            "instead of resolving Contents/Resources/engine."
        )
    if '"/Users/' in engine:
        blockers.append(
            "The default research checkout fallback is a developer-specific absolute path."
        )
    if not developer_id_signed:
        blockers.append(
            "Developer ID signing and Apple notarization are not performed by this workflow."
        )
    elif not notarized:
        blockers.append(
            "Developer ID signing succeeded, but Apple notarization was not performed."
        )
    if "CFBundleDocumentTypes" not in main and "OpenUrls" not in main and "Opened" not in main:
        blockers.append(
            "Startup does not consume Finder/LaunchServices document-open events; "
            "document associations are intentionally not claimed."
        )
    return blockers


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def file_manifest(base: Path, paths: Iterable[Path]) -> list[dict[str, object]]:
    rows = []
    for path in sorted(paths, key=lambda item: item.relative_to(base).as_posix()):
        rows.append(
            {
                "path": path.relative_to(base).as_posix(),
                "bytes": path.stat().st_size,
                "sha256": sha256_file(path),
            }
        )
    return rows


def canonical_json_bytes(value: object) -> bytes:
    return json.dumps(
        value, ensure_ascii=False, separators=(",", ":"), sort_keys=True
    ).encode("utf-8")


def runtime_sbom_documents(app_version: str = "0.1.1") -> tuple[dict, dict]:
    components = []
    packages = [
        {
            "SPDXID": "SPDXRef-Package-ReynStudio",
            "comment": "Packaged application, lightweight Python source closure, and exact arm64 factory compute runtime.",
            "downloadLocation": "NOASSERTION",
            "filesAnalyzed": False,
            "licenseConcluded": "NOASSERTION",
            "licenseDeclared": "NOASSERTION",
            "name": "Reyn Studio",
            "versionInfo": app_version,
        }
    ]
    relationships = []
    for distribution in RUNTIME_DISTRIBUTIONS:
        name = distribution["name"]
        version = distribution["version"]
        license_id = distribution["license"]
        component = {
            "type": "framework" if name == "Python" else "library",
            "name": name,
            "version": version,
            "purl": distribution["purl"],
            "licenses": [
                {"expression": license_id}
                if " OR " in license_id
                else {"license": {"id": license_id}}
            ],
        }
        components.append(component)
        spdx_id = "SPDXRef-Package-" + re.sub(r"[^A-Za-z0-9.-]", "-", name)
        packages.append(
            {
                "SPDXID": spdx_id,
                "comment": "Bundled arm64 factory-runtime distribution.",
                "downloadLocation": (
                    "https://www.python.org/downloads/"
                    if name == "Python"
                    else f"https://pypi.org/project/{distribution['purl'].split('/')[1].split('@')[0]}/{version}/"
                ),
                "filesAnalyzed": False,
                "licenseConcluded": license_id,
                "licenseDeclared": license_id,
                "name": name,
                "versionInfo": version,
            }
        )
        relationships.append(
            {
                "relatedSpdxElement": spdx_id,
                "relationshipType": "DEPENDS_ON",
                "spdxElementId": "SPDXRef-Package-ReynStudio",
            }
        )
    cyclonedx = {
        "bomFormat": "CycloneDX",
        "specVersion": "1.6",
        "serialNumber": "urn:uuid:48f1f77f-012f-4b20-a928-fb861be3f011",
        "version": 1,
        "metadata": {
            "component": {
                "type": "application",
                "name": "Reyn Studio arm64 factory runtime",
                "version": app_version,
            }
        },
        "components": components,
    }
    spdx = {
        "SPDXID": "SPDXRef-DOCUMENT",
        "creationInfo": {
            "created": "1980-01-01T00:00:00Z",
            "creators": ["Organization: Reyn Studio"],
        },
        "dataLicense": "CC0-1.0",
        "documentNamespace": f"https://reyn.studio/spdx/reyn-studio-{app_version}-macos-python-runtime-v3",
        "name": f"Reyn Studio {app_version} macOS arm64 Python runtime inventory",
        "packages": packages,
        "relationships": relationships,
        "spdxVersion": "SPDX-2.3",
    }
    return spdx, cyclonedx


def validate_runtime_sboms(root: Path) -> list[str]:
    expected_spdx, expected_cyclonedx = runtime_sbom_documents()
    errors = []
    for path, expected, label in (
        (root / "packaging/macos/SBOM.spdx.json", expected_spdx, "SPDX SBOM"),
        (
            root / "packaging/macos/runtime-sbom.cdx.json",
            expected_cyclonedx,
            "CycloneDX SBOM",
        ),
    ):
        try:
            observed = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
            errors.append(f"{label} is unreadable: {error}")
            continue
        if observed != expected:
            errors.append(f"{label} does not match python-runtime.lock.json")
    return errors


def research_closure_sha256(resources: Path) -> str:
    closure_paths = [
        *(resources / "engine" / name for name in ENGINE_RESOURCES),
        *(resources / "research" / name for name in RESEARCH_RESOURCES),
    ]
    rows = [
        {
            "path": path.relative_to(resources).as_posix(),
            "size": path.stat().st_size,
            "sha256": sha256_file(path),
        }
        for path in closure_paths
    ]
    rows.sort(key=lambda row: row["path"])
    return hashlib.sha256(canonical_json_bytes(rows)).hexdigest()


def validate_factory_runtime(runtime_root: Path) -> list[str]:
    errors = []
    interpreter = runtime_root / "bin/python3.14"
    if not interpreter.is_file():
        return [f"factory runtime interpreter is missing: {interpreter}"]
    if not os.access(interpreter, os.X_OK):
        errors.append(f"factory runtime interpreter is not executable: {interpreter}")
    try:
        architectures = executable_architectures(interpreter)
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        errors.append(f"cannot inspect factory runtime architecture: {error}")
    else:
        if architectures != ("arm64",):
            errors.append(
                f"factory runtime interpreter architectures are {architectures}, "
                "expected arm64 only"
            )
    for path in runtime_root.rglob("*"):
        if not path.is_symlink():
            continue
        try:
            resolved = path.resolve(strict=True)
            resolved.relative_to(runtime_root.resolve())
        except (OSError, ValueError) as error:
            errors.append(f"factory runtime symlink escapes its prefix: {path}: {error}")
    metadata_names = {
        name: {
            "PyTorch": "torch",
            "NumPy": "numpy",
            "python-tuf": "tuf",
        }.get(name, name)
        for name in RUNTIME_DEPENDENCIES
        if name != "Python"
    }
    probe = (
        "import importlib.metadata as m,json,platform,sys;"
        f"names={metadata_names!r};"
        "print(json.dumps({'architecture':platform.machine(),"
        "'Python':platform.python_version(),"
        "'distributions':{name:m.version(metadata) for name,metadata in names.items()},"
        "'prefix':sys.prefix},sort_keys=True))"
    )
    try:
        completed = run(
            [str(interpreter), "-I", "-s", "-c", probe],
            cwd=runtime_root,
            capture=True,
        )
        observed = json.loads(completed.stdout.strip())
    except (
        OSError,
        subprocess.CalledProcessError,
        UnicodeDecodeError,
        json.JSONDecodeError,
    ) as error:
        errors.append(f"factory runtime dependency probe failed: {error}")
    else:
        expected = {
            "architecture": "arm64",
            "Python": RUNTIME_DEPENDENCIES["Python"][0],
        }
        for name, version in expected.items():
            if observed.get(name) != version:
                errors.append(
                    f"factory runtime {name}={observed.get(name)!r}, expected {version!r}"
                )
        observed_distributions = observed.get("distributions", {})
        for name, (version, _license) in RUNTIME_DEPENDENCIES.items():
            if name == "Python":
                continue
            if observed_distributions.get(name) != version:
                errors.append(
                    f"factory runtime {name}={observed_distributions.get(name)!r}, "
                    f"expected {version!r}"
                )
        try:
            Path(str(observed.get("prefix", ""))).resolve().relative_to(
                runtime_root.resolve()
            )
        except ValueError:
            errors.append("factory runtime reports a non-relocatable external sys.prefix")
    return errors


def stage_factory_runtime(
    source: Path,
    destination: Path,
    *,
    resources: Path,
    source_revision: str,
    build_epoch: int,
    compliance_root: Path,
) -> dict[str, object]:
    if not source.is_dir():
        raise FileNotFoundError(
            f"exact arm64 factory runtime is missing: {source}; "
            "pass --runtime-dir with a preassembled relocatable prefix"
        )
    if destination.exists():
        shutil.rmtree(destination)
    shutil.copytree(source, destination, symlinks=True)
    for stale in ("runtime-manifest.cjson", "runtime-manifest.sig"):
        (destination / stale).unlink(missing_ok=True)
    sbom_errors = validate_runtime_sboms(compliance_root.parents[1])
    if sbom_errors:
        raise RuntimeError("runtime SBOM validation failed: " + "; ".join(sbom_errors))
    _, runtime_cyclonedx = runtime_sbom_documents()
    (destination / "runtime-sbom.cdx.json").write_text(
        json.dumps(runtime_cyclonedx, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    shutil.copy2(
        compliance_root / "RUNTIME_THIRD_PARTY_NOTICES.html",
        destination / "THIRD_PARTY_NOTICES.html",
    )
    errors = validate_factory_runtime(destination)
    if errors:
        raise RuntimeError("factory runtime validation failed: " + "; ".join(errors))
    payload_paths = [
        path
        for path in destination.rglob("*")
        if path.is_file() and path.name != "runtime-manifest.cjson"
    ]
    files = [
        {
            "path": path.relative_to(destination).as_posix(),
            "size": path.stat().st_size,
            "sha256": sha256_file(path),
        }
        for path in payload_paths
    ]
    files.sort(key=lambda row: row["path"])
    by_path = {row["path"]: row for row in files}
    manifest = {
        "schema": "com.reyn.runtime-manifest/1",
        "runtime_id": "",
        "platform": "macos",
        "architecture": "arm64",
        "minimum_macos": "14.0",
        "python": RUNTIME_DEPENDENCIES["Python"][0],
        "torch": RUNTIME_DEPENDENCIES["PyTorch"][0],
        "numpy": RUNTIME_DEPENDENCIES["NumPy"][0],
        "engine_protocol": 1,
        "research_closure_sha256": research_closure_sha256(resources),
        "source_revision": source_revision,
        "build_epoch": build_epoch,
        "files": files,
        "sbom_sha256": by_path["runtime-sbom.cdx.json"]["sha256"],
        "notices_sha256": by_path["THIRD_PARTY_NOTICES.html"]["sha256"],
    }
    identity = dict(manifest)
    identity.pop("runtime_id")
    manifest["runtime_id"] = "sha256:" + hashlib.sha256(
        canonical_json_bytes(identity)
    ).hexdigest()
    (destination / "runtime-manifest.cjson").write_bytes(
        canonical_json_bytes(manifest)
    )
    return manifest


def validate_factory_runtime_manifest(runtime_root: Path) -> list[str]:
    manifest_path = runtime_root / "runtime-manifest.cjson"
    try:
        encoded = manifest_path.read_bytes()
        manifest = json.loads(encoded)
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        return [f"factory runtime manifest is unreadable: {error}"]
    errors = []
    if encoded != canonical_json_bytes(manifest):
        errors.append("factory runtime manifest is not canonical JSON")
    identity = dict(manifest)
    declared_id = identity.pop("runtime_id", None)
    calculated_id = "sha256:" + hashlib.sha256(
        canonical_json_bytes(identity)
    ).hexdigest()
    if declared_id != calculated_id:
        errors.append(
            f"factory runtime identity is {declared_id!r}, expected {calculated_id!r}"
        )
    declared_files = manifest.get("files")
    if not isinstance(declared_files, list):
        return [*errors, "factory runtime manifest files must be a list"]
    actual_paths = {
        path.relative_to(runtime_root).as_posix()
        for path in runtime_root.rglob("*")
        if path.is_file() and path.name != "runtime-manifest.cjson"
    }
    declared_paths = {
        row.get("path") for row in declared_files if isinstance(row, dict)
    }
    if declared_paths != actual_paths:
        errors.append("factory runtime manifest file inventory differs from staged prefix")
    for row in declared_files:
        if not isinstance(row, dict) or not isinstance(row.get("path"), str):
            errors.append("factory runtime manifest contains a malformed file row")
            continue
        path = runtime_root / row["path"]
        if not path.is_file():
            continue
        if row.get("size") != path.stat().st_size or row.get("sha256") != sha256_file(
            path
        ):
            errors.append(f"factory runtime file digest differs: {row['path']}")
    errors.extend(validate_factory_runtime(runtime_root))
    return errors


def manifest_digest(rows: Iterable[dict[str, object]]) -> str:
    encoded = json.dumps(
        list(rows), ensure_ascii=False, separators=(",", ":"), sort_keys=True
    ).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def resource_metadata(resources: Path) -> dict[str, object]:
    paths = [
        path
        for path in resources.rglob("*")
        if path.is_file() and path.name != "release-manifest.json"
    ]
    rows = file_manifest(resources, paths)
    return {
        "schema": "com.reyn.studio.resource-set.v1",
        "sha256": manifest_digest(rows),
        "files": rows,
    }


def validate_resource_metadata(
    resources: Path, metadata: object
) -> list[str]:
    if not isinstance(metadata, dict):
        return ["release manifest resource_set must be an object"]
    actual = resource_metadata(resources)
    errors = []
    if metadata.get("schema") != actual["schema"]:
        errors.append(
            f"resource_set.schema={metadata.get('schema')!r}, "
            f"expected {actual['schema']!r}"
        )
    if metadata.get("files") != actual["files"]:
        errors.append("release manifest resource_set.files differs from staged resources")
    if metadata.get("sha256") != actual["sha256"]:
        errors.append("release manifest resource_set.sha256 differs from staged resources")
    return errors


def resolve_research_source(
    root: Path, override: Path | None = None
) -> Path:
    configured = override
    if configured is None:
        environment = os.environ.get("REYN_RESEARCH_SOURCE_DIR", "").strip()
        if environment:
            configured = Path(environment)
        else:
            candidates = [
                root.parent / "reyn-research",
                *(
                    ancestor / "reyn-research"
                    for ancestor in list(root.parents)[:3]
                ),
            ]
            configured = next(
                (candidate for candidate in candidates if candidate.is_dir()),
                candidates[0],
            )
    if not configured.is_absolute():
        configured = root / configured
    configured = configured.resolve()
    if not configured.is_dir():
        raise FileNotFoundError(
            "research runtime source is missing: "
            f"{configured}; pass --research-source-dir or set REYN_RESEARCH_SOURCE_DIR"
        )
    return configured


def load_macos_release_pins(root: Path) -> dict[str, object]:
    path = root / "packaging/macos/release-pins.json"
    try:
        pins = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise RuntimeError(f"macOS release pins are unreadable: {error}") from error
    required = {
        "research_repository",
        "research_revision",
        "research_subdirectory",
        "runtime_architecture",
        "minimum_compute_macos",
        "python",
        "torch",
        "numpy",
    }
    missing = sorted(key for key in required if not pins.get(key))
    if missing:
        raise RuntimeError(f"macOS release pins omit: {', '.join(missing)}")
    revision = str(pins["research_revision"])
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise RuntimeError("macOS research revision must be an exact lowercase Git commit")
    expected_versions = {
        "python": RUNTIME_DEPENDENCIES["Python"][0],
        "torch": RUNTIME_DEPENDENCIES["PyTorch"][0],
        "numpy": RUNTIME_DEPENDENCIES["NumPy"][0],
    }
    for key, expected in expected_versions.items():
        if pins.get(key) != expected:
            raise RuntimeError(
                f"macOS release pin {key}={pins.get(key)!r}, expected {expected!r}"
            )
    if pins["runtime_architecture"] != "arm64":
        raise RuntimeError("the qualified macOS compute runtime must be arm64")
    return pins


def validate_research_source_pin(root: Path, research_source: Path) -> list[str]:
    pins = load_macos_release_pins(root)
    errors = [
        f"pinned research source omits {name}"
        for name in RESEARCH_RESOURCES
        if not (research_source / name).is_file()
    ]
    try:
        revision = run(
            ["git", "-C", str(research_source), "rev-parse", "HEAD"],
            cwd=root,
            capture=True,
        ).stdout.strip()
    except (OSError, subprocess.CalledProcessError) as error:
        errors.append(f"cannot verify pinned research Git revision: {error}")
    else:
        if revision != pins["research_revision"]:
            errors.append(
                f"research checkout is {revision}, expected {pins['research_revision']}"
            )
    return errors


def validate_runtime_dependency_lock(root: Path) -> list[str]:
    path = root / "packaging/macos/python-runtime.lock.json"
    try:
        lock = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        return [f"cannot read macOS Python runtime lock: {error}"]
    errors = []
    expected = [dict(distribution) for distribution in RUNTIME_DISTRIBUTIONS]
    if lock.get("distributions") != expected:
        errors.append("macOS Python runtime lock does not match the release inventory")
    for key, expected_value in (
        ("platform", "macos"),
        ("architecture", "arm64"),
        ("minimum_macos", "14.0"),
    ):
        if lock.get(key) != expected_value:
            errors.append(
                f"macOS Python runtime lock {key}={lock.get(key)!r}, "
                f"expected {expected_value!r}"
            )
    return errors


def validate_research_dependency_lock(research_source: Path) -> list[str]:
    lock_path = research_source / "uv.lock"
    project_path = research_source / "pyproject.toml"
    try:
        lock = tomllib.loads(lock_path.read_text(encoding="utf-8"))
        project = tomllib.loads(project_path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError) as error:
        return [f"cannot read research dependency metadata: {error}"]
    locked = {
        package.get("name"): package.get("version")
        for package in lock.get("package", [])
        if isinstance(package, dict)
    }
    errors = []
    for lock_name in ("numpy", "torch"):
        if not locked.get(lock_name):
            errors.append(f"research uv.lock omits {lock_name}")
    python_requirement = project.get("project", {}).get("requires-python")
    if not isinstance(python_requirement, str) or "3.14" not in python_requirement:
        errors.append(
            f"research pyproject requires-python={python_requirement!r}, expected Python 3.14"
        )
    return errors


def _json_file(path: Path, label: str) -> tuple[object | None, list[str]]:
    try:
        return json.loads(path.read_text(encoding="utf-8")), []
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        return None, [f"{label} is not valid UTF-8 JSON: {error}"]


def _contains_secret_field(value: object) -> bool:
    if isinstance(value, dict):
        for key, item in value.items():
            normalized = str(key).lower().replace("-", "_")
            if normalized in {"private", "private_key", "secret", "secret_key", "seed"}:
                return True
            if _contains_secret_field(item):
                return True
    elif isinstance(value, list):
        return any(_contains_secret_field(item) for item in value)
    return False


def _target_hash_errors(
    target: object, path: Path, relative: str, label: str
) -> list[str]:
    if not isinstance(target, dict):
        return [f"{label} target entry is missing"]
    errors = []
    hashes = target.get("hashes")
    expected_hash = hashes.get("sha256") if isinstance(hashes, dict) else None
    if expected_hash != sha256_file(path):
        errors.append(f"{label} target SHA-256 does not match {relative}")
    if target.get("length") != path.stat().st_size:
        errors.append(f"{label} target length does not match {relative}")
    return errors


def validate_model_assets(
    resources: Path, *, allow_model_assets: bool = False
) -> list[str]:
    """Validate adjacent bundle/signature/TUF layout without claiming TUF trust."""
    bundles = sorted(resources.rglob("*.reynmodel"))
    signatures = sorted(resources.rglob("*.reynmodel.sig"))
    repositories = sorted(
        path for path in resources.rglob("*.reynmodel.tuf") if path.is_dir()
    )
    bundle_names = {str(path) for path in bundles}
    expected_signatures = {f"{path}.sig" for path in bundles}
    expected_repositories = {f"{path}.tuf" for path in bundles}
    errors = [
        f"orphan detached signature: {path.relative_to(resources).as_posix()}"
        for path in signatures
        if str(path).removesuffix(".sig") not in bundle_names
    ]
    errors.extend(
        f"orphan TUF repository: {path.relative_to(resources).as_posix()}"
        for path in repositories
        if str(path).removesuffix(".tuf") not in bundle_names
    )
    if bundles and not allow_model_assets:
        errors.append(
            "model assets are forbidden while the production TUF root is intentionally unset"
        )

    for bundle in bundles:
        relative = bundle.relative_to(resources).as_posix()
        signature = Path(f"{bundle}.sig")
        repository = Path(f"{bundle}.tuf")
        if str(signature) not in expected_signatures or not signature.is_file():
            errors.append(f"{relative} is missing adjacent {bundle.name}.sig")
            continue
        if str(repository) not in expected_repositories or not repository.is_dir():
            errors.append(f"{relative} is missing adjacent {bundle.name}.tuf")
            continue

        document, document_errors = _json_file(
            signature, f"{relative}.sig"
        )
        errors.extend(document_errors)
        if not isinstance(document, dict):
            continue
        if _contains_secret_field(document):
            errors.append(f"{relative}.sig contains a forbidden secret field")
        payload = document.get("signed")
        if document.get("schema") != "com.reyn.inference-model-signature/1":
            errors.append(f"{relative}.sig has an incompatible document schema")
        if not isinstance(payload, dict):
            errors.append(f"{relative}.sig has no signed payload")
            continue
        if payload.get("schema") != "com.reyn.inference-model-signature-payload/1":
            errors.append(f"{relative}.sig has an incompatible payload schema")
        if payload.get("bundle_schema") != "com.reyn.inference-model-bundle/1":
            errors.append(f"{relative}.sig has an incompatible bundle schema")
        if payload.get("algorithm") != "ed25519":
            errors.append(f"{relative}.sig does not declare Ed25519")
        if payload.get("bundle_sha256") != sha256_file(bundle):
            errors.append(f"{relative}.sig bundle SHA-256 does not match bundle bytes")
        try:
            encoded_signature = document.get("signature")
            decoded_signature = base64.b64decode(encoded_signature, validate=True)
            if (
                not isinstance(encoded_signature, str)
                or len(decoded_signature) != 64
                or base64.b64encode(decoded_signature).decode("ascii")
                != encoded_signature
            ):
                raise ValueError("not canonical 64-byte base64")
        except (binascii.Error, TypeError, ValueError):
            errors.append(f"{relative}.sig has an invalid signature encoding")

        model = payload.get("model")
        if not isinstance(model, dict):
            errors.append(f"{relative}.sig has no model identity")
            continue
        model_id = model.get("id")
        model_version = model.get("version")
        if not all(
            isinstance(item, str) and re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}", item)
            for item in (model_id, model_version)
        ):
            errors.append(f"{relative}.sig has an invalid model identity")
            continue

        metadata = repository / "metadata"
        if not metadata.is_dir() or metadata.is_symlink():
            errors.append(f"{relative}.tuf must contain a real metadata directory")
            continue
        entries = sorted(metadata.iterdir())
        invalid_entries = [
            path.name
            for path in entries
            if path.is_symlink()
            or not path.is_file()
            or not TUF_METADATA_NAME.fullmatch(path.name)
        ]
        if invalid_entries:
            errors.append(
                f"{relative}.tuf has unsupported metadata entries: "
                f"{', '.join(invalid_entries)}"
            )
            continue
        names = {path.name for path in entries}
        for role in ("root", "targets", "models", "snapshot"):
            if not any(
                re.fullmatch(rf"[1-9][0-9]*\.{role}\.json", name)
                for name in names
            ):
                errors.append(f"{relative}.tuf is missing versioned {role} metadata")
        if "timestamp.json" not in names:
            errors.append(f"{relative}.tuf is missing timestamp.json")

        parsed_metadata: dict[str, object] = {}
        for entry in entries:
            value, metadata_errors = _json_file(
                entry, f"{relative}.tuf/metadata/{entry.name}"
            )
            errors.extend(metadata_errors)
            if value is not None:
                parsed_metadata[entry.name] = value
                if _contains_secret_field(value):
                    errors.append(
                        f"{relative}.tuf/metadata/{entry.name} contains a forbidden secret field"
                    )

        model_documents = [
            value
            for name, value in parsed_metadata.items()
            if re.fullmatch(r"[1-9][0-9]*\.models\.json", name)
        ]
        if len(model_documents) != 1 or not isinstance(model_documents[0], dict):
            continue
        signed = model_documents[0].get("signed")
        targets = signed.get("targets") if isinstance(signed, dict) else None
        if not isinstance(targets, dict):
            errors.append(f"{relative}.tuf models metadata has no targets object")
            continue
        target_path = f"models/{model_id}/{model_version}/{bundle.name}"
        signature_target_path = target_path + ".sig"
        if Path(target_path).is_absolute() or ".." in Path(target_path).parts:
            errors.append(f"{relative}.tuf contains a non-relative target path")
            continue
        bundle_target = targets.get(target_path)
        signature_target = targets.get(signature_target_path)
        errors.extend(
            _target_hash_errors(bundle_target, bundle, relative, "bundle")
        )
        errors.extend(
            _target_hash_errors(
                signature_target, signature, f"{relative}.sig", "signature"
            )
        )
        custom = (
            bundle_target.get("custom")
            if isinstance(bundle_target, dict)
            else None
        )
        detached = custom.get("detached_signature") if isinstance(custom, dict) else None
        if (
            not isinstance(custom, dict)
            or custom.get("schema") != "com.reyn.tuf-model-target/1"
            or custom.get("model") != model
            or custom.get("release_sequence") != payload.get("release_sequence")
            or not isinstance(detached, dict)
            or detached.get("target_path") != signature_target_path
            or detached.get("algorithm") != "ed25519"
            or detached.get("key_id") != payload.get("key_id")
        ):
            errors.append(
                f"{relative}.tuf target custom metadata is incompatible with its signature"
            )
    return errors


def forbidden_security_assets(resources: Path) -> list[str]:
    errors = []
    for path in sorted(resources.rglob("*")):
        relative = path.relative_to(resources).as_posix()
        if path.is_relative_to(resources / "ReynPython"):
            continue
        lowered_parts = tuple(part.lower() for part in path.parts)
        if any(
            token in part
            for part in lowered_parts
            for token in ("test-root", "test_root", "ephemeral-key", "fixture-key")
        ):
            errors.append(f"forbidden test/ephemeral security asset: {relative}")
        if path.is_symlink():
            errors.append(f"symbolic links are forbidden in resources: {relative}")
            continue
        if not path.is_file():
            continue
        lowered_name = path.name.lower()
        if path.suffix.lower() in (*FORBIDDEN_MODEL_SUFFIXES, ".pem", ".key", ".p12", ".pfx"):
            errors.append(f"forbidden model/key file: {relative}")
        try:
            data = path.read_bytes()
        except OSError:
            continue
        if any(pattern.search(data) for pattern in FORBIDDEN_SECRET_BYTE_PATTERNS):
            errors.append(f"forbidden private-key material: {relative}")
        if lowered_name in {"root.json", "test-root.json", "test_root.json"}:
            errors.append(f"unversioned or test TUF root is forbidden: {relative}")
    return errors


def validate_security_artifacts(resources: Path) -> list[str]:
    security = resources / "security"
    missing = [name for name in SECURITY_RESOURCES if not (security / name).is_file()]
    if missing:
        return [f"missing security artifact(s): {', '.join(missing)}"]

    errors: list[str] = []
    contract, contract_errors = _json_file(
        security / "MODEL_TRUST_CONTRACT.json", "model trust contract"
    )
    errors.extend(contract_errors)
    if isinstance(contract, dict):
        expected = {
            ("schema",): "com.reyn.studio.model-trust-contract.v1",
            ("production_root", "pinned"): True,
            ("production_root", "status"): "yc-0.1.1-preview",
            (
                "production_root",
                "sha256",
            ): "a713d759f4f3549dad76a20dc6342275f3f07c2fe455cf7352dce66ddd5381f1",
            ("model_assets_bundled",): True,
            ("detached_signature", "required_adjacent_suffix"): ".sig",
            ("tuf", "detached_repository_suffix"): ".tuf",
            ("tuf", "metadata_subdirectory"): "metadata",
            ("tuf", "transport"): "offline-local-only",
        }
        for keys, value in expected.items():
            current: object = contract
            for key in keys:
                current = current.get(key) if isinstance(current, dict) else None
            if current != value:
                errors.append(
                    f"model trust contract {'.'.join(keys)}={current!r}, expected {value!r}"
                )
        if _contains_secret_field(contract):
            errors.append("model trust contract contains a forbidden secret field")
    else:
        errors.append("model trust contract must be an object")

    model_bundle_path = resources / "engine/model_bundle.py"
    try:
        tree = ast.parse(model_bundle_path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, SyntaxError) as error:
        errors.append(f"cannot inspect staged model_bundle.py trust constants: {error}")
    else:
        if "from pinned_model_trust import PINNED_TUF_ROOT_JSON" not in (
            model_bundle_path.read_text(encoding="utf-8")
        ):
            errors.append(
                "staged model_bundle.py does not import the pinned preview trust root"
            )
        names = {
            "BUNDLE_SCHEMA",
            "SIGNATURE_DOCUMENT_SCHEMA",
            "SIGNATURE_PAYLOAD_SCHEMA",
            "SIGNATURE_SUFFIX",
            "TUF_TARGET_CUSTOM_SCHEMA",
            "TUF_REPOSITORY_SUFFIX",
            "TUF_MIN_ROOT_THRESHOLD",
            "TUF_MIN_TARGETS_THRESHOLD",
        }
        constants: dict[str, object] = {}
        for node in tree.body:
            target = None
            value = None
            if (
                isinstance(node, ast.Assign)
                and len(node.targets) == 1
                and isinstance(node.targets[0], ast.Name)
            ):
                target, value = node.targets[0].id, node.value
            elif isinstance(node, ast.AnnAssign) and isinstance(node.target, ast.Name):
                target, value = node.target.id, node.value
            if target in names and value is not None:
                try:
                    constants[target] = ast.literal_eval(value)
                except (ValueError, TypeError):
                    constants[target] = object()
        expected_constants = {
            "BUNDLE_SCHEMA": "com.reyn.inference-model-bundle/1",
            "SIGNATURE_DOCUMENT_SCHEMA": "com.reyn.inference-model-signature/1",
            "SIGNATURE_PAYLOAD_SCHEMA": "com.reyn.inference-model-signature-payload/1",
            "SIGNATURE_SUFFIX": ".sig",
            "TUF_TARGET_CUSTOM_SCHEMA": "com.reyn.tuf-model-target/1",
            "TUF_REPOSITORY_SUFFIX": ".tuf",
            "TUF_MIN_ROOT_THRESHOLD": 2,
            "TUF_MIN_TARGETS_THRESHOLD": 2,
        }
        for name, value in expected_constants.items():
            if constants.get(name, object()) != value:
                errors.append(
                    f"staged model_bundle.py {name} does not match the fail-closed trust contract"
                )

    pinned_root_path = resources / "engine/pinned_model_trust.py"
    try:
        pinned_tree = ast.parse(pinned_root_path.read_text(encoding="utf-8"))
        pinned_constants = {
            node.targets[0].id: ast.literal_eval(node.value)
            for node in pinned_tree.body
            if isinstance(node, ast.Assign)
            and len(node.targets) == 1
            and isinstance(node.targets[0], ast.Name)
            and node.targets[0].id
            in {"PINNED_TUF_ROOT_JSON", "PINNED_TUF_ROOT_SHA256"}
        }
        pinned_bytes = pinned_constants.get("PINNED_TUF_ROOT_JSON")
        pinned_sha256 = pinned_constants.get("PINNED_TUF_ROOT_SHA256")
        if not isinstance(pinned_bytes, bytes):
            errors.append("pinned preview TUF root must be literal public metadata bytes")
        elif hashlib.sha256(pinned_bytes).hexdigest() != pinned_sha256:
            errors.append("pinned preview TUF root SHA-256 does not match its bytes")
        if pinned_sha256 != (
            "a713d759f4f3549dad76a20dc6342275f3f07c2fe455cf7352dce66ddd5381f1"
        ):
            errors.append("pinned preview TUF root SHA-256 is not the reviewed release root")
    except (OSError, UnicodeDecodeError, SyntaxError, ValueError, TypeError) as error:
        errors.append(f"cannot inspect staged pinned preview TUF root: {error}")

    sbom, sbom_errors = _json_file(security / "SBOM.spdx.json", "SBOM")
    errors.extend(sbom_errors)
    if isinstance(sbom, dict):
        packages = sbom.get("packages")
        indexed = {
            package.get("name"): package
            for package in packages
            if isinstance(package, dict) and isinstance(package.get("name"), str)
        } if isinstance(packages, list) else {}
        if sbom.get("spdxVersion") != "SPDX-2.3":
            errors.append("SBOM must use SPDX-2.3")
        for name, (version, license_id) in RUNTIME_DEPENDENCIES.items():
            package = indexed.get(name)
            if not isinstance(package, dict):
                errors.append(f"SBOM omits {name}")
                continue
            if package.get("versionInfo") != version:
                errors.append(
                    f"SBOM {name} version={package.get('versionInfo')!r}, expected {version!r}"
                )
            if package.get("licenseDeclared") != license_id:
                errors.append(
                    f"SBOM {name} license={package.get('licenseDeclared')!r}, "
                    f"expected {license_id!r}"
                )
            if "Bundled arm64" not in str(package.get("comment", "")):
                errors.append(f"SBOM must identify {name} as a bundled arm64 dependency")
    else:
        errors.append("SBOM must be an object")

    try:
        notices = (security / "THIRD_PARTY_NOTICES.md").read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError) as error:
        errors.append(f"third-party notices are unreadable: {error}")
    else:
        for name, (version, _license) in RUNTIME_DEPENDENCIES.items():
            marker = f"{name} {version}" if name != "Python" else "Python 3.14"
            if marker not in notices:
                errors.append(f"third-party notices omit {marker}")

    errors.extend(validate_model_assets(resources, allow_model_assets=True))
    errors.extend(forbidden_security_assets(resources))
    return errors


def developer_path_leaks(
    bundle: Path,
    *,
    forbidden_roots: Iterable[Path] = (),
) -> list[str]:
    contents = bundle / "Contents"
    candidates = sorted(
        (
            path
            for path in contents.rglob("*")
            if path.is_file()
            and not path.is_relative_to(contents / "Resources/ReynPython")
        ),
        key=lambda path: path.relative_to(contents).as_posix(),
    )
    markers = {
        str(root.resolve()).encode("utf-8")
        for root in forbidden_roots
        if str(root).strip()
    }
    leaks: list[str] = []
    for path in candidates:
        try:
            data = path.read_bytes()
        except OSError:
            continue
        relative = path.relative_to(contents).as_posix()
        hits: set[str] = set()
        for marker in markers:
            if marker and marker in data:
                hits.add(marker.decode("utf-8", errors="replace"))
        patterns = (
            DEVELOPER_PATH_BYTE_PATTERNS[:5]
            if relative == "Resources/release-manifest.json"
            else DEVELOPER_PATH_BYTE_PATTERNS
        )
        for pattern in patterns:
            for match in pattern.finditer(data):
                hits.add(match.group(0).decode("utf-8", errors="replace"))
        leaks.extend(f"{relative}: {hit}" for hit in sorted(hits))
    return leaks


def validate_release_manifest(contents: Path, manifest_path: Path) -> list[str]:
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        return [f"cannot read release manifest: {error}"]
    rows = manifest.get("files")
    if not isinstance(rows, list):
        return ["release manifest files must be a list"]

    errors: list[str] = []
    indexed: dict[str, dict[str, object]] = {}
    for row in rows:
        if not isinstance(row, dict) or not isinstance(row.get("path"), str):
            errors.append("release manifest contains a malformed file row")
            continue
        relative = row["path"]
        path = Path(relative)
        if (
            path.is_absolute()
            or ".." in path.parts
            or re.match(r"^[A-Za-z]:[\\/]", relative)
        ):
            errors.append(f"release manifest path is not portable: {relative}")
            continue
        if relative in indexed:
            errors.append(f"release manifest path is duplicated: {relative}")
            continue
        indexed[relative] = row

    expected = {
        path.relative_to(contents).as_posix()
        for path in contents.rglob("*")
        if path.is_file() and path != manifest_path
    }
    listed = set(indexed)
    for relative in sorted(expected - listed):
        errors.append(f"release manifest omits {relative}")
    for relative in sorted(listed - expected):
        errors.append(f"release manifest references missing {relative}")
    for relative in sorted(expected & listed):
        path = contents / relative
        row = indexed[relative]
        if row.get("bytes") != path.stat().st_size:
            errors.append(f"release manifest byte count differs for {relative}")
        if row.get("sha256") != sha256_file(path):
            errors.append(f"release manifest SHA-256 differs for {relative}")
    return errors


def validate_runtime_contract(
    path: Path,
    *,
    developer_id_signed: bool = False,
    notarized: bool = False,
) -> list[str]:
    try:
        contract = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        return [f"cannot read runtime contract: {error}"]
    expected = {
        ("engine", "used_by_current_binary"): True,
        ("python", "bundled"): True,
        ("research_runtime", "bundled"): True,
        ("checkpoints", "bundled"): True,
        ("checkpoints", "pickle_formats_permitted"): False,
        ("model_trust", "production_root_pinned"): True,
        ("model_trust", "model_assets_bundled"): True,
        ("model_trust", "failure_mode"): "fail-closed",
        ("documentation", "bundled"): True,
        ("documentation", "network_required"): False,
        ("apple_distribution", "developer_id_signing_performed"): bool(
            developer_id_signed
        ),
        ("apple_distribution", "notarization_performed"): bool(notarized),
    }
    errors = [
        f"{section}.{key}={contract.get(section, {}).get(key)!r}, expected {value!r}"
        for (section, key), value in expected.items()
        if contract.get(section, {}).get(key) != value
    ]
    if tuple(contract.get("engine", {}).get("bundled_modules", ())) != ENGINE_RESOURCES:
        errors.append("engine.bundled_modules does not match packaged sidecar resources")
    if (
        tuple(contract.get("research_runtime", {}).get("bundled_modules", ()))
        != RESEARCH_RESOURCES
    ):
        errors.append(
            "research_runtime.bundled_modules does not match packaged research resources"
        )
    expected_distributions = {
        name: version
        for name, (version, _license) in RUNTIME_DEPENDENCIES.items()
    }
    if contract.get("python", {}).get("required_distributions") != expected_distributions:
        errors.append(
            "python.required_distributions does not match the locked runtime inventory"
        )
    expected_security_paths = {
        "contract": "security/MODEL_TRUST_CONTRACT.json",
        "sbom": "security/SBOM.spdx.json",
        "third_party_notices": "security/THIRD_PARTY_NOTICES.md",
    }
    actual_security_paths = {
        "contract": contract.get("model_trust", {}).get("contract"),
        "sbom": contract.get("supply_chain", {}).get("sbom"),
        "third_party_notices": contract.get("supply_chain", {}).get(
            "third_party_notices"
        ),
    }
    if actual_security_paths != expected_security_paths:
        errors.append("runtime contract security artifact paths are incomplete")
    return errors


def write_json(path: Path, value: object) -> None:
    path.write_text(
        json.dumps(value, indent=2, sort_keys=True, ensure_ascii=False) + "\n",
        encoding="utf-8",
    )


def deterministic_zip(source: Path, destination: Path, source_date_epoch: int) -> None:
    timestamp = datetime.fromtimestamp(
        max(source_date_epoch, DEFAULT_SOURCE_DATE_EPOCH), tz=timezone.utc
    )
    zip_time = (timestamp.year, timestamp.month, timestamp.day, timestamp.hour, timestamp.minute, 0)
    with zipfile.ZipFile(destination, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
        for path in sorted(source.rglob("*"), key=lambda item: item.relative_to(source.parent).as_posix()):
            relative = path.relative_to(source.parent).as_posix()
            is_symlink = path.is_symlink()
            if path.is_dir() and not is_symlink:
                relative += "/"
            info = zipfile.ZipInfo(relative, zip_time)
            mode = path.lstat().st_mode
            info.create_system = 3
            file_type = (
                stat.S_IFLNK
                if is_symlink
                else (stat.S_IFDIR if path.is_dir() else stat.S_IFREG)
            )
            info.external_attr = ((stat.S_IMODE(mode) | file_type) << 16)
            info.compress_type = zipfile.ZIP_DEFLATED
            payload = (
                os.readlink(path).encode("utf-8")
                if is_symlink
                else (b"" if path.is_dir() else path.read_bytes())
            )
            archive.writestr(info, payload)


def write_sha256sums(paths: Iterable[Path], destination: Path) -> None:
    rows = [
        f"{sha256_file(path)}  {path.name}"
        for path in sorted(paths, key=lambda item: item.name)
    ]
    temporary = destination.with_suffix(destination.suffix + ".tmp")
    temporary.write_text("".join(f"{row}\n" for row in rows), encoding="utf-8")
    temporary.replace(destination)


def executable_architectures(executable: Path) -> tuple[str, ...]:
    result = run(["lipo", "-archs", str(executable)], cwd=executable.parent, capture=True)
    discovered = set(result.stdout.strip().split())
    return tuple(
        architecture
        for architecture in ARCHITECTURE_ORDER
        if architecture in discovered
    )


def require_executable_architectures(
    executable: Path,
    expected: tuple[str, ...],
    *,
    context: str = "Mach-O executable",
) -> tuple[str, ...]:
    actual = executable_architectures(executable)
    if set(actual) != set(expected):
        raise RuntimeError(
            f"{context} has architectures {', '.join(actual) or 'none'}; "
            f"expected {', '.join(expected)}"
        )
    try:
        run(
            ["lipo", str(executable), "-verify_arch", *expected],
            cwd=executable.parent,
            capture=True,
        )
    except subprocess.CalledProcessError as error:
        raise RuntimeError(
            f"{context} failed lipo slice validation for {', '.join(expected)}"
        ) from error
    return actual


def linked_libraries(executable: Path, architecture: str | None = None) -> list[str]:
    command = ["otool"]
    if architecture is not None:
        command.extend(["-arch", architecture])
    command.extend(["-L", str(executable)])
    result = run(command, cwd=executable.parent, capture=True)
    libraries = []
    for line in result.stdout.splitlines()[1:]:
        value = line.strip().split(" (", 1)[0]
        if value and not value.endswith(":"):
            libraries.append(value)
    return libraries


def parse_macho_minimum_macos(output: str) -> str | None:
    lines = [line.strip() for line in output.splitlines()]
    for index, line in enumerate(lines):
        if line == "cmd LC_BUILD_VERSION":
            for candidate in lines[index + 1 : index + 8]:
                if candidate.startswith("minos "):
                    return candidate.removeprefix("minos ").strip()
        if line == "cmd LC_VERSION_MIN_MACOSX":
            for candidate in lines[index + 1 : index + 8]:
                if candidate.startswith("version "):
                    return candidate.removeprefix("version ").strip()
    return None


def executable_minimum_macos(
    executable: Path, architecture: str | None = None
) -> str | None:
    command = ["otool"]
    if architecture is not None:
        command.extend(["-arch", architecture])
    command.extend(["-l", str(executable)])
    result = run(command, cwd=executable.parent, capture=True)
    return parse_macho_minimum_macos(result.stdout)


def validate_architecture_metadata(
    manifest: object,
    actual_architectures: tuple[str, ...],
    expected_architectures: tuple[str, ...] | None,
) -> list[str]:
    if not isinstance(manifest, dict):
        return ["release manifest must be an object"]
    errors = []
    if manifest.get("schema") != "com.reyn.studio.local-package.v2":
        errors.append(
            f"release manifest schema={manifest.get('schema')!r}, "
            "expected 'com.reyn.studio.local-package.v2'"
        )
    target = manifest.get("rust_target")
    if target not in TARGET_ARCHITECTURES:
        errors.append(f"release manifest has unsupported rust_target {target!r}")
        target_architectures: tuple[str, ...] = ()
    else:
        target_architectures = TARGET_ARCHITECTURES[target]
    declared_value = manifest.get("architectures")
    declared = (
        tuple(declared_value)
        if isinstance(declared_value, list)
        and all(isinstance(item, str) for item in declared_value)
        else ()
    )
    if not declared:
        errors.append("release manifest architectures must be a non-empty string list")
    if target_architectures and set(declared) != set(target_architectures):
        errors.append(
            f"release manifest target {target!r} requires "
            f"{', '.join(target_architectures)}, declared {', '.join(declared) or 'none'}"
        )
    if set(declared) != set(actual_architectures):
        errors.append(
            "release manifest architectures do not match executable slices: "
            f"declared {', '.join(declared) or 'none'}, "
            f"actual {', '.join(actual_architectures) or 'none'}"
        )
    if expected_architectures is not None and set(declared) != set(
        expected_architectures
    ):
        errors.append(
            f"release manifest declares {', '.join(declared) or 'none'}, "
            f"expected {', '.join(expected_architectures)}"
        )

    slices = manifest.get("architecture_slices")
    if not isinstance(slices, list):
        errors.append("release manifest architecture_slices must be a list")
        return errors
    slice_map: dict[str, dict[str, object]] = {}
    for row in slices:
        if not isinstance(row, dict) or not isinstance(row.get("architecture"), str):
            errors.append("release manifest contains a malformed architecture slice")
            continue
        architecture = str(row["architecture"])
        if architecture in slice_map:
            errors.append(f"release manifest duplicates {architecture} slice metadata")
            continue
        slice_map[architecture] = row
    if set(slice_map) != set(declared):
        errors.append("release manifest architecture_slices do not match architectures")
    expected_targets = {
        architecture: rust_target
        for rust_target, architecture in RUST_TARGET_ARCHITECTURES.items()
    }
    for architecture, row in slice_map.items():
        if row.get("rust_target") != expected_targets.get(architecture):
            errors.append(
                f"{architecture} slice has rust_target {row.get('rust_target')!r}, "
                f"expected {expected_targets.get(architecture)!r}"
            )
        if not isinstance(row.get("bytes"), int) or int(row["bytes"]) <= 0:
            errors.append(f"{architecture} slice has invalid byte count")
        digest = row.get("source_binary_sha256")
        if not isinstance(digest, str) or not re.fullmatch(r"[0-9a-f]{64}", digest):
            errors.append(f"{architecture} slice has invalid source binary SHA-256")
    return errors


def signing_state(bundle: Path) -> str:
    result = subprocess.run(
        ["codesign", "-dvvv", str(bundle)],
        cwd=bundle.parent,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
    )
    output = result.stdout
    if "TeamIdentifier=" in output and "TeamIdentifier=not set" not in output:
        return "credential-signed"
    if "Signature=adhoc" in output:
        return "ad-hoc"
    return "unsigned"


_MACH_O_MAGICS = {
    b"\xfe\xed\xfa\xce",
    b"\xce\xfa\xed\xfe",
    b"\xfe\xed\xfa\xcf",
    b"\xcf\xfa\xed\xfe",
    b"\xca\xfe\xba\xbe",
    b"\xbe\xba\xfe\xca",
}


def is_mach_o(path: Path) -> bool:
    try:
        with path.open("rb") as stream:
            magic = stream.read(4)
    except OSError:
        return False
    return magic in _MACH_O_MAGICS


def nested_mach_o_paths(bundle: Path) -> list[Path]:
    """Mach-O files inside the bundle, deepest paths first (sign inside-out)."""
    paths = [
        path
        for path in bundle.rglob("*")
        if path.is_file() and not path.is_symlink() and is_mach_o(path)
    ]
    paths.sort(key=lambda path: (len(path.parts), str(path)), reverse=True)
    return paths


def bundle_main_executable(bundle: Path) -> Path | None:
    """Return the primary `Contents/MacOS` executable when it can be identified."""
    macos = bundle / "Contents" / "MacOS"
    if not macos.is_dir():
        return None
    candidates = [
        path
        for path in macos.iterdir()
        if path.is_file() and not path.name.startswith(".")
    ]
    if not candidates:
        return None
    preferred = macos / "reyn-studio"
    if preferred.is_file():
        return preferred
    return sorted(candidates, key=lambda path: path.name)[0]


def developer_id_sign(
    bundle: Path,
    identity: str | None,
    *,
    entitlements: Path | None,
) -> bool:
    """Opt-in Developer ID Application signing. Fail closed when requested."""
    if identity is None:
        return False
    identity = identity.strip()
    if not identity:
        raise RuntimeError("Developer ID signing requested but --sign-identity is empty")
    codesign = shutil.which("codesign")
    if codesign is None:
        raise RuntimeError("Developer ID signing requested but codesign is unavailable")
    if entitlements is None or not entitlements.is_file():
        raise RuntimeError(
            "Developer ID signing requested but entitlements file is missing"
        )
    main_executable = bundle_main_executable(bundle)
    # Nested libraries/helpers get hardened-runtime signing without the app
    # entitlements. Entitlements apply only to the main executable and the .app.
    for path in nested_mach_o_paths(bundle):
        command = [
            codesign,
            "--force",
            "--options",
            "runtime",
            "--timestamp",
            "--sign",
            identity,
        ]
        if main_executable is not None and path.resolve() == main_executable.resolve():
            command.extend(["--entitlements", str(entitlements)])
        command.append(str(path))
        subprocess.run(command, check=True)
    subprocess.run(
        [
            codesign,
            "--force",
            "--options",
            "runtime",
            "--timestamp",
            "--entitlements",
            str(entitlements),
            "--sign",
            identity,
            str(bundle),
        ],
        check=True,
    )
    verify = subprocess.run(
        [codesign, "--verify", "--deep", "--strict", "--verbose=2", str(bundle)],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
    )
    if verify.returncode != 0:
        raise RuntimeError(
            f"codesign --verify failed after Developer ID signing:\n{verify.stdout}"
        )
    if signing_state(bundle) != "credential-signed":
        raise RuntimeError(
            "Developer ID signing finished but codesign still reports no Team ID "
            "(identity may be ad-hoc or not a Developer ID Application certificate)"
        )
    return True


def notarize_and_staple(
    bundle: Path,
    *,
    keychain_profile: str | None,
    source_date_epoch: int,
) -> bool:
    """Opt-in notarytool submit + stapler staple. Fail closed when requested."""
    if keychain_profile is None:
        return False
    profile = keychain_profile.strip()
    if not profile:
        raise RuntimeError("Notarization requested but --notarize-profile is empty")
    if signing_state(bundle) != "credential-signed":
        raise RuntimeError(
            "Notarization requested but the bundle is not Developer ID signed"
        )
    xcrun = shutil.which("xcrun")
    if xcrun is None:
        raise RuntimeError("Notarization requested but xcrun is unavailable")
    with tempfile.TemporaryDirectory(prefix="reyn-notarize-") as temporary:
        archive = Path(temporary) / f"{bundle.name}.zip"
        # notarytool wants a zip of the .app; use ditto for Apple-compatible layout.
        ditto = shutil.which("ditto")
        if ditto is None:
            raise RuntimeError("Notarization requested but ditto is unavailable")
        subprocess.run(
            [ditto, "-c", "-k", "--keepParent", str(bundle), str(archive)],
            check=True,
        )
        submit = subprocess.run(
            [
                xcrun,
                "notarytool",
                "submit",
                str(archive),
                "--keychain-profile",
                profile,
                "--wait",
            ],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
        )
        if submit.returncode != 0:
            raise RuntimeError(f"notarytool submit failed:\n{submit.stdout}")
        if "status: Accepted" not in submit.stdout and "Accepted" not in submit.stdout:
            raise RuntimeError(
                "notarytool finished without an Accepted status:\n" + submit.stdout
            )
    staple = subprocess.run(
        [xcrun, "stapler", "staple", str(bundle)],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
    )
    if staple.returncode != 0:
        raise RuntimeError(f"stapler staple failed:\n{staple.stdout}")
    _ = source_date_epoch  # reserved: ZIP of notarized app is rebuilt by the caller
    return True


def validate_bundle(
    bundle: Path,
    config: ReleaseConfig,
    *,
    expected_architectures: tuple[str, ...] | None = None,
    require_runnable_architectures: bool = False,
    developer_id_signed: bool | None = None,
    notarized: bool | None = None,
) -> list[Check]:
    checks: list[Check] = []
    contents = bundle / "Contents"
    plist_path = contents / "Info.plist"
    executable = contents / "MacOS" / config.executable
    resources = contents / "Resources"
    if developer_id_signed is None or notarized is None:
        manifest_path = resources / "release-manifest.json"
        if manifest_path.is_file():
            try:
                manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
            except (OSError, UnicodeDecodeError, json.JSONDecodeError):
                manifest = {}
            if developer_id_signed is None:
                developer_id_signed = bool(manifest.get("developer_id_signed"))
            if notarized is None:
                notarized = bool(manifest.get("notarized"))
    developer_id_signed = bool(developer_id_signed)
    notarized = bool(notarized)

    if not plist_path.is_file():
        return [Check("FAIL", "Info.plist", f"missing {plist_path}")]
    with plist_path.open("rb") as stream:
        plist = plistlib.load(stream)

    expected = {
        "CFBundleIdentifier": config.bundle_identifier,
        "CFBundleShortVersionString": config.version,
        "CFBundleExecutable": config.executable,
        "LSMinimumSystemVersion": config.minimum_system_version,
    }
    metadata_errors = [
        f"{key}={plist.get(key)!r}, expected {value!r}"
        for key, value in expected.items()
        if plist.get(key) != value
    ]
    checks.append(
        Check(
            "PASS" if not metadata_errors else "FAIL",
            "bundle metadata",
            "identity, executable, version, and deployment target match Cargo metadata"
            if not metadata_errors
            else "; ".join(metadata_errors),
        )
    )

    exported = plist.get("UTExportedTypeDeclarations", [])
    extensions = {
        extension
        for declaration in exported
        for extension in declaration.get("UTTypeTagSpecification", {}).get(
            "public.filename-extension", []
        )
    }
    expected_extensions = set(PROJECT_EXTENSIONS + TEMPLATE_EXTENSIONS)
    checks.append(
        Check(
            "PASS" if extensions == expected_extensions else "FAIL",
            "document type metadata",
            f"declares {', '.join(sorted(extensions))}; Finder open association remains disabled",
        )
    )
    checks.append(
        Check(
            "PASS" if "CFBundleDocumentTypes" not in plist else "FAIL",
            "association honesty",
            "does not claim Finder open support before startup handles document-open events",
        )
    )

    executable_ok = executable.is_file() and os.access(executable, os.X_OK)
    checks.append(
        Check(
            "PASS" if executable_ok else "FAIL",
            "bundle executable",
            str(executable) if executable_ok else "missing or not executable",
        )
    )
    architectures: tuple[str, ...] = ()
    if executable_ok:
        try:
            architectures = executable_architectures(executable)
            verified_architectures = (
                expected_architectures
                if expected_architectures is not None
                else architectures
            )
            require_executable_architectures(executable, verified_architectures)
            architecture_error = ""
        except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
            architecture_error = str(error)
        checks.append(
            Check(
                "PASS" if not architecture_error else "FAIL",
                "architecture",
                (
                    f"{', '.join(architectures)}; every declared Mach-O slice passed lipo validation"
                    if not architecture_error
                    else architecture_error
                ),
            )
        )
        for architecture in architectures:
            minimum = executable_minimum_macos(executable, architecture)
            checks.append(
                Check(
                    "PASS" if minimum == config.minimum_system_version else "FAIL",
                    f"Mach-O deployment target ({architecture})",
                    minimum or "not found",
                )
            )
            libraries = linked_libraries(executable, architecture)
            non_system = [
                item
                for item in libraries
                if not item.startswith(("/System/Library/", "/usr/lib/"))
            ]
            checks.append(
                Check(
                    "PASS" if not non_system else "FAIL",
                    f"dynamic libraries ({architecture})",
                    "system frameworks only"
                    if not non_system
                    else f"unbundled libraries: {', '.join(non_system)}",
                )
            )
        if "x86_64" in architectures and platform.machine() in {"arm64", "aarch64"}:
            status = rosetta_status()
            rosetta_required_but_missing = (
                require_runnable_architectures and status != "available"
            )
            checks.append(
                Check(
                    "FAIL"
                    if rosetta_required_but_missing
                    else ("PASS" if status == "available" else "BLOCKED"),
                    "x86_64 execution layer",
                    (
                        "Rosetta is available; this verifies local x86_64 execution support, "
                        "not application startup"
                        if status == "available"
                        else "Rosetta is unavailable; install it with "
                        "`softwareupdate --install-rosetta` before local x86_64 runtime tests"
                    ),
                )
            )

    required_resources = [
        resources / "ReynStudio.icns",
        resources / "LICENSE",
        resources / "NOTICE",
        resources / "runtime-requirements.json",
        resources / "release-manifest.json",
        *(resources / "docs" / name for name in DOCUMENTATION_RESOURCES),
        *(resources / "engine" / name for name in ENGINE_RESOURCES),
        *(resources / "research" / name for name in RESEARCH_RESOURCES),
        *(resources / "security" / name for name in SECURITY_RESOURCES),
    ]
    missing_resources = [
        path.relative_to(resources).as_posix()
        for path in required_resources
        if not path.is_file()
    ]
    checks.append(
        Check(
            "PASS" if not missing_resources else "FAIL",
            "resources",
            "icon, runtime contract, manifest, sidecar, research, and security inventory included"
            if not missing_resources
            else f"missing: {', '.join(missing_resources)}",
        )
    )

    runtime_path = resources / "runtime-requirements.json"
    runtime_errors = (
        validate_runtime_contract(
            runtime_path,
            developer_id_signed=developer_id_signed,
            notarized=notarized,
        )
        if runtime_path.is_file()
        else ["runtime contract is missing"]
    )
    checks.append(
        Check(
            "PASS" if not runtime_errors else "FAIL",
            "runtime contract honesty",
            "bundled arm64 Python/research code and checkpoint/Apple gates are explicit"
            if not runtime_errors
            else "; ".join(runtime_errors),
        )
    )
    factory_runtime = contents / "Resources/ReynPython"
    factory_runtime_errors = validate_factory_runtime_manifest(factory_runtime)
    checks.append(
        Check(
            "PASS" if not factory_runtime_errors else "FAIL",
            "arm64 factory runtime",
            (
                "relocatable arm64 interpreter, exact dependencies, canonical manifest, "
                "SBOM, and notices validated"
                if not factory_runtime_errors
                else "; ".join(factory_runtime_errors)
            ),
        )
    )

    security_errors = validate_security_artifacts(resources)
    checks.append(
        Check(
            "PASS" if not security_errors else "FAIL",
            "model trust and supply chain",
            (
                "Python closure, external dependency SBOM/licenses, authenticated preview model, "
                "and fail-closed trust policy agree"
                if not security_errors
                else "; ".join(security_errors)
            ),
        )
    )

    manifest_path = resources / "release-manifest.json"
    manifest_errors = (
        validate_release_manifest(contents, manifest_path)
        if manifest_path.is_file()
        else ["release manifest is missing"]
    )
    checks.append(
        Check(
            "PASS" if not manifest_errors else "FAIL",
            "release manifest",
            "all staged files have matching relative byte counts and SHA-256 digests"
            if not manifest_errors
            else "; ".join(manifest_errors),
        )
    )
    try:
        release_manifest: object = json.loads(
            manifest_path.read_text(encoding="utf-8")
        )
    except (OSError, UnicodeDecodeError, json.JSONDecodeError):
        release_manifest = None
    architecture_metadata_errors = validate_architecture_metadata(
        release_manifest,
        architectures,
        expected_architectures,
    )
    checks.append(
        Check(
            "PASS" if not architecture_metadata_errors else "FAIL",
            "architecture metadata",
            (
                "release target, slice metadata, and executable architectures agree"
                if not architecture_metadata_errors
                else "; ".join(architecture_metadata_errors)
            ),
        )
    )
    resource_metadata_errors = (
        validate_resource_metadata(resources, release_manifest.get("resource_set"))
        if isinstance(release_manifest, dict)
        else ["release manifest resource_set is unavailable"]
    )
    checks.append(
        Check(
            "PASS" if not resource_metadata_errors else "FAIL",
            "resource parity metadata",
            (
                "architecture-neutral resource inventory and digest match staged files"
                if not resource_metadata_errors
                else "; ".join(resource_metadata_errors)
            ),
        )
    )

    path_leaks = developer_path_leaks(
        bundle,
        forbidden_roots=(config.root, Path.home()),
    )
    checks.append(
        Check(
            "PASS" if not path_leaks else "FAIL",
            "portable staged content",
            "no workspace, user-home, or build-temporary paths found in any staged file"
            if not path_leaks
            else "; ".join(path_leaks),
        )
    )

    state = signing_state(bundle)
    if state == "credential-signed":
        distribution_detail = (
            f"{state}; Developer ID signature present "
            "(notarization reported separately in the release manifest)"
        )
    else:
        distribution_detail = (
            f"{state}; Developer ID signing and notarization were not performed "
            "by this workflow unless --sign-identity / --notarize-profile were set"
        )
    checks.append(Check("INFO", "Apple distribution", distribution_detail))
    for index, blocker in enumerate(
        standalone_blockers(
            config.root,
            developer_id_signed=developer_id_signed,
            notarized=notarized,
        ),
        start=1,
    ):
        checks.append(Check("BLOCKED", f"standalone gate {index}", blocker))
    return checks


def print_checks(checks: Iterable[Check]) -> None:
    for check in checks:
        print(f"[{check.level:7}] {check.name}: {check.detail}")


def has_failures(checks: Iterable[Check]) -> bool:
    return any(check.level == "FAIL" for check in checks)


def copy_engine_resources(root: Path, destination: Path) -> list[Path]:
    destination.mkdir(parents=True)
    copied = []
    for name in ENGINE_RESOURCES:
        source = root / "engine" / name
        target = destination / name
        shutil.copy2(source, target)
        copied.append(target)
    return copied


def copy_documentation_resources(root: Path, destination: Path) -> list[Path]:
    destination.mkdir(parents=True)
    copied = []
    for name in DOCUMENTATION_RESOURCES:
        source = root / name
        target = destination / name
        shutil.copy2(source, target)
        copied.append(target)
    return copied


def copy_security_resources(root: Path, destination: Path) -> list[Path]:
    destination.mkdir(parents=True)
    copied = []
    source_dir = root / "packaging/macos"
    for name in SECURITY_RESOURCES:
        source = source_dir / name
        if not source.is_file():
            raise FileNotFoundError(f"required security artifact is missing: {source}")
        target = destination / name
        shutil.copy2(source, target)
        copied.append(target)
    return copied


def copy_research_resources(source: Path, destination: Path) -> list[Path]:
    destination.mkdir(parents=True)
    copied = []
    for name in RESEARCH_RESOURCES:
        module = source / name
        if not module.is_file():
            raise FileNotFoundError(f"required research module is missing: {module}")
        target = destination / name
        shutil.copy2(module, target)
        copied.append(target)
    return copied


def copy_preview_model_resources(root: Path, resources: Path) -> list[Path]:
    source = root / PREVIEW_MODEL_ROOT
    research = resources / "research"
    documentation = resources / "docs/models"
    research.mkdir(parents=True, exist_ok=True)
    documentation.mkdir(parents=True, exist_ok=True)
    copied: list[Path] = []
    for name in PREVIEW_MODEL_FILES:
        target = research / name
        shutil.copy2(source / name, target)
        copied.append(target)
    repository = source / f"{PREVIEW_MODEL_NAME}.tuf"
    repository_target = research / repository.name
    shutil.copytree(repository, repository_target)
    copied.extend(path for path in repository_target.rglob("*") if path.is_file())
    manifest_target = documentation / "model-release-manifest.json"
    shutil.copy2(source / "model-release-manifest.json", manifest_target)
    copied.append(manifest_target)
    for name in PREVIEW_MODEL_EVIDENCE:
        target = documentation / name
        shutil.copy2(source / "evidence" / name, target)
        copied.append(target)
    return copied


def copy_flow3d_research_model_resources(root: Path, resources: Path) -> list[Path]:
    source = root / FLOW3D_RESEARCH_MODEL_ROOT
    research = resources / "research"
    documentation = resources / "docs/models"
    research.mkdir(parents=True, exist_ok=True)
    documentation.mkdir(parents=True, exist_ok=True)
    copied: list[Path] = []
    for name in FLOW3D_RESEARCH_MODEL_FILES:
        target = research / name
        shutil.copy2(source / name, target)
        target.chmod(target.stat().st_mode | 0o644)
        copied.append(target)
    repository = source / f"{FLOW3D_RESEARCH_MODEL_NAME}.tuf"
    repository_target = research / repository.name
    shutil.copytree(repository, repository_target)
    copied.extend(path for path in repository_target.rglob("*") if path.is_file())
    manifest_target = documentation / FLOW3D_RESEARCH_MODEL_MANIFEST
    shutil.copy2(source / "model-release-manifest.json", manifest_target)
    copied.append(manifest_target)
    return copied
