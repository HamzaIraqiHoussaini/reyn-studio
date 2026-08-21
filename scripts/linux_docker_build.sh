#!/bin/bash
# Build Reyn Studio Linux x86_64 packages inside Ubuntu 24.04 (amd64).
set -euo pipefail

STUDIO_ROOT="${STUDIO_ROOT:-/work/pioneer/.worktrees/reyn-studio-step-import}"
RESEARCH_ROOT="${RESEARCH_ROOT:-/work/pioneer/.release/reyn-research-c9a9/reyn-research}"
SKIP_CARGO_BUILD="${SKIP_CARGO_BUILD:-0}"
SOURCE_REVISION="${SOURCE_REVISION:?SOURCE_REVISION must be set from the host}"
RESEARCH_REVISION="${RESEARCH_REVISION:?RESEARCH_REVISION must be set from the host}"
export SOURCE_DATE_EPOCH=315532800
export REYN_ACCESS_REQUIRED=1
export REYN_ACCESS_ENDPOINT=https://reynflow.com/api/yc-access/v1/session
export REYN_RESEARCH_SOURCE_DIR="$RESEARCH_ROOT"
export DEBIAN_FRONTEND=noninteractive
export CARGO_HOME="${CARGO_HOME:-/opt/cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-/opt/rustup}"
export PATH="/opt/cargo/bin:/root/.cargo/bin:/usr/local/bin:$PATH"
export APPIMAGE_EXTRACT_AND_RUN=1

cd "$STUDIO_ROOT"

actual_research=$(git -C "$RESEARCH_ROOT" rev-parse HEAD)
if [[ "$actual_research" != "$RESEARCH_REVISION" ]]; then
  echo "research checkout HEAD $actual_research does not match RESEARCH_REVISION $RESEARCH_REVISION" >&2
  exit 1
fi

echo "==> apt packages"
apt-get update -qq
apt-get install -y --no-install-recommends \
  build-essential pkg-config curl ca-certificates git xz-utils \
  libssl-dev libgtk-3-dev libxkbcommon-dev libxkbcommon-x11-0 \
  libwayland-dev libvulkan-dev mesa-vulkan-drivers libsecret-1-dev \
  dpkg-dev libasound2-dev python3 file squashfs-tools

echo "==> rustup 1.97.0"
if ! command -v rustc >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y \
    --default-toolchain 1.97.0 --profile minimal -c rustfmt \
    --target x86_64-unknown-linux-gnu
fi
# shellcheck disable=SC1091
source /opt/cargo/env 2>/dev/null || true
source "$HOME/.cargo/env" 2>/dev/null || true
export PATH="/opt/cargo/bin:$HOME/.cargo/bin:$PATH"
rustc --version

echo "==> uv"
if ! command -v uv >/dev/null 2>&1; then
  curl -LsSf https://astral.sh/uv/install.sh | sh
fi
export PATH="$HOME/.local/bin:$PATH"
uv --version

echo "==> appimagetool"
if [[ ! -x /usr/local/bin/appimagetool ]]; then
  curl -fsSL -o /usr/local/bin/appimagetool \
    https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage
  chmod +x /usr/local/bin/appimagetool
fi

# Resume packaging from an already-validated portable stage when present.
RESUME_STAGE="${RESUME_STAGE:-}"
if [[ -n "$RESUME_STAGE" && -d "$RESUME_STAGE" ]]; then
  echo "==> resuming from stage $RESUME_STAGE"
  version=$(python3 -c 'import pathlib; t=pathlib.Path("Cargo.toml").read_text();
print(next(line.split("\"")[1] for line in t.splitlines() if line.startswith("version = ")))')
  python3 - "$RESUME_STAGE" "$version" <<'PY'
import sys
from pathlib import Path
sys.path.insert(0, "scripts")
from linux_packaging import build_appimage, build_deb, sha256_file, write_sha256sums

stage = Path(sys.argv[1]).resolve()
version = sys.argv[2]
root = Path(".").resolve()
output = stage.parent
appimage = output / f"Reyn-Studio-{version}-linux-x86_64.AppImage"
deb = output / f"reyn-studio_{version}_amd64.deb"
archive = output / f"{stage.name}.tar.xz"
build_appimage(root, stage, appimage, version, appimagetool=Path("/usr/local/bin/appimagetool"))
build_deb(root, stage, deb, version)
checksum_inputs = [path for path in (archive, appimage, deb) if path.is_file()]
write_sha256sums(checksum_inputs, output / "SHA256SUMS")
print(f"AppImage: {appimage}")
print(f"Debian package: {deb}")
print(f"SHA-256: {sha256_file(archive) if archive.is_file() else 'n/a'}")
print("DONE")
PY
  exit 0
fi

BINARY="$STUDIO_ROOT/target/x86_64-unknown-linux-gnu/release/reyn-studio"
if [[ "$SKIP_CARGO_BUILD" != "1" || ! -x "$BINARY" ]]; then
  echo "==> cargo build (access-gated release)"
  cargo build --locked --release --target x86_64-unknown-linux-gnu --bin reyn-studio
else
  echo "==> reusing existing release binary"
fi
test -x "$BINARY"

echo "==> packaging unit tests"
python3 -m unittest tests.test_linux_packaging -v

echo "==> assemble ReynPython"
python_version=$(python3 -c 'import json; print(json.load(open("packaging/linux/release-pins.json"))["python"])')
uv python install "$python_version"
python_bin=$(uv python find "$python_version")
source_root=$(cd "$(dirname "$python_bin")/.." && pwd)
runtime=/tmp/ReynPython
rm -rf "$runtime"
mkdir -p "$runtime"
tar -C "$source_root" -cf - . | tar -C "$runtime" -xf -
find "$runtime" -type l -print0 | while IFS= read -r -d '' link; do
  target=$(readlink -f "$link" || true)
  if [[ -n "${target:-}" && -e "$target" ]]; then
    rm -f "$link"
    cp -a "$target" "$link"
  fi
done
test -x "$runtime/bin/python3.14"
uv pip install --python "$runtime/bin/python3.14" \
  --system --break-system-packages --require-hashes \
  --index-url https://download.pytorch.org/whl/cpu \
  --extra-index-url https://pypi.org/simple \
  --index-strategy unsafe-best-match \
  --requirements packaging/linux/python-runtime.lock
uv pip uninstall --python "$runtime/bin/python3.14" \
  --system --break-system-packages pip || true

echo "==> package_linux"
python3 scripts/package_linux.py \
  --runtime-dir "$runtime" \
  --research-source-dir "$RESEARCH_ROOT" \
  --binary "$BINARY" \
  --appimagetool /usr/local/bin/appimagetool \
  --source-revision "$SOURCE_REVISION" \
  --research-revision "$RESEARCH_REVISION" \
  --runtime-smoke

stage=$(find dist/linux -maxdepth 1 -type d -name 'Reyn-Studio-*-linux-x86_64' | head -n 1)
test -n "$stage"
python3 scripts/validate_linux_package.py "$stage" --runtime-smoke
python3 scripts/linux_engine_smoke.py "$stage"

echo "==> artifacts"
ls -lh dist/linux/
echo "DONE"
