# Sapphire Launcher

A Windows GUI installer and manager for [Sapphire](https://github.com/ddxfish/sapphire) — the AI voice assistant. Built with Rust and [Iced](https://iced.rs).

Sapphire Launcher handles everything so you don't have to touch a terminal. One exe, double-click, done.

## What it does

- **Install** — Automatically installs Git, Miniconda, creates a Python environment, clones Sapphire, and installs all dependencies. Just click Go.
- **Update** — Pull the latest changes and update dependencies with one button. Handles dirty repos by auto-stashing your changes.
- **Launch** — Start Sapphire and see its live output in the Log tab. Open Browser button takes you straight to the web UI.
- **Branch Switching** — Switch between Stable (main) and Development (dev) branches from the dropdown. Auto-stops and restarts Sapphire if needed.
- **Troubleshoot** — Checks if Sapphire is responding, verifies package versions (catches the starlette issue), checks dependency health, scans optional voice features (STT/TTS/Wake Word), and detects your GPU. Fix buttons for common issues.
- **Uninstall** — Quick resets for password and API keys. Danger zone for removing the conda environment, user data, or the entire install.

## Features

- Single 8 MB exe, no installer needed
- Finds Git, Conda, and pip on disk automatically (no PATH dependency)
- Detects if Sapphire is already running on startup
- Update checker with badge indicator
- Persists your install path and branch between sessions
- Collapsible launcher log panel
- Copy Log and Open in Notepad buttons for support/debugging
- Two-click confirmation on all destructive actions
- No console window flashing

## Requirements

- **Windows 10 (1809+) or Windows 11** — uses winget for installing Git and Miniconda
- That's it. The launcher installs everything else.

## Usage

1. Download `sapphire-launcher.exe`
2. Double-click it
3. Click **Scan System** to see what's already installed
4. Click **Go** to install anything missing
5. Click **Launch**

## Building from source

```
cargo build --release
```

Requires Rust toolchain and MSVC build tools (Visual Studio C++ workload).

The release exe will be at `target/release/sapphire-launcher.exe`.

## Tech

- **Rust** with **Iced 0.13** for the GUI
- Async command execution via Tokio
- Single file: `src/main.rs`
- Config stored at `%APPDATA%/Sapphire/launcher.json`
