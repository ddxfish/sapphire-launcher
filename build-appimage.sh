#!/usr/bin/env bash
# Build the Linux release outputs into dist/: the AppImage (one double-clickable
# file wrapping the release binary) and the bare Linux binary it contains. Same
# source tree as the Windows .exe (build-windows.bat drops that into dist\ too), so
# dist/ is the one folder to attach to a GitHub release.
#
# The binary is compiled INSIDE an Ubuntu 22.04 container (needs Docker). Why:
# a binary's glibc floor is set by the machine that links it. Built on a newer
# box, the AppImage dies on Ubuntu 22.04 / Debian 12 with "version GLIBC_2.39
# not found" (that's how AppImageHub rejected it; they test on 22.04). Packaging
# (linuxdeploy) still runs on the host. Re-run anytime — the image, the cargo
# cache and the packaging tools are all cached after the first run.
#
#   ./build-appimage.sh
#
set -euo pipefail
cd "$(dirname "$0")"

# Run the packaging tools (themselves AppImages) by extraction, so the build
# never depends on the host having FUSE set up.
export APPIMAGE_EXTRACT_AND_RUN=1
export ARCH=x86_64

RUST_VERSION="1.93.0"                          # pin; bump together with the host toolchain
IMAGE="sapphire-launcher-build:ubuntu22.04-rust$RUST_VERSION"
GLIBC_MAX="2.35"                               # Ubuntu 22.04's glibc — the floor we promise
BIN="build/target/release/sapphire-launcher"   # container output (kept apart from target/)
DIST="dist"                                    # release outputs (gitignored)
APPDIR="build/AppDir"
TOOLS="build/tools"
ICON="icon-512.png"
DESKTOP="build/sapphire-launcher.desktop"

command -v docker >/dev/null 2>&1 || {
  echo "This script needs Docker: the binary is compiled in an Ubuntu 22.04 container." >&2
  exit 1
}

# 1) Build image: Ubuntu 22.04 + the pinned Rust toolchain. Built once, reused.
#    The Dockerfile is inline (stdin), so no build context is uploaded.
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  echo "==> building $IMAGE (one time)"
  docker build -t "$IMAGE" --build-arg RUST_VERSION="$RUST_VERSION" - <<'EOF'
FROM ubuntu:22.04
ARG RUST_VERSION
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl gcc libc6-dev binutils \
 && rm -rf /var/lib/apt/lists/*
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo
RUN curl -fsSL https://sh.rustup.rs \
      | sh -s -- -y --profile minimal --default-toolchain "$RUST_VERSION" --no-modify-path \
 && chmod -R a+rX /usr/local/rustup
# Put the real toolchain binaries on PATH (no rustup proxy) so any uid can run them.
ENV PATH=/usr/local/rustup/toolchains/${RUST_VERSION}-x86_64-unknown-linux-gnu/bin:$PATH
WORKDIR /src
EOF
fi

# 2) Compile in the container as the host user, so everything it writes under
#    build/ (cargo cache, target dir) is ours. --locked: build exactly Cargo.lock.
echo "==> cargo build --release (inside $IMAGE)"
mkdir -p build/cargo-home build/target
docker run --rm -u "$(id -u):$(id -g)" \
  -v "$PWD":/src -w /src \
  -e HOME=/tmp -e CARGO_HOME=/src/build/cargo-home -e CARGO_TARGET_DIR=/src/build/target \
  "$IMAGE" cargo build --release --locked

# 3) Guard: never package a binary that needs a newer glibc than we promise.
need=$(docker run --rm -v "$PWD":/src:ro "$IMAGE" \
  sh -c "objdump -T /src/$BIN | grep -oE 'GLIBC_[0-9.]+' | sort -Vu | tail -1")
if [ "$(printf '%s\n' "$need" "GLIBC_$GLIBC_MAX" | sort -V | tail -1)" != "GLIBC_$GLIBC_MAX" ]; then
  echo "ERROR: binary needs $need but the AppImage promises GLIBC_$GLIBC_MAX (Ubuntu 22.04)." >&2
  exit 1
fi
echo "==> glibc floor: $need (runs on Ubuntu 22.04+)"
mkdir -p "$DIST"
cp "$BIN" "$DIST/sapphire-launcher-linux-x86_64"

# 4) Fetch the packaging tools once (cached in build/tools).
mkdir -p "$TOOLS"
export PATH="$PWD/$TOOLS:$PATH"   # so linuxdeploy can find its appimage plugin
fetch() {  # <url> <dest>
  if [ ! -x "$2" ]; then
    echo "==> downloading $(basename "$2")"
    curl -fSL -o "$2" "$1"
    chmod +x "$2"
  fi
}
fetch https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage \
      "$TOOLS/linuxdeploy-x86_64.AppImage"
fetch https://github.com/linuxdeploy/linuxdeploy-plugin-appimage/releases/download/continuous/linuxdeploy-plugin-appimage-x86_64.AppImage \
      "$TOOLS/linuxdeploy-plugin-appimage-x86_64.AppImage"

# 5) Desktop metadata baked into the AppImage (appimagetool requires a .desktop
#    + matching icon). This is separate from the runtime menu entry the launcher
#    self-installs; keep Name/StartupWMClass identical so they collapse to one.
cat > "$DESKTOP" <<'EOF'
[Desktop Entry]
Type=Application
Name=Sapphire Launcher
Comment=Install and manage Sapphire
Exec=sapphire-launcher
Icon=sapphire-launcher
Terminal=false
Categories=Utility;
StartupWMClass=sapphire-launcher
EOF

# 6) Fresh AppDir, then let linuxdeploy assemble it (binary + deps via ldd, which
#    is near-empty for us) and the appimage plugin package it with the modern
#    fuse-free runtime.
rm -rf "$APPDIR"
mkdir -p "$APPDIR"
cp "$ICON" build/sapphire-launcher.png

echo "==> packaging AppImage"
# OUTPUT is the old var name, LDAI_OUTPUT the new one — set both so the script
# works across appimage-plugin versions.
OUTPUT="Sapphire_Launcher-x86_64.AppImage" \
LDAI_OUTPUT="Sapphire_Launcher-x86_64.AppImage" \
  "$TOOLS/linuxdeploy-x86_64.AppImage" \
    --appdir "$APPDIR" \
    -e "$BIN" \
    -d "$DESKTOP" \
    -i build/sapphire-launcher.png \
    --output appimage
mv -f Sapphire_Launcher-x86_64.AppImage "$DIST/"

echo "==> done, release outputs in $DIST/:"
ls -1 "$DIST"
