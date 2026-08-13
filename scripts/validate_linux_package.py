#!/usr/bin/env python3
"""Validate an assembled Reyn Studio Linux portable directory."""

from __future__ import annotations

import argparse
from pathlib import Path

from linux_packaging import validate_stage


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("stage", type=Path)
    parser.add_argument("--runtime-smoke", action="store_true")
    args = parser.parse_args()
    errors = validate_stage(args.stage.resolve(), args.runtime_smoke)
    if errors:
        for error in errors:
            print(f"FAIL: {error}")
        return 1
    print("PASS: Linux portable package structure")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
