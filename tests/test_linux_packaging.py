import json
import os
import stat
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))

from package_linux import (  # noqa: E402
    EXPECTED_ACCESS_ENDPOINT,
    EXPECTED_PRIVACY_VERSION,
    EXPECTED_TERMS_VERSION,
    preview_access_contract,
)
from linux_packaging import (  # noqa: E402
    RESEARCH_RESOURCES,
    deterministic_tar_xz,
    inventory,
    resolve_python_binary,
    validate_stage,
    write_json,
)


class LinuxPackagingTests(unittest.TestCase):
    def make_stage(self, root: Path) -> Path:
        stage = root / "Reyn-Studio-0.4.1-linux-x86_64"
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
            "resources/engine/pinned_model_trust.py",
            "resources/docs/PRD.md",
            "resources/docs/MODEL_BUNDLE_PROVENANCE.md",
            "THIRD_PARTY_NOTICES.md",
            "SBOM.spdx.json",
            "dependency-closure.json",
        )
        for relative in required:
            path = stage / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(relative.encode("utf-8"))
        (stage / "reyn-studio").chmod(
            (stage / "reyn-studio").stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH
        )
        for name in RESEARCH_RESOURCES:
            path = stage / "resources/research" / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("# fixture\n", encoding="utf-8")
        packages = [
            {
                "ecosystem": "python",
                "name": "CPython",
                "normalized_name": "cpython",
                "version": "3.14.6",
                "license": "PSF-2.0",
                "source": "https://www.python.org/",
            }
        ]
        closure = {
            "schema": "com.reyn.dependency-closure/1",
            "target": "linux-x86_64",
            "cargo_lock_sha256": "a" * 64,
            "python_lock_sha256": "b" * 64,
            "packages": packages,
        }
        write_json(stage / "dependency-closure.json", closure)
        write_json(
            stage / "SBOM.spdx.json",
            {
                "spdxVersion": "SPDX-2.3",
                "packages": [
                    {
                        "name": "CPython",
                        "versionInfo": "3.14.6",
                        "licenseDeclared": "PSF-2.0",
                        "downloadLocation": "https://www.python.org/",
                    }
                ],
            },
        )
        write_json(
            stage / "ReynPython/runtime-sbom.cdx.json",
            {
                "bomFormat": "CycloneDX",
                "components": [{"name": "CPython", "version": "3.14.6"}],
            },
        )
        (stage / "THIRD_PARTY_NOTICES.md").write_text(
            "# Notices\n\n## CPython 3.14.6 (python)\n", encoding="utf-8"
        )
        write_json(
            stage / "release-manifest.json",
            {
                "platform": "linux",
                "architecture": "x86_64",
                "cuda_supported": False,
                "linux_verified": False,
                "verification_boundary": (
                    "Linux preview pending verification. Package structure validated."
                ),
                "preview_access": {
                    "schema": "com.reyn.studio.preview-access/1",
                    "required": True,
                    "endpoint": EXPECTED_ACCESS_ENDPOINT,
                    "terms_version": EXPECTED_TERMS_VERSION,
                    "privacy_version": EXPECTED_PRIVACY_VERSION,
                },
                "cargo_lock_sha256": "a" * 64,
                "python_lock_sha256": "b" * 64,
                "model_loader_probe": {
                    "bundle_schema": "com.reyn.inference-model-bundle/1",
                    "bundled_model_authenticity": "verified",
                    "bundled_model_dimension": 2,
                    "bundled_model_id": "reyn-h64-tail-brinkman-seed0-v1.reynmodel",
                    "bundled_model_max_steps": 64,
                    "bundled_model_sha256": (
                        "1282395279cbbe8dea50524bb5844938edb5df44e4f15c6a8b4cb1bf5fd0e022"
                    ),
                    "bundled_model_status": "clean",
                    "import_ok": False,
                    "loader_error": "unsigned_or_malformed",
                    "loader_origin": "engine",
                    "model_card_status": "invalid",
                    "production_tuf_root_pinned": True,
                },
                "bundled_models": [
                    {
                        "schema": "com.reyn.yc-preview-model-release/1",
                        "bundle_sha256": (
                            "1282395279cbbe8dea50524bb5844938edb5df44e4f15c6a8b4cb1bf5fd0e022"
                        ),
                        "qualification_boundary": (
                            "Three-seed research replication passed; production "
                            "scientific/runtime/distribution qualification remains incomplete."
                        ),
                    }
                ],
            },
        )
        write_json(
            stage / "resource-inventory.json",
            inventory(
                stage,
                excluded={"release-manifest.json", "resource-inventory.json"},
            ),
        )
        return stage

    def test_validate_stage_accepts_complete_fixture(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            stage = self.make_stage(Path(temporary))
            self.assertEqual(validate_stage(stage), [])

    def test_validate_stage_rejects_verified_claim(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            stage = self.make_stage(Path(temporary))
            manifest = json.loads((stage / "release-manifest.json").read_text(encoding="utf-8"))
            manifest["linux_verified"] = True
            write_json(stage / "release-manifest.json", manifest)
            errors = validate_stage(stage)
            self.assertTrue(any("must not claim Linux verification" in error for error in errors))

    def test_validate_stage_requires_preview_label(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            stage = self.make_stage(Path(temporary))
            manifest = json.loads((stage / "release-manifest.json").read_text(encoding="utf-8"))
            manifest["verification_boundary"] = "shipped"
            write_json(stage / "release-manifest.json", manifest)
            errors = validate_stage(stage)
            self.assertTrue(
                any("Linux preview pending verification" in error for error in errors)
            )

    def test_resolve_python_binary(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            runtime = Path(temporary) / "ReynPython"
            binary = runtime / "bin/python3.14"
            binary.parent.mkdir(parents=True)
            binary.write_text("#!/bin/sh\n", encoding="utf-8")
            self.assertEqual(resolve_python_binary(runtime), binary)

    def test_deterministic_tar_xz(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            stage = root / "stage"
            stage.mkdir()
            (stage / "reyn-studio").write_bytes(b"binary")
            (stage / "reyn-studio").chmod(0o755)
            first = root / "a.tar.xz"
            second = root / "b.tar.xz"
            deterministic_tar_xz(stage, first, 315532800)
            deterministic_tar_xz(stage, second, 315532800)
            self.assertEqual(first.read_bytes(), second.read_bytes())

    def test_preview_access_contract_matches(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            binary = Path(temporary) / "reyn-studio"
            contract = {
                "schema": "com.reyn.studio.preview-access/1",
                "required": True,
                "endpoint": EXPECTED_ACCESS_ENDPOINT,
                "terms_version": EXPECTED_TERMS_VERSION,
                "privacy_version": EXPECTED_PRIVACY_VERSION,
            }
            binary.write_text("#!/bin/sh\n", encoding="utf-8")
            binary.chmod(0o755)

            class Result:
                stdout = json.dumps(contract) + "\n"

            with patch("package_linux.subprocess.run", return_value=Result()):
                self.assertEqual(preview_access_contract(binary), contract)

    def test_packaging_assets_exist(self) -> None:
        for relative in (
            "packaging/linux/release-pins.json",
            "packaging/linux/python-runtime.lock",
            "packaging/linux/python-runtime.in",
            "packaging/linux/AppRun",
            "packaging/linux/ReynStudio.desktop",
            "packaging/linux/ReynStudio.png",
            "packaging/linux/ReynStudio.svg",
            "scripts/package_linux.py",
            "scripts/linux_packaging.py",
            "scripts/validate_linux_package.py",
            "scripts/linux_engine_smoke.py",
        ):
            self.assertTrue((ROOT / relative).is_file(), relative)


if __name__ == "__main__":
    unittest.main()
