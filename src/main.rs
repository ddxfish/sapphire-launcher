#![windows_subsystem = "windows"]

use iced::widget::{scrollable, text_editor};
use iced::{color, Subscription, Task, Theme};
use iced::window;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, atomic::{AtomicU32, Ordering}};
use std::time::Instant;

mod ui;
mod service;
mod platform;
mod install;
mod troubleshoot;
use crate::platform::*;
use crate::service::*;
use crate::install::*;
use crate::troubleshoot::*;


// ── Config persistence ─────────────────────────────────────────────────────

fn config_path() -> PathBuf {
    app_config_dir().join("launcher.json")
}

fn load_config() -> (String, Branch) {
    let install_path = read_install_path(&config_path()).unwrap_or_else(default_install_path);
    // Detect actual branch from the repo instead of trusting config
    let branch = detect_git_branch(&install_path);
    (install_path, branch)
}

/// `install_path` from a launcher.json, if the file parses and holds a non-empty one.
fn read_install_path(path: &std::path::Path) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    v.get("install_path")?.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

fn detect_git_branch(install_path: &str) -> Branch {
    let git_dir = PathBuf::from(install_path).join(".git");
    if !git_dir.exists() {
        return Branch::Stable;
    }
    let mut cmd = hidden_cmd(&find_git());
    cmd.args(["-C", install_path, "rev-parse", "--abbrev-ref", "HEAD"]);
    match cmd.output() {
        Ok(out) if out.status.success() => {
            let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if branch == "dev" { Branch::Development } else { Branch::Stable }
        }
        _ => Branch::Stable,
    }
}

fn save_config(install_path: &str, branch: Branch) {
    let _ = write_config(&config_path(), install_path, branch);
}

fn write_config(path: &std::path::Path, install_path: &str, branch: Branch) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let branch_str = match branch {
        Branch::Development => "Development",
        Branch::Stable => "Stable",
    };
    let json = serde_json::json!({ "install_path": install_path, "branch": branch_str });
    std::fs::write(path, format!("{:#}\n", json))
}

fn main() -> iced::Result {
    // iced 0.13 on native Wayland spawns a phantom second toplevel (default title,
    // no app_id) that shows as a stray "unknown" icon in the dock — upstream bug
    // iced-rs/iced#2564. Force XWayland by clearing WAYLAND_DISPLAY before the event
    // loop starts: winit then falls back to X11, where the phantom doesn't occur and
    // the window's WM_CLASS matches the .desktop StartupWMClass (one clean icon).
    // Temporary bridge — revisit when we move to iced 0.14 (fixed there). Safe here:
    // first statement in main(), before any threads spawn.
    #[cfg(not(windows))]
    unsafe {
        std::env::remove_var("WAYLAND_DISPLAY");
        std::env::remove_var("WAYLAND_SOCKET");
    }

    // Linux: register the app with the desktop (apps menu / dock icon).
    #[cfg(not(windows))]
    install_desktop_entry();

    let icon = load_window_icon();

    let win_settings = window::Settings {
        size: iced::Size::new(800.0, 550.0),
        icon,
        // Linux only: app_id must match the .desktop StartupWMClass so the
        // compositor shows our icon. (The field doesn't exist on Windows.)
        #[cfg(not(windows))]
        platform_specific: window::settings::PlatformSpecific {
            application_id: "sapphire-launcher".to_string(),
            ..Default::default()
        },
        ..Default::default()
    };

    iced::application(concat!("Sapphire Launcher v", env!("CARGO_PKG_VERSION")), App::update, App::view)
        .theme(|_| {
            Theme::custom(
                "Sapphire Dark".to_string(),
                iced::theme::Palette {
                    background: color!(0x1e1e2e),
                    text: color!(0xdde1e6),
                    primary: color!(0x3d85c6),
                    success: color!(0x4caf50),
                    danger: color!(0xe74c3c),
                },
            )
        })
        .subscription(App::subscription)
        .window(win_settings)
        // We handle close ourselves: stop the Sapphire we launched, then exit.
        .exit_on_close_request(false)
        .run_with(|| {
            let app = App::default();
            let path = app.install_path.clone();
            let branch = app.selected_branch.unwrap_or(Branch::Stable);
            let port = app.web_port;
            let task = Task::batch(vec![
                // Detect a systemd --user service (Linux) → flips into service mode
                Task::perform(
                    async { tokio::task::spawn_blocking(detect_sapphire_service).await.unwrap_or(None) },
                    Message::ServiceDetected,
                ),
                // Is systemd --user available at all? (gates the Autostart tab's Enable)
                Task::perform(
                    async { tokio::task::spawn_blocking(systemd_user_available).await.unwrap_or(false) },
                    Message::SystemdChecked,
                ),
                // Load any saved service env vars into the editor
                Task::perform(read_env_file(), Message::EnvLoaded),
                // Check if sapphire is already running
                Task::perform(health_up(port, 3), Message::LaunchPreCheck),
                // Check for updates (Sapphire's repo, and the launcher's own releases)
                Task::perform(
                    check_for_updates(path, branch),
                    Message::UpdatesAvailable,
                ),
                Task::perform(check_launcher_release(), Message::LauncherReleaseChecked),
                // Auto-scan install status
                Task::done(Message::ScanClicked),
            ]);
            (app, task)
        })
}

fn load_window_icon() -> Option<window::Icon> {
    let png_bytes = include_bytes!("../icon-512.png");
    let img = image::load_from_memory(png_bytes).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    window::icon::from_rgba(rgba.into_raw(), w, h).ok()
}

/// Register the launcher with the Linux desktop: install the icon into the
/// hicolor theme at several sizes and write a `.desktop` entry, so it shows up
/// in the apps menu / dock with our icon. Idempotent (skips if already done for
/// this binary path). XDG user dirs — no root needed.
#[cfg(not(windows))]
fn install_desktop_entry() {
    use std::io::Write;
    let Ok(home) = std::env::var("HOME") else { return };
    let Ok(exe) = std::env::current_exe() else { return };
    // Under an AppImage, current_exe() points into the ephemeral mount
    // (/tmp/.mount_*) that vanishes on exit — a menu entry pointing there would be
    // dead, and the mount's random suffix would defeat the "already installed" check
    // below (rewriting on every launch). $APPIMAGE is the persistent path to the
    // .AppImage the user launched; prefer it when present.
    let exe_str = std::env::var("APPIMAGE").unwrap_or_else(|_| exe.display().to_string());

    let desktop_path = format!("{}/.local/share/applications/sapphire-launcher.desktop", home);
    // Skip if we've already installed for this exact binary location.
    if let Ok(existing) = std::fs::read_to_string(&desktop_path)
        && existing.contains(&format!("Exec={}", exe_str))
    {
        return;
    }

    // Icons at common sizes into the hicolor theme.
    if let Ok(img) = image::load_from_memory(include_bytes!("../icon-512.png")) {
        for size in [512u32, 256, 128, 64, 48, 32, 16] {
            let dir = format!("{}/.local/share/icons/hicolor/{}x{}/apps", home, size, size);
            if std::fs::create_dir_all(&dir).is_ok() {
                let resized = img.resize_exact(size, size, image::imageops::FilterType::Lanczos3);
                let _ = resized.save(format!("{}/sapphire-launcher.png", dir));
            }
        }
    }

    // The .desktop entry. StartupWMClass matches the window app_id set in main().
    let apps_dir = format!("{}/.local/share/applications", home);
    if std::fs::create_dir_all(&apps_dir).is_ok() {
        let entry = format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=Sapphire Launcher\n\
             Comment=Install and manage Sapphire\n\
             Exec={}\n\
             Icon=sapphire-launcher\n\
             Terminal=false\n\
             Categories=Utility;\n\
             StartupWMClass=sapphire-launcher\n",
            exe_str
        );
        if let Ok(mut f) = std::fs::File::create(&desktop_path) {
            let _ = f.write_all(entry.as_bytes());
        }
    }

    // Best-effort icon cache refresh (GNOME usually picks up ~/.local without it).
    let _ = hidden_cmd("gtk-update-icon-cache")
        .args(["-f", "-t", &format!("{}/.local/share/icons/hicolor", home)])
        .output();
}

// ── App state ──────────────────────────────────────────────────────────────

struct App {
    install_path: String,
    selected_branch: Option<Branch>,
    active_tab: Tab,
    log_visible: bool,
    log_lines: Vec<String>,
    steps: Vec<(Step, StepStatus)>,
    scanning: bool,
    installing: bool,
    uninstalling: bool,
    // Confirmation states for destructive actions — must click twice
    confirm_remove_env: bool,
    confirm_delete_folder: bool,
    confirm_delete_userdata: bool,
    // Update
    updates_available: Option<u32>, // None = unknown, Some(0) = up to date, Some(n) = behind
    checking_updates: bool,
    updating: bool,
    update_status: Vec<(String, StepStatus)>, // label, status
    // Troubleshoot
    ts_checks: Vec<(TsCheck, TsStatus)>,
    ts_running: bool,
    // Animation
    spinner_tick: usize,
    // Sapphire process
    sapphire_running: bool,
    sapphire_stopping: bool,
    sapphire_log: Vec<String>,
    sapphire_pid: Arc<AtomicU32>,
    // Linux systemd --user service (None on Windows / when no unit is detected)
    service: Option<ServiceInfo>,
    // While true, a Subscription follows journalctl for live service logs.
    streaming_journal: bool,
    // Bumped on each (re)start so iced treats the follower as a fresh subscription.
    journal_epoch: usize,
    // Spawn-mode log follower (when WE launched Sapphire, not a systemd unit). Same
    // shape as the journal one: a Subscription (so iced parks it when idle), with an
    // epoch bumped on each launch to force a fresh follower.
    streaming_spawn: bool,
    spawn_epoch: usize,
    // Stop flow (spawn mode): bumped per Stop so late timers/sweeps from an older
    // stop are ignored, and what to do once she is actually down.
    stop_epoch: usize,
    after_stop: AfterStop,
    // Sapphire's web port (user/settings.json WEB_UI_PORT); refreshed with the path.
    web_port: u16,
    // Is there a newer launcher on GitHub? (badge in the header, row on the Update tab)
    launcher_check: LauncherCheck,
    // Autostart tab (Linux): whether systemd --user is available; remove confirm.
    systemd_available: bool,
    confirm_remove_service: bool,
    // Service env vars (EnvironmentFile), edited in the manage view.
    env_content: text_editor::Content,
    // Live install output (streamed by run_cmd_streamed, drained on Tick) + step timer.
    install_output: Arc<Mutex<Vec<String>>>,
    step_started: Option<Instant>,
}

impl Default for App {
    fn default() -> Self {
        let (install_path, branch) = load_config();
        let port = web_port(&install_path);

        Self {
            install_path,
            selected_branch: Some(branch),
            active_tab: Tab::default(),
            log_visible: true,
            log_lines: vec![format!("Sapphire Launcher v{} ({}). Ready.", env!("CARGO_PKG_VERSION"), std::env::consts::OS)],
            steps: ALL_STEPS
                .iter()
                .map(|s| (*s, StepStatus::NotStarted))
                .collect(),
            scanning: false,
            installing: false,
            uninstalling: false,
            spinner_tick: 0,
            updates_available: None,
            checking_updates: false,
            updating: false,
            update_status: vec![
                ("Pull latest changes".into(), StepStatus::NotStarted),
                ("Update dependencies".into(), StepStatus::NotStarted),
            ],
            ts_checks: vec![
                (TsCheck::SapphireRunning, TsStatus::NotChecked),
                (TsCheck::WebUi, TsStatus::NotChecked),
                (TsCheck::DepsHealth, TsStatus::NotChecked),
                (TsCheck::Plugins, TsStatus::NotChecked),
                (TsCheck::PluginDeps, TsStatus::NotChecked),
                (TsCheck::Gpu, TsStatus::NotChecked),
            ],
            ts_running: false,
            confirm_remove_env: false,
            confirm_delete_folder: false,
            confirm_delete_userdata: false,
            sapphire_running: false,
            sapphire_stopping: false,
            sapphire_log: Vec::new(),
            sapphire_pid: Arc::new(AtomicU32::new(0)),
            service: None,
            streaming_journal: false,
            journal_epoch: 0,
            streaming_spawn: false,
            spawn_epoch: 0,
            stop_epoch: 0,
            after_stop: AfterStop::None,
            web_port: port,
            launcher_check: LauncherCheck::Pending,
            systemd_available: false,
            confirm_remove_service: false,
            env_content: text_editor::Content::new(),
            install_output: Arc::new(Mutex::new(Vec::new())),
            step_started: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Tab {
    #[default]
    Install,
    Update,
    Uninstall,
    Troubleshoot,
    Running,
    Service,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Branch {
    Stable,
    Development,
}

impl std::fmt::Display for Branch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Branch::Stable => write!(f, "Stable"),
            Branch::Development => write!(f, "Development"),
        }
    }
}

const BRANCHES: &[Branch] = &[Branch::Stable, Branch::Development];

// ── Messages ───────────────────────────────────────────────────────────────

/// What a Stop was for — runs once Sapphire is actually down.
#[derive(Debug, Clone, Copy, Default)]
enum AfterStop {
    #[default]
    None,
    Update,
    SwitchBranch,
    Close(window::Id),
}

#[derive(Debug, Clone)]
enum Message {
    Tick,
    PathChanged(String),
    BranchSelected(Branch),
    BrowsePath,
    Launch,
    StopSapphire,
    OpenBrowser,
    SwitchBranch,
    SwitchBranchResult(String, bool),
    TabSelected(Tab),
    ToggleLog,
    // Install flow
    ScanClicked,
    GoClicked,
    StepResult(Step, StepStatus),
    InstallStepResult(Step, StepStatus, String), // step, status, log output
    Log(String),
    CopyRunLog,
    OpenRunLog,
    ScrollRunLog,
    // Launch
    LaunchPreCheck(bool),   // startup probe: is a Sapphire already up (started outside this app)?
    HealthPoll,             // periodic: is an out-of-band Sapphire up? (e.g. autostart)
    HealthResult(bool),     // result of the periodic health poll
    SapphireLines(Vec<String>), // a coalesced batch of log lines (burst-safe)
    SapphireExited(Result<Option<i32>, String>), // runner ended: Ok(exit code) / Err(never started)
    SapphireStopConfirmed(bool),  // health probe after a sweep/service stop: false = still up
    StopSignalSent(bool, usize),  // (sent_ok, stop_epoch): the graceful signal went out (or not)
    StopStillWaiting(usize),      // 15 s into a stop: reassure the user
    StopTimeout(usize),           // 100 s into a stop: force it
    StopSwept(usize),             // the hard kill finished
    CloseRequested(window::Id),   // user closed the window
    // Uninstall flow (two-click confirmation)
    ResetPassword,
    ResetCredentials,
    ResetResult(String),
    UninstallCondaEnvClick,   // first click → confirm, second click → go
    UninstallDeleteFolderClick,
    UninstallDeleteUserdataClick,
    UninstallResult(String, bool), // message, success
    // Update
    CheckForUpdates,
    UpdatesAvailable(Option<u32>), // None = check failed, Some(n) = n commits behind
    LauncherReleaseChecked(LauncherCheck), // the launcher's own GitHub release check
    OpenLauncherRelease,                   // open the newer launcher's release page
    UpdateClicked,
    UpdateAfterStop,
    UpdateStepDone(String, bool), // message, success — chains to next step
    // Troubleshoot
    TroubleshootCheck,
    TroubleshootResult(TsCheck, TsStatus),
    TroubleshootFix(TsCheck),
    TroubleshootFixResult(TsCheck, String, bool),
    // Service (Linux systemd --user)
    ServiceDetected(Option<ServiceInfo>),
    ServiceStart,
    ServiceStop,
    #[cfg(not(windows))]
    ServiceRestart,
    #[cfg(not(windows))]
    ServiceEnable,
    #[cfg(not(windows))]
    ServiceDisable,
    ServiceActionResult(String, bool),
    ServiceRefreshed(Option<ServiceInfo>),
    SystemdChecked(bool),
    ServiceInstall,
    ServiceInstallResult(String, bool),
    ServiceRemoveClick,
    ServiceRemoveResult(String, bool),
    EnvLoaded(String),
    #[cfg(not(windows))]
    EnvEdit(text_editor::Action),
    #[cfg(not(windows))]
    EnvSaveRestart,
    #[cfg(not(windows))]
    EnvSaved(bool),
}

// ── Sapphire's web endpoint ────────────────────────────────────────────────

const DEFAULT_WEB_PORT: u16 = 8073;

/// Sapphire's web UI port: `WEB_UI_PORT` in `<install>/user/settings.json` (number or
/// numeric string), else core's default 8073. Anything unreadable or odd → default.
fn web_port(install_path: &str) -> u16 {
    let path = PathBuf::from(install_path).join("user").join("settings.json");
    let Ok(txt) = std::fs::read_to_string(path) else { return DEFAULT_WEB_PORT };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else { return DEFAULT_WEB_PORT };
    let port = match v.get("WEB_UI_PORT") {
        Some(serde_json::Value::Number(n)) => n.as_u64().and_then(|n| u16::try_from(n).ok()),
        Some(serde_json::Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    };
    port.filter(|p| *p > 0).unwrap_or(DEFAULT_WEB_PORT)
}

fn web_url(port: u16) -> String {
    format!("https://localhost:{}", port)
}

/// One health probe: is Sapphire answering on `port`? Silent during boot, so it tells
/// "up" from "not up" but never "booting" from "not running".
async fn health_up(port: u16, timeout_s: u32) -> bool {
    run_cmd_full_async("curl".into(), vec![
        "-sk".into(), "--max-time".into(), timeout_s.to_string(),
        format!("{}/api/health", web_url(port)),
    ]).await.map(|b| b.contains("ok")).unwrap_or(false)
}

/// Poll the health endpoint every 500 ms until it stops answering (true) or `tries`
/// run out (false). Confirms a stop when no exit notice can come (service mode, or
/// an instance we didn't spawn).
async fn wait_health_down(port: u16, tries: usize) -> bool {
    for _ in 0..tries {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if !health_up(port, 2).await {
            return true;
        }
    }
    false
}

// ── Update ─────────────────────────────────────────────────────────────────

impl App {
    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Tick => {
                self.spinner_tick = self.spinner_tick.wrapping_add(1);
                // Drain any live install output into the launcher log.
                let drained: Vec<String> = match self.install_output.lock() {
                    Ok(mut v) => std::mem::take(&mut *v),
                    Err(_) => Vec::new(),
                };
                if !drained.is_empty() {
                    self.log_lines.extend(drained);
                    self.cap_log_lines();
                }
                Task::none()
            }
            Message::PathChanged(path) => {
                if !path.is_empty() {
                    self.install_path = path;
                    self.web_port = web_port(&self.install_path);
                    save_config(&self.install_path, self.selected_branch.unwrap_or(Branch::Stable));
                }
                Task::none()
            }
            Message::BranchSelected(branch) => {
                self.selected_branch = Some(branch);
                save_config(&self.install_path, branch);
                let git_dir = PathBuf::from(&self.install_path).join(".git");
                if !git_dir.exists() {
                    return Task::none();
                }

                // Stop Sapphire first if she's up; the checkout runs once she is down.
                if self.sapphire_running || self.sapphire_stopping {
                    self.log_lines.push("Stopping Sapphire for branch switch...".to_string());
                    return self.begin_stop(AfterStop::SwitchBranch);
                }
                self.update(Message::SwitchBranch)
            }
            Message::BrowsePath => {
                Task::perform(async {
                    rfd::AsyncFileDialog::new()
                        .set_title("Choose Sapphire install folder")
                        .pick_folder()
                        .await
                        .map(|f| f.path().to_string_lossy().to_string())
                }, |result| {
                    match result {
                        Some(path) => Message::PathChanged(path),
                        None => Message::Log("Browse cancelled.".into()),
                    }
                })
            }
            Message::Launch => {
                if self.sapphire_running {
                    self.log_lines.push("Sapphire is already running.".to_string());
                    return Task::none();
                }

                // Service mode: start the systemd unit + follow journalctl instead.
                if self.service.is_some() {
                    return self.update(Message::ServiceStart);
                }

                let main_py = PathBuf::from(&self.install_path).join("main.py");
                if !main_py.exists() {
                    self.log_lines.push(format!(
                        "Can't find {}. Run the installer first.",
                        main_py.display()
                    ));
                    return Task::none();
                }

                // Just start her. If another Sapphire already holds this user/, core
                // says so and exits 43 within a second — SapphireExited shows the line.
                // (The health probe is blind for the whole boot, so it can't be the gate.)
                // The follower is a Subscription keyed on spawn_epoch; flipping
                // streaming_spawn on (and bumping the epoch) starts a fresh process + log stream.
                self.sapphire_running = true;
                self.sapphire_log.clear();
                self.active_tab = Tab::Running;
                self.log_lines.push("Launching Sapphire...".to_string());
                self.streaming_spawn = true;
                self.spawn_epoch += 1;
                Task::none()
            }
            Message::LaunchPreCheck(already_running) => {
                // Startup probe only. Service mode: running state is owned by ServiceDetected.
                if self.service.is_some() {
                    self.sapphire_running = already_running;
                    return Task::none();
                }
                if already_running {
                    self.sapphire_running = true;
                    self.sapphire_log.clear();
                    self.sapphire_log.push("Sapphire is already running (started outside this app).".into());
                    self.sapphire_log.push("".into());
                    self.sapphire_log.push("No live logs available for this session.".into());
                    self.sapphire_log.push("To see logs: click Stop, then Launch again.".into());
                    self.log_lines.push("Sapphire already running.".to_string());
                }
                Task::none()
            }
            Message::StopSapphire => {
                // Service mode: stop the unit (and refresh status).
                if self.service.is_some() {
                    return self.update(Message::ServiceStop);
                }
                self.begin_stop(AfterStop::None)
            }
            Message::OpenBrowser => {
                open_url(&web_url(self.web_port));
                Task::none()
            }
            Message::SwitchBranch => {
                let path = self.install_path.clone();
                let git_dir = PathBuf::from(&path).join(".git");
                if !git_dir.exists() {
                    self.log_lines.push("No repo found — install first.".to_string());
                    return Task::none();
                }
                let branch = match self.selected_branch {
                    Some(Branch::Development) => "dev",
                    _ => "main",
                };
                self.log_lines.push(format!("Switching to {} branch...", branch));
                let b = branch.to_string();
                Task::perform(
                    async move {
                        run_cmd_full_async(find_git(), vec!["-C".into(), path, "checkout".into(), b.clone()]).await
                            .map(|out| (format!("Switched to {}: {}", b, out), true))
                            .unwrap_or_else(|e| (format!("Branch switch failed: {}", e), false))
                    },
                    |(msg, ok)| Message::SwitchBranchResult(msg, ok),
                )
            }
            Message::SwitchBranchResult(msg, _ok) => {
                self.sapphire_running = false;
                self.sapphire_stopping = false;
                self.sapphire_log.clear();
                self.log_lines.push(msg);
                Task::none()
            }

            // ── Update flow ───────────────────────────────────────────
            Message::CheckForUpdates => {
                if self.checking_updates { return Task::none(); }
                self.checking_updates = true;
                let path = self.install_path.clone();
                let branch = self.selected_branch.unwrap_or(Branch::Stable);
                Task::batch(vec![
                    Task::perform(check_for_updates(path, branch), Message::UpdatesAvailable),
                    Task::perform(check_launcher_release(), Message::LauncherReleaseChecked),
                ])
            }
            Message::UpdatesAvailable(count) => {
                self.checking_updates = false;
                self.updates_available = count;
                Task::none()
            }
            Message::LauncherReleaseChecked(check) => {
                // Log it once (not on every re-check); the badge and the Update tab
                // carry it from here.
                if let LauncherCheck::Available(rel) = &check
                    && self.launcher_check != check
                {
                    self.log_lines.push(format!(
                        "A newer launcher is available: v{} (you have v{}). The Update tab has the download.",
                        rel.version, env!("CARGO_PKG_VERSION")
                    ));
                }
                self.launcher_check = check;
                Task::none()
            }
            Message::OpenLauncherRelease => {
                if let LauncherCheck::Available(rel) = &self.launcher_check {
                    open_url(&rel.url);
                }
                Task::none()
            }
            Message::UpdateClicked => {
                if self.sapphire_running || self.sapphire_stopping {
                    // Stop her first; UpdateAfterStop continues once she is down.
                    self.log_lines.push("[update] Stopping Sapphire first...".to_string());
                    return self.begin_stop(AfterStop::Update);
                }
                let git_dir = PathBuf::from(&self.install_path).join(".git");
                if !git_dir.exists() {
                    self.log_lines.push("No repo found — install first.".to_string());
                    return Task::none();
                }
                self.updating = true;
                // Reset steps
                for (_, s) in &mut self.update_status {
                    *s = StepStatus::NotStarted;
                }
                // Step 1: git pull
                self.update_status[0].1 = StepStatus::Installing;
                self.log_lines.push("[update] Pulling latest changes...".to_string());
                let path = self.install_path.clone();
                Task::perform(git_stash_and_pull(path), |(msg, ok)| Message::UpdateStepDone(msg, ok))
            }
            Message::UpdateAfterStop => {
                self.sapphire_running = false;
                self.sapphire_stopping = false;
                self.sapphire_log.clear();
                self.log_lines.push("Sapphire stopped. Starting update...".to_string());
                let git_dir = PathBuf::from(&self.install_path).join(".git");
                if !git_dir.exists() {
                    self.log_lines.push("No repo found — install first.".to_string());
                    return Task::none();
                }
                self.updating = true;
                for (_, s) in &mut self.update_status {
                    *s = StepStatus::NotStarted;
                }
                self.update_status[0].1 = StepStatus::Installing;
                self.log_lines.push("[update] Pulling latest changes...".to_string());
                let path = self.install_path.clone();
                Task::perform(git_stash_and_pull(path), |(msg, ok)| Message::UpdateStepDone(msg, ok))
            }
            Message::UpdateStepDone(msg, success) => {
                self.log_lines.push(format!("[update] {}", msg));

                // Figure out which step just finished
                let current_step = self.update_status.iter().position(|(_, s)| *s == StepStatus::Installing);

                if !success {
                    if let Some(i) = current_step {
                        self.update_status[i].1 = StepStatus::Failed(msg);
                    }
                    self.updating = false;
                    self.log_lines.push("[update] Stopped — fix the issue and try again.".to_string());
                    return Task::none();
                }

                if let Some(i) = current_step {
                    self.update_status[i].1 = StepStatus::Done(msg);

                    // Kick next step
                    if i == 0 {
                        // Step 2: pip install -r requirements.txt
                        self.update_status[1].1 = StepStatus::Installing;
                        self.log_lines.push("[update] Updating dependencies...".to_string());
                        let req = PathBuf::from(&self.install_path).join("requirements.txt");
                        return Task::perform(async move {
                            match pip_install(vec!["-r".into(), req.to_string_lossy().to_string()]).await {
                                Ok(_) => ("Dependencies updated.".to_string(), true),
                                Err(e) => (format!("pip install failed: {}", e), false),
                            }
                        }, |(msg, ok)| Message::UpdateStepDone(msg, ok));
                    }
                }

                // All done
                self.updating = false;
                self.log_lines.push("[update] Update complete!".to_string());
                self.updates_available = Some(0);
                Task::none()
            }
            Message::SapphireLines(lines) => {
                self.sapphire_log.extend(lines);
                self.cap_sapphire_log();
                Task::none()
            }
            Message::SapphireExited(result) => {
                // A late notice from a follower we already closed (forced stop): ignore.
                if !self.streaming_spawn {
                    return Task::none();
                }
                let msg = exit_message(result, self.sapphire_stopping);
                self.sapphire_log.push(format!("--- {} ---", msg));
                self.log_lines.push(msg);
                if self.sapphire_stopping {
                    self.finish_stop()
                } else {
                    self.sapphire_running = false;
                    self.streaming_spawn = false;
                    self.sapphire_pid.store(0, Ordering::Relaxed);
                    Task::none()
                }
            }
            Message::SapphireStopConfirmed(stopped) => {
                if !self.sapphire_stopping {
                    return Task::none();
                }
                if stopped {
                    self.log_lines.push("Sapphire stopped.".to_string());
                    return self.finish_stop();
                }
                // Still answering after the grace window (e.g. an elevated instance we
                // can't touch). Keep her shown as running so the button stays Stop; a
                // pending window close still closes.
                self.sapphire_stopping = false;
                self.sapphire_running = true;
                self.log_lines.push("Sapphire didn't stop — it may be running with admin rights. Try Stop again.".to_string());
                match std::mem::take(&mut self.after_stop) {
                    AfterStop::Close(id) => window::close(id),
                    _ => Task::none(),
                }
            }
            Message::StopSignalSent(sent, epoch) => {
                if !self.sapphire_stopping || epoch != self.stop_epoch {
                    return Task::none();
                }
                if !sent {
                    self.log_lines.push("Couldn't signal Sapphire to stop — forcing it.".to_string());
                    return self.force_stop(epoch);
                }
                // She got the signal; SapphireExited arrives when the runner is gone
                // (normally ~8 s). Reassure at 15 s, force at 100 s (core's worst case).
                Task::batch(vec![
                    Task::perform(tokio::time::sleep(std::time::Duration::from_secs(15)), move |_| Message::StopStillWaiting(epoch)),
                    Task::perform(tokio::time::sleep(std::time::Duration::from_secs(100)), move |_| Message::StopTimeout(epoch)),
                ])
            }
            Message::StopStillWaiting(epoch) => {
                if self.sapphire_stopping && epoch == self.stop_epoch {
                    self.log_lines.push("Still stopping... Sapphire is finishing up (this can take up to 100 seconds).".to_string());
                }
                Task::none()
            }
            Message::StopTimeout(epoch) => {
                if !self.sapphire_stopping || epoch != self.stop_epoch {
                    return Task::none();
                }
                self.log_lines.push("Sapphire didn't stop in time — forcing it.".to_string());
                self.force_stop(epoch)
            }
            Message::StopSwept(epoch) => {
                if !self.sapphire_stopping || epoch != self.stop_epoch {
                    return Task::none();
                }
                self.log_lines.push("Sapphire stopped (forced).".to_string());
                self.finish_stop()
            }
            Message::CloseRequested(id) => {
                // Close stops the Sapphire WE launched (she lives while the window lives).
                // A systemd unit or an instance started elsewhere is left alone.
                // (A service-mode stop in flight finishes on its own: closing is safe.)
                let ours = self.service.is_none()
                    && self.streaming_spawn
                    && self.sapphire_pid.load(Ordering::Relaxed) > 0;
                if !ours {
                    return window::close(id);
                }
                if !self.sapphire_stopping {
                    self.log_lines.push("Closing: stopping Sapphire first...".to_string());
                }
                self.begin_stop(AfterStop::Close(id))
            }
            Message::HealthPoll => {
                // Skip when our own flows already own the running-state.
                if self.streaming_spawn || self.sapphire_stopping || self.service.is_some() {
                    return Task::none();
                }
                Task::perform(health_up(self.web_port, 3), Message::HealthResult)
            }
            Message::HealthResult(running) => {
                if !self.streaming_spawn && !self.sapphire_stopping && self.service.is_none() {
                    self.sapphire_running = running;
                }
                Task::none()
            }
            Message::TabSelected(tab) => {
                self.active_tab = tab;
                // Reset any armed confirmations
                self.confirm_remove_env = false;
                self.confirm_delete_folder = false;
                self.confirm_delete_userdata = false;
                self.confirm_remove_service = false;
                // Auto-check for updates when opening Update tab
                if tab == Tab::Update {
                    return self.update(Message::CheckForUpdates);
                }
                Task::none()
            }
            Message::ToggleLog => {
                self.log_visible = !self.log_visible;
                Task::none()
            }

            // ── Install flow ───────────────────────────────────────────
            Message::ScanClicked => {
                self.scanning = true;
                self.web_port = web_port(&self.install_path); // settings.json may be new
                self.log_lines.push("Scanning system...".to_string());

                // Set all steps to Checking
                for (_, status) in &mut self.steps {
                    *status = StepStatus::Checking;
                }
                // Done step stays NotStarted during scan
                if let Some((_, status)) = self.steps.iter_mut().find(|(s, _)| *s == Step::Done) {
                    *status = StepStatus::NotStarted;
                }

                let path = self.install_path.clone();

                // Fire off all checks in parallel
                #[allow(unused_mut)] // Windows pushes the VC++ check below
                let mut checks = vec![
                    Task::perform(check_git(), |(step, status)| {
                        Message::StepResult(step, status)
                    }),
                    Task::perform(check_conda(), |(step, status)| {
                        Message::StepResult(step, status)
                    }),
                    Task::perform(check_conda_init(), |(step, status)| {
                        Message::StepResult(step, status)
                    }),
                    Task::perform(check_python_env(), |(step, status)| {
                        Message::StepResult(step, status)
                    }),
                    Task::perform(check_clone(path.clone()), |(step, status)| {
                        Message::StepResult(step, status)
                    }),
                    Task::perform(check_deps(path), |(step, status)| {
                        Message::StepResult(step, status)
                    }),
                ];
                #[cfg(windows)]
                checks.push(Task::perform(check_vcredist(), |(step, status)| {
                    Message::StepResult(step, status)
                }));
                Task::batch(checks)
            }

            Message::StepResult(step, status) => {
                // Log it
                let detail = status.detail().unwrap_or("").to_string();
                let label = step_label(step);
                self.log_lines.push(format!("[scan] {} — {}", label, detail));

                // Update step status
                if let Some((_, s)) = self.steps.iter_mut().find(|(st, _)| *st == step) {
                    *s = status;
                }

                // Check if scan is complete (no more Checking steps, ignoring Done)
                let still_checking = self
                    .steps
                    .iter()
                    .any(|(s, st)| *s != Step::Done && *st == StepStatus::Checking);

                if !still_checking {
                    self.scanning = false;
                    self.log_lines.push("Scan complete.".to_string());

                    // If everything is Found, mark Done as green
                    let all_found = self
                        .steps
                        .iter()
                        .filter(|(s, _)| *s != Step::Done)
                        .all(|(_, st)| matches!(st, StepStatus::Found(_)));

                    if all_found {
                        if let Some((_, s)) = self.steps.iter_mut().find(|(s, _)| *s == Step::Done) {
                            *s = StepStatus::Done("Sapphire looks ready! Hit Launch.".to_string());
                        }
                        self.log_lines
                            .push("Everything looks good! Sapphire is ready to launch.".to_string());
                    }
                }

                Task::none()
            }

            Message::GoClicked => {
                self.installing = true;
                if let Ok(mut v) = self.install_output.lock() { v.clear(); }

                self.log_lines.push("Starting install...".to_string());

                // Find the first step that needs work
                self.kick_next_install()
            }

            Message::InstallStepResult(step, status, log_output) => {
                let label = step_label(step);

                // Log the output — strip ANSI, collapse \r progress bars, drop blanks.
                if !log_output.is_empty() {
                    for raw in log_output.lines() {
                        let mut s = strip_ansi(raw);
                        if let Some(i) = s.rfind('\r') { s = s[i + 1..].to_string(); }
                        let s = s.trim_end();
                        if !s.is_empty() {
                            self.log_lines.push(format!("  {}", s));
                        }
                    }
                    self.cap_log_lines();
                }

                let failed = matches!(status, StepStatus::Failed(_));
                let detail = status.detail().unwrap_or("").to_string();
                self.log_lines.push(format!("[install] {} — {}", label, detail));

                // Update step status
                if let Some((_, s)) = self.steps.iter_mut().find(|(st, _)| *st == step) {
                    *s = status;
                }

                if failed {
                    self.installing = false;
                    self.log_lines.push("Install stopped — fix the issue above and try again.".to_string());
                    return Task::none();
                }

                // Kick off the next step
                self.kick_next_install()
            }

            Message::CopyRunLog => {
                // iced's own clipboard — works on Wayland/X11/Windows/macOS, no subprocess.
                let log_text = self.sapphire_log.join("\n");
                self.log_lines.push("Copied to clipboard.".to_string());
                iced::clipboard::write(log_text)
            }
            Message::OpenRunLog => {
                // Write log to temp file and open in Notepad for selection/copying
                let log_text = self.sapphire_log.join("\n");
                let log_path = std::env::temp_dir().join("sapphire_log.txt");
                if std::fs::write(&log_path, &log_text).is_ok() {
                    open_file(&log_path);
                } else {
                    self.log_lines.push("Couldn't write log file.".to_string());
                }
                Task::none()
            }
            Message::ScrollRunLog => {
                scrollable::snap_to(
                    scrollable::Id::new("run-log"),
                    scrollable::RelativeOffset { x: 0.0, y: 1.0 },
                )
            }
            Message::Log(line) => {
                self.log_lines.push(line);
                Task::none()
            }

            // ── Resets ─────────────────────────────────────────────────
            Message::ResetPassword => {
                let key_path = app_config_dir().join("secret_key");
                let msg = if key_path.exists() {
                    match std::fs::remove_file(&key_path) {
                        Ok(()) => "Password reset. Sapphire will ask for a new password on next launch.".into(),
                        Err(e) => format!("Couldn't delete {}: {}", key_path.display(), e),
                    }
                } else {
                    "No password file found — nothing to reset.".into()
                };
                self.log_lines.push(msg.clone());
                Task::done(Message::ResetResult(msg))
            }
            Message::ResetCredentials => {
                let cred_path = app_config_dir().join("credentials.json");
                let msg = if cred_path.exists() {
                    match std::fs::remove_file(&cred_path) {
                        Ok(()) => "API keys cleared. You'll need to re-enter them in Sapphire.".into(),
                        Err(e) => format!("Couldn't delete {}: {}", cred_path.display(), e),
                    }
                } else {
                    "No credentials file found — nothing to reset.".into()
                };
                self.log_lines.push(msg.clone());
                Task::done(Message::ResetResult(msg))
            }
            Message::ResetResult(_msg) => {
                Task::none()
            }

            // ── Uninstall flow (two-click confirmation) ─────────────
            Message::UninstallCondaEnvClick => {
                if !self.confirm_remove_env {
                    // First click — arm it
                    self.confirm_remove_env = true;
                    return Task::none();
                }
                // Second click — do it
                self.confirm_remove_env = false;
                self.uninstalling = true;

                self.log_lines.push("[uninstall] Removing conda environment 'sapphire'...".to_string());
                Task::perform(uninstall_conda_env(), |(msg, ok)| {
                    Message::UninstallResult(msg, ok)
                })
            }

            Message::UninstallDeleteFolderClick => {
                if !self.confirm_delete_folder {
                    self.confirm_delete_folder = true;
                    return Task::none();
                }
                self.confirm_delete_folder = false;
                self.uninstalling = true;

                let path = self.install_path.clone();
                self.log_lines.push(format!("[uninstall] Deleting {}...", path));
                Task::perform(uninstall_delete_folder(path), |(msg, ok)| {
                    Message::UninstallResult(msg, ok)
                })
            }

            Message::UninstallDeleteUserdataClick => {
                if !self.confirm_delete_userdata {
                    self.confirm_delete_userdata = true;
                    return Task::none();
                }
                self.confirm_delete_userdata = false;
                self.uninstalling = true;

                let path = self.install_path.clone();
                self.log_lines.push(format!("[uninstall] Deleting {}\\user...", path));
                Task::perform(uninstall_delete_userdata(path), |(msg, ok)| {
                    Message::UninstallResult(msg, ok)
                })
            }

            Message::UninstallResult(msg, success) => {
                self.uninstalling = false;
                for line in msg.lines() {
                    self.log_lines.push(format!("  {}", line));
                }
                if success {
                    self.log_lines.push("[uninstall] Done.".to_string());
                    for (_, status) in &mut self.steps {
                        *status = StepStatus::NotStarted;
                    }
                } else {
                    self.log_lines.push("[uninstall] Something went wrong — check the log above.".to_string());
                }
                Task::none()
            }

            // ── Troubleshoot ──────────────────────────────────────────
            Message::TroubleshootCheck => {
                self.ts_running = true;
                for (_, status) in &mut self.ts_checks {
                    *status = TsStatus::Checking;
                }
                self.log_lines.push("[troubleshoot] Running checks...".to_string());
                let path = self.install_path.clone();
                let port = self.web_port;

                Task::batch(vec![
                    Task::perform(ts_check_running(port), |(c, s)| Message::TroubleshootResult(c, s)),
                    Task::perform(ts_check_webui(port), |(c, s)| Message::TroubleshootResult(c, s)),
                    Task::perform(ts_check_deps(), |(c, s)| Message::TroubleshootResult(c, s)),
                    Task::perform(ts_check_plugins(path.clone()), |(c, s)| Message::TroubleshootResult(c, s)),
                    Task::perform(ts_check_plugin_deps(path), |(c, s)| Message::TroubleshootResult(c, s)),
                    Task::perform(ts_check_gpu(), |(c, s)| Message::TroubleshootResult(c, s)),
                ])
            }
            Message::TroubleshootResult(check, status) => {
                if let Some((_, s)) = self.ts_checks.iter_mut().find(|(c, _)| *c == check) {
                    *s = status;
                }
                let still_checking = self.ts_checks.iter().any(|(_, s)| *s == TsStatus::Checking);
                if !still_checking {
                    self.ts_running = false;
                    self.log_lines.push("[troubleshoot] Done.".to_string());
                }
                Task::none()
            }
            Message::TroubleshootFix(check) => {
                if let Some((_, s)) = self.ts_checks.iter_mut().find(|(c, _)| *c == check) {
                    *s = TsStatus::Fixing;
                }
                self.log_lines.push(format!("[troubleshoot] Fixing {}...", ts_label(check)));
                let path = self.install_path.clone();
                match check {
                    TsCheck::DepsHealth => Task::perform(ts_fix_deps(path), |(c, msg, ok)| {
                        Message::TroubleshootFixResult(c, msg, ok)
                    }),
                    TsCheck::Plugins => Task::perform(ts_fix_plugins(path), |(c, msg, ok)| {
                        Message::TroubleshootFixResult(c, msg, ok)
                    }),
                    TsCheck::PluginDeps => Task::perform(ts_fix_plugin_deps(path), |(c, msg, ok)| {
                        Message::TroubleshootFixResult(c, msg, ok)
                    }),
                    _ => Task::none(),
                }
            }
            Message::TroubleshootFixResult(check, msg, success) => {
                self.log_lines.push(format!("[troubleshoot] {}", msg));
                if let Some((_, s)) = self.ts_checks.iter_mut().find(|(c, _)| *c == check) {
                    if success {
                        *s = TsStatus::Fixed(msg);
                    } else {
                        *s = TsStatus::Problem(msg);
                    }
                }
                Task::none()
            }

            // ── Service (Linux systemd --user) ────────────────────────
            Message::ServiceDetected(info) => {
                self.service = info.clone();
                let mut tasks: Vec<Task<Message>> = Vec::new();
                if let Some(info) = info {
                    self.log_lines.push(format!("Detected sapphire.service ({}).", info.sub_state));
                    // Auto-fill path from the unit's WorkingDirectory, but only if our
                    // current path isn't already a valid install (don't override saved).
                    if let Some(wd) = &info.working_dir {
                        let have_valid = PathBuf::from(&self.install_path).join("main.py").exists();
                        if !have_valid && PathBuf::from(wd).join("main.py").exists() {
                            self.install_path = wd.clone();
                            self.web_port = web_port(&self.install_path);
                            self.selected_branch = Some(detect_git_branch(wd));
                            save_config(&self.install_path, self.selected_branch.unwrap_or(Branch::Stable));
                            self.log_lines.push(format!("Using service path: {}", wd));
                            tasks.push(Task::done(Message::ScanClicked));
                        }
                    }
                    self.sapphire_running = info.active;
                    if info.active {
                        // Attach live logs (the journalctl Subscription starts itself).
                        self.sapphire_log.clear();
                        self.streaming_journal = true;
                        self.journal_epoch += 1;
                    }
                }
                Task::batch(tasks)
            }
            Message::ServiceStart => {
                self.sapphire_running = true;
                self.sapphire_stopping = false;
                self.sapphire_log.clear();
                self.streaming_journal = true;
                self.journal_epoch += 1;
                self.active_tab = Tab::Running;
                self.log_lines.push("Starting Sapphire service...".to_string());
                Task::perform(
                    async {
                        run_cmd_full_async("systemctl".into(), vec!["--user".into(), "start".into(), "sapphire".into()]).await
                            .map(|_| ("Service started.".to_string(), true))
                            .unwrap_or_else(|e| (format!("Service start failed: {}", e), false))
                    },
                    |(m, ok)| Message::ServiceActionResult(m, ok),
                )
            }
            Message::ServiceStop => {
                self.sapphire_stopping = true;
                self.streaming_journal = false;
                self.log_lines.push("Stopping Sapphire service...".to_string());
                Task::perform(
                    async {
                        let r = run_cmd_full_async("systemctl".into(), vec!["--user".into(), "stop".into(), "sapphire".into()]).await;
                        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                        r.map(|_| ("Service stopped.".to_string(), true))
                            .unwrap_or_else(|e| (format!("Service stop failed: {}", e), false))
                    },
                    |(m, ok)| Message::ServiceActionResult(m, ok),
                )
            }
            #[cfg(not(windows))]
            Message::ServiceRestart => {
                self.sapphire_running = true;
                self.sapphire_stopping = false;
                self.streaming_journal = true;
                self.journal_epoch += 1;
                self.active_tab = Tab::Running;
                self.log_lines.push("Restarting Sapphire service...".to_string());
                Task::perform(
                    async {
                        run_cmd_full_async("systemctl".into(), vec!["--user".into(), "restart".into(), "sapphire".into()]).await
                            .map(|_| ("Service restarted.".to_string(), true))
                            .unwrap_or_else(|e| (format!("Service restart failed: {}", e), false))
                    },
                    |(m, ok)| Message::ServiceActionResult(m, ok),
                )
            }
            #[cfg(not(windows))]
            Message::ServiceEnable => {
                self.log_lines.push("Enabling sapphire.service (autostart at login)...".to_string());
                Task::perform(
                    async {
                        run_cmd_full_async("systemctl".into(), vec!["--user".into(), "enable".into(), "sapphire".into()]).await
                            .map(|_| ("Autostart enabled.".to_string(), true))
                            .unwrap_or_else(|e| (format!("Enable failed: {}", e), false))
                    },
                    |(m, ok)| Message::ServiceActionResult(m, ok),
                )
            }
            #[cfg(not(windows))]
            Message::ServiceDisable => {
                self.log_lines.push("Disabling sapphire.service autostart...".to_string());
                Task::perform(
                    async {
                        run_cmd_full_async("systemctl".into(), vec!["--user".into(), "disable".into(), "sapphire".into()]).await
                            .map(|_| ("Autostart disabled.".to_string(), true))
                            .unwrap_or_else(|e| (format!("Disable failed: {}", e), false))
                    },
                    |(m, ok)| Message::ServiceActionResult(m, ok),
                )
            }
            Message::ServiceActionResult(msg, ok) => {
                self.log_lines.push(format!("[service] {}", msg));
                if !ok {
                    self.sapphire_running = false;
                    self.sapphire_stopping = false;
                }
                // Re-read the unit state to sync the UI.
                Task::perform(
                    async { tokio::task::spawn_blocking(detect_sapphire_service).await.unwrap_or(None) },
                    Message::ServiceRefreshed,
                )
            }
            Message::ServiceRefreshed(info) => {
                if let Some(info) = &info {
                    self.sapphire_running = info.active;
                    self.sapphire_stopping = false;
                }
                self.service = info;
                Task::none()
            }
            Message::SystemdChecked(available) => {
                self.systemd_available = available;
                Task::none()
            }
            Message::ServiceInstall => {
                self.log_lines.push("[service] Setting up autostart...".to_string());
                let path = self.install_path.clone();
                Task::perform(install_service(path), |(m, ok)| Message::ServiceInstallResult(m, ok))
            }
            Message::ServiceInstallResult(msg, ok) => {
                self.log_lines.push(format!("[service] {}", msg));
                if ok {
                    // Linux `enable --now` also starts it + streams journal. On Windows,
                    // enabling autostart only registers the logon task — it runs at the
                    // next sign-in, so we don't touch running/streaming state here.
                    #[cfg(not(windows))]
                    {
                        self.sapphire_running = true;
                        self.streaming_journal = true;
                        self.journal_epoch += 1;
                        self.sapphire_log.clear();
                    }
                }
                // Re-detect so the tab flips to the manage view.
                Task::perform(
                    async { tokio::task::spawn_blocking(detect_sapphire_service).await.unwrap_or(None) },
                    Message::ServiceRefreshed,
                )
            }
            Message::ServiceRemoveClick => {
                if !self.confirm_remove_service {
                    self.confirm_remove_service = true;
                    return Task::none();
                }
                self.confirm_remove_service = false;
                self.streaming_journal = false;
                self.log_lines.push("[service] Removing systemd --user unit...".to_string());
                Task::perform(remove_service(), |(m, ok)| Message::ServiceRemoveResult(m, ok))
            }
            Message::ServiceRemoveResult(msg, ok) => {
                self.log_lines.push(format!("[service] {}", msg));
                if ok {
                    // Linux removes the running unit too. On Windows, turning off
                    // autostart shouldn't change whatever's running right now.
                    #[cfg(not(windows))]
                    {
                        self.sapphire_running = false;
                        self.sapphire_stopping = false;
                    }
                }
                Task::perform(
                    async { tokio::task::spawn_blocking(detect_sapphire_service).await.unwrap_or(None) },
                    Message::ServiceRefreshed,
                )
            }
            Message::EnvLoaded(s) => {
                self.env_content = text_editor::Content::with_text(&s);
                Task::none()
            }
            #[cfg(not(windows))]
            Message::EnvEdit(action) => {
                self.env_content.perform(action);
                Task::none()
            }
            #[cfg(not(windows))]
            Message::EnvSaveRestart => {
                self.log_lines.push("[service] Saving environment, then restarting...".to_string());
                Task::perform(write_env_file(self.env_content.text()), Message::EnvSaved)
            }
            #[cfg(not(windows))]
            Message::EnvSaved(ok) => {
                if ok {
                    self.log_lines.push("[service] Environment saved.".to_string());
                    return self.update(Message::ServiceRestart);
                }
                self.log_lines.push("[service] Couldn't write the environment file.".to_string());
                Task::none()
            }
        }
    }

    /// Keep the live log buffer bounded (scrollback cap).
    fn cap_sapphire_log(&mut self) {
        const CAP: usize = 2000;
        if self.sapphire_log.len() > CAP {
            let excess = self.sapphire_log.len() - CAP;
            self.sapphire_log.drain(..excess);
        }
    }

    /// Keep the launcher log bounded (install output can be verbose).
    fn cap_log_lines(&mut self) {
        const CAP: usize = 1000;
        if self.log_lines.len() > CAP {
            let excess = self.log_lines.len() - CAP;
            self.log_lines.drain(..excess);
        }
    }

    /// The one stop path — Stop button, branch switch, update, window close.
    /// Signals first (SIGTERM / CTRL_BREAK) and waits for the runner to exit; the
    /// hard kill is only the fallback. Nothing here blocks the UI thread.
    fn begin_stop(&mut self, after: AfterStop) -> Task<Message> {
        if self.sapphire_stopping {
            // A stop is already in flight: just update what happens afterwards.
            if !matches!(after, AfterStop::None) {
                self.after_stop = after;
            }
            return Task::none();
        }
        self.sapphire_stopping = true;
        self.after_stop = after;
        self.stop_epoch += 1;
        let epoch = self.stop_epoch;
        let port = self.web_port;
        self.log_lines.push("Stopping Sapphire...".to_string());

        // Service mode: stop the unit, confirm through the health probe.
        if self.service.is_some() {
            self.streaming_journal = false;
            return Task::perform(
                async move {
                    let _ = run_cmd_full_async("systemctl".into(), vec!["--user".into(), "stop".into(), "sapphire".into()]).await;
                    wait_health_down(port, 20).await
                },
                Message::SapphireStopConfirmed,
            );
        }

        let pid = self.sapphire_pid.load(Ordering::Relaxed);
        if pid == 0 || !self.streaming_spawn {
            // Not ours (started outside this app): no runner to signal and no exit
            // notice coming. Sweep, then confirm through the health probe.
            return Task::perform(
                async move {
                    let _ = tokio::task::spawn_blocking(|| kill_process_tree(0)).await;
                    wait_health_down(port, 20).await
                },
                Message::SapphireStopConfirmed,
            );
        }

        // Ours: ask nicely. The follower stays attached, so SapphireExited arrives
        // when the runner is gone. Off the UI thread (kill(1) is a process spawn).
        Task::perform(
            async move { tokio::task::spawn_blocking(move || signal_stop(pid)).await.unwrap_or(false) },
            move |sent| Message::StopSignalSent(sent, epoch),
        )
    }

    /// Hard kill (tree + sweep) off the UI thread; StopSwept finishes the stop.
    fn force_stop(&mut self, epoch: usize) -> Task<Message> {
        let pid = self.sapphire_pid.load(Ordering::Relaxed);
        Task::perform(
            async move { let _ = tokio::task::spawn_blocking(move || kill_process_tree(pid)).await; },
            move |_| Message::StopSwept(epoch),
        )
    }

    /// Sapphire is down: clear the running state and do what the stop was for.
    fn finish_stop(&mut self) -> Task<Message> {
        self.sapphire_stopping = false;
        self.sapphire_running = false;
        self.streaming_spawn = false;
        self.sapphire_pid.store(0, Ordering::Relaxed);
        match std::mem::take(&mut self.after_stop) {
            AfterStop::None => Task::none(),
            AfterStop::Update => self.update(Message::UpdateAfterStop),
            AfterStop::SwitchBranch => self.update(Message::SwitchBranch),
            AfterStop::Close(id) => window::close(id),
        }
    }

    fn kick_next_install(&mut self) -> Task<Message> {
        // Find the first NotFound step
        let next = self
            .steps
            .iter()
            .find(|(s, st)| *s != Step::Done && matches!(st, StepStatus::NotFound(_)))
            .map(|(s, _)| *s);

        let Some(step) = next else {
            // Nothing left to install — we're done!
            self.installing = false;
            self.step_started = None;
            if let Some((_, s)) = self.steps.iter_mut().find(|(s, _)| *s == Step::Done) {
                *s = StepStatus::Done("Sapphire is ready! Hit Launch.".to_string());
            }
            self.log_lines.push("All done! Sapphire is installed and ready.".to_string());
            return Task::none();
        };

        // Mark it as Installing
        if let Some((_, s)) = self.steps.iter_mut().find(|(st, _)| *st == step) {
            *s = StepStatus::Installing;
        }
        self.step_started = Some(Instant::now());
        self.log_lines.push(format!("[install] {}...", step_label(step)));

        let path = self.install_path.clone();
        let branch = self
            .selected_branch
            .map(|b| b.to_string())
            .unwrap_or_else(|| "Stable".to_string());

        match step {
            Step::Git => Task::perform(install_git(), |(s, st, log)| {
                Message::InstallStepResult(s, st, log)
            }),
            Step::Conda => Task::perform(install_conda(), |(s, st, log)| {
                Message::InstallStepResult(s, st, log)
            }),
            Step::CondaInit => Task::perform(install_conda_init(), |(s, st, log)| {
                Message::InstallStepResult(s, st, log)
            }),
            Step::PythonEnv => Task::perform(install_python_env(), |(s, st, log)| {
                Message::InstallStepResult(s, st, log)
            }),
            Step::Clone => Task::perform(install_clone(path, branch), |(s, st, log)| {
                Message::InstallStepResult(s, st, log)
            }),
            #[cfg(windows)]
            Step::VcRedist => Task::perform(install_vcredist(), |(s, st, log)| {
                Message::InstallStepResult(s, st, log)
            }),
            Step::Deps => Task::perform(install_deps(path, self.install_output.clone()), |(s, st, log)| {
                Message::InstallStepResult(s, st, log)
            }),
            Step::Done => unreachable!(),
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subs: Vec<Subscription<Message>> = Vec::new();

        // Only tick when something is animating
        let anything_busy = self.scanning || self.installing || self.updating
            || self.uninstalling || self.ts_running || self.checking_updates
            || self.sapphire_stopping;
        if anything_busy {
            subs.push(iced::time::every(std::time::Duration::from_millis(200)).map(|_| Message::Tick));
        }

        // Follow service logs via journalctl while in service mode (parks when idle).
        // The epoch id makes a Stop→Start cycle spawn a fresh follower.
        if self.streaming_journal {
            subs.push(Subscription::run_with_id(self.journal_epoch, journal_log_stream()));
        }

        // Spawn-mode follower (when WE launched Sapphire). A Subscription, not a
        // Task::stream, so iced parks it when idle — fixes the redraw CPU-spin.
        if self.streaming_spawn {
            subs.push(Subscription::run_with_id(
                ("spawn", self.spawn_epoch),
                spawn_log_stream(self.install_path.clone(), self.sapphire_pid.clone()),
            ));
        }

        // Periodically detect an out-of-band Sapphire (e.g. started by the Windows
        // autostart task) so the header's Launch/Stop stays accurate. Skipped while we
        // own the process (spawn follower), while stopping, and in Linux service mode.
        if self.service.is_none() && !self.streaming_spawn && !self.sapphire_stopping {
            subs.push(iced::time::every(std::time::Duration::from_secs(5)).map(|_| Message::HealthPoll));
        }

        // Window close goes through us (exit_on_close_request is off) so the
        // Sapphire we launched gets a graceful stop first.
        subs.push(window::close_requests().map(Message::CloseRequested));

        Subscription::batch(subs)
    }

}

// ── Helpers ────────────────────────────────────────────────────────────────

/// The pane/log line for a runner exit. `stopping` = we asked her to stop, so the
/// code is ours (a forced stop reports 1) and the answer is simply "stopped".
/// Otherwise core's exit contract: 0 clean, 43 twin, 44 config, 1 ended from outside
/// (or a crash loop that gave up — hence the hedge), None = killed by a signal.
fn exit_message(result: Result<Option<i32>, String>, stopping: bool) -> String {
    if stopping {
        return "Sapphire stopped.".to_string();
    }
    match result {
        Ok(Some(0)) => "Sapphire shut down.".to_string(),
        Ok(Some(43)) => "Another Sapphire is already running (see the line above). Stop it first.".to_string(),
        Ok(Some(44)) => "Sapphire could not start: configuration error (web host, port or certificate). See the lines above.".to_string(),
        Ok(Some(1)) => "Sapphire ended with code 1. If there is no error above, something outside this app ended it (Task Manager, taskkill).".to_string(),
        Ok(Some(code)) => format!("Sapphire exited (code {}).", code),
        Ok(None) => "Sapphire was ended by a signal.".to_string(),
        Err(e) => e,
    }
}

/// Strip ANSI escape sequences from a string
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip until we hit a letter (the terminator of the escape sequence)
            for c2 in chars.by_ref() {
                if c2.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

// ── Tests: pure helpers + the Stop state machine. No process is ever spawned here:
// the Tasks update() returns are inert until iced polls them, and we drop them. ──
#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sapphire-launcher-test-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn config_round_trips_awkward_paths() {
        let cfg = scratch("config").join("sub").join("launcher.json");
        let awkward = r#"C:\Users\Krem's "lab"\sapphire"#;
        write_config(&cfg, awkward, Branch::Development).unwrap();
        assert_eq!(read_install_path(&cfg).as_deref(), Some(awkward));
        let text = std::fs::read_to_string(&cfg).unwrap();
        assert!(text.contains("\"branch\": \"Development\""), "{}", text);
    }

    #[test]
    fn config_read_is_forgiving() {
        let d = scratch("config-bad");
        assert_eq!(read_install_path(&d.join("missing.json")), None);
        let p = d.join("garbage.json");
        std::fs::write(&p, "{not json").unwrap();
        assert_eq!(read_install_path(&p), None);
        std::fs::write(&p, r#"{"install_path": "   "}"#).unwrap();
        assert_eq!(read_install_path(&p), None);
    }

    #[test]
    fn web_port_reads_settings_or_defaults() {
        let d = scratch("port");
        let install = d.to_string_lossy().into_owned();
        assert_eq!(web_port(&install), DEFAULT_WEB_PORT, "no settings file");
        let settings = d.join("user").join("settings.json");
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        for (body, want) in [
            (r#"{"WEB_UI_PORT": 9000}"#, 9000),
            (r#"{"WEB_UI_PORT": "9001"}"#, 9001),
            (r#"{"WEB_UI_PORT": 0}"#, DEFAULT_WEB_PORT),
            (r#"{"WEB_UI_PORT": 70000}"#, DEFAULT_WEB_PORT),
            (r#"{"WEB_UI_PORT": "lots"}"#, DEFAULT_WEB_PORT),
            (r#"{"OTHER": 1}"#, DEFAULT_WEB_PORT),
            ("{broken", DEFAULT_WEB_PORT),
        ] {
            std::fs::write(&settings, body).unwrap();
            assert_eq!(web_port(&install), want, "settings body: {}", body);
        }
        assert_eq!(web_url(8073), "https://localhost:8073");
    }

    #[test]
    fn exit_wording_follows_the_core_contract() {
        assert_eq!(exit_message(Ok(Some(1)), true), "Sapphire stopped.");
        assert_eq!(exit_message(Ok(None), true), "Sapphire stopped.");
        assert_eq!(exit_message(Ok(Some(0)), false), "Sapphire shut down.");
        assert!(exit_message(Ok(Some(43)), false).contains("Another Sapphire is already running"));
        assert!(exit_message(Ok(Some(44)), false).contains("configuration error"));
        assert!(exit_message(Ok(Some(1)), false).contains("code 1"));
        assert_eq!(exit_message(Ok(Some(7)), false), "Sapphire exited (code 7).");
        assert!(exit_message(Ok(None), false).contains("signal"));
        assert_eq!(exit_message(Err("Failed to start: nope".into()), false), "Failed to start: nope");
    }

    #[test]
    fn strip_ansi_removes_colour_codes() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m plain"), "red plain");
    }

    #[test]
    fn git_branch_detection() {
        let d = scratch("branch");
        assert_eq!(detect_git_branch(&d.to_string_lossy()), Branch::Stable, "no repo → Stable");
        for (branch, want) in [("dev", Branch::Development), ("main", Branch::Stable)] {
            let r = scratch(&format!("branch-{}", branch));
            // A real install is a clone with history; an unborn branch has no HEAD to
            // resolve, so give the test repo one commit.
            let git = |args: &[&str]| std::process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args)
                .current_dir(&r).status().map(|s| s.success()).unwrap_or(false);
            let ok = git(&["init", "-q", "-b", branch]) && git(&["commit", "-q", "--allow-empty", "-m", "init"]);
            if !ok {
                eprintln!("skipped: git init failed");
                return;
            }
            assert_eq!(detect_git_branch(&r.to_string_lossy()), want, "branch {}", branch);
        }
    }

    #[cfg(unix)]
    #[test]
    fn own_process_group_is_readable() {
        assert!(process_group_of(std::process::id()).is_some());
    }

    // ── Stop state machine ────────────────────────────────────────────────
    fn app() -> App {
        let mut a = App::default();
        a.log_lines.clear();
        a.service = None;
        a
    }
    /// She is OURS: spawned by this launcher, follower attached, pid known.
    fn ours(a: &mut App) {
        a.sapphire_pid.store(4242, Ordering::Relaxed);
        a.streaming_spawn = true;
        a.sapphire_running = true;
    }
    fn logged(a: &App, needle: &str) -> bool {
        a.log_lines.iter().any(|l| l.contains(needle))
    }

    #[test]
    fn exit_43_when_not_stopping_marks_not_running() {
        let mut a = app();
        ours(&mut a);
        let _ = a.update(Message::SapphireExited(Ok(Some(43))));
        assert!(!a.sapphire_running && !a.streaming_spawn);
        assert!(logged(&a, "Another Sapphire is already running"));
    }

    #[test]
    fn exit_while_stopping_finishes_the_stop() {
        let mut a = app();
        ours(&mut a);
        a.sapphire_stopping = true;
        let _ = a.update(Message::SapphireExited(Ok(Some(1))));
        assert!(logged(&a, "Sapphire stopped."));
        assert!(!a.sapphire_stopping && !a.sapphire_running && !a.streaming_spawn);
        assert_eq!(a.sapphire_pid.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn late_exit_after_a_forced_stop_is_ignored() {
        let mut a = app();
        a.streaming_spawn = false;
        let _ = a.update(Message::SapphireExited(Ok(Some(1))));
        assert!(a.log_lines.is_empty());
    }

    #[test]
    fn stale_stop_timers_are_ignored() {
        let mut a = app();
        ours(&mut a);
        a.sapphire_stopping = true;
        a.stop_epoch = 5;
        let _ = a.update(Message::StopTimeout(4));
        let _ = a.update(Message::StopStillWaiting(4));
        let _ = a.update(Message::StopSwept(4));
        let _ = a.update(Message::StopSignalSent(false, 4));
        assert!(a.log_lines.is_empty() && a.sapphire_stopping);
    }

    #[test]
    fn begin_stop_runs_once_and_keeps_the_latest_continuation() {
        let mut a = app();
        ours(&mut a);
        let _ = a.begin_stop(AfterStop::None);
        assert!(a.sapphire_stopping && a.stop_epoch == 1 && a.streaming_spawn, "follower stays attached");
        let _ = a.begin_stop(AfterStop::Update);
        assert!(a.stop_epoch == 1 && matches!(a.after_stop, AfterStop::Update));
        let _ = a.begin_stop(AfterStop::None);
        assert!(matches!(a.after_stop, AfterStop::Update), "None never overrides a pending continuation");
        assert_eq!(a.log_lines.iter().filter(|l| l.contains("Stopping Sapphire...")).count(), 1);
    }

    #[test]
    fn forced_stop_finishes_and_runs_the_continuation() {
        let mut a = app();
        ours(&mut a);
        a.sapphire_stopping = true;
        a.stop_epoch = 1;
        a.after_stop = AfterStop::Update;
        let _ = a.update(Message::StopSwept(1));
        assert!(logged(&a, "Sapphire stopped (forced)."));
        assert!(logged(&a, "Starting update"));
        assert!(!a.sapphire_stopping && !a.sapphire_running && matches!(a.after_stop, AfterStop::None));
    }

    #[test]
    fn signal_failure_falls_back_to_force() {
        let mut a = app();
        ours(&mut a);
        a.sapphire_stopping = true;
        a.stop_epoch = 1;
        let _ = a.update(Message::StopSignalSent(false, 1));
        assert!(logged(&a, "forcing"));
    }

    #[test]
    fn close_leaves_a_foreign_sapphire_alone() {
        let mut a = app();
        a.sapphire_running = true; // health-detected, not spawned by us
        let _ = a.update(Message::CloseRequested(window::Id::unique()));
        assert!(!a.sapphire_stopping && !logged(&a, "Closing"));
    }

    #[test]
    fn close_stops_our_sapphire_first() {
        let mut a = app();
        ours(&mut a);
        let _ = a.update(Message::CloseRequested(window::Id::unique()));
        assert!(a.sapphire_stopping && matches!(a.after_stop, AfterStop::Close(_)));
        assert!(logged(&a, "Closing: stopping Sapphire first"));
    }

    #[test]
    fn stop_confirmed_false_keeps_her_shown_as_running() {
        let mut a = app();
        a.sapphire_running = true;
        a.sapphire_stopping = true;
        let _ = a.update(Message::SapphireStopConfirmed(false));
        assert!(a.sapphire_running && !a.sapphire_stopping && logged(&a, "didn't stop"));
    }

    #[test]
    fn launcher_update_is_logged_once_and_kept() {
        let mut a = app();
        let rel = LauncherRelease { version: "9.9.9".into(), url: "https://example.invalid/r".into() };
        let _ = a.update(Message::LauncherReleaseChecked(LauncherCheck::Available(rel.clone())));
        let _ = a.update(Message::LauncherReleaseChecked(LauncherCheck::Available(rel.clone())));
        assert_eq!(a.log_lines.iter().filter(|l| l.contains("newer launcher")).count(), 1);
        assert_eq!(a.launcher_check, LauncherCheck::Available(rel));
        let _ = a.update(Message::LauncherReleaseChecked(LauncherCheck::UpToDate));
        assert_eq!(a.launcher_check, LauncherCheck::UpToDate);
    }

    #[test]
    fn startup_probe_marks_an_external_instance() {
        let mut a = app();
        let _ = a.update(Message::LaunchPreCheck(true));
        assert!(a.sapphire_running);
        assert!(a.sapphire_log.iter().any(|l| l.contains("started outside this app")));
        let mut b = app();
        let _ = b.update(Message::LaunchPreCheck(false));
        assert!(!b.sapphire_running);
    }
}
