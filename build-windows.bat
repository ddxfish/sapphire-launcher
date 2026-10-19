@echo off
rem Build the Windows release exe and drop it in dist\ next to the Linux outputs
rem (built by build-appimage.sh), so dist\ is the one folder to attach to a release.
rem Needs the Rust toolchain + MSVC build tools, same as plain `cargo build --release`.
cargo build --release || exit /b 1
if not exist dist mkdir dist
copy /y target\release\sapphire-launcher.exe dist\sapphire-launcher.exe >nul || exit /b 1
echo done: dist\sapphire-launcher.exe
