import ast
import base64
import hashlib
import json
import plistlib
import stat
import struct
import subprocess
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))

from macos_packaging import (  # noqa: E402
    DEFAULT_SOURCE_DATE_EPOCH,
    DOCUMENTATION_RESOURCES,
    ENGINE_RESOURCES,
    FLOW3D_RESEARCH_MODEL_FILES,
    FLOW3D_RESEARCH_MODEL_MANIFEST,
    FLOW3D_RESEARCH_MODEL_NAME,
    PACKAGE_RUST_TARGETS,
    PROJECT_EXTENSIONS,
    RESEARCH_RESOURCES,
    RUNTIME_DEPENDENCIES,
    SECURITY_RESOURCES,
    TARGET_ARCHITECTURES,
    TEMPLATE_EXTENSIONS,
    copy_documentation_resources,
    copy_engine_resources,
    copy_flow3d_research_model_resources,
    copy_research_resources,
    copy_security_resources,
    developer_id_sign,
    developer_path_leaks,
    deterministic_zip,
    file_manifest,
    info_plist,
    load_config,
    load_macos_release_pins,
    notarize_and_staple,
    parse_macho_minimum_macos,
    require_executable_architectures,
    require_local_architecture_runtime,
    require_packaging_toolchain,
    resolve_research_source,
    resource_metadata,
    rosetta_status,
    runtime_requirements,
    stage_factory_runtime,
    standalone_blockers,
    validate_bundle,
    validate_architecture_metadata,
    validate_build_number,
    validate_release_manifest,
    validate_research_dependency_lock,
    validate_research_source_pin,
    validate_resource_metadata,
    validate_runtime_contract,
    validate_runtime_dependency_lock,
    validate_runtime_sboms,
    validate_factory_runtime_manifest,
    validate_model_assets,
    validate_security_artifacts,
    write_sha256sums,
    write_json,
)
from package_macos import (  # noqa: E402
    EXPECTED_ACCESS_ENDPOINT,
    build_binary,
    package_input_fingerprint,
    preview_access_contract,
    release_build_environment,
    release_input_fingerprint,
)


class MacOSPackagingTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.config = load_config(ROOT)

    def test_bundle_identity_and_version_are_canonical_cargo_metadata(self):
        self.assertEqual(self.config.bundle_identifier, "com.reyn.studio")
        self.assertEqual(self.config.display_name, "Reyn Studio")
        self.assertEqual(self.config.version, "0.4.1")
        self.assertEqual(self.config.minimum_system_version, "11.0")

    def test_plist_declares_types_without_claiming_broken_finder_open(self):
        plist = info_plist(self.config, "42")
        self.assertNotIn("CFBundleDocumentTypes", plist)
        declarations = plist["UTExportedTypeDeclarations"]
        extensions = {
            extension
            for declaration in declarations
            for extension in declaration["UTTypeTagSpecification"][
                "public.filename-extension"
            ]
        }
        self.assertEqual(
            extensions, set(PROJECT_EXTENSIONS + TEMPLATE_EXTENSIONS)
        )
        encoded = plistlib.dumps(plist, fmt=plistlib.FMT_BINARY, sort_keys=True)
        self.assertEqual(plistlib.loads(encoded), plist)

    def test_build_number_rejects_non_apple_version_syntax(self):
        for value in ("1", "1.2", "1.2.3"):
            validate_build_number(value)
        for value in ("", "1-beta", "1.2.3.4", "v1"):
            with self.assertRaises(ValueError):
                validate_build_number(value)

    def test_runtime_contract_declares_arm64_factory_runtime_without_signing(self):
        contract = runtime_requirements()
        self.assertTrue(contract["python"]["bundled"])
        self.assertEqual(contract["python"]["architecture"], "arm64")
        self.assertEqual(contract["python"]["compute_unsupported_on"], ["x86_64"])
        self.assertTrue(contract["checkpoints"]["bundled"])
        self.assertFalse(
            contract["apple_distribution"]["developer_id_signing_performed"]
        )
        self.assertFalse(contract["apple_distribution"]["notarization_performed"])
        self.assertEqual(
            tuple(contract["engine"]["bundled_modules"]), ENGINE_RESOURCES
        )
        self.assertTrue(contract["engine"]["used_by_current_binary"])
        self.assertTrue(contract["research_runtime"]["bundled"])
        self.assertTrue(contract["documentation"]["bundled"])
        self.assertFalse(contract["documentation"]["network_required"])
        self.assertEqual(contract["documentation"]["entrypoint"], "docs/PRD.md")
        self.assertTrue(contract["model_trust"]["production_root_pinned"])
        self.assertTrue(contract["model_trust"]["model_assets_bundled"])
        self.assertEqual(contract["model_trust"]["failure_mode"], "fail-closed")
        self.assertEqual(
            contract["python"]["required_distributions"],
            {
                name: version
                for name, (version, _license) in RUNTIME_DEPENDENCIES.items()
            },
        )
        self.assertEqual(
            tuple(contract["research_runtime"]["bundled_modules"]),
            RESEARCH_RESOURCES,
        )
        self.assertIn("REYN_ENGINE_SCRIPT", contract["engine"]["resolution"])
        self.assertIn("REYN_PYTHON", contract["python"]["resolution"])
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "runtime-requirements.json"
            write_json(path, contract)
            self.assertEqual(validate_runtime_contract(path), [])
            contract["python"]["bundled"] = False
            write_json(path, contract)
            self.assertIn(
                "python.bundled=False, expected True",
                validate_runtime_contract(path),
            )

    def test_runtime_contract_accepts_signed_apple_distribution_flags(self):
        contract = runtime_requirements()
        contract["apple_distribution"] = {
            "developer_id_signing_performed": True,
            "notarization_performed": True,
        }
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "runtime-requirements.json"
            write_json(path, contract)
            self.assertEqual(
                validate_runtime_contract(
                    path, developer_id_signed=True, notarized=True
                ),
                [],
            )
            self.assertTrue(
                any(
                    "developer_id_signing_performed=True, expected False" in error
                    for error in validate_runtime_contract(path)
                )
            )
            contract["apple_distribution"]["notarization_performed"] = False
            write_json(path, contract)
            self.assertEqual(
                validate_runtime_contract(
                    path, developer_id_signed=True, notarized=False
                ),
                [],
            )

    def test_macho_minimum_version_parser_handles_current_and_legacy_commands(self):
        self.assertEqual(
            parse_macho_minimum_macos(
                "cmd LC_BUILD_VERSION\ncmdsize 32\nplatform 1\nminos 11.0\nsdk 26.0"
            ),
            "11.0",
        )
        self.assertEqual(
            parse_macho_minimum_macos(
                "cmd LC_VERSION_MIN_MACOSX\ncmdsize 16\nversion 10.15\nsdk 13.3"
            ),
            "10.15",
        )

    def test_target_matrix_maps_universal2_to_both_macho_architectures(self):
        self.assertEqual(
            PACKAGE_RUST_TARGETS["universal2"],
            ("aarch64-apple-darwin", "x86_64-apple-darwin"),
        )
        self.assertEqual(
            TARGET_ARCHITECTURES["universal2"], ("arm64", "x86_64")
        )

    def test_toolchain_preflight_names_missing_rust_target_and_install_command(self):
        completed = subprocess.CompletedProcess(
            ["xcrun"], 0, stdout="/Applications/Xcode.app/SDK\n"
        )
        with (
            patch("macos_packaging.shutil.which", return_value="/usr/bin/tool"),
            patch("macos_packaging.run", return_value=completed),
            patch(
                "macos_packaging.installed_rust_targets",
                return_value=("aarch64-apple-darwin",),
            ),
        ):
            with self.assertRaisesRegex(
                RuntimeError,
                r"rustup target add x86_64-apple-darwin",
            ):
                require_packaging_toolchain(ROOT, "universal2")

    def test_rosetta_status_reports_actionable_unavailability_on_apple_silicon(self):
        completed = subprocess.CompletedProcess(
            ["arch"], 1, stdout="Bad CPU type in executable\n"
        )
        with (
            patch("macos_packaging.platform.system", return_value="Darwin"),
            patch("macos_packaging.platform.machine", return_value="arm64"),
            patch("macos_packaging.subprocess.run", return_value=completed),
        ):
            self.assertEqual(rosetta_status(), "unavailable")

    def test_required_x86_runtime_names_rosetta_install_remedy(self):
        with patch("macos_packaging.rosetta_status", return_value="unavailable"):
            with self.assertRaisesRegex(
                RuntimeError, r"softwareupdate --install-rosetta"
            ):
                require_local_architecture_runtime(("arm64", "x86_64"))

    def test_lipo_architecture_gate_requires_every_expected_slice(self):
        executable = Path("/tmp/Reyn Studio.app/Contents/MacOS/reyn-studio")
        with patch(
            "macos_packaging.executable_architectures", return_value=("arm64",)
        ):
            with self.assertRaisesRegex(RuntimeError, "expected arm64, x86_64"):
                require_executable_architectures(
                    executable, ("arm64", "x86_64"), context="universal fixture"
                )

        completed = subprocess.CompletedProcess(["lipo"], 0, stdout="")
        with (
            patch(
                "macos_packaging.executable_architectures",
                return_value=("arm64", "x86_64"),
            ),
            patch("macos_packaging.run", return_value=completed) as mocked_run,
        ):
            require_executable_architectures(
                executable, ("arm64", "x86_64"), context="universal fixture"
            )
        self.assertEqual(
            mocked_run.call_args.args[0][0:4],
            ["lipo", str(executable), "-verify_arch", "arm64"],
        )
        self.assertEqual(mocked_run.call_args.args[0][4], "x86_64")

    def test_dual_architecture_builder_validates_thin_inputs_and_merged_output(self):
        with tempfile.TemporaryDirectory() as directory:
            work = Path(directory)
            target_dir = work / "target"
            config = SimpleNamespace(
                root=ROOT,
                executable="reyn-studio",
                minimum_system_version="11.0",
            )
            run_commands = []

            def fake_run(command, **_kwargs):
                run_commands.append(command)
                if command[:2] == ["cargo", "build"]:
                    rust_target = command[command.index("--target") + 1]
                    binary = target_dir / rust_target / "release/reyn-studio"
                    binary.parent.mkdir(parents=True, exist_ok=True)
                    binary.write_bytes(rust_target.encode("ascii"))
                elif command[:2] == ["lipo", "-create"]:
                    output = Path(command[command.index("-output") + 1])
                    output.write_bytes(b"universal fixture")
                return subprocess.CompletedProcess(command, 0, stdout="")

            with (
                patch("package_macos.run", side_effect=fake_run),
                patch(
                    "package_macos.require_executable_architectures",
                    side_effect=lambda _path, expected, **_kwargs: expected,
                ) as architecture_gate,
            ):
                binary, slices, source_fingerprint = build_binary(
                    config, "universal2", target_dir, work
                )

            self.assertEqual(binary, work / "reyn-studio")
            self.assertEqual(
                [row["architecture"] for row in slices],
                ["arm64", "x86_64"],
            )
            cargo_targets = [
                command[command.index("--target") + 1]
                for command in run_commands
                if command[:2] == ["cargo", "build"]
            ]
            self.assertEqual(
                cargo_targets,
                ["aarch64-apple-darwin", "x86_64-apple-darwin"],
            )
            self.assertEqual(architecture_gate.call_count, 3)
            self.assertEqual(
                architecture_gate.call_args.args[1], ("arm64", "x86_64")
            )
            self.assertRegex(source_fingerprint, r"^[0-9a-f]{64}$")

    def test_dual_architecture_builder_rejects_source_changes_between_slices(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "src").mkdir()
            source = root / "src/main.rs"
            source.write_text("fn main() {}\n", encoding="utf-8")
            (root / "Cargo.toml").write_text(
                "[package]\nname='reyn-studio'\nversion='0.1.1'\n",
                encoding="utf-8",
            )
            target_dir = root / "target"
            config = SimpleNamespace(
                root=root,
                executable="reyn-studio",
                minimum_system_version="11.0",
            )

            def mutate_during_build(command, **_kwargs):
                rust_target = command[command.index("--target") + 1]
                binary = target_dir / rust_target / "release/reyn-studio"
                binary.parent.mkdir(parents=True, exist_ok=True)
                binary.write_bytes(rust_target.encode("ascii"))
                source.write_text("fn main() { println!(\"changed\"); }\n", encoding="utf-8")
                return subprocess.CompletedProcess(command, 0, stdout="")

            with patch("package_macos.run", side_effect=mutate_during_build):
                with self.assertRaisesRegex(
                    RuntimeError,
                    "Rust release inputs changed during architecture builds",
                ):
                    build_binary(config, "universal2", target_dir, root)

    def test_release_input_fingerprint_changes_with_embedded_assets(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "src").mkdir()
            (root / "assets").mkdir()
            (root / "Cargo.toml").write_text("[package]\n", encoding="utf-8")
            (root / "src/main.rs").write_text("fn main() {}\n", encoding="utf-8")
            docs = root / "PRD.md"
            docs.write_bytes(b"docs-first")
            asset = root / "assets/font.bin"
            asset.write_bytes(b"first")
            first = release_input_fingerprint(root)
            asset.write_bytes(b"second")
            self.assertNotEqual(release_input_fingerprint(root), first)
            asset.write_bytes(b"first")
            docs.write_bytes(b"docs-second")
            self.assertNotEqual(release_input_fingerprint(root), first)

    def test_release_build_environment_remaps_and_strips_deterministically(self):
        with tempfile.TemporaryDirectory() as directory:
            target_dir = Path(directory) / "target with spaces"
            config = SimpleNamespace(
                root=ROOT,
                minimum_system_version="11.0",
            )
            with patch.dict(
                "package_macos.os.environ",
                {
                    "HOME": "/Users/release-builder",
                    "RUSTFLAGS": "-Copt-level=2",
                    "CARGO_ENCODED_RUSTFLAGS": "",
                },
                clear=False,
            ):
                first = release_build_environment(config, target_dir)
                second = release_build_environment(config, target_dir)

            flags = first["CARGO_ENCODED_RUSTFLAGS"].split("\x1f")
            self.assertEqual(
                first["CARGO_ENCODED_RUSTFLAGS"],
                second["CARGO_ENCODED_RUSTFLAGS"],
            )
            self.assertIn("-Copt-level=2", flags)
            self.assertIn(f"--remap-path-prefix={ROOT}=reyn-studio", flags)
            self.assertIn(
                "--remap-path-prefix=/Users/release-builder/.cargo/registry/src=crate-sources",
                flags,
            )
            self.assertIn(
                "--remap-path-prefix=/Users/release-builder=BUILD_HOME",
                flags,
            )
            self.assertIn("-Cdebuginfo=0", flags)
            self.assertIn("-Cstrip=symbols", flags)
            self.assertLess(
                flags.index(
                    "--remap-path-prefix=/Users/release-builder=BUILD_HOME"
                ),
                flags.index(
                    "--remap-path-prefix=/Users/release-builder/.cargo/registry/src=crate-sources"
                ),
            )
            self.assertEqual(first["CARGO_INCREMENTAL"], "0")
            self.assertEqual(first["REYN_ACCESS_REQUIRED"], "1")
            self.assertEqual(
                first["REYN_ACCESS_ENDPOINT"], EXPECTED_ACCESS_ENDPOINT
            )
            self.assertNotIn("RUSTFLAGS", first)
            wrapper = Path(first["RUSTC_WORKSPACE_WRAPPER"])
            self.assertTrue(wrapper.is_file())
            self.assertIn(
                "export CARGO_MANIFEST_DIR=.",
                wrapper.read_text(encoding="utf-8"),
            )

    def test_preview_access_contract_rejects_ungated_binary(self):
        with patch("package_macos.subprocess.run") as run:
            run.return_value = SimpleNamespace(
                stdout=json.dumps(
                    {
                        "schema": "com.reyn.studio.preview-access/1",
                        "required": False,
                        "endpoint": None,
                        "terms_version": "1.0",
                        "privacy_version": "1.0",
                    }
                )
            )
            with self.assertRaisesRegex(ValueError, "access contract"):
                preview_access_contract(Path("/tmp/reyn-studio"))

    def test_architecture_metadata_rejects_incomplete_universal_claim(self):
        manifest = {
            "schema": "com.reyn.studio.local-package.v2",
            "rust_target": "universal2",
            "architectures": ["arm64", "x86_64"],
            "architecture_slices": [
                {
                    "architecture": "arm64",
                    "rust_target": "aarch64-apple-darwin",
                    "bytes": 10,
                    "source_binary_sha256": "a" * 64,
                },
                {
                    "architecture": "x86_64",
                    "rust_target": "x86_64-apple-darwin",
                    "bytes": 11,
                    "source_binary_sha256": "b" * 64,
                },
            ],
        }
        self.assertEqual(
            validate_architecture_metadata(
                manifest, ("arm64", "x86_64"), ("arm64", "x86_64")
            ),
            [],
        )
        errors = validate_architecture_metadata(
            manifest, ("arm64",), ("arm64", "x86_64")
        )
        self.assertTrue(
            any("do not match executable slices" in error for error in errors)
        )

    def test_standalone_validator_surfaces_only_remaining_source_blockers(self):
        blockers = "\n".join(standalone_blockers(ROOT))
        self.assertNotIn("CARGO_MANIFEST_DIR", blockers)
        self.assertNotIn("developer-specific absolute path", blockers)
        self.assertNotIn("Python, NumPy, and PyTorch are not bundled", blockers)
        self.assertNotIn("production TUF root is intentionally unset", blockers)
        self.assertNotIn("No authenticated .reynmodel/.sig/.tuf triplet", blockers)
        self.assertIn("Developer ID signing and Apple notarization", blockers)
        self.assertIn("document associations are intentionally not claimed", blockers)
        signed = "\n".join(
            standalone_blockers(
                ROOT, developer_id_signed=True, notarized=True
            )
        )
        self.assertNotIn("Developer ID signing and Apple notarization", signed)
        self.assertNotIn("notarization was not performed", signed)
        signed_only = "\n".join(
            standalone_blockers(
                ROOT, developer_id_signed=True, notarized=False
            )
        )
        self.assertIn("notarization was not performed", signed_only)

    def test_developer_id_sign_is_opt_in_and_fail_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            bundle = Path(directory) / "Reyn Studio.app"
            bundle.mkdir()
            self.assertFalse(
                developer_id_sign(bundle, None, entitlements=None)
            )
            with self.assertRaisesRegex(RuntimeError, "entitlements file is missing"):
                developer_id_sign(
                    bundle,
                    "Developer ID Application: Example",
                    entitlements=None,
                )
            self.assertFalse(
                notarize_and_staple(
                    bundle, keychain_profile=None, source_date_epoch=0
                )
            )
            with self.assertRaisesRegex(RuntimeError, "not Developer ID signed"):
                notarize_and_staple(
                    bundle,
                    keychain_profile="reyn-notary",
                    source_date_epoch=0,
                )

    def test_all_lightweight_runtime_modules_copy_without_tests_or_checkpoints(self):
        with tempfile.TemporaryDirectory() as directory:
            bundle = Path(directory) / "Reyn Studio.app"
            contents = bundle / "Contents"
            destination = contents / "Resources"
            documentation = copy_documentation_resources(
                ROOT, destination / "docs"
            )
            engine = copy_engine_resources(ROOT, destination / "engine")
            research_source = resolve_research_source(ROOT)
            research = copy_research_resources(
                research_source, destination / "research"
            )
            security = copy_security_resources(ROOT, destination / "security")
            with (contents / "Info.plist").open("wb") as stream:
                plistlib.dump(info_plist(self.config, "1"), stream)
            write_json(destination / "runtime-requirements.json", runtime_requirements())
            self.assertEqual(
                {path.name for path in documentation},
                set(DOCUMENTATION_RESOURCES),
            )
            self.assertEqual(
                {path.name for path in engine}, set(ENGINE_RESOURCES)
            )
            self.assertEqual(
                {path.name for path in research}, set(RESEARCH_RESOURCES)
            )
            self.assertEqual(
                {path.name for path in security}, set(SECURITY_RESOURCES)
            )
            self.assertFalse(any(destination.rglob("test_*.py")))
            self.assertFalse(any(destination.rglob("*.pth")))
            self.assertEqual(developer_path_leaks(bundle), [])

    def test_flow3d_research_model_copies_into_bundled_research(self):
        with tempfile.TemporaryDirectory() as directory:
            resources = Path(directory) / "Resources"
            copied = copy_flow3d_research_model_resources(ROOT, resources)
            names = {path.name for path in copied}
            self.assertTrue(set(FLOW3D_RESEARCH_MODEL_FILES).issubset(names))
            self.assertIn(FLOW3D_RESEARCH_MODEL_MANIFEST, names)
            bundle = resources / "research" / FLOW3D_RESEARCH_MODEL_NAME
            self.assertTrue(bundle.is_file())
            self.assertTrue(bundle.stat().st_size > 0)
            self.assertTrue((resources / f"research/{FLOW3D_RESEARCH_MODEL_NAME}.sig").is_file())
            self.assertTrue((resources / f"research/{FLOW3D_RESEARCH_MODEL_NAME}.tuf").is_dir())
            manifest = json.loads(
                (
                    resources / "docs/models" / FLOW3D_RESEARCH_MODEL_MANIFEST
                ).read_text(encoding="utf-8")
            )
            self.assertEqual(manifest["schema"], "com.reyn.research-model-release/1")
            self.assertEqual(manifest["model"]["id"], "reyn-flow3d-obstacle-32")
            self.assertFalse(any(resources.rglob("*.pth")))

    def test_research_resource_list_is_the_sidecar_import_closure(self):
        research_source = resolve_research_source(ROOT)
        local_modules = {
            path.stem: path for path in research_source.glob("*.py")
        }

        def local_imports(path):
            tree = ast.parse(path.read_text(encoding="utf-8"))
            imports = {
                node.module.split(".", 1)[0]
                for node in ast.walk(tree)
                if isinstance(node, ast.ImportFrom) and node.module
            } | {
                alias.name.split(".", 1)[0]
                for node in ast.walk(tree)
                if isinstance(node, ast.Import)
                for alias in node.names
            }
            return imports & set(local_modules)

        engine_modules = {Path(name).stem for name in ENGINE_RESOURCES}
        closure = (
            local_imports(ROOT / "engine/reyn_engine.py")
            | local_imports(ROOT / "engine/model_bundle.py")
        ) - engine_modules
        pending = list(closure)
        while pending:
            module = pending.pop()
            discovered = local_imports(local_modules[module]) - closure
            closure.update(discovered)
            pending.extend(discovered)

        self.assertEqual(
            closure, {Path(name).stem for name in RESEARCH_RESOURCES}
        )

    def test_security_inventory_is_fail_closed_and_rejects_forbidden_assets(self):
        with tempfile.TemporaryDirectory() as directory:
            resources = Path(directory)
            copy_security_resources(ROOT, resources / "security")
            copy_engine_resources(ROOT, resources / "engine")
            copy_research_resources(
                resolve_research_source(ROOT), resources / "research"
            )
            self.assertEqual(validate_security_artifacts(resources), [])

            (resources / "unsigned.pth").write_bytes(b"pickle")
            errors = validate_security_artifacts(resources)
            self.assertTrue(any("forbidden model/key file" in error for error in errors))
            (resources / "unsigned.pth").unlink()

            secret = resources / "security/private.pem"
            secret.write_text(
                "-----BEGIN PRIVATE KEY-----\nnot-a-real-key\n",
                encoding="utf-8",
            )
            errors = validate_security_artifacts(resources)
            self.assertTrue(
                any("forbidden private-key material" in error for error in errors)
            )

    def test_dependency_inventory_matches_research_lock(self):
        research_source = resolve_research_source(ROOT)
        self.assertEqual(
            validate_research_dependency_lock(research_source),
            [],
        )
        self.assertEqual(validate_runtime_dependency_lock(ROOT), [])
        self.assertEqual(validate_runtime_sboms(ROOT), [])
        pins = load_macos_release_pins(ROOT)
        self.assertRegex(pins["research_revision"], r"^[0-9a-f]{40}$")
        self.assertEqual(pins["runtime_architecture"], "arm64")
        locked_names = {
            distribution["name"]
            for distribution in json.loads(
                (ROOT / "packaging/macos/python-runtime.lock.json").read_text()
            )["distributions"]
        }
        for name in (
            "cryptography",
            "safetensors",
            "securesystemslib",
            "python-tuf",
        ):
            self.assertIn(name, locked_names)

    def test_spdx_and_cyclonedx_cover_every_locked_distribution(self):
        lock = json.loads(
            (ROOT / "packaging/macos/python-runtime.lock.json").read_text()
        )
        expected = {
            (distribution["name"], distribution["version"])
            for distribution in lock["distributions"]
        }
        spdx = json.loads((ROOT / "packaging/macos/SBOM.spdx.json").read_text())
        cyclonedx = json.loads(
            (ROOT / "packaging/macos/runtime-sbom.cdx.json").read_text()
        )
        spdx_inventory = {
            (package["name"], package["versionInfo"])
            for package in spdx["packages"]
            if package["name"] != "Reyn Studio"
        }
        cyclonedx_inventory = {
            (component["name"], component["version"])
            for component in cyclonedx["components"]
        }
        self.assertEqual(spdx_inventory, expected)
        self.assertEqual(cyclonedx_inventory, expected)
        self.assertEqual(len(spdx_inventory), len(lock["distributions"]))

    def test_research_lock_requires_only_the_scientific_source_dependencies(self):
        with tempfile.TemporaryDirectory() as directory:
            research_source = Path(directory)
            (research_source / "pyproject.toml").write_text(
                '[project]\nrequires-python = ">=3.14"\n',
                encoding="utf-8",
            )
            packages = ("numpy",)
            (research_source / "uv.lock").write_text(
                "".join(
                    f'[[package]]\nname = "{name}"\nversion = "1.0.0"\n'
                    for name in packages
                ),
                encoding="utf-8",
            )
            self.assertIn(
                "research uv.lock omits torch",
                validate_research_dependency_lock(research_source),
            )

    def test_factory_runtime_staging_is_manifested_and_arm64_only(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source-runtime"
            interpreter = source / "bin/python3.14"
            interpreter.parent.mkdir(parents=True)
            interpreter.write_text("#!/bin/sh\n", encoding="utf-8")
            interpreter.chmod(0o755)
            (source / "lib").mkdir()
            (source / "lib/payload.bin").write_bytes(b"runtime payload")
            resources = root / "Resources"
            for folder, names in (
                ("engine", ENGINE_RESOURCES),
                ("research", RESEARCH_RESOURCES),
            ):
                (resources / folder).mkdir(parents=True)
                for name in names:
                    (resources / folder / name).write_text(name, encoding="utf-8")
            observed = {
                "architecture": "arm64",
                "Python": RUNTIME_DEPENDENCIES["Python"][0],
                "distributions": {
                    name: version
                    for name, (version, _license) in RUNTIME_DEPENDENCIES.items()
                    if name != "Python"
                },
                "prefix": str((root / "factory").resolve()),
            }
            completed = subprocess.CompletedProcess(
                ["python"], 0, stdout=json.dumps(observed)
            )
            with (
                patch(
                    "macos_packaging.executable_architectures",
                    return_value=("arm64",),
                ),
                patch("macos_packaging.run", return_value=completed),
            ):
                manifest = stage_factory_runtime(
                    source,
                    root / "factory",
                    resources=resources,
                    source_revision="a" * 40,
                    build_epoch=315532800,
                    compliance_root=ROOT / "packaging/macos",
                )
                self.assertEqual(
                    validate_factory_runtime_manifest(root / "factory"), []
                )
            self.assertEqual(manifest["architecture"], "arm64")
            self.assertEqual(manifest["python"], "3.14.6")
            self.assertTrue(manifest["runtime_id"].startswith("sha256:"))
            self.assertTrue(
                (root / "factory/runtime-sbom.cdx.json").is_file()
            )
            self.assertTrue(
                (root / "factory/THIRD_PARTY_NOTICES.html").is_file()
            )

    def test_model_triplet_layout_requires_matching_relative_hashes(self):
        with tempfile.TemporaryDirectory() as directory:
            resources = Path(directory)
            bundle = resources / "candidate.reynmodel"
            bundle.write_bytes(b"deterministic model bundle")
            payload = {
                "schema": "com.reyn.inference-model-signature-payload/1",
                "algorithm": "ed25519",
                "key_id": "publisher-1",
                "bundle_schema": "com.reyn.inference-model-bundle/1",
                "bundle_sha256": hashlib.sha256(bundle.read_bytes()).hexdigest(),
                "model": {"id": "reyn-flow", "version": "1.0.0"},
                "release_sequence": 7,
                "issued_at": "2026-07-01T00:00:00Z",
                "expires_at": "2027-07-01T00:00:00Z",
            }
            signature = Path(f"{bundle}.sig")
            signature.write_text(
                json.dumps(
                    {
                        "schema": "com.reyn.inference-model-signature/1",
                        "signed": payload,
                        "signature": base64.b64encode(b"\0" * 64).decode("ascii"),
                    }
                ),
                encoding="utf-8",
            )
            target_path = "models/reyn-flow/1.0.0/candidate.reynmodel"
            signature_target_path = target_path + ".sig"
            targets = {
                target_path: {
                    "length": bundle.stat().st_size,
                    "hashes": {"sha256": hashlib.sha256(bundle.read_bytes()).hexdigest()},
                    "custom": {
                        "schema": "com.reyn.tuf-model-target/1",
                        "model": payload["model"],
                        "release_sequence": 7,
                        "detached_signature": {
                            "target_path": signature_target_path,
                            "algorithm": "ed25519",
                            "key_id": "publisher-1",
                            "public_key": "public-only",
                        },
                    },
                },
                signature_target_path: {
                    "length": signature.stat().st_size,
                    "hashes": {
                        "sha256": hashlib.sha256(signature.read_bytes()).hexdigest()
                    },
                },
            }
            metadata = Path(f"{bundle}.tuf") / "metadata"
            metadata.mkdir(parents=True)
            for name in ("1.root.json", "1.targets.json", "1.snapshot.json"):
                (metadata / name).write_text("{}\n", encoding="utf-8")
            (metadata / "1.models.json").write_text(
                json.dumps({"signed": {"targets": targets}}),
                encoding="utf-8",
            )
            (metadata / "timestamp.json").write_text("{}\n", encoding="utf-8")

            self.assertEqual(
                validate_model_assets(resources, allow_model_assets=True), []
            )
            bundle.write_bytes(b"tampered model bundle")
            errors = validate_model_assets(resources, allow_model_assets=True)
            self.assertTrue(
                any("bundle SHA-256 does not match" in error for error in errors)
            )
            self.assertTrue(
                any("bundle target SHA-256 does not match" in error for error in errors)
            )

    def test_package_fingerprint_covers_security_and_exact_python_closure(self):
        research_source = resolve_research_source(ROOT)
        baseline = package_input_fingerprint(ROOT, research_source)
        with tempfile.TemporaryDirectory() as directory:
            copied_research = Path(directory) / "research"
            copied_research.mkdir()
            for name in RESEARCH_RESOURCES:
                (copied_research / name).write_bytes(
                    (research_source / name).read_bytes()
                )
            for name in ("pyproject.toml", "uv.lock"):
                (copied_research / name).write_bytes(
                    (research_source / name).read_bytes()
                )
            copied_root = Path(directory) / "studio"
            copied_root.mkdir()
            for relative in (
                "Cargo.toml",
                "Cargo.lock",
                "PRD.md",
                "scripts/macos_packaging.py",
                "scripts/package_macos.py",
            ):
                target = copied_root / relative
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes((ROOT / relative).read_bytes())
            for name in ENGINE_RESOURCES:
                target = copied_root / "engine" / name
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes((ROOT / "engine" / name).read_bytes())
            packaging = copied_root / "packaging/macos"
            packaging.mkdir(parents=True)
            for path in (ROOT / "packaging/macos").iterdir():
                if path.is_file():
                    (packaging / path.name).write_bytes(path.read_bytes())

            first = package_input_fingerprint(copied_root, copied_research)
            self.assertRegex(baseline, r"^[0-9a-f]{64}$")
            (copied_root / "engine/model_bundle.py").write_bytes(b"# changed\n")
            self.assertNotEqual(
                package_input_fingerprint(copied_root, copied_research), first
            )
            (copied_root / "engine/model_bundle.py").write_bytes(
                (ROOT / "engine/model_bundle.py").read_bytes()
            )
            (packaging / "SBOM.spdx.json").write_bytes(b"{}\n")
            self.assertNotEqual(
                package_input_fingerprint(copied_root, copied_research), first
            )

    def test_release_manifest_is_complete_relative_and_checksums_every_file(self):
        with tempfile.TemporaryDirectory() as directory:
            contents = Path(directory) / "Reyn Studio.app/Contents"
            executable = contents / "MacOS/reyn-studio"
            resource = contents / "Resources/engine/reyn_engine.py"
            executable.parent.mkdir(parents=True)
            resource.parent.mkdir(parents=True)
            executable.write_bytes(b"binary")
            resource.write_text("# sidecar\n", encoding="utf-8")
            manifest_path = contents / "Resources/release-manifest.json"
            write_json(
                manifest_path,
                {"files": file_manifest(contents, [executable, resource])},
            )
            self.assertEqual(
                validate_release_manifest(contents, manifest_path), []
            )
            (contents / "Resources/unlisted.txt").write_text(
                "missing from manifest", encoding="utf-8"
            )
            self.assertIn(
                "release manifest omits Resources/unlisted.txt",
                validate_release_manifest(contents, manifest_path),
            )

    def test_bundle_metadata_and_resources_reject_developer_home_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            bundle = Path(directory) / "Reyn Studio.app"
            contents = bundle / "Contents"
            resources = contents / "Resources"
            resources.mkdir(parents=True)
            with (contents / "Info.plist").open("wb") as stream:
                plistlib.dump(info_plist(self.config, "1"), stream)
            write_json(resources / "runtime-requirements.json", runtime_requirements())
            self.assertEqual(developer_path_leaks(bundle), [])
            write_json(
                resources / "release-manifest.json",
                {"source": "/Users/developer/private/reyn-studio"},
            )
            leaks = developer_path_leaks(bundle)
            self.assertEqual(len(leaks), 1)
            self.assertIn("Resources/release-manifest.json", leaks[0])

    def test_path_scanner_checks_binary_and_all_staged_files(self):
        with tempfile.TemporaryDirectory() as directory:
            bundle = Path(directory) / "Reyn Studio.app"
            contents = bundle / "Contents"
            executable = contents / "MacOS/reyn-studio"
            opaque_resource = contents / "Resources/runtime.dat"
            executable.parent.mkdir(parents=True)
            opaque_resource.parent.mkdir(parents=True)
            executable.write_bytes(
                b"/System/Library/Frameworks/Metal.framework\x00"
                b"/usr/lib/libSystem.B.dylib\x00"
                b"file:///Users/release-builder/work/reyn-studio/PRD.md\x00"
                b"file://./PRD.md\x00"
                b"/Users/release-builder/.cargo/registry/src/crate/lib.rs\x00"
                b"/opt/ci/build/reyn-studio/src/app.rs\x00"
            )
            opaque_resource.write_bytes(
                b"portable\x00/home/linux-builder/project/private.txt\x00"
                b"/private/var/folders/ab/cd/T/rustc-build\x00"
                b"/opt/cargo/registry/src/crate/lib.rs\x00"
            )

            leaks = developer_path_leaks(
                bundle,
                forbidden_roots=(Path("/opt/ci/build/reyn-studio"),),
            )
            self.assertTrue(
                any("MacOS/reyn-studio: /Users/release-builder/" in leak for leak in leaks)
            )
            self.assertTrue(
                any("MacOS/reyn-studio: file://./PRD.md" in leak for leak in leaks)
            )
            self.assertTrue(
                any(
                    "MacOS/reyn-studio: /opt/ci/build/reyn-studio" in leak
                    for leak in leaks
                )
            )
            self.assertTrue(
                any("Resources/runtime.dat: /home/linux-builder/" in leak for leak in leaks)
            )
            self.assertTrue(
                any(
                    "Resources/runtime.dat: /private/var/folders/ab/" in leak
                    for leak in leaks
                )
            )
            self.assertTrue(
                any(
                    "Resources/runtime.dat: /opt/cargo/registry/" in leak
                    for leak in leaks
                )
            )
            self.assertFalse(any("/System/Library/" in leak for leak in leaks))
            self.assertFalse(any("/usr/lib/" in leak for leak in leaks))

    def test_bundle_validator_reports_every_missing_runtime_resource(self):
        with tempfile.TemporaryDirectory() as directory:
            bundle = Path(directory) / "Reyn Studio.app"
            contents = bundle / "Contents"
            contents.mkdir(parents=True)
            with (contents / "Info.plist").open("wb") as stream:
                plistlib.dump(info_plist(self.config, "1"), stream)
            checks = validate_bundle(bundle, self.config)
            resource_check = next(
                check for check in checks if check.name == "resources"
            )
            self.assertEqual(resource_check.level, "FAIL")
            self.assertIn("LICENSE", resource_check.detail)
            self.assertIn("engine/reyn_engine.py", resource_check.detail)
            self.assertIn("research/time_moe_operator.py", resource_check.detail)
            manifest_check = next(
                check for check in checks if check.name == "release manifest"
            )
            self.assertEqual(manifest_check.level, "FAIL")

    def test_complete_staged_bundle_passes_portability_validation(self):
        with tempfile.TemporaryDirectory() as directory:
            bundle = Path(directory) / "Reyn Studio.app"
            contents = bundle / "Contents"
            executable = contents / "MacOS/reyn-studio"
            resources = contents / "Resources"
            executable.parent.mkdir(parents=True)
            resources.mkdir(parents=True)
            executable.write_bytes(b"fixture Mach-O")
            executable.chmod(0o755)
            with (contents / "Info.plist").open("wb") as stream:
                plistlib.dump(info_plist(self.config, "1"), stream)
            (resources / "ReynStudio.icns").write_bytes(b"icns fixture")
            (resources / "LICENSE").write_bytes((ROOT / "LICENSE").read_bytes())
            (resources / "NOTICE").write_bytes((ROOT / "NOTICE").read_bytes())
            copy_documentation_resources(ROOT, resources / "docs")
            copy_engine_resources(ROOT, resources / "engine")
            copy_research_resources(
                resolve_research_source(ROOT), resources / "research"
            )
            copy_security_resources(ROOT, resources / "security")
            write_json(
                resources / "runtime-requirements.json", runtime_requirements()
            )
            runtime_source = Path(directory) / "runtime-source"
            runtime_python = runtime_source / "bin/python3.14"
            runtime_python.parent.mkdir(parents=True)
            runtime_python.write_text("#!/bin/sh\n", encoding="utf-8")
            runtime_python.chmod(0o755)
            runtime_destination = contents / "Resources/ReynPython"
            observed = {
                "architecture": "arm64",
                "Python": RUNTIME_DEPENDENCIES["Python"][0],
                "distributions": {
                    name: version
                    for name, (version, _license) in RUNTIME_DEPENDENCIES.items()
                    if name != "Python"
                },
                "prefix": str(runtime_destination.resolve()),
            }
            with (
                patch(
                    "macos_packaging.executable_architectures",
                    return_value=("arm64",),
                ),
                patch(
                    "macos_packaging.run",
                    return_value=subprocess.CompletedProcess(
                        ["python"], 0, stdout=json.dumps(observed)
                    ),
                ),
            ):
                stage_factory_runtime(
                    runtime_source,
                    runtime_destination,
                    resources=resources,
                    source_revision="a" * 40,
                    build_epoch=315532800,
                    compliance_root=ROOT / "packaging/macos",
                )
            manifest_path = resources / "release-manifest.json"
            manifest_files = [
                path for path in contents.rglob("*") if path.is_file()
            ]
            write_json(
                manifest_path,
                {
                    "schema": "com.reyn.studio.local-package.v2",
                    "rust_target": "aarch64-apple-darwin",
                    "architectures": ["arm64"],
                    "architecture_slices": [
                        {
                            "architecture": "arm64",
                            "rust_target": "aarch64-apple-darwin",
                            "bytes": executable.stat().st_size,
                            "source_binary_sha256": "a" * 64,
                        }
                    ],
                    "resource_set": resource_metadata(resources),
                    "files": file_manifest(contents, manifest_files),
                },
            )

            with (
                patch(
                    "macos_packaging.executable_architectures",
                    return_value=("arm64",),
                ),
                patch(
                    "macos_packaging.require_executable_architectures",
                    return_value=("arm64",),
                ),
                patch(
                    "macos_packaging.executable_minimum_macos",
                    return_value=self.config.minimum_system_version,
                ),
                patch("macos_packaging.linked_libraries", return_value=[]),
                patch("macos_packaging.signing_state", return_value="ad-hoc"),
                patch(
                    "macos_packaging.run",
                    return_value=subprocess.CompletedProcess(
                        ["python"], 0, stdout=json.dumps(observed)
                    ),
                ),
            ):
                checks = validate_bundle(
                    bundle,
                    self.config,
                    expected_architectures=("arm64",),
                )
            self.assertEqual(
                [check for check in checks if check.level == "FAIL"], []
            )
            self.assertEqual(
                next(
                    check
                    for check in checks
                    if check.name == "portable staged content"
                ).level,
                "PASS",
            )

    def test_resource_metadata_is_target_neutral_and_detects_mutation(self):
        with tempfile.TemporaryDirectory() as directory:
            resources = Path(directory)
            (resources / "engine").mkdir()
            (resources / "engine/reyn_engine.py").write_text(
                "# engine\n", encoding="utf-8"
            )
            metadata = resource_metadata(resources)
            self.assertEqual(validate_resource_metadata(resources, metadata), [])
            (resources / "engine/reyn_engine.py").write_text(
                "# changed\n", encoding="utf-8"
            )
            errors = validate_resource_metadata(resources, metadata)
            self.assertIn(
                "release manifest resource_set.files differs from staged resources",
                errors,
            )
            self.assertIn(
                "release manifest resource_set.sha256 differs from staged resources",
                errors,
            )

    def test_checksum_index_is_sorted_and_reproducible(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            arm = root / "Reyn-arm64.zip"
            intel = root / "Reyn-x86_64.zip"
            arm.write_bytes(b"arm")
            intel.write_bytes(b"intel")
            destination = root / "SHA256SUMS"
            write_sha256sums([intel, arm], destination)
            first = destination.read_bytes()
            write_sha256sums([arm, intel], destination)
            self.assertEqual(destination.read_bytes(), first)
            names = [
                line.split("  ", 1)[1]
                for line in destination.read_text(encoding="utf-8").splitlines()
            ]
            self.assertEqual(names, sorted(names))

    def test_source_icon_is_rgba_1024(self):
        icon = ROOT / "packaging/macos/ReynStudio-1024.png"
        data = icon.read_bytes()
        self.assertEqual(data[:8], b"\x89PNG\r\n\x1a\n")
        width, height, bit_depth, color_type = struct.unpack(">IIBB", data[16:26])
        self.assertEqual((width, height, bit_depth, color_type), (1024, 1024, 8, 6))
        icns = (ROOT / "packaging/macos/ReynStudio.icns").read_bytes()
        self.assertEqual(icns[:4], b"icns")
        self.assertEqual(struct.unpack(">I", icns[4:8])[0], len(icns))

    def test_archive_is_deterministic_and_preserves_executable_mode(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            app = root / "Reyn Studio.app"
            executable = app / "Contents/MacOS/reyn-studio"
            executable.parent.mkdir(parents=True)
            executable.write_bytes(b"fixture-binary")
            executable.chmod(0o755)
            interpreter_link = app / "Contents/Resources/ReynPython/bin/python3"
            interpreter_link.parent.mkdir(parents=True)
            interpreter_link.symlink_to("python3.14")
            (app / "Contents/Info.plist").write_bytes(b"fixture-plist")

            first = root / "first.zip"
            second = root / "second.zip"
            deterministic_zip(app, first, DEFAULT_SOURCE_DATE_EPOCH)
            deterministic_zip(app, second, DEFAULT_SOURCE_DATE_EPOCH)
            self.assertEqual(
                hashlib.sha256(first.read_bytes()).digest(),
                hashlib.sha256(second.read_bytes()).digest(),
            )
            with zipfile.ZipFile(first) as archive:
                info = archive.getinfo(
                    "Reyn Studio.app/Contents/MacOS/reyn-studio"
                )
                mode = info.external_attr >> 16
                self.assertTrue(mode & stat.S_IXUSR)
                link = archive.getinfo(
                    "Reyn Studio.app/Contents/Resources/ReynPython/bin/python3"
                )
                self.assertTrue(stat.S_ISLNK(link.external_attr >> 16))
                self.assertEqual(archive.read(link), b"python3.14")


if __name__ == "__main__":
    unittest.main()
