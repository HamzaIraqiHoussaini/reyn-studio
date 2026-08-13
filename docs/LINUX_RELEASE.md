# Reyn Studio Ubuntu 24.04 x86_64 preview

## Status

Linux support is in development. The current target is an Ubuntu 24.04 x86_64
AppImage with the same staged payload also published as a portable `.tar.xz`
and a thin `.deb` under `/opt/reyn-studio`. The package bundles CPython, NumPy,
and a CPU-only PyTorch runtime. CUDA and ROCm are not supported or qualified.

The package must remain labeled **"Linux preview pending verification"** until
the Ubuntu CI artifact passes the clean-machine matrix below. A successful
macOS or Windows build does not establish Linux support. Cross-building the
full AppImage from macOS is out of scope; build and package on Ubuntu 24.04
x86_64 (CI runner or VM).

## Package layout

```text
Reyn-Studio-<version>-linux-x86_64/
  reyn-studio
  ReynStudio.png
  LICENSE
  NOTICE
  ReynPython/
    bin/python3.14
  resources/
    engine/
    research/
    docs/
  THIRD_PARTY_NOTICES.md
  SBOM.spdx.json
  dependency-closure.json
  release-manifest.json
  resource-inventory.json
```

Primary published artifact:

```text
Reyn-Studio-<version>-linux-x86_64.AppImage
```

Also produced from the same stage:

```text
Reyn-Studio-<version>-linux-x86_64.tar.xz
reyn-studio_<version>_amd64.deb
```

The executable resolves resources from `resources/` beside `reyn-studio`.
The factory runtime is `ReynPython/` beside the executable. Managed runtime
state uses `$XDG_DATA_HOME/reyn-studio/runtime` (default
`~/.local/share/reyn-studio/runtime`). Settings use
`$XDG_CONFIG_HOME/reyn-studio/settings.json` (default
`~/.config/reyn-studio/settings.json`).

Release inputs are pinned in `packaging/linux/release-pins.json`.
`packaging/linux/python-runtime.lock` pins the complete manylinux x86_64 Python
closure with artifact hashes. The research checkout must match the recorded
commit exactly.

## Host requirements (Ubuntu 24.04 preview)

- `mesa-vulkan-drivers` (or another working Vulkan ICD) for the wgpu viewport
- `libxkbcommon-x11-0` (and typical GTK/X11 runtime libs) for the desktop window
- A Freedesktop Secret Service provider (for example GNOME Keyring / KWallet via
  `libsecret`) for YC session credential save/load — fails closed if missing
- `libsecret-1-0` is listed as a `.deb` dependency; AppImage / portable users
  should install the same host packages

## Build commands

On an Ubuntu 24.04 x86_64 builder:

```bash
sudo apt-get update
sudo apt-get install -y build-essential pkg-config libssl-dev \
  libgtk-3-dev libxkbcommon-dev libwayland-dev libvulkan-dev \
  mesa-vulkan-drivers libsecret-1-dev dpkg-dev xz-utils curl

rustup toolchain install 1.97.0 --profile minimal --component rustfmt \
  --target x86_64-unknown-linux-gnu
rustup default 1.97.0

export SOURCE_DATE_EPOCH=315532800
export REYN_ACCESS_REQUIRED=1
export REYN_ACCESS_ENDPOINT=https://reynflow.com/api/yc-access/v1/session
export REYN_RESEARCH_SOURCE_DIR=/path/to/reyn-research

cargo test --locked --all-targets
cargo check --locked --target x86_64-unknown-linux-gnu
cargo build --locked --release --target x86_64-unknown-linux-gnu

python3 -m unittest tests.test_linux_packaging -v

# Assemble pinned CPU runtime (uv)
uv python install 3.14.6
PYTHON=$(uv python find 3.14.6)
RUNTIME=$(mktemp -d)/ReynPython
SOURCE_ROOT=$(cd "$(dirname "$PYTHON")/.." && pwd)
mkdir -p "$RUNTIME"
tar -C "$SOURCE_ROOT" -cf - . | tar -C "$RUNTIME" -xf -
find "$RUNTIME" -type l -print0 | while IFS= read -r -d '' link; do
  target=$(readlink -f "$link")
  rm -f "$link"
  cp -a "$target" "$link"
done
# Ensure bin/python3.14 exists for the staged layout expected by the packager.
uv pip install --python "$RUNTIME/bin/python3.14" \
  --system --break-system-packages --require-hashes \
  --index-url https://download.pytorch.org/whl/cpu \
  --extra-index-url https://pypi.org/simple \
  --index-strategy unsafe-best-match \
  --requirements packaging/linux/python-runtime.lock

# Install appimagetool (required for the AppImage artifact)
curl -L -o /usr/local/bin/appimagetool \
  https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage
chmod +x /usr/local/bin/appimagetool

python3 scripts/package_linux.py \
  --runtime-dir "$RUNTIME" \
  --research-source-dir "$REYN_RESEARCH_SOURCE_DIR" \
  --binary target/x86_64-unknown-linux-gnu/release/reyn-studio \
  --runtime-smoke

python3 scripts/validate_linux_package.py \
  dist/linux/Reyn-Studio-*-linux-x86_64 \
  --runtime-smoke
python3 scripts/linux_engine_smoke.py \
  dist/linux/Reyn-Studio-*-linux-x86_64
```

The official YC artifact must be built with those two access variables. The
packager executes `reyn-studio --print-access-contract` and fails if the
actual binary does not require the exact HTTPS endpoint and legal-policy
versions. Credentials remain Worker secrets and are never build variables.

The runtime directory must be relocatable and must contain `bin/python3.14`,
NumPy 2.5.1, the CPU build of PyTorch 2.13.0, Cryptography 49.0.0,
Safetensors 0.8.0, secure-systems-lib 1.4.0, and python-tuf 6.0.0. The package
validator rejects a runtime that reports CUDA. It also imports the app-owned
`model_bundle.py` from the staged engine directory and exercises the real model
card and import rejection paths; file presence alone is not sufficient.

Regenerate the Python lock only when intentionally updating the runtime:

```bash
uv pip compile packaging/linux/python-runtime.in \
  --output-file packaging/linux/python-runtime.lock \
  --generate-hashes \
  --python-platform x86_64-manylinux_2_28 \
  --python-version 3.14 \
  --index-strategy unsafe-best-match
```

Packaging generates the SPDX SBOM, runtime CycloneDX SBOM, dependency closure,
and notices from locked Cargo metadata and installed Python distribution
metadata. Packaging fails if a dependency lacks required license or source
metadata, or if the staged Python closure differs from the hashed lock.

Use `--skip-appimage` or `--skip-deb` only for local dry-runs when the matching
tooling is unavailable. Official preview artifacts must include all three
outputs.

## Clean-machine acceptance matrix (Ubuntu 24.04 x86_64)

Until every row is green on a clean Ubuntu 24.04 machine that did not build the
package, keep the **Linux preview pending verification** label.

Automated subset (container-friendly) against published `dist/linux` artifacts:

```bash
docker run --rm --platform linux/amd64 \
  -v "$PWD/dist/linux:/artifacts" \
  -v "$PWD/scripts:/scripts:ro" \
  -v "$PWD/test-geometry/corpus/vendor:/fixtures:ro" \
  ubuntu:24.04 \
  bash /scripts/linux_clean_machine_matrix.sh
```

The script writes `dist/linux/CLEAN_MACHINE_MATRIX.md`.

| Check | Pass criteria |
| --- | --- |
| AppImage launch | `Reyn-Studio-*-linux-x86_64.AppImage` starts; YC access gate appears before the engine |
| Viewport | wgpu creates a Vulkan surface, or fails with a clear mesa-vulkan-drivers install hint |
| Bundled engine | `linux_engine_smoke.py` READY on loopback with CPU torch |
| Project I/O | Open/save project works under XDG paths |
| STEP / VTK | STEP import of staged vendor fixtures and VTK export smoke path succeed |
| Secret Service | YC credential save/load works only when Secret Service is available; fails closed otherwise |
| Portable tar.xz | Extracted tree launches equivalently to the AppImage payload |
| `.deb` | Installs under `/opt/reyn-studio` with a working `.desktop` entry |

## Explicit non-goals (this pass)

- CUDA / ROCm qualification
- macOS-style notarization analogue
- Cross-building the full AppImage from a Mac host
- Claiming Linux support from CI packaging alone without the clean-machine matrix
