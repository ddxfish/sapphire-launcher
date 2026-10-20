# Sapphire Launcher

A GUI installer and manager for [Sapphire](https://github.com/ddxfish/sapphire) — the AI voice assistant. Built with Rust and [Iced](https://iced.rs).

Sapphire Launcher handles everything so you don't have to touch a terminal. One file, double-click, done. Windows and Linux.

<img width="869" height="657" alt="image" src="https://github.com/user-attachments/assets/406e8a2f-8364-4038-bea1-a8c0c390ff29" />


## What it does

- **Install** — Sets up everything Sapphire needs: Git, Miniconda, a dedicated Python environment, clones the repo, and installs all dependencies. Just click Go.
- **Update** — Pull the latest changes and update dependencies with one button. Handles dirty repos by auto-stashing your changes.
- **Launch** — Start Sapphire and see its live output in the Log tab. She keeps running when you close the launcher; open it again and it picks her back up (live log, Stop). Open Browser takes you straight to the web UI.
- **Branch Switching** — Switch between Stable (main) and Development (dev) branches from the dropdown. Auto-stops and restarts Sapphire if needed.
- **Autostart** — Optionally launch Sapphire when you log in. On Windows a logon task starts her exactly the way Launch does, so the launcher can follow and stop her. On Linux it can install Sapphire as a systemd user service with custom environment variables.
- **Troubleshoot** — Checks if Sapphire is responding and the web UI loads, runs `pip check` on her packages, scans optional voice features (STT/TTS/Wake Word), checks that every enabled plugin has its packages, and detects your GPU(s). Install/Fix buttons stream their output into the log; they wait for Sapphire to be stopped.
- **Uninstall** — Quick resets for password and API keys. Danger zone for removing the conda environment, user data, or the entire install.

## Features

- Single file, no installer needed
- Finds Git, Conda, and pip on disk automatically (no PATH dependency)
- Finds a Sapphire that is already running (autostart, a terminal, an earlier launcher), follows her log and can stop her
- Update checker with badge indicator
- Persists your install path and branch between sessions
- Collapsible launcher log panel
- Copy Log and Open in Notepad buttons for support/debugging
- Two-click confirmation on all destructive actions
- No console window flashing
- Software-rendered UI (no GPU driver required)

## Requirements

- **Windows 10 (1809+) or Windows 11** — uses winget to install Git and Miniconda.
- **Linux (x86-64)** — the AppImage runs on Ubuntu 22.04 or newer (any distro with glibc 2.35+). Uses your package manager / shipped tools for Git and Miniconda.
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
- Linux produces `target/release/sapphire-launcher` (for your own machine; its glibc floor is whatever you built on).

Release outputs all land in `dist/` (gitignored), one folder to attach to a GitHub release:

```
./build-appimage.sh     # Linux: dist/Sapphire_Launcher-x86_64.AppImage + dist/sapphire-launcher-linux-x86_64
build-windows.bat       # Windows: dist\sapphire-launcher.exe
```

The Linux script needs Docker: the binary is compiled in an Ubuntu 22.04 container so the files run on older systems; packaging happens on your machine.

Publishing a beta (pre-release) from the repo root, after both builds:

```
gh release create v0.9.6 dist/* --prerelease --title "Sapphire Launcher v0.9.6 (beta)" --generate-notes
```

Add a file built elsewhere later with `gh release upload v0.9.6 <file>`; promote it with `gh release edit v0.9.6 --prerelease=false`.

CI (`.github/workflows/ci.yml`) runs clippy with warnings-as-errors and the tests on Linux and Windows for every push, and keeps the Windows release exe as a build artifact. The launcher checks the GitHub releases of this repo on start and shows a badge when a newer one exists.

Tests (Linux, needs the sapphire conda env for the two process tests):

```
cargo test
```

## Tech

- **Rust** with **Iced 0.13** for the GUI, software-rendered via tiny-skia (no wgpu / GPU driver dependency)
- Async command execution via Tokio
- Source modules: `src/main.rs` (app state + message loop), `src/ui.rs`, `src/install.rs`, `src/troubleshoot.rs`, `src/service.rs`, `src/platform.rs`
- Config stored alongside Sapphire's own: `%APPDATA%\Sapphire\launcher.json` on Windows, `$XDG_CONFIG_HOME/sapphire/launcher.json` (or `~/.config/sapphire/launcher.json`) on Linux. Her console output from Launch/autostart goes to `sapphire-launcher.log` in the same folder (core's own `user/logs/sapphire.log` is unaffected); `sapphire-run.json` there is how a reopened launcher finds her
</content>
</invoke>
