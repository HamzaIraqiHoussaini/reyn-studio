#!/usr/bin/env python3
"""Verify the staged Linux engine reaches READY on loopback."""

from __future__ import annotations

import argparse
import json
import queue
import socket
import subprocess
import threading
from pathlib import Path


def read_line(stream: object, output: queue.Queue[str]) -> None:
    output.put(stream.readline())


def resolve_python(stage: Path) -> Path:
    candidates = (
        stage / "ReynPython/bin/python3.14",
        stage / "ReynPython/bin/python3",
        stage / "ReynPython/bin/python",
    )
    for candidate in candidates:
        if candidate.is_file():
            return candidate
    raise RuntimeError(f"bundled Python binary missing under {stage / 'ReynPython/bin'}")


def smoke(stage: Path) -> None:
    python = resolve_python(stage)
    engine = stage / "resources/engine/reyn_engine.py"
    research = stage / "resources/research"
    process = subprocess.Popen(
        [
            str(python),
            "-B",
            "-u",
            str(engine),
            "--research-dir",
            str(research),
            "--device",
            "cpu",
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert process.stdout is not None
    assert process.stderr is not None
    output: queue.Queue[str] = queue.Queue(maxsize=1)
    threading.Thread(
        target=read_line,
        args=(process.stdout, output),
        daemon=True,
    ).start()
    try:
        try:
            line = output.get(timeout=30)
        except queue.Empty as error:
            raise RuntimeError("engine did not emit READY within 30 seconds") from error
        if not line.startswith("READY "):
            detail = process.stderr.read(64 * 1024)
            raise RuntimeError(f"engine returned {line.strip()!r}; stderr: {detail}")
        ready = json.loads(line.removeprefix("READY "))
        if ready.get("error"):
            raise RuntimeError(f"engine readiness failed: {ready['error']}")
        if ready.get("device") != "cpu":
            raise RuntimeError(f"engine selected unexpected device: {ready.get('device')}")
        port = ready.get("port")
        if not isinstance(port, int) or not 1 <= port <= 65535:
            raise RuntimeError(f"engine returned invalid port: {port!r}")
        with socket.create_connection(("127.0.0.1", port), timeout=5):
            pass
        print(f"PASS: bundled engine READY on loopback port {port} using CPU")
    finally:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
        if process.poll() is None:
            raise RuntimeError("engine process could not be reaped")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("stage", type=Path)
    args = parser.parse_args()
    smoke(args.stage.absolute())
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
