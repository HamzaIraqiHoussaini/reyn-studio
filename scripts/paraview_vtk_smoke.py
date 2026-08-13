#!/usr/bin/env python3
"""Version-pinned ParaView smoke for Reyn Studio VTK field exports.

Always validates:
  - pin.json schema / expected contract
  - golden fixture ASCII STRUCTURED_GRID shape and provenance keys

When ParaView's pvpython is available (or --require-paraview), also opens the
fixture under the pinned ParaView build and checks arrays via the real reader.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SMOKE_DIR = ROOT / "docs" / "qa" / "paraview-vtk-smoke"
PIN_PATH = SMOKE_DIR / "pin.json"
CHECK_SCRIPT = Path(__file__).resolve().with_name("paraview_vtk_smoke_check.py")
SCHEMA = "com.reyn.studio.paraview-vtk-smoke/1"


def load_pin() -> dict:
    pin = json.loads(PIN_PATH.read_text(encoding="utf-8"))
    if pin.get("schema") != SCHEMA:
        raise SystemExit(f"unexpected pin schema {pin.get('schema')!r}; want {SCHEMA}")
    for key in ("paraview_version", "vtk_version_prefix", "fixture", "expected"):
        if key not in pin:
            raise SystemExit(f"pin.json missing {key!r}")
    return pin


def decode_byte_field_ascii(lines: list[str], name: str) -> str:
    header_prefix = f"{name} 1 "
    for index, line in enumerate(lines):
        if not line.startswith(header_prefix):
            continue
        parts = line.split()
        if len(parts) < 4 or parts[3] != "unsigned_char":
            raise SystemExit(f"FieldData {name} is not unsigned_char bytes")
        count = int(parts[2])
        values: list[int] = []
        cursor = index + 1
        while len(values) < count and cursor < len(lines):
            values.extend(int(token) for token in lines[cursor].split())
            cursor += 1
        if len(values) < count:
            raise SystemExit(f"FieldData {name} truncated ({len(values)} < {count})")
        return bytes(values[:count]).decode("utf-8")
    raise SystemExit(f"missing FieldData key {name!r}")


def validate_ascii_fixture(fixture: Path, expected: dict) -> None:
    text = fixture.read_text(encoding="utf-8")
    lines = text.splitlines()
    if not lines or not lines[0].startswith("# vtk DataFile Version"):
        raise SystemExit("fixture is not a legacy VTK data file")
    version = lines[0].removeprefix("# vtk DataFile Version ").strip()
    if version != expected["vtk_data_file_version"]:
        raise SystemExit(
            f"VTK data file version {version!r} != {expected['vtk_data_file_version']!r}"
        )
    if "DATASET STRUCTURED_GRID" not in lines:
        raise SystemExit("fixture is not DATASET STRUCTURED_GRID")
    dims = expected["dimensions"]
    dim_line = f"DIMENSIONS {dims[0]} {dims[1]} {dims[2]}"
    if dim_line not in lines:
        raise SystemExit(f"missing {dim_line}")
    points_line = f"POINTS {expected['point_count']} double"
    if points_line not in lines:
        raise SystemExit(f"missing {points_line}")
    if f"POINT_DATA {expected['point_count']}" not in lines:
        raise SystemExit("missing POINT_DATA header")
    for name in expected["point_arrays"]:
        if name == "velocity_m_per_s":
            needle = "VECTORS velocity_m_per_s double"
        elif name == "traction_pa":
            needle = "VECTORS traction_pa double"
        else:
            needle = f"SCALARS {name} float 1"
        if needle not in lines:
            raise SystemExit(f"missing point array header {needle!r}")
    for key in expected["field_data_keys"]:
        value = decode_byte_field_ascii(lines, key)
        if key in expected and isinstance(expected[key], str) and value != expected[key]:
            raise SystemExit(f"FieldData {key}={value!r} != {expected[key]!r}")
    print(
        f"PASS: ASCII fixture {fixture.name} is STRUCTURED_GRID "
        f"{dims[0]}³ with provenance FieldData"
    )


def candidate_pvpythons(explicit: str | None) -> list[Path]:
    found: list[Path] = []
    if explicit:
        found.append(Path(explicit).expanduser())
    env = os.environ.get("REYN_PVPYTHON")
    if env:
        found.append(Path(env).expanduser())
    which = shutil.which("pvpython")
    if which:
        found.append(Path(which))
    applications = Path("/Applications")
    if applications.is_dir():
        for app in sorted(applications.glob("ParaView*.app")):
            candidate = app / "Contents" / "bin" / "pvpython"
            if candidate.is_file():
                found.append(candidate)
    # Deduplicate while preserving order.
    unique: list[Path] = []
    seen: set[Path] = set()
    for path in found:
        resolved = path if path.is_absolute() else path.resolve()
        if resolved in seen:
            continue
        seen.add(resolved)
        unique.append(resolved)
    return unique


def run_paraview_check(
    pvpython: Path,
    fixture: Path,
    allow_version_mismatch: bool,
) -> None:
    if not pvpython.is_file():
        raise SystemExit(f"pvpython not found: {pvpython}")
    if not CHECK_SCRIPT.is_file():
        raise SystemExit(f"missing check script: {CHECK_SCRIPT}")
    command = [
        str(pvpython),
        str(CHECK_SCRIPT),
        "--fixture",
        str(fixture),
        "--pin",
        str(PIN_PATH),
    ]
    if allow_version_mismatch:
        command.append("--allow-version-mismatch")
    print(f"running: {' '.join(command)}")
    completed = subprocess.run(command, check=False, text=True, capture_output=True)
    if completed.stdout:
        sys.stdout.write(completed.stdout)
        if not completed.stdout.endswith("\n"):
            sys.stdout.write("\n")
    if completed.returncode != 0:
        detail = completed.stderr.strip() or completed.stdout.strip() or "no output"
        raise SystemExit(f"ParaView smoke failed ({completed.returncode}): {detail}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--require-paraview",
        action="store_true",
        help="fail if the pinned ParaView pvpython is unavailable",
    )
    parser.add_argument(
        "--allow-version-mismatch",
        action="store_true",
        help="local-only: accept a non-pinned ParaView/VTK version",
    )
    parser.add_argument(
        "--pvpython",
        help="explicit path to pvpython (otherwise REYN_PVPYTHON, PATH, /Applications)",
    )
    args = parser.parse_args()

    pin = load_pin()
    fixture = SMOKE_DIR / pin["fixture"]
    if not fixture.is_file():
        raise SystemExit(
            f"missing golden fixture {fixture}; regenerate with "
            f"{pin.get('regenerate', 'REYN_WRITE_PARAVIEW_VTK_FIXTURE=1 cargo test')}"
        )

    print(f"pin: ParaView {pin['paraview_version']} / VTK {pin['vtk_version_prefix']}.*")
    validate_ascii_fixture(fixture, pin["expected"])

    candidates = candidate_pvpythons(args.pvpython)
    selected = next((path for path in candidates if path.is_file()), None)
    if selected is None:
        message = (
            "ParaView pvpython not found; install ParaView "
            f"{pin['paraview_version']} or pass --pvpython"
        )
        if args.require_paraview:
            raise SystemExit(message)
        print(f"SKIP: {message}")
        print("PASS: structural VTK smoke (ParaView reader stage skipped)")
        return 0

    run_paraview_check(selected, fixture, args.allow_version_mismatch)
    print(f"PASS: ParaView smoke via {selected}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
