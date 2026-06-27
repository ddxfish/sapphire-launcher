# Sapphire Launcher

A GUI installer and manager for [Sapphire](https://github.com/ddxfish/sapphire) — the AI voice assistant. Built with Rust and [Iced](https://iced.rs).

Sapphire Launcher handles everything so you don't have to touch a terminal. One file, double-click, done. Windows and Linux.

<img width="869" height="657" alt="image" src="https://github.com/user-attachments/assets/406e8a2f-8364-4038-bea1-a8c0c390ff29" />


## What it does

- **Install** — Sets up everything Sapphire needs: Git, Miniconda, a dedicated Python environment, clones the repo, and installs all dependencies. Just click Go.
- **Update** — Pull the latest changes and update dependencies with one button. Handles dirty repos by auto-stashing your changes.
- **Launch** — Start Sapphire and see its live output in the Log tab. Open Browser button takes you straight to the web UI.
- **Branch Switching** — Switch between Stable (main) and Development (dev) branches from the dropdown. Auto-stops and restarts Sapphire if needed.
- **Autostart** — Optionally launch Sapphire when you log in. On Linux it can install Sapphire as a systemd user service with custom environment variables.
- **Troubleshoot** — Checks if Sapphire is responding, verifies package versions (catches the starlette issue), checks dependency health, scans optional voice features (STT/TTS/Wake Word), and detects your GPU. Fix buttons for common issues.
- **Uninstall** — Quick resets for password and API keys. Danger zone for removing the conda environment, user data, or the entire install.

## Features

- Single ~5 MB file, no installer needed
- Finds Git, Conda, and pip on disk automatically (no PATH dependency)
- Detects if Sapphire is already running on startup
- Update checker with badge indicator
- Persists your install path and branch between sessions
- Collapsible launcher log panel
- Copy Log and Open in Notepad buttons for support/debugging
- Two-click confirmation on all destructive actions
- No console window flashing
- Software-rendered UI (no GPU driver required)

## Requirements

- **Windows 10 (1809+) or Windows 11** — uses winget to install Git and Miniconda.
- **Linux (x86-64)** — the AppImage runs on a modern glibc (built on Ubuntu 24.04+). Uses your package manager / shipped tools for Git and Miniconda.
- That's it. The launcher installs everything else.

## Usage

1. Download `sapphire-launcher.exe` (Windows) or `Sapphire_Launcher-x86_64.AppImage` (Linux)
2. Run it (on Linux, `chmod +x` first if needed, then double-click)
3. Click **Scan System** to see what's already installed
4. Click **Go** to install anything missing
5. Click **Launch**

## Building from source

Windows / Linux native binary:

```
cargo build --release
```

- Windows requires the Rust toolchain plus MSVC build tools (Visual Studio C++ workload). The exe lands at `target/release/sapphire-launcher.exe`.
- Linux produces `target/release/sapphire-launcher`.

To package the Linux AppImage (bundles the release binary + icon + metadata):

```
./build-appimage.sh
```

Output: `Sapphire_Launcher-x86_64.AppImage`.

## Tech

- **Rust** with **Iced 0.13** for the GUI, software-rendered via tiny-skia (no wgpu / GPU driver dependency)
- Async command execution via Tokio
- Source modules: `src/main.rs`, `src/ui.rs`, `src/service.rs`, `src/platform.rs`
- Config stored alongside Sapphire's own: `%APPDATA%\Sapphire\launcher.json` on Windows, `$XDG_CONFIG_HOME/sapphire/launcher.json` (or `~/.config/sapphire/launcher.json`) on Linux
</content>
</invoke>
