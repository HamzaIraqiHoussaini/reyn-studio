#!/usr/bin/env python3
"""pvpython-side checks for Reyn Studio VTK field exports.

This script is intended to run under ParaView's pvpython, not system Python.
It opens the golden (or supplied) legacy VTK StructuredGrid and verifies
dimensions, point arrays, and selected FieldData provenance keys.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path


def decode_byte_field(dataset, name: str) -> str:
    field_data = dataset.GetFieldData()
    array = field_data.GetArray(name)
    if array is None:
        array = field_data.GetAbstractArray(name)
    if array is None:
        raise AssertionError(f"missing FieldData array {name!r}")
    values = [int(array.GetValue(index)) for index in range(array.GetNumberOfTuples())]
    return bytes(values).decode("utf-8")


def version_tuple() -> tuple[int, int, int]:
    try:
        from paraview.modules.vtkRemotingCore import vtkProcessModule

        major = int(vtkProcessModule.GetProcessModuleVersionMajor())
        minor = int(vtkProcessModule.GetProcessModuleVersionMinor())
        patch = int(vtkProcessModule.GetProcessModuleVersionPatch())
        return major, minor, patch
    except Exception:
        pass
    try:
        import paraview

        version = getattr(paraview, "__version__", None) or getattr(
            paraview, "version", None
        )
        if isinstance(version, str) and version.count(".") >= 1:
            parts = [int(part) for part in version.split(".")[:3]]
            while len(parts) < 3:
                parts.append(0)
            return parts[0], parts[1], parts[2]
    except Exception:
        pass
    raise RuntimeError("could not determine ParaView version from pvpython")


def vtk_version_string() -> str:
    try:
        from vtkmodules.vtkCommonCore import vtkVersion

        return str(vtkVersion.GetVTKVersion())
    except Exception:
        import vtk

        return str(vtk.vtkVersion.GetVTKVersion())


def open_dataset(path: Path):
    from paraview.simple import LegacyVTKReader, UpdatePipeline, servermanager

    reader = LegacyVTKReader(FileNames=[str(path)])
    UpdatePipeline()
    client = reader.GetClientSideObject()
    if client is None:
        raise RuntimeError("ParaView reader has no client-side object")
    dataset = client.GetOutputDataObject(0)
    if dataset is None:
        # Fall back through the proxy output port.
        dataset = servermanager.Fetch(reader)
    if dataset is None:
        raise RuntimeError(f"ParaView failed to materialize dataset from {path}")
    return reader, dataset


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--fixture", type=Path, required=True)
    parser.add_argument("--pin", type=Path, required=True)
    parser.add_argument("--allow-version-mismatch", action="store_true")
    args = parser.parse_args()

    pin = json.loads(args.pin.read_text(encoding="utf-8"))
    expected = pin["expected"]
    pinned = tuple(int(part) for part in str(pin["paraview_version"]).split("."))
    actual = version_tuple()
    vtk_version = vtk_version_string()
    print(f"paraview_version={'.'.join(str(part) for part in actual)}")
    print(f"vtk_version={vtk_version}")

    if actual != pinned and not args.allow_version_mismatch:
        raise SystemExit(
            f"ParaView version {actual[0]}.{actual[1]}.{actual[2]} does not match "
            f"pin {pin['paraview_version']}; install the pinned build or pass "
            f"--allow-version-mismatch for local iteration only"
        )
    if not vtk_version.startswith(str(pin["vtk_version_prefix"])):
        if args.allow_version_mismatch:
            print(
                f"WARN: VTK {vtk_version} does not start with pinned prefix "
                f"{pin['vtk_version_prefix']}"
            )
        else:
            raise SystemExit(
                f"VTK version {vtk_version} does not start with pinned prefix "
                f"{pin['vtk_version_prefix']}"
            )

    _reader, dataset = open_dataset(args.fixture.resolve())
    dims = [int(dataset.GetDimensions()[axis]) for axis in range(3)]
    if dims != list(expected["dimensions"]):
        raise SystemExit(f"dimensions {dims} != {expected['dimensions']}")
    point_count = int(dataset.GetNumberOfPoints())
    if point_count != int(expected["point_count"]):
        raise SystemExit(f"point count {point_count} != {expected['point_count']}")

    point_data = dataset.GetPointData()
    for name in expected["point_arrays"]:
        array = point_data.GetArray(name)
        if array is None:
            array = point_data.GetAbstractArray(name)
        if array is None:
            raise SystemExit(f"missing point array {name!r}")
        if int(array.GetNumberOfTuples()) != point_count:
            raise SystemExit(
                f"point array {name!r} has {array.GetNumberOfTuples()} tuples; "
                f"expected {point_count}"
            )

    for key in expected["field_data_keys"]:
        value = decode_byte_field(dataset, key)
        if key in expected and isinstance(expected[key], str) and value != expected[key]:
            raise SystemExit(f"FieldData {key}={value!r} != {expected[key]!r}")
        print(f"field_data.{key}={value}")

    print("PASS: ParaView opened Reyn VTK StructuredGrid with expected arrays")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except SystemExit:
        raise
    except Exception as error:  # pragma: no cover - pvpython diagnostics
        print(f"FAIL: {error}", file=sys.stderr)
        raise SystemExit(1)
