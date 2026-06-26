#!/usr/bin/env bash
# Build the Linux AppImage — one double-clickable file that wraps the release
# binary. Same source tree as the Windows .exe and the plain Linux binary; this
# script just bundles the Linux binary + icon + metadata into an AppImage.
#
# Runs on Ubuntu 24.04+ (the glibc floor is set by THIS build host's glibc).
# Re-run anytime — the packaging tools are cached in build/tools after the first
# download, so only the first run hits the network.
#
#   ./build-appimage.sh
#
set -euo pipefail
cd "$(dirname "$0")"

# Run the packaging tools (themselves AppImages) by extraction, so the build
# never depends on the host having FUSE set up.
export APPIMAGE_EXTRACT_AND_RUN=1
export ARCH=x86_64

BIN="target/release/sapphire-launcher"
APPDIR="build/AppDir"
TOOLS="build/tools"
ICON="icon-512.png"
DESKTOP="build/sapphire-launcher.desktop"

# 1) Compile the release binary (the one and only source tree).
echo "==> cargo build --release"
cargo build --release

# 2) Fetch the packaging tools once (cached in build/tools).
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

# 3) Desktop metadata baked into the AppImage (appimagetool requires a .desktop
#    + matching icon). This is separate from the runtime menu entry the launcher
#    self-installs; keep Name/StartupWMClass identical so they collapse to one.
mkdir -p build
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

# 4) Fresh AppDir, then let linuxdeploy assemble it (binary + deps via ldd, which
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

echo "==> done: $(ls -1 Sapphire_Launcher-*.AppImage | tail -1)"
