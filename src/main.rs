#![windows_subsystem = "windows"]

use iced::widget::scrollable;
#[cfg(not(windows))]
use iced::widget::text_editor;
use iced::{color, Subscription, Task, Theme};
use iced::window;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

mod ui;
mod service;
mod platform;
mod install;
mod troubleshoot;
mod llm;
mod llm_tab;
use crate::platform::*;
use crate::service::*;
use crate::install::*;
use crate::troubleshoot::*;
use crate::llm_tab::{LlmMsg, LlmUi};


// ── Config persistence ─────────────────────────────────────────────────────

fn config_path() -> PathBuf {
    app_config_dir().join("launcher.json")
}

fn load_config() -> (String, Option<Branch>) {
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

/// The branch a checkout is on: None when it's on anything but Stable or Development
/// (a feature branch, a detached commit), which the picker must not dress up as Stable.
/// No checkout yet: Stable, what an install would clone.
fn detect_git_branch(install_path: &str) -> Option<Branch> {
    if !PathBuf::from(install_path).join(".git").exists() {
        return Some(Branch::Stable);
    }
    match git_head_name(install_path).as_deref() {
        Some("dev") => Some(Branch::Development),
        Some("main") => Some(Branch::Stable),
        _ => None,
    }
}

/// `rev-parse --abbrev-ref HEAD`: a branch name, or "HEAD" when detached.
fn git_head_name(install_path: &str) -> Option<String> {
    let mut cmd = hidden_cmd(&find_git());
    cmd.args(["-C", install_path, "rev-parse", "--abbrev-ref", "HEAD"]);
    let out = cmd.output().ok().filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// What to show for a checkout on neither branch.
fn off_branch_name(install_path: &str, branch: Option<Branch>) -> Option<String> {
    if branch.is_some() {
        return None;
    }
    Some(match git_head_name(install_path).as_deref() {
        Some("HEAD") | None => "a specific commit".to_string(),
        Some(name) => name.to_string(),
    })
}

fn save_config(install_path: &str, branch: Branch) {
    // Tests drive PathChanged and friends; they must never touch the real config.
    if cfg!(test) {
        return;
    }
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

    // Autostart's `--start-llm`: start the local LLM with the saved settings, no window.
    if std::env::args().any(|a| a == "--start-llm") {
        llm::headless_start();
        return Ok(());
    }

    // Linux: register the app with the desktop (apps menu / dock icon).
    #[cfg(not(windows))]
    install_desktop_entry();

    let icon = load_window_icon();

    let win_settings = window::Settings {
        size: iced::Size::new(800.0, 550.0),
        icon,
        exit_on_close_request: false,
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
        // Closing the window just closes it: Sapphire is started detached and keeps
        // running (the launcher is an assistant, not a parent). Only the launcher's own
        // long jobs make us ask first; see CloseRequested.
        .run_with(|| {
            let app = App::default();
            let path = app.install_path.clone();
            let branch = app.selected_branch;
            #[cfg(windows)]
            let shim_path = path.clone();
            #[allow(unused_mut)] // Windows pushes the autostart check below
            let mut tasks = vec![
                // Detect a systemd --user service (Linux) → flips into service mode.
                // Without one, ServiceDetected asks whether she is running anyway.
                Task::perform(
                    async { tokio::task::spawn_blocking(detect_sapphire_service).await.unwrap_or(None) },
                    Message::ServiceDetected,
                ),
                // Is systemd --user available at all? (gates the Autostart tab's Enable)
                Task::perform(
                    async { tokio::task::spawn_blocking(systemd_user_available).await.unwrap_or(false) },
                    Message::SystemdChecked,
                ),
                // Check for updates (Sapphire's repo, and the launcher's own releases)
                Task::perform(
                    check_for_updates(path, branch),
                    Message::UpdatesAvailable,
                ),
                Task::perform(check_launcher_release(), Message::LauncherReleaseChecked),
                // The local LLM: what this computer has, and whether it is running.
                Task::done(Message::Llm(LlmMsg::Poll)),
                Task::perform(llm::probe(app.llm.cfg.engine.clone()), |h| Message::Llm(LlmMsg::Probed(h))),
                // Auto-scan install status
                Task::done(Message::ScanClicked),
            ];
            // Windows: an autostart shim from before core's 2026-10-16 change can't
            // start her. Regenerate it (no admin needed) and say so in the log.
            #[cfg(windows)]
            tasks.push(Task::perform(
                async move { tokio::task::spawn_blocking(move || repair_autostart_shim(&shim_path)).await.unwrap_or(None) },
                Message::AutostartRepaired,
            ));
            (app, Task::batch(tasks))
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

    let exec = desktop_exec(&exe_str);

    let desktop_path = format!("{}/.local/share/applications/sapphire-launcher.desktop", home);
    // Skip if we've already installed for this exact binary location.
    if let Ok(existing) = std::fs::read_to_string(&desktop_path)
        && existing.contains(&format!("Exec={}", exec))
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
            exec
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

/// A path as a `.desktop` Exec value: double-quoted, with the characters the spec
/// reserves inside quotes escaped (and `%` doubled — it introduces field codes). An
/// AppImage under "~/Apps/My Tools/" stays one argument.
#[cfg(not(windows))]
pub(crate) fn desktop_exec(path: &str) -> String {
    let mut s = String::from("\"");
    for c in path.chars() {
        match c {
            '"' | '`' | '$' | '\\' => { s.push('\\'); s.push(c); }
            '%' => s.push_str("%%"),
            c => s.push(c),
        }
    }
    s.push('"');
    s
}

// ── App state ──────────────────────────────────────────────────────────────

struct App {
    install_path: String,
    selected_branch: Option<Branch>,
    active_tab: Tab,
    log_visible: bool,
    log_lines: Vec<String>,
    steps: Vec<(Step, StepStatus)>,
    // The one long job running. Its folder is `install_path`, which is frozen until
    // it ends (see `path_frozen`).
    op: Op,
    // Bumped per scan, so results from a scan of another folder are dropped.
    scan_epoch: usize,
    // The one two-click confirmation armed right now, and when.
    armed: Option<(Arm, Instant)>,
    // A branch picked but not yet switched to, and why the last switch failed.
    pending_branch: Option<Branch>,
    branch_note: Option<String>,
    // The branch or commit a checkout is on when it's neither Stable nor Development.
    off_branch: Option<String>,
    // Update
    updates_available: Option<u32>, // None = unknown, Some(0) = up to date, Some(n) = behind
    checking_updates: bool,
    update_unfinished: bool, // an update stopped between new code and new packages
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
    // Who she is — pid, who started her, where her output goes. Some while she runs
    // (or is stopping), None otherwise. Her pid is 0 for a moment after Launch.
    attached: Option<Instance>,
    // When Launch was last clicked: a double-click's second click lands on Stop.
    launched_at: Option<Instant>,
    // Linux systemd --user service (None on Windows / when no unit is detected)
    service: Option<ServiceInfo>,
    // While true, a Subscription follows journalctl for live service logs.
    streaming_journal: bool,
    // Bumped on each (re)start so iced treats the follower as a fresh subscription.
    journal_epoch: usize,
    // We hold the Child of a Sapphire this launcher started, so her exit notice comes
    // to us. A Subscription keyed on child_epoch starts her (detached) and waits.
    holding_child: bool,
    child_epoch: usize,
    // The file tail feeding the Log tab (her launch log, or core's own log for an
    // instance started elsewhere); bumped whenever the source changes.
    tail_epoch: usize,
    // Stop flow (spawn mode): bumped per Stop so late timers/sweeps from an older
    // stop are ignored, and what to do once she is actually down.
    stop_epoch: usize,
    after_stop: AfterStop,
    // Sapphire's web port (user/settings.json WEB_UI_PORT); refreshed with the path.
    web_port: u16,
    // Is there a newer launcher on GitHub? (badge in the header, row on the Update tab)
    launcher_check: LauncherCheck,
    // Autostart tab (Linux): whether systemd --user is available; remove confirm;
    // what an older/hand-written unit is missing (None = fine or no unit).
    systemd_available: bool,
    #[cfg(not(windows))]
    unit_issues: Option<UnitIssues>,
    // Service env vars (the unit's EnvironmentFile), edited in the manage view.
    #[cfg(not(windows))]
    env_content: text_editor::Content,
    // Live install output (streamed by run_cmd_streamed, drained on Tick) + step timer.
    install_output: Arc<Mutex<Vec<String>>>,
    step_started: Option<Instant>,
    // The local LLM (LLM tab).
    llm: LlmUi,
}

impl Default for App {
    fn default() -> Self {
        let (install_path, branch) = load_config();
        let port = web_port(&install_path);
        let off_branch = off_branch_name(&install_path, branch);
        let update_unfinished = update_unfinished(&update_marker_path(), &install_path);

        Self {
            install_path,
            selected_branch: branch,
            active_tab: Tab::default(),
            log_visible: true,
            log_lines: vec![format!("Sapphire Launcher v{} ({}). Ready.", env!("CARGO_PKG_VERSION"), std::env::consts::OS)],
            steps: ALL_STEPS
                .iter()
                .map(|s| (*s, StepStatus::NotStarted))
                .collect(),
            op: Op::Idle,
            scan_epoch: 0,
            armed: None,
            pending_branch: None,
            branch_note: None,
            off_branch,
            spinner_tick: 0,
            updates_available: None,
            checking_updates: false,
            update_unfinished,
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
            sapphire_running: false,
            sapphire_stopping: false,
            sapphire_log: Vec::new(),
            attached: None,
            launched_at: None,
            service: None,
            streaming_journal: false,
            journal_epoch: 0,
            holding_child: false,
            child_epoch: 0,
            tail_epoch: 0,
            stop_epoch: 0,
            after_stop: AfterStop::None,
            web_port: port,
            launcher_check: LauncherCheck::Pending,
            systemd_available: false,
            #[cfg(not(windows))]
            unit_issues: None,
            #[cfg(not(windows))]
            env_content: text_editor::Content::new(),
            install_output: Arc::new(Mutex::new(Vec::new())),
            step_started: None,
            llm: LlmUi::new(),
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
    Llm,
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

/// The long job the launcher is running: one at a time, and every button that could
/// start another asks `refuse_if_busy` / `idle()` first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Op {
    #[default]
    Idle,
    Scanning,
    Installing,
    Updating,
    Uninstalling,
    Fixing,
    SwitchingBranch,
}

impl Op {
    fn words(self) -> &'static str {
        match self {
            Op::Idle => "current job",
            Op::Scanning => "scan",
            Op::Installing => "install",
            Op::Updating => "update",
            Op::Uninstalling => "uninstall",
            Op::Fixing => "repair",
            Op::SwitchingBranch => "branch switch",
        }
    }
    /// Stopping partway through this would leave the install half-changed.
    fn unsafe_to_close(self) -> bool {
        !matches!(self, Op::Idle | Op::Scanning)
    }
}

/// A two-click confirmation. A second click counts only after a beat (so a double-click
/// can't be both clicks) and before it expires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arm {
    RemoveEnv,
    DeleteFolder,
    DeleteUserData,
    ResetKeys,
    ResetPassword,
    RemoveService,
    Sweep,
    CloseWindow,
    LlmDelete,
}

const ARM_MIN: std::time::Duration = std::time::Duration::from_secs(1);
const ARM_SECS: u64 = 10;

/// What a Stop was for — runs once Sapphire is actually down. (Closing the window
/// is not one: she keeps running.)
#[derive(Debug, Clone, Copy, Default)]
enum AfterStop {
    #[default]
    None,
    Update,
    SwitchBranch,
}

#[derive(Debug, Clone)]
enum Message {
    Tick,
    PathChanged(String),
    PathPicked(String), // from the Browse dialog: PathChanged + a scan
    BranchSelected(Branch),
    BrowsePath,
    Launch,
    StopSapphire,
    OpenBrowser,
    BranchConfirm,
    BranchCancel,
    SwitchBranch,
    SwitchBranchResult(Branch, String, bool),
    SwitchDepsDone(String, bool),
    ArmExpired,
    CloseRequested(window::Id),
    TabSelected(Tab),
    ToggleLog,
    // Install flow
    ScanClicked,
    GoClicked,
    StepResult(usize, Step, StepStatus), // scan epoch, step, status
    InstallStepResult(Step, StepStatus, String), // step, status, log output
    Log(String),
    CopyRunLog,
    OpenRunLog,
    ScrollRunLog,
    // Launch
    InstancePoll,                    // startup + every 5 s: is a Sapphire running on this install?
    InstanceFound(Option<Instance>), // the answer (None = nobody we can vouch for)
    TwinLookup(Option<Instance>),    // the same question after a Launch exited 43
    SapphireStarted(u32),            // our detached runner is up: its pid
    SapphireLines(Vec<String>), // a coalesced batch of log lines (burst-safe)
    SapphireExited(Result<Option<i32>, String>), // runner ended: Ok(exit code) / Err(never started)
    SapphireStopConfirmed(bool),  // pid watch / health probe after a stop: false = still up
    StopSignalSent(bool, usize),  // (sent_ok, stop_epoch): the graceful signal went out (or not)
    StopStillWaiting(usize),      // 15 s into a stop: reassure the user
    StopTimeout(usize),           // 100 s into a stop: force it
    StopSwept(usize, bool),       // the hard kill finished; true = she is still there
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
    #[cfg(not(windows))]
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
    #[cfg(not(windows))]
    UnitChecked(Option<UnitIssues>),   // unit health (Linux): what the unit file is missing
    #[cfg(not(windows))]
    UnitPatch,                         // add the missing lines to the unit file
    #[cfg(not(windows))]
    UnitPatched(String, bool),
    #[cfg(windows)]
    AutostartRepaired(Option<String>), // startup shim check: a note for the log, if any
    #[cfg(windows)]
    AutostartRepoint,                  // start her from the folder on screen
    #[cfg(not(windows))]
    EnvLoaded(String),
    #[cfg(not(windows))]
    EnvEdit(text_editor::Action),
    #[cfg(not(windows))]
    EnvSaveRestart,
    #[cfg(not(windows))]
    EnvSaved(bool),
    // Local LLM
    Llm(LlmMsg),
}

// ── Sapphire's web endpoint ────────────────────────────────────────────────

const DEFAULT_WEB_PORT: u16 = 8073;

/// Sapphire's web UI port: `server.WEB_UI_PORT` in `<install>/user/settings.json`
/// (core nests it under `server`; a top-level key is honoured too), number or numeric
/// string, else core's default 8073. Anything unreadable or odd → default.
fn web_port(install_path: &str) -> u16 {
    let path = PathBuf::from(install_path).join("user").join("settings.json");
    let Ok(txt) = std::fs::read_to_string(path) else { return DEFAULT_WEB_PORT };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(strip_bom(&txt)) else { return DEFAULT_WEB_PORT };
    let raw = v.get("server").and_then(|s| s.get("WEB_UI_PORT")).or_else(|| v.get("WEB_UI_PORT"));
    let port = match raw {
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
    ]).await.is_ok_and(|b| health_says_ok(&b))
}

/// Core answers `{"status": "ok", ...}`. Another app on the port that merely has "ok"
/// somewhere in its reply is not her.
fn health_says_ok(body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body)
        .is_ok_and(|v| v.get("status").and_then(|s| s.as_str()) == Some("ok"))
}

/// Poll the health endpoint every 500 ms until it stops answering (true) or `tries`
/// run out (false). Confirms a stop when there is no pid to watch (service mode, or
/// an old core found on the web port only).
async fn wait_health_down(port: u16, tries: usize) -> bool {
    for _ in 0..tries {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if !health_up(port, 2).await {
            return true;
        }
    }
    false
}

/// Is a Sapphire running on this install? `find_instance` (core's lock + our
/// handshake, pid identity-checked) first — it sees her during boot, when the port
/// is blind. Only a real checkout of a core with no lock at all (Stable, as of this
/// writing) asks the web port, and a yes is an instance with pid 0: shown as running,
/// followed through core's log, stopped by the sweep after a confirmation. No lock file
/// anywhere else means she has never run here, whoever answers on that port. Off the UI
/// thread.
async fn instance_lookup(install: String, port: u16) -> Option<Instance> {
    let i = install.clone();
    let (found, may_be_old_core) = tokio::task::spawn_blocking(move || (find_instance(&i), is_lockless_core(&i)))
        .await
        .unwrap_or((None, false));
    match (found, may_be_old_core) {
        (Some(inst), _) => Some(inst),
        (None, false) => None,
        (None, true) => health_up(port, 3).await.then(|| Instance {
            pid: 0,
            since: String::new(),
            origin: Origin::Elsewhere,
            log: core_log_path(&install),
        }),
    }
}

/// `pip install -r` for this folder's requirements, as the last step of a code change:
/// on success it clears the "unfinished" marker. Streamed: a torch bump is minutes of
/// download, not a frozen spinner.
async fn install_requirements(path: String, sink: Arc<Mutex<Vec<String>>>, failed: &str) -> (String, bool) {
    let req = PathBuf::from(&path).join("requirements.txt");
    match pip_install_streamed(vec!["-r".into(), req.to_string_lossy().to_string()], sink.clone()).await {
        Ok(_) => {
            let _ = std::fs::remove_file(update_marker_path());
            ("Dependencies updated.".to_string(), true)
        }
        Err(e) => (pip_failure(failed, &sink, &e), false),
    }
}

/// A Sapphire checkout whose core predates the instance lock and has none on disk.
fn is_lockless_core(install: &str) -> bool {
    looks_like_install(install)
        && !PathBuf::from(install).join("core").join("single_instance.py").exists()
        && !has_lock_file(install)
}

/// `msg`, `secs` seconds from when iced starts the Task. The sleep is built inside
/// the future: `tokio::time::sleep` wants a runtime the moment it is called, and
/// `update()` has one only because iced enters it (the tests don't).
fn after(secs: u64, msg: Message) -> Task<Message> {
    Task::perform(
        async move { tokio::time::sleep(std::time::Duration::from_secs(secs)).await },
        move |_| msg.clone(),
    )
}

/// Watch a pid we signalled until it is gone (true) or `tries` × 500 ms run out
/// (false). Gone = no longer alive-and-hers, so a recycled number reads as gone.
/// Waits a moment after, so the log tail has shown her last lines.
async fn wait_pid_gone(pid: u32, tries: usize) -> bool {
    for _ in 0..tries {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if !tokio::task::spawn_blocking(move || pid_is_hers(pid)).await.unwrap_or(true) {
            tokio::time::sleep(std::time::Duration::from_millis(700)).await;
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
                self.drain_install_output();
                Task::none()
            }
            Message::PathChanged(path) => {
                // The field is read-only while frozen; a Browse dialog opened before
                // the job started can still land here.
                if self.path_frozen() {
                    self.log_lines.push(format!("Wait for the {} to finish before changing folders.", self.op.words()));
                    return Task::none();
                }
                if !path.is_empty() {
                    self.install_path = path;
                    self.web_port = web_port(&self.install_path);
                    // The picker must show the branch THIS folder is on.
                    self.selected_branch = detect_git_branch(&self.install_path);
                    self.off_branch = off_branch_name(&self.install_path, self.selected_branch);
                    self.pending_branch = None;
                    self.branch_note = None;
                    self.update_unfinished = update_unfinished(&update_marker_path(), &self.install_path);
                    save_config(&self.install_path, self.selected_branch.unwrap_or(Branch::Stable));
                    // An armed "YES, delete" must never carry over to a new folder,
                    // and the scan on screen was for the old one.
                    self.disarm_confirms();
                    self.let_go_of_found();
                    self.follow_unit_for_this_folder();
                    if self.op == Op::Scanning {
                        self.op = Op::Idle;
                    }
                    self.scan_epoch += 1;
                    for (_, status) in &mut self.steps { *status = StepStatus::NotStarted; }
                }
                Task::none()
            }
            Message::PathPicked(path) => {
                let frozen = self.path_frozen();
                let _ = self.update(Message::PathChanged(path));
                if frozen {
                    return Task::none();
                }
                self.update(Message::ScanClicked)
            }
            Message::BranchSelected(branch) => {
                if self.refuse_if_busy() {
                    return Task::none();
                }
                self.branch_note = None;
                // Nothing here yet: this is simply what an install would download.
                if !looks_like_install(&self.install_path) {
                    self.selected_branch = Some(branch);
                    save_config(&self.install_path, branch);
                    return Task::none();
                }
                // A real install: changing it stops her and swaps code, so the picker
                // only proposes. The banner explains and confirms.
                self.pending_branch = (Some(branch) != self.selected_branch).then_some(branch);
                Task::none()
            }
            Message::BranchCancel => {
                self.pending_branch = None;
                self.branch_note = None;
                Task::none()
            }
            Message::BranchConfirm => {
                if self.pending_branch.is_none() || self.refuse_if_busy() {
                    return Task::none();
                }
                if self.sapphire_running {
                    return self.begin_stop(AfterStop::SwitchBranch);
                }
                self.update(Message::SwitchBranch)
            }
            Message::BrowsePath => {
                if self.path_frozen() {
                    return Task::none();
                }
                Task::perform(async {
                    rfd::AsyncFileDialog::new()
                        .set_title("Choose Sapphire install folder")
                        .pick_folder()
                        .await
                        .map(|f| f.path().to_string_lossy().to_string())
                }, |result| {
                    match result {
                        Some(path) => Message::PathPicked(path),
                        None => Message::Log("Browse cancelled.".into()),
                    }
                })
            }
            Message::Launch => {
                if self.sapphire_running {
                    self.log_lines.push("Sapphire is already running.".to_string());
                    return Task::none();
                }
                if self.refuse_if_busy() {
                    return Task::none();
                }

                // Already up? Attach instead of starting a twin, whose start would
                // wipe the log she is writing to. A unit mid-start is handled below.
                if !self.unit_here().is_some_and(|s| s.holds_her())
                    && let Some(her) = find_instance(&self.install_path)
                {
                    self.active_tab = Tab::Running;
                    return self.update(Message::InstanceFound(Some(her)));
                }

                // Service mode: start the systemd unit + follow journalctl instead.
                if self.unit_here().is_some() {
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

                // Start her. If one we could not see holds this user/, core says so and
                // exits 43 within a second; SapphireExited then goes looking for her.
                // The child follower is a Subscription keyed on child_epoch: flipping
                // holding_child on (and bumping the epoch) starts her, detached.
                // SapphireStarted brings her pid and starts the log tail.
                self.sapphire_running = true;
                self.sapphire_log.clear();
                self.active_tab = Tab::Running;
                self.log_lines.push("Launching Sapphire...".to_string());
                self.attached = Some(Instance { pid: 0, since: String::new(), origin: Origin::Launcher, log: launch_log_path() });
                self.holding_child = true;
                self.child_epoch += 1;
                self.launched_at = Some(Instant::now());
                Task::none()
            }
            Message::SapphireStarted(pid) => {
                // Our detached runner is up and her log file is fresh: follow it.
                if self.holding_child
                    && let Some(i) = self.attached.as_mut()
                {
                    i.pid = pid;
                    self.tail_epoch += 1;
                }
                Task::none()
            }
            Message::StopSapphire => {
                // The header buttons swap in place, so a double-click on Launch lands
                // its second click here. Ignore a Stop inside the first second.
                if self.launched_at.is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(1)) {
                    self.log_lines.push("Sapphire is still starting — give it a second before stopping.".to_string());
                    return Task::none();
                }
                self.begin_stop(AfterStop::None)
            }
            Message::OpenBrowser => {
                open_url(&web_url(self.web_port));
                Task::none()
            }
            Message::SwitchBranch => {
                let Some(target) = self.pending_branch else { return Task::none() };
                if self.refuse_if_busy() {
                    return Task::none();
                }
                if let Some(why) = self.env_in_use() {
                    self.branch_note = Some(why);
                    return Task::none();
                }
                self.op = Op::SwitchingBranch;
                self.branch_note = None;
                let branch = if target == Branch::Development { "dev" } else { "main" };
                self.log_lines.push(format!("Switching to {} branch...", branch));
                let path = self.install_path.clone();
                let b = branch.to_string();
                Task::perform(
                    async move {
                        run_cmd_full_async(find_git(), vec!["-C".into(), path, "checkout".into(), b.clone()]).await
                            .map(|out| (format!("Switched to {}: {}", b, out), true))
                            .unwrap_or_else(|e| (e, false))
                    },
                    move |(msg, ok)| Message::SwitchBranchResult(target, msg, ok),
                )
            }
            Message::SwitchBranchResult(target, msg, ok) => {
                self.log_lines.push(msg.clone());
                self.pending_branch = None;
                if !ok {
                    // The picker must show the branch we are really on.
                    self.op = Op::Idle;
                    self.selected_branch = detect_git_branch(&self.install_path);
                    self.off_branch = off_branch_name(&self.install_path, self.selected_branch);
                    self.branch_note = Some(if msg.contains("would be overwritten") {
                        "Nothing changed: you have edited files this switch would overwrite.".to_string()
                    } else {
                        plain_failure("Couldn't switch, so nothing changed.", &msg)
                    });
                    return Task::none();
                }
                self.selected_branch = Some(target);
                self.off_branch = None;
                save_config(&self.install_path, target);
                // New code wants its own packages. Installed in the same job, marked
                // unfinished until they're in, like an update.
                self.log_lines.push(format!("Installing the packages {} needs...", target));
                self.update_unfinished = true;
                let path = self.install_path.clone();
                let sink = self.install_output.clone();
                if let Ok(mut v) = sink.lock() { v.clear(); }
                Task::perform(
                    async move {
                        let _ = std::fs::create_dir_all(app_config_dir());
                        let _ = std::fs::write(update_marker_path(), &path);
                        install_requirements(path, sink, "Switched branches, but installing its packages failed.").await
                    },
                    |(msg, ok)| Message::SwitchDepsDone(msg, ok),
                )
            }
            Message::SwitchDepsDone(msg, ok) => {
                self.op = Op::Idle;
                self.drain_install_output();
                self.log_lines.push(msg.clone());
                if ok {
                    self.update_unfinished = false;
                } else {
                    self.branch_note = Some(format!("{} Click Update to try again.", msg));
                }
                // The badge counted commits behind the OLD branch — count again.
                self.updates_available = None;
                self.update(Message::CheckForUpdates)
            }
            Message::ArmExpired => {
                if self.armed.is_some_and(|(_, at)| at.elapsed().as_secs() >= ARM_SECS) {
                    self.armed = None;
                }
                Task::none()
            }
            Message::CloseRequested(id) => {
                if !self.op.unsafe_to_close() || self.confirm(Arm::CloseWindow) {
                    return window::close(id);
                }
                self.log_lines.push(format!("The {} is still running. Closing now can leave Sapphire half-changed. Close again to close anyway.", self.op.words()));
                after(ARM_SECS, Message::ArmExpired)
            }

            // ── Update flow ───────────────────────────────────────────
            Message::CheckForUpdates => {
                if self.checking_updates { return Task::none(); }
                self.checking_updates = true;
                let path = self.install_path.clone();
                let branch = self.selected_branch;
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
                if self.refuse_if_busy() {
                    return Task::none();
                }
                if !looks_like_install(&self.install_path) {
                    self.log_lines.push(format!("{} doesn't hold a Sapphire install, so there's nothing to update.", self.install_path));
                    return Task::none();
                }
                if self.sapphire_running || self.sapphire_stopping {
                    // Stop her first; UpdateAfterStop continues once she is down.
                    return self.begin_stop(AfterStop::Update);
                }
                if let Some(why) = self.env_in_use() {
                    self.log_lines.push(why);
                    return Task::none();
                }
                self.op = Op::Updating;
                // Reset steps
                for (_, s) in &mut self.update_status {
                    *s = StepStatus::NotStarted;
                }
                // Step 1: git pull
                self.update_status[0].1 = StepStatus::Installing;
                self.log_lines.push("[update] Pulling latest changes...".to_string());
                self.update_unfinished = true;
                let path = self.install_path.clone();
                let sink = self.install_output.clone();
                if let Ok(mut v) = sink.lock() { v.clear(); }
                Task::perform(
                    async move {
                        let _ = std::fs::create_dir_all(app_config_dir());
                        let _ = std::fs::write(update_marker_path(), &path);
                        update_code(path, sink).await
                    },
                    |(msg, ok)| Message::UpdateStepDone(msg, ok),
                )
            }
            Message::UpdateAfterStop => {
                // finish_stop already cleared the running state; the pane keeps her
                // last lines (the teardown is worth seeing).
                self.log_lines.push("Sapphire stopped. Starting update...".to_string());
                self.update(Message::UpdateClicked)
            }
            Message::UpdateStepDone(msg, success) => {
                self.drain_install_output(); // the last pip lines (the error, usually)
                self.log_lines.push(format!("[update] {}", msg));

                // Figure out which step just finished
                let current_step = self.update_status.iter().position(|(_, s)| *s == StepStatus::Installing);

                if !success {
                    if let Some(i) = current_step {
                        self.update_status[i].1 = StepStatus::Failed(msg);
                    }
                    self.op = Op::Idle;
                    self.log_lines.push("[update] Stopped. Click Update to try again.".to_string());
                    return Task::none();
                }

                if let Some(i) = current_step {
                    self.update_status[i].1 = StepStatus::Done(msg);

                    // Kick next step
                    if i == 0 {
                        // Step 2: pip install -r requirements.txt
                        self.update_status[1].1 = StepStatus::Installing;
                        self.log_lines.push("[update] Updating dependencies...".to_string());
                        let path = self.install_path.clone();
                        let sink = self.install_output.clone();
                        if let Ok(mut v) = sink.lock() { v.clear(); }
                        return Task::perform(
                            async move { install_requirements(path, sink, "Installing the update's packages failed.").await },
                            |(msg, ok)| Message::UpdateStepDone(msg, ok),
                        );
                    }
                }

                // All done
                self.op = Op::Idle;
                self.log_lines.push("[update] Update complete!".to_string());
                self.updates_available = Some(0);
                self.update_unfinished = false;
                Task::none()
            }
            Message::SapphireLines(lines) => {
                self.sapphire_log.extend(lines);
                self.cap_sapphire_log();
                Task::none()
            }
            Message::SapphireExited(result) => {
                // Only the Child we hold reports exits; anything else is stale.
                if !self.holding_child {
                    return Task::none();
                }
                let twin = !self.sapphire_stopping && matches!(result, Ok(Some(43)));
                let msg = exit_message(result, self.sapphire_stopping);
                self.sapphire_log.push(format!("--- {} ---", msg));
                self.log_lines.push(msg);
                if self.sapphire_stopping {
                    self.finish_stop()
                } else {
                    self.sapphire_running = false;
                    self.holding_child = false;
                    self.attached = None;
                    if twin {
                        // She is up and we have a way to reach her: get the Stop button back.
                        return Task::perform(instance_lookup(self.install_path.clone(), self.web_port), Message::TwinLookup);
                    }
                    Task::none()
                }
            }
            Message::TwinLookup(found) => {
                if found.is_some() {
                    return self.update(Message::InstanceFound(found));
                }
                self.log_lines.push("Couldn't find that Sapphire from here. It may belong to another Windows or Linux user, or another app may be using her web port. See the line above.".to_string());
                Task::none()
            }
            Message::SapphireStopConfirmed(stopped) => {
                if !self.sapphire_stopping {
                    return Task::none();
                }
                if stopped {
                    self.log_lines.push("Sapphire stopped.".to_string());
                    return self.finish_stop();
                }
                // Still there after the grace window (e.g. an elevated instance we
                // can't touch). Keep her shown as running so the button stays Stop.
                self.sapphire_stopping = false;
                self.sapphire_running = true;
                self.after_stop = AfterStop::None;
                self.log_lines.push("Sapphire didn't stop — it may be running with admin rights. Try Stop again.".to_string());
                Task::none()
            }
            Message::StopSignalSent(sent, epoch) => {
                if !self.sapphire_stopping || epoch != self.stop_epoch {
                    return Task::none();
                }
                if !sent {
                    self.log_lines.push("Couldn't signal Sapphire to stop — forcing it.".to_string());
                    return self.force_stop(epoch);
                }
                // She got the signal. Her end reaches us as SapphireExited when we hold
                // her Child, else as SapphireStopConfirmed from a pid watch (normally
                // ~8 s either way). Reassure at 15 s, force at 100 s (core's worst case).
                let mut tasks = vec![
                    after(15, Message::StopStillWaiting(epoch)),
                    after(100, Message::StopTimeout(epoch)),
                ];
                if !self.holding_child
                    && let Some(pid) = self.attached.as_ref().map(|i| i.pid).filter(|p| *p > 0)
                {
                    // Outlasts the force timer on purpose: whichever notices her end
                    // first finishes the stop; the other finds it over and is ignored.
                    tasks.push(Task::perform(wait_pid_gone(pid, 220), Message::SapphireStopConfirmed));
                }
                Task::batch(tasks)
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
            Message::StopSwept(epoch, alive) => {
                // With her Child held, the exit notice normally finished the stop
                // seconds ago and this is stale. Otherwise the kill's own look says
                // whether she is gone — and a Sapphire still there 3 s after a
                // SIGKILL/taskkill survived it: say so instead of pretending.
                if !self.sapphire_stopping || epoch != self.stop_epoch {
                    return Task::none();
                }
                if !alive {
                    self.log_lines.push("Sapphire stopped (forced).".to_string());
                    return self.finish_stop();
                }
                self.sapphire_stopping = false;
                self.sapphire_running = true;
                self.after_stop = AfterStop::None;
                self.log_lines.push("Couldn't force Sapphire to stop — she may be running with admin rights or as another user. Check for python processes yourself.".to_string());
                Task::none()
            }
            Message::InstancePoll => {
                // Skip while our own flows own the running state.
                if self.holding_child || self.sapphire_stopping {
                    return Task::none();
                }
                // Service mode: ask systemd — the unit is the truth there.
                let lookup = Task::perform(instance_lookup(self.install_path.clone(), self.web_port), Message::InstanceFound);
                if self.service.is_none() {
                    return lookup;
                }
                let holds = self.unit_here().is_some_and(|u| u.holds_her());
                let refresh = Task::perform(
                    async { tokio::task::spawn_blocking(detect_sapphire_service).await.unwrap_or(None) },
                    Message::ServiceRefreshed,
                );
                // A unit that isn't running her doesn't rule out a Sapphire started
                // some other way: Launch would just exit 43 against her.
                if holds { refresh } else { Task::batch([refresh, lookup]) }
            }
            Message::InstanceFound(found) => {
                if self.holding_child || self.sapphire_stopping {
                    return Task::none();
                }
                // With a unit: it owns her while it runs her. Otherwise only a pid is
                // worth attaching to; the no-pid sweep is not for a box with a unit.
                if let Some(unit) = self.unit_here() {
                    if unit.holds_her() {
                        return Task::none();
                    }
                    if found.as_ref().is_some_and(|i| i.pid == 0) {
                        return Task::none();
                    }
                }
                let same = match (&found, &self.attached) {
                    (Some(i), Some(c)) => c.pid == i.pid && c.log == i.log,
                    _ => false,
                };
                match found {
                    Some(_) if same => self.sapphire_running = true, // the same her, still there
                    Some(inst) => {
                        // Found her (autostart, a terminal, an earlier launcher): say
                        // who she is and follow her log.
                        self.sapphire_running = true;
                        self.streaming_journal = false;
                        self.sapphire_log.clear();
                        self.log_lines.push(format!("{}.", inst.describe()));
                        self.attached = Some(inst);
                        self.tail_epoch += 1;
                    }
                    None if self.attached.is_some() => {
                        self.sapphire_running = false;
                        self.attached = None;
                        self.sapphire_log.push("--- Sapphire is no longer running. ---".to_string());
                        self.log_lines.push("Sapphire is no longer running.".to_string());
                    }
                    None => {}
                }
                Task::none()
            }
            Message::TabSelected(tab) => {
                self.active_tab = tab;
                self.disarm_confirms();
                // Auto-check for updates when opening Update tab
                if tab == Tab::Update {
                    return self.update(Message::CheckForUpdates);
                }
                Task::none()
            }
            Message::Llm(m) => self.llm_update(m),
            Message::ToggleLog => {
                self.log_visible = !self.log_visible;
                Task::none()
            }

            // ── Install flow ───────────────────────────────────────────
            Message::ScanClicked => {
                if !matches!(self.op, Op::Idle | Op::Scanning) {
                    return Task::none();
                }
                self.op = Op::Scanning;
                self.scan_epoch += 1;
                let epoch = self.scan_epoch;
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
                    Task::perform(check_git(), move |(step, status)| {
                        Message::StepResult(epoch, step, status)
                    }),
                    Task::perform(check_conda(), move |(step, status)| {
                        Message::StepResult(epoch, step, status)
                    }),
                    Task::perform(check_conda_init(), move |(step, status)| {
                        Message::StepResult(epoch, step, status)
                    }),
                    Task::perform(check_python_env(), move |(step, status)| {
                        Message::StepResult(epoch, step, status)
                    }),
                    Task::perform(check_clone(path.clone()), move |(step, status)| {
                        Message::StepResult(epoch, step, status)
                    }),
                    Task::perform(check_deps(path), move |(step, status)| {
                        Message::StepResult(epoch, step, status)
                    }),
                ];
                #[cfg(windows)]
                checks.push(Task::perform(check_vcredist(), move |(step, status)| {
                    Message::StepResult(epoch, step, status)
                }));
                Task::batch(checks)
            }

            Message::StepResult(epoch, step, status) => {
                // From a scan of another folder, or overtaken by a newer one.
                if epoch != self.scan_epoch {
                    return Task::none();
                }
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
                    if self.op == Op::Scanning {
                        self.op = Op::Idle;
                    }
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
                // A scan in flight would overwrite the steps this is about to run.
                if self.op == Op::Scanning || self.refuse_if_busy() {
                    return Task::none();
                }
                // Before any slow step: a folder that can't take a clone stops here.
                if let Some(why) = clone_blocker(&self.install_path) {
                    self.log_lines.push(why);
                    return Task::none();
                }
                if let Some(why) = self.env_in_use() {
                    self.log_lines.push(why);
                    return Task::none();
                }
                self.op = Op::Installing;
                if let Ok(mut v) = self.install_output.lock() { v.clear(); }

                self.log_lines.push("Starting install...".to_string());

                // Find the first step that needs work
                self.kick_next_install()
            }

            Message::InstallStepResult(step, status, log_output) => {
                self.drain_install_output(); // the last pip lines (the error, usually)
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
                    self.op = Op::Idle;
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
                // Two clicks, and only while she is down, same as the API keys.
                if self.refuse_if_busy() {
                    return Task::none();
                }
                if let Some(why) = self.uninstall_blocker(false, false) {
                    self.disarm(Arm::ResetPassword);
                    self.log_lines.push(why);
                    return Task::none();
                }
                if !self.confirm(Arm::ResetPassword) {
                    return after(ARM_SECS, Message::ArmExpired);
                }
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
                // Two clicks, and only while she is down (a running Sapphire rewrites
                // the file from memory). Blanks the API keys only — see clear_api_keys.
                if self.refuse_if_busy() {
                    return Task::none();
                }
                if let Some(why) = self.uninstall_blocker(false, false) {
                    self.disarm(Arm::ResetKeys);
                    self.log_lines.push(why);
                    return Task::none();
                }
                if !self.confirm(Arm::ResetKeys) {
                    return after(ARM_SECS, Message::ArmExpired);
                }
                let msg = clear_api_keys_file(&app_config_dir().join("credentials.json"));
                self.log_lines.push(msg.clone());
                Task::done(Message::ResetResult(msg))
            }
            Message::ResetResult(_msg) => {
                Task::none()
            }

            // ── Uninstall flow (two-click confirmation) ─────────────
            Message::UninstallCondaEnvClick => {
                if self.refuse_if_busy() {
                    return Task::none();
                }
                if let Some(why) = self.uninstall_blocker(false, true) {
                    self.disarm(Arm::RemoveEnv);
                    self.log_lines.push(why);
                    return Task::none();
                }
                if !self.confirm(Arm::RemoveEnv) {
                    return after(ARM_SECS, Message::ArmExpired);
                }
                self.op = Op::Uninstalling;

                self.log_lines.push("[uninstall] Removing conda environment 'sapphire'...".to_string());
                Task::perform(uninstall_conda_env(), |(msg, ok)| {
                    Message::UninstallResult(msg, ok)
                })
            }

            Message::UninstallDeleteFolderClick => {
                if self.refuse_if_busy() {
                    return Task::none();
                }
                if let Some(why) = self.uninstall_blocker(true, false) {
                    self.disarm(Arm::DeleteFolder);
                    self.log_lines.push(why);
                    return Task::none();
                }
                if !self.confirm(Arm::DeleteFolder) {
                    return after(ARM_SECS, Message::ArmExpired);
                }
                self.op = Op::Uninstalling;

                let path = self.install_path.clone();
                self.log_lines.push(format!("[uninstall] Deleting {}...", path));
                Task::perform(uninstall_delete_folder(path), |(msg, ok)| {
                    Message::UninstallResult(msg, ok)
                })
            }

            Message::UninstallDeleteUserdataClick => {
                if self.refuse_if_busy() {
                    return Task::none();
                }
                if let Some(why) = self.uninstall_blocker(true, false) {
                    self.disarm(Arm::DeleteUserData);
                    self.log_lines.push(why);
                    return Task::none();
                }
                if !self.confirm(Arm::DeleteUserData) {
                    return after(ARM_SECS, Message::ArmExpired);
                }
                self.op = Op::Uninstalling;

                let path = self.install_path.clone();
                self.log_lines.push(format!("[uninstall] Deleting {}...", PathBuf::from(&path).join("user").display()));
                Task::perform(uninstall_delete_userdata(path), |(msg, ok)| {
                    Message::UninstallResult(msg, ok)
                })
            }

            Message::UninstallResult(msg, success) => {
                self.op = Op::Idle;
                for line in msg.lines() {
                    self.log_lines.push(format!("  {}", line));
                }
                if success {
                    self.log_lines.push("[uninstall] Done.".to_string());
                    // What the Install tab showed is out of date: look again.
                    return self.update(Message::ScanClicked);
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
                    Task::perform(ts_check_deps(path.clone()), |(c, s)| Message::TroubleshootResult(c, s)),
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
                if self.ts_running && !still_checking {
                    self.ts_running = false;
                    self.log_lines.push("[troubleshoot] Done.".to_string());
                }
                Task::none()
            }
            Message::TroubleshootFix(check) => {
                if self.ts_running || self.refuse_if_busy() {
                    return Task::none();
                }
                if self.env_in_use().is_some() {
                    self.log_lines.push("Stop Sapphire before installing packages — she has them open.".to_string());
                    return Task::none();
                }
                if let Some((_, s)) = self.ts_checks.iter_mut().find(|(c, _)| *c == check) {
                    *s = TsStatus::Fixing;
                }
                self.op = Op::Fixing;
                if let Ok(mut v) = self.install_output.lock() { v.clear(); }
                self.log_lines.push(format!("[troubleshoot] Fixing {}...", ts_label(check)));
                let path = self.install_path.clone();
                let sink = self.install_output.clone();
                match check {
                    TsCheck::DepsHealth => Task::perform(ts_fix_deps(path, sink), |(c, msg, ok)| {
                        Message::TroubleshootFixResult(c, msg, ok)
                    }),
                    TsCheck::Plugins => Task::perform(ts_fix_plugins(path, sink), |(c, msg, ok)| {
                        Message::TroubleshootFixResult(c, msg, ok)
                    }),
                    TsCheck::PluginDeps => Task::perform(ts_fix_plugin_deps(path, sink), |(c, msg, ok)| {
                        Message::TroubleshootFixResult(c, msg, ok)
                    }),
                    _ => {
                        self.op = Op::Idle;
                        Task::none()
                    }
                }
            }
            Message::TroubleshootFixResult(check, msg, success) => {
                self.op = Op::Idle;
                self.drain_install_output();
                self.log_lines.push(format!("[troubleshoot] {}", msg));
                if let Some((_, s)) = self.ts_checks.iter_mut().find(|(c, _)| *c == check) {
                    *s = if success { TsStatus::Fixed(msg) } else { TsStatus::Problem(msg) };
                }
                if !success {
                    return Task::none();
                }
                // "Repaired" is a claim — run that one check again to prove it.
                let path = self.install_path.clone();
                match check {
                    TsCheck::DepsHealth => Task::perform(ts_check_deps(path), |(c, s)| Message::TroubleshootResult(c, s)),
                    TsCheck::Plugins => Task::perform(ts_check_plugins(path), |(c, s)| Message::TroubleshootResult(c, s)),
                    TsCheck::PluginDeps => Task::perform(ts_check_plugin_deps(path), |(c, s)| Message::TroubleshootResult(c, s)),
                    _ => Task::none(),
                }
            }

            // ── Service (Linux systemd --user) ────────────────────────
            Message::ServiceDetected(info) => {
                let mut tasks: Vec<Task<Message>> = vec![self.env_reload(&info)];
                self.service = info.clone();
                if self.unit_here().is_none_or(|i| !i.holds_her()) {
                    // No unit running her: is she running anyway (autostart, a terminal,
                    // an earlier launcher)? Asked now, then every 5 s.
                    tasks.push(Task::done(Message::InstancePoll));
                }
                if let Some(info) = info {
                    self.log_lines.push(format!("Detected sapphire.service ({}).", info.sub_state));
                    // Is the unit itself in shape (stop contract, paths still valid)?
                    #[cfg(not(windows))]
                    tasks.push(Task::perform(check_unit(), Message::UnitChecked));
                    // Auto-fill path from the unit's WorkingDirectory, but only if our
                    // current path isn't already a valid install (don't override saved).
                    if let Some(wd) = &info.working_dir {
                        let have_valid = PathBuf::from(&self.install_path).join("main.py").exists();
                        if !have_valid && PathBuf::from(wd).join("main.py").exists() {
                            self.install_path = wd.clone();
                            self.web_port = web_port(&self.install_path);
                            self.selected_branch = detect_git_branch(wd);
                            self.off_branch = off_branch_name(wd, self.selected_branch);
                            save_config(&self.install_path, self.selected_branch.unwrap_or(Branch::Stable));
                            self.log_lines.push(format!("Using service path: {}", wd));
                            tasks.push(Task::done(Message::ScanClicked));
                        }
                    }
                    let holds = self.unit_here().is_some_and(|u| u.holds_her());
                    self.sapphire_running = holds;
                    if holds {
                        // Attach live logs (the journalctl Subscription starts itself).
                        self.sapphire_log.clear();
                        self.streaming_journal = true;
                        self.journal_epoch += 1;
                    }
                }
                Task::batch(tasks)
            }
            Message::ServiceStart => {
                if self.refuse_if_busy() {
                    return Task::none();
                }
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
            #[cfg(not(windows))]
            Message::ServiceStop => self.begin_stop(AfterStop::None),
            #[cfg(not(windows))]
            Message::ServiceRestart => {
                if self.refuse_if_busy() {
                    return Task::none();
                }
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
                // Only a unit running THIS folder says anything about her here.
                let here = info.as_ref().filter(|u| unit_runs(u, &self.install_path));
                // A stop in flight owns the running state until it confirms.
                if let Some(unit) = here
                    && !self.sapphire_stopping
                {
                    // The unit took over from an instance we had found.
                    if unit.holds_her() && !self.holding_child {
                        self.attached = None;
                    }
                    self.sapphire_running = unit.holds_her() || self.attached.is_some();
                    // She came up behind our back (crash restart, a terminal start):
                    // attach the log follower.
                    if unit.holds_her() && !self.streaming_journal {
                        self.sapphire_log.clear();
                        self.streaming_journal = true;
                        self.journal_epoch += 1;
                    }
                }
                #[cfg(not(windows))]
                if info.is_none() {
                    self.unit_issues = None;
                }
                let env_task = self.env_reload(&info);
                self.service = info;
                env_task
            }
            Message::SystemdChecked(available) => {
                self.systemd_available = available;
                Task::none()
            }
            Message::ServiceInstall => {
                if self.refuse_if_busy() {
                    return Task::none();
                }
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
                if self.refuse_if_busy() {
                    return Task::none();
                }
                if !self.confirm(Arm::RemoveService) {
                    return after(ARM_SECS, Message::ArmExpired);
                }
                self.streaming_journal = false;
                self.log_lines.push("[service] Removing autostart...".to_string());
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
            #[cfg(not(windows))]
            Message::UnitChecked(issues) => {
                // Say it once in the log; the Autostart tab carries it from there.
                if let Some(i) = &issues
                    && self.unit_issues != issues
                {
                    for b in &i.broken {
                        self.log_lines.push(format!("[service] sapphire.service is broken: {} Remove it and set it up again from the Autostart tab.", b));
                    }
                    if !i.missing.is_empty() {
                        self.log_lines.push(format!(
                            "[service] sapphire.service predates Sapphire's stop rules (missing {}). The Autostart tab can add that for you.",
                            i.missing.join(", ")
                        ));
                    }
                }
                self.unit_issues = issues;
                Task::none()
            }
            #[cfg(not(windows))]
            Message::UnitPatch => {
                let (fragment, lines) = match &self.unit_issues {
                    Some(i) if !i.missing.is_empty() => (i.fragment.clone(), i.missing.clone()),
                    _ => return Task::none(),
                };
                let Some(fragment) = fragment else {
                    self.log_lines.push("[service] Couldn't find the unit file to update.".to_string());
                    return Task::none();
                };
                self.log_lines.push("[service] Updating the service file...".to_string());
                Task::perform(patch_unit(fragment, lines), |(m, ok)| Message::UnitPatched(m, ok))
            }
            #[cfg(not(windows))]
            Message::UnitPatched(msg, _ok) => {
                self.log_lines.push(format!("[service] {}", msg));
                Task::perform(check_unit(), Message::UnitChecked)
            }
            #[cfg(windows)]
            Message::AutostartRepaired(note) => {
                if let Some(n) = note {
                    self.log_lines.push(n);
                }
                Task::none()
            }
            #[cfg(windows)]
            Message::AutostartRepoint => {
                if looks_like_install(&self.install_path) {
                    self.log_lines.push(repoint_autostart(&self.install_path));
                }
                Task::none()
            }
            #[cfg(not(windows))]
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
                let Some(path) = self.service.as_ref().and_then(|i| i.env_file.clone()) else {
                    self.log_lines.push("[service] This unit has no EnvironmentFile= line, so there is nothing to save to.".to_string());
                    return Task::none();
                };
                self.log_lines.push(format!("[service] Saving {}, then restarting...", path));
                Task::perform(write_env_file(path, self.env_content.text()), Message::EnvSaved)
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

    /// Move any live command output (streamed pip) into the launcher log.
    fn drain_install_output(&mut self) {
        let drained: Vec<String> = match self.install_output.lock() {
            Ok(mut v) => std::mem::take(&mut *v),
            Err(_) => Vec::new(),
        };
        if !drained.is_empty() {
            self.log_lines.extend(drained);
            self.cap_log_lines();
        }
    }

    /// Drop the armed two-click confirmation (tab change, path change).
    fn disarm_confirms(&mut self) {
        self.armed = None;
    }

    fn disarm(&mut self, arm: Arm) {
        if self.armed.is_some_and(|(a, _)| a == arm) {
            self.armed = None;
        }
    }

    /// Is this confirmation armed and not yet expired? (The view asks.)
    fn is_armed(&self, arm: Arm) -> bool {
        self.armed.is_some_and(|(a, at)| a == arm && at.elapsed().as_secs() < ARM_SECS)
    }

    /// A click on a two-click action: true = this is the confirming click, go. A click
    /// too soon after arming is the rest of a double-click and counts for nothing.
    fn confirm(&mut self, arm: Arm) -> bool {
        if self.is_armed(arm) {
            let settled = self.armed.is_some_and(|(_, at)| at.elapsed() >= ARM_MIN);
            if settled {
                self.armed = None;
            }
            return settled;
        }
        self.armed = Some((arm, Instant::now()));
        false
    }

    /// Nothing long-running, and she isn't mid-stop.
    fn idle(&self) -> bool {
        self.op == Op::Idle && !self.sapphire_stopping
    }

    /// The gate on everything that starts work. True = refused, and the log says why.
    /// A scan only reads, and it restarts cleanly, so it gives way.
    fn refuse_if_busy(&mut self) -> bool {
        if self.sapphire_stopping {
            self.log_lines.push("Wait for Sapphire to finish stopping.".to_string());
            return true;
        }
        if !matches!(self.op, Op::Idle | Op::Scanning) {
            self.log_lines.push(format!("Wait for the {} to finish.", self.op.words()));
            return true;
        }
        false
    }

    /// While a job runs, or a stop has work queued behind it, `install_path` is that
    /// job's folder. A scan just gives way to the new folder.
    fn path_frozen(&self) -> bool {
        !matches!(self.op, Op::Idle | Op::Scanning) || self.sapphire_stopping || self.llm.job.is_some()
    }

    /// The unit, if it runs the folder on screen. A unit running another folder says
    /// nothing about this one.
    fn unit_here(&self) -> Option<&ServiceInfo> {
        self.service.as_ref().filter(|u| unit_runs(u, &self.install_path))
    }

    /// The path changed: the unit's say over the running state and the log follows it.
    fn follow_unit_for_this_folder(&mut self) {
        if self.service.is_none() || self.holding_child {
            return;
        }
        let holds = self.unit_here().is_some_and(|u| u.holds_her());
        self.sapphire_running = holds || self.attached.is_some();
        if holds != self.streaming_journal {
            self.streaming_journal = holds;
            self.sapphire_log.clear();
            self.journal_epoch += 1;
        }
    }

    /// A fresh look, not the 5-second-old flag: is she up on this folder?
    fn up_here(&self) -> bool {
        self.sapphire_running || self.sapphire_stopping
            || self.unit_here().is_some_and(|u| u.holds_her())
            || find_instance(&self.install_path).is_some()
    }

    /// Is anything running out of the one shared Python env, from any folder? Her env
    /// must not change under her. Some = why not.
    fn env_in_use(&self) -> Option<String> {
        if self.up_here() {
            return Some("Stop Sapphire first.".to_string());
        }
        let elsewhere = self.service.as_ref().filter(|u| u.holds_her()).map(|u| u.working_dir.clone())
            .or_else(|| {
                let r = read_run_state(&run_state_path())?;
                (pid_look(r.pid) == Some(true)).then_some(r.install)
            })?;
        let from = elsewhere.unwrap_or_else(|| "another folder".to_string());
        Some(format!("Sapphire is running from {}. Pick that folder and stop her first.", from))
    }

    /// A Sapphire we only found (not started) belongs to the folder we found her in:
    /// drop her when the path changes; the next poll looks at the new folder.
    fn let_go_of_found(&mut self) {
        if !self.holding_child && !self.sapphire_stopping && self.attached.is_some() {
            self.attached = None;
            self.sapphire_running = false;
        }
    }

    /// Why a destructive action can't run right now (None = go ahead): she must be
    /// down, and — for the folder deletes — the path must really be a Sapphire install.
    fn uninstall_blocker(&self, needs_install: bool, uses_env: bool) -> Option<String> {
        if self.up_here() {
            return Some("Stop Sapphire first.".to_string());
        }
        if uses_env && let Some(why) = self.env_in_use() {
            return Some(why);
        }
        // Its binary and models live in this folder's user/models/llm/.
        if self.llm.job.is_some() {
            return Some("Wait for the LLM download to finish (or cancel it on the LLM tab).".to_string());
        }
        if self.llm.running().is_some_and(|r| r.exe.starts_with(llm::llm_root(&self.install_path))) {
            return Some("Stop the LLM first (LLM tab).".to_string());
        }
        if needs_install && !can_delete(&self.install_path) {
            return Some(format!("{} doesn't look like a Sapphire install — not touching it.", self.install_path));
        }
        None
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

    /// The one stop path — Stop button, branch switch, update. Signals first
    /// (SIGTERM / CTRL_BREAK, by pid: our Child's or the one core's lock names) and
    /// waits for the runner to be gone; the hard kill is only the fallback. Nothing
    /// here blocks the UI thread.
    fn begin_stop(&mut self, after: AfterStop) -> Task<Message> {
        if self.sapphire_stopping {
            // A stop is already in flight: just update what happens afterwards.
            if !matches!(after, AfterStop::None) {
                self.after_stop = after;
            }
            return Task::none();
        }
        let pid = self.attached.as_ref().map_or(0, |i| i.pid);
        if self.holding_child && pid == 0 {
            // Launched a moment ago and the runner hasn't reported its pid yet: there
            // is nothing to signal, and the sweep would hit every sapphire-env python.
            self.log_lines.push("Sapphire is still starting — try again in a moment.".to_string());
            return Task::none();
        }
        // No pid and no unit: all we have is the sweep, which ends every python in her
        // env. Never silently, whatever asked for this stop.
        if pid == 0 && self.unit_here().is_none() && !self.confirm(Arm::Sweep) {
            self.log_lines.push("Sapphire was started outside this app, so stopping her also ends anything else using her Python environment. Click again to do that.".to_string());
            return crate::after(ARM_SECS, Message::ArmExpired);
        }
        self.sapphire_stopping = true;
        self.after_stop = after;
        self.stop_epoch += 1;
        let epoch = self.stop_epoch;
        let port = self.web_port;
        self.log_lines.push(match after {
            AfterStop::None => "Stopping Sapphire...",
            AfterStop::Update => "Stopping Sapphire for the update...",
            AfterStop::SwitchBranch => "Stopping Sapphire for the branch switch...",
        }.to_string());

        // A pid (our Child's, or the one her lock names): ask nicely. Her end comes
        // back as SapphireExited (Child held) or through the pid watch StopSignalSent
        // starts. Off the UI thread. Checked before service mode on purpose — a unit
        // detected after a Launch doesn't change who started this one.
        if pid > 0 {
            return Task::perform(
                async move { tokio::task::spawn_blocking(move || signal_stop(pid)).await.unwrap_or(false) },
                move |sent| Message::StopSignalSent(sent, epoch),
            );
        }

        // Service mode: stop the unit, confirm through the health probe.
        if self.unit_here().is_some() {
            self.streaming_journal = false;
            return Task::perform(
                async move {
                    let _ = run_cmd_full_async("systemctl".into(), vec!["--user".into(), "stop".into(), "sapphire".into()]).await;
                    wait_health_down(port, 20).await
                },
                Message::SapphireStopConfirmed,
            );
        }

        // No pid to name (an old core started elsewhere, found on the web port only):
        // sweep, then confirm through the port.
        Task::perform(
            async move {
                let _ = tokio::task::spawn_blocking(|| kill_process_tree(0)).await;
                wait_health_down(port, 20).await
            },
            Message::SapphireStopConfirmed,
        )
    }

    /// Hard kill off the UI thread — her tree by pid, the sweep only when no pid is
    /// known — then a look 3 s later: is she still there? With her Child held, the
    /// runner's exit notice normally finishes the stop first and StopSwept finds it
    /// over; otherwise StopSwept's answer finishes it, or reports a kill that did not take.
    fn force_stop(&mut self, epoch: usize) -> Task<Message> {
        let pid = self.attached.as_ref().map_or(0, |i| i.pid);
        let port = self.web_port;
        Task::perform(
            async move {
                let _ = tokio::task::spawn_blocking(move || kill_process_tree(pid)).await;
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                if pid > 0 {
                    tokio::task::spawn_blocking(move || pid_is_hers(pid)).await.unwrap_or(true)
                } else {
                    health_up(port, 2).await
                }
            },
            move |alive| Message::StopSwept(epoch, alive),
        )
    }

    /// The unit's EnvironmentFile changed (first detect, a re-created unit): load it
    /// into the editor. Windows has no unit.
    #[cfg(not(windows))]
    fn env_reload(&self, new: &Option<ServiceInfo>) -> Task<Message> {
        let new_env = new.as_ref().and_then(|i| i.env_file.clone());
        let old_env = self.service.as_ref().and_then(|i| i.env_file.clone());
        match new_env {
            Some(p) if Some(&p) != old_env.as_ref() => Task::perform(read_env_file(p), Message::EnvLoaded),
            _ => Task::none(),
        }
    }
    #[cfg(windows)]
    fn env_reload(&self, _new: &Option<ServiceInfo>) -> Task<Message> {
        Task::none()
    }

    /// Sapphire is down: clear the running state and do what the stop was for.
    fn finish_stop(&mut self) -> Task<Message> {
        self.sapphire_stopping = false;
        self.sapphire_running = false;
        self.holding_child = false;
        self.attached = None;
        let next = match std::mem::take(&mut self.after_stop) {
            AfterStop::None => Task::none(),
            AfterStop::Update => self.update(Message::UpdateAfterStop),
            AfterStop::SwitchBranch => self.update(Message::SwitchBranch),
        };
        if self.service.is_none() {
            return next;
        }
        // Service mode: the status rows read the unit — re-read it now that it is down.
        Task::batch(vec![next, Task::perform(
            async { tokio::task::spawn_blocking(detect_sapphire_service).await.unwrap_or(None) },
            Message::ServiceRefreshed,
        )])
    }

    fn kick_next_install(&mut self) -> Task<Message> {
        // The first step still needing work — a failed one is retried, not skipped
        // (the steps after it depend on it).
        let next = self
            .steps
            .iter()
            .find(|(s, st)| *s != Step::Done && matches!(st, StepStatus::NotFound(_) | StepStatus::Failed(_)))
            .map(|(s, _)| *s);

        let Some(step) = next else {
            // Nothing left to install — we're done!
            self.op = Op::Idle;
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
        let anything_busy = self.op != Op::Idle || self.ts_running || self.checking_updates
            || self.sapphire_stopping || self.llm.busy();
        if anything_busy {
            subs.push(iced::time::every(std::time::Duration::from_millis(200)).map(|_| Message::Tick));
        }

        // Follow service logs via journalctl while in service mode (parks when idle).
        // The epoch id makes a Stop→Start cycle spawn a fresh follower.
        if self.streaming_journal {
            subs.push(Subscription::run_with_id(self.journal_epoch, journal_log_stream()));
        }

        // The Child we started: a Subscription (so iced parks it when idle) that
        // spawns her detached on first poll and waits for her exit.
        if self.holding_child {
            subs.push(Subscription::run_with_id(
                ("child", self.child_epoch),
                child_follower(self.install_path.clone(), launch_log_path(), run_state_path()),
            ));
        }

        subs.push(window::close_requests().map(Message::CloseRequested));

        // Her log, from a file: ours (Launch, autostart) or core's (started elsewhere).
        // For our own Child it waits for SapphireStarted, when the file is fresh.
        if let Some(i) = &self.attached
            && !(self.holding_child && i.pid == 0)
        {
            subs.push(Subscription::run_with_id(("tail", self.tail_epoch), tail_log_stream(i.log.clone())));
        }

        // Every 5 s: is she running on this install (autostart, a terminal, an
        // earlier launcher — or gone)? Skipped while we hold her Child (its exit
        // notice is the truth) and while stopping. In service mode the poll asks systemd.
        if !self.holding_child && !self.sapphire_stopping {
            subs.push(iced::time::every(std::time::Duration::from_secs(5)).map(|_| Message::InstancePoll));
        }

        // The LLM: its download/install job, and a look at the server — every 2 s
        // while it matters (tab open, loading, stopping), else every 10 s.
        if let Some(job) = &self.llm.job {
            subs.push(Subscription::run_with_id(("llm-job", self.llm.job_epoch), llm::job_stream(job.clone())));
        }
        let llm_secs = if self.active_tab == Tab::Llm || self.llm.busy() { 2 } else { 10 };
        subs.push(iced::time::every(std::time::Duration::from_secs(llm_secs)).map(|_| Message::Llm(LlmMsg::Poll)));

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

/// Drop a UTF-8 byte-order mark (Windows editors add one; serde_json rejects it).
fn strip_bom(s: &str) -> &str {
    s.trim_start_matches('\u{feff}')
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
            (r#"{"server": {"WEB_UI_PORT": 9002, "LOG_LEVEL": "INFO"}}"#, 9002), // where core keeps it
            (r#"{"server": {"WEB_UI_PORT": "9003"}}"#, 9003),
            (r#"{"server": {"WEB_UI_PORT": 9004}, "WEB_UI_PORT": 1}"#, 9004), // nested wins
            ("\u{feff}{\"server\": {\"WEB_UI_PORT\": 9005}}", 9005), // BOM from a Windows editor
            (r#"{"server": {"LOG_LEVEL": "INFO"}}"#, DEFAULT_WEB_PORT), // Krem's: no port set
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
        assert_eq!(detect_git_branch(&d.to_string_lossy()), Some(Branch::Stable), "no repo → Stable");
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
            assert_eq!(detect_git_branch(&r.to_string_lossy()), Some(want), "branch {}", branch);
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
        // Never the real folder from launcher.json: Krem's live Sapphire runs there.
        a.install_path = "/no/such/sapphire-launcher-test-folder".to_string();
        a.selected_branch = Some(Branch::Stable);
        a.off_branch = None;
        a.update_unfinished = false;
        a
    }
    /// She is OURS: started by this launcher, Child held, pid known.
    fn ours(a: &mut App) {
        a.attached = Some(Instance { pid: 4242, since: String::new(), origin: Origin::Launcher, log: PathBuf::from("/x/sapphire-launcher.log") });
        a.holding_child = true;
        a.sapphire_running = true;
    }
    /// She was FOUND: started elsewhere, known through core's lock (pid), no Child.
    fn found(a: &mut App) -> Instance {
        let i = Instance { pid: 5151, since: "2026-10-20 08:01:02".into(), origin: Origin::Elsewhere, log: core_log_path(&a.install_path) };
        let _ = a.update(Message::InstanceFound(Some(i.clone())));
        i
    }
    fn logged(a: &App, needle: &str) -> bool {
        a.log_lines.iter().any(|l| l.contains(needle))
    }

    #[test]
    fn exit_43_when_not_stopping_marks_not_running() {
        let mut a = app();
        ours(&mut a);
        let _ = a.update(Message::SapphireExited(Ok(Some(43))));
        assert!(!a.sapphire_running && !a.holding_child && a.attached.is_none());
        assert!(logged(&a, "Another Sapphire is already running"));
    }

    #[test]
    fn exit_while_stopping_finishes_the_stop_and_runs_the_continuation() {
        let mut a = app();
        ours(&mut a);
        a.sapphire_stopping = true;
        a.after_stop = AfterStop::Update;
        let _ = a.update(Message::SapphireExited(Ok(Some(1))));
        assert!(logged(&a, "Sapphire stopped."));
        assert!(logged(&a, "Starting update"));
        assert!(!a.sapphire_stopping && !a.sapphire_running && !a.holding_child && a.attached.is_none());
        assert!(matches!(a.after_stop, AfterStop::None));
    }

    #[test]
    fn late_exit_after_a_forced_stop_is_ignored() {
        let mut a = app();
        a.holding_child = false;
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
        let _ = a.update(Message::StopSwept(4, true));
        let _ = a.update(Message::StopSwept(4, false));
        let _ = a.update(Message::StopSignalSent(false, 4));
        assert!(a.log_lines.is_empty() && a.sapphire_stopping);
    }

    #[test]
    fn begin_stop_runs_once_and_keeps_the_latest_continuation() {
        let mut a = app();
        ours(&mut a);
        let _ = a.begin_stop(AfterStop::None);
        assert!(a.sapphire_stopping && a.stop_epoch == 1 && a.holding_child, "the Child stays held");
        let _ = a.begin_stop(AfterStop::Update);
        assert!(a.stop_epoch == 1 && matches!(a.after_stop, AfterStop::Update));
        let _ = a.begin_stop(AfterStop::None);
        assert!(matches!(a.after_stop, AfterStop::Update), "None never overrides a pending continuation");
        assert_eq!(a.log_lines.iter().filter(|l| l.contains("Stopping Sapphire...")).count(), 1);
    }

    #[test]
    fn a_kill_that_did_not_take_is_reported_not_claimed() {
        let mut a = app();
        ours(&mut a);
        a.sapphire_stopping = true;
        a.stop_epoch = 1;
        a.after_stop = AfterStop::Update;
        let _ = a.update(Message::StopSwept(1, true));
        assert!(logged(&a, "Couldn't force Sapphire to stop"));
        assert!(!logged(&a, "Starting update"), "no update on top of a live Sapphire");
        assert!(!a.sapphire_stopping && a.sapphire_running && a.holding_child);
        assert!(matches!(a.after_stop, AfterStop::None));
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
    fn a_found_sapphire_is_attached_followed_and_let_go() {
        let mut a = app();
        let i = found(&mut a);
        assert!(a.sapphire_running && !a.holding_child);
        assert_eq!(a.attached.as_ref(), Some(&i));
        assert_eq!(a.tail_epoch, 1, "her log is followed");
        assert!(logged(&a, "Sapphire is running (started elsewhere, pid 5151, since 2026-10-20 08:01:02)."));
        // The same her on the next poll: nothing to say, nothing restarted.
        let _ = a.update(Message::InstanceFound(Some(i.clone())));
        assert_eq!((a.tail_epoch, a.log_lines.len()), (1, 1));
        // A different instance under the same lock (restarted by hand): re-attach.
        let _ = a.update(Message::InstanceFound(Some(Instance { pid: 6161, ..i.clone() })));
        assert_eq!(a.tail_epoch, 2);
        // Gone: say so once, offer Launch again.
        let _ = a.update(Message::InstanceFound(None));
        assert!(!a.sapphire_running && a.attached.is_none() && logged(&a, "no longer running"));
        let _ = a.update(Message::InstanceFound(None));
        assert_eq!(a.log_lines.iter().filter(|l| l.contains("no longer running")).count(), 1);
    }

    #[test]
    fn a_poll_never_touches_our_child_or_a_stop_in_flight() {
        let mut a = app();
        ours(&mut a);
        let _ = a.update(Message::InstanceFound(None));
        assert!(a.sapphire_running && a.attached.is_some(), "the Child's exit notice is the truth");
        let mut b = app();
        found(&mut b);
        b.sapphire_stopping = true;
        let _ = b.update(Message::InstanceFound(None));
        assert!(b.sapphire_stopping && b.attached.is_some());
    }

    #[test]
    fn stop_of_a_found_sapphire_signals_her_pid_and_a_pid_watch_confirms() {
        let mut a = app();
        found(&mut a);
        let _ = a.update(Message::StopSapphire);
        assert!(a.sapphire_stopping && a.stop_epoch == 1 && logged(&a, "Stopping Sapphire..."));
        let _ = a.update(Message::StopSignalSent(true, 1));
        assert!(a.sapphire_stopping, "the signal went out; the pid watch is on");
        let _ = a.update(Message::SapphireStopConfirmed(true));
        assert!(!a.sapphire_stopping && !a.sapphire_running && a.attached.is_none());
        assert!(logged(&a, "Sapphire stopped."));
    }

    #[test]
    fn a_forced_stop_that_took_finishes_the_stop() {
        let mut a = app();
        found(&mut a);
        a.sapphire_stopping = true;
        a.stop_epoch = 1;
        a.after_stop = AfterStop::Update;
        let _ = a.update(Message::StopSwept(1, false));
        assert!(!a.sapphire_stopping && !a.sapphire_running && a.attached.is_none());
        assert!(logged(&a, "Sapphire stopped (forced).") && logged(&a, "Starting update"));
    }

    #[test]
    fn a_path_change_lets_go_of_a_found_sapphire_but_not_our_child() {
        let mut a = app();
        found(&mut a);
        a.let_go_of_found();
        assert!(!a.sapphire_running && a.attached.is_none());
        let mut b = app();
        ours(&mut b);
        b.let_go_of_found();
        assert!(b.sapphire_running && b.attached.is_some());
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
    fn destructive_actions_wait_for_her_to_stop() {
        let mut a = app();
        a.sapphire_running = true;
        let _ = a.update(Message::UninstallDeleteFolderClick);
        let _ = a.update(Message::UninstallDeleteUserdataClick);
        let _ = a.update(Message::UninstallCondaEnvClick);
        let _ = a.update(Message::ResetCredentials);
        assert!(a.armed.is_none());
        assert!(!(a.op == Op::Uninstalling));
        assert_eq!(a.log_lines.iter().filter(|l| l.contains("Stop Sapphire first")).count(), 4);
    }

    #[test]
    fn delete_folder_needs_a_real_install_and_arms_before_it_fires() {
        let mut a = app();
        a.install_path = scratch("not-an-install").to_string_lossy().into_owned();
        std::fs::write(PathBuf::from(&a.install_path).join("notes.txt"), "mine").unwrap();
        let _ = a.update(Message::UninstallDeleteFolderClick);
        assert!(a.armed.is_none() && logged(&a, "doesn't look like a Sapphire install"));
        // Somebody else's Python project: main.py and .git aren't enough.
        let mine = scratch("my-project");
        std::fs::write(mine.join("main.py"), "").unwrap();
        std::fs::create_dir_all(mine.join(".git")).unwrap();
        a.install_path = mine.to_string_lossy().into_owned();
        let _ = a.update(Message::UninstallDeleteFolderClick);
        assert!(a.armed.is_none());
        let d = fake_checkout("an-install");
        a.install_path = d.to_string_lossy().into_owned();
        let _ = a.update(Message::UninstallDeleteFolderClick);
        assert!(a.is_armed(Arm::DeleteFolder) && a.op == Op::Idle, "first click only arms");
        assert!(d.exists());
        a.disarm_confirms();
        assert!(!a.is_armed(Arm::DeleteFolder));
    }

    /// The files that make a folder Sapphire, and nothing else.
    fn fake_checkout(name: &str) -> PathBuf {
        let d = scratch(name);
        std::fs::create_dir_all(d.join(".git")).unwrap();
        std::fs::create_dir_all(d.join("core")).unwrap();
        for f in ["main.py", "sapphire.py", "core/setup.py"] {
            std::fs::write(d.join(f), "").unwrap();
        }
        d
    }

    #[test]
    fn a_double_click_cannot_be_both_clicks() {
        let mut a = app();
        let d = fake_checkout("double-click");
        a.install_path = d.to_string_lossy().into_owned();
        let _ = a.update(Message::UninstallDeleteFolderClick);
        let _ = a.update(Message::UninstallDeleteFolderClick);
        assert!(a.op == Op::Idle && a.is_armed(Arm::DeleteFolder), "the second click of a double-click does nothing");
        // A deliberate second click, a moment later: fires. (The task it returns is
        // inert until polled, so nothing is deleted here.)
        a.armed = Some((Arm::DeleteFolder, Instant::now() - std::time::Duration::from_secs(2)));
        let _ = a.update(Message::UninstallDeleteFolderClick);
        assert_eq!(a.op, Op::Uninstalling);
        // Left armed too long: it expires, and the next click only arms again.
        let mut b = app();
        b.install_path = d.to_string_lossy().into_owned();
        b.armed = Some((Arm::DeleteFolder, Instant::now() - std::time::Duration::from_secs(ARM_SECS + 1)));
        assert!(!b.is_armed(Arm::DeleteFolder));
        let _ = b.update(Message::UninstallDeleteFolderClick);
        assert!(b.op == Op::Idle && b.is_armed(Arm::DeleteFolder));
        let _ = b.update(Message::ArmExpired);
        assert!(b.is_armed(Arm::DeleteFolder), "a timer from an older arming doesn't disarm a fresh one");
    }

    #[test]
    fn reset_password_has_the_same_guards_as_the_keys() {
        let mut a = app();
        a.sapphire_running = true;
        let _ = a.update(Message::ResetPassword);
        assert!(a.armed.is_none() && logged(&a, "Stop Sapphire first"));
        let mut b = app();
        let _ = b.update(Message::ResetPassword);
        assert!(b.is_armed(Arm::ResetPassword), "first click only arms");
    }

    #[test]
    fn only_one_job_at_a_time_and_its_folder_is_frozen() {
        let d = fake_checkout("one-job");
        let path = d.to_string_lossy().into_owned();
        for op in [Op::Installing, Op::Updating, Op::Uninstalling, Op::Fixing, Op::SwitchingBranch] {
            let mut a = app();
            a.install_path = path.clone();
            a.op = op;
            for m in [Message::GoClicked, Message::Launch, Message::UpdateClicked, Message::UninstallCondaEnvClick,
                      Message::UninstallDeleteFolderClick, Message::ResetCredentials, Message::ResetPassword,
                      Message::TroubleshootFix(TsCheck::DepsHealth), Message::ServiceStart, Message::ServiceRemoveClick] {
                let _ = a.update(m);
                assert_eq!(a.op, op, "nothing else starts during a {}", op.words());
                assert!(a.armed.is_none() && !a.holding_child && !a.sapphire_running);
            }
            let _ = a.update(Message::PathChanged("/somewhere/else".into()));
            assert_eq!(a.install_path, path, "a job's folder doesn't move under it");
            assert!(logged(&a, "before changing folders"));
        }
        // A stop with work queued behind it holds the folder too.
        let mut b = app();
        b.install_path = path.clone();
        b.sapphire_stopping = true;
        let _ = b.update(Message::PathChanged("/somewhere/else".into()));
        assert_eq!(b.install_path, path);
    }

    #[test]
    fn results_of_a_scan_of_another_folder_are_dropped() {
        let mut a = app();
        let _ = a.update(Message::ScanClicked);
        let old = a.scan_epoch;
        let _ = a.update(Message::PathChanged(scratch("scan-new").to_string_lossy().into_owned()));
        assert_eq!(a.op, Op::Idle, "a scan gives way to the new folder");
        let _ = a.update(Message::StepResult(old, Step::Git, StepStatus::Found("old folder".into())));
        assert!(a.steps.iter().all(|(_, s)| *s == StepStatus::NotStarted));
    }

    #[test]
    fn a_folder_that_cannot_take_a_clone_stops_go_before_any_slow_step() {
        let mut a = app();
        let d = scratch("not-empty");
        std::fs::write(d.join("taxes.txt"), "mine").unwrap();
        a.install_path = d.to_string_lossy().into_owned();
        for (s, st) in &mut a.steps {
            if *s != Step::Done { *st = StepStatus::NotFound("needed".into()); }
        }
        let _ = a.update(Message::GoClicked);
        assert_eq!(a.op, Op::Idle);
        assert!(a.steps.iter().all(|(s, st)| *s == Step::Done || matches!(st, StepStatus::NotFound(_))), "Git wasn't started");
        assert!(logged(&a, "already has other files"));
    }

    #[test]
    fn no_sweep_without_a_second_click() {
        let mut a = app();
        let i = Instance { pid: 0, since: String::new(), origin: Origin::Elsewhere, log: PathBuf::new() };
        let _ = a.update(Message::InstanceFound(Some(i)));
        assert!(a.sapphire_running);
        // Stop & Update asks the same way: nothing reaches the sweep unconfirmed.
        a.install_path = fake_checkout("sweep").to_string_lossy().into_owned();
        let _ = a.update(Message::UpdateClicked);
        assert!(!a.sapphire_stopping && a.is_armed(Arm::Sweep) && logged(&a, "Click again"));
        a.armed = Some((Arm::Sweep, Instant::now() - std::time::Duration::from_secs(2)));
        let _ = a.update(Message::StopSapphire);
        assert!(a.sapphire_stopping);
    }

    #[test]
    fn only_a_real_status_ok_is_her() {
        assert!(health_says_ok(r#"{"status": "ok", "instance_name": "x"}"#));
        for no in ["ok", "<html>token ok</html>", r#"{"status": "error"}"#, r#"{"token": "ok"}"#, ""] {
            assert!(!health_says_ok(no), "{}", no);
        }
    }

    #[test]
    fn a_folder_with_no_lock_only_asks_the_port_if_it_is_an_old_core() {
        let d = scratch("empty-path");
        assert!(!is_lockless_core(&d.to_string_lossy()), "an empty folder never asks");
        assert!(!is_lockless_core("/no/such/folder/half-typed"));
        let c = fake_checkout("stable-core");
        assert!(is_lockless_core(&c.to_string_lossy()), "Stable: no lock in core at all");
        std::fs::write(c.join("core/single_instance.py"), "").unwrap();
        assert!(!is_lockless_core(&c.to_string_lossy()), "a locking core with no lock has never run here");
    }

    #[test]
    fn a_unit_on_another_folder_says_nothing_about_this_one() {
        let a_dir = fake_checkout("unit-a");
        let b_dir = fake_checkout("unit-b");
        let unit = ServiceInfo { active_state: "active".into(), enabled: true, sub_state: "running".into(),
            working_dir: Some(a_dir.to_string_lossy().into_owned()), env_file: None };
        let mut a = app();
        a.install_path = b_dir.to_string_lossy().into_owned();
        a.service = Some(unit.clone());
        let _ = a.update(Message::ServiceRefreshed(Some(unit.clone())));
        assert!(!a.sapphire_running && !a.streaming_journal, "she isn't running from this folder");
        // Launch here means this folder, not the unit's.
        let _ = a.update(Message::Launch);
        assert!(!logged(&a, "Starting Sapphire service"));
        // Point at the unit's folder: it's hers again.
        let mut c = app();
        c.install_path = b_dir.to_string_lossy().into_owned();
        c.service = Some(unit);
        let _ = c.update(Message::PathChanged(a_dir.to_string_lossy().into_owned()));
        assert!(c.sapphire_running && c.streaming_journal);
    }

    #[test]
    fn a_crash_looping_unit_offers_stop() {
        let looping = ServiceInfo { active_state: "activating".into(), enabled: true, sub_state: "auto-restart".into(), working_dir: None, env_file: None };
        let mut a = app();
        a.service = Some(looping.clone());
        let _ = a.update(Message::ServiceRefreshed(Some(looping)));
        assert!(a.sapphire_running, "the header shows Stop, and the destructive guards hold");
        let _ = a.update(Message::StopSapphire);
        assert!(a.sapphire_stopping, "systemctl stop, which cancels the restart");
    }

    #[test]
    fn a_branch_pick_on_a_real_install_only_proposes() {
        let d = fake_checkout("branch-pick");
        let mut a = app();
        a.install_path = d.to_string_lossy().into_owned();
        a.selected_branch = Some(Branch::Stable);
        a.sapphire_running = true;
        ours(&mut a);
        let _ = a.update(Message::BranchSelected(Branch::Development));
        assert_eq!(a.pending_branch, Some(Branch::Development));
        assert_eq!(a.selected_branch, Some(Branch::Stable), "still what the folder is on");
        assert!(!a.sapphire_stopping, "nothing stops until it's confirmed");
        let _ = a.update(Message::BranchCancel);
        assert_eq!(a.pending_branch, None);
        let _ = a.update(Message::BranchSelected(Branch::Development));
        let _ = a.update(Message::BranchConfirm);
        assert!(a.sapphire_stopping && matches!(a.after_stop, AfterStop::SwitchBranch));
        // The stop fails: the picker never lied, and the proposal is still on screen.
        let _ = a.update(Message::SapphireStopConfirmed(false));
        assert_eq!(a.selected_branch, Some(Branch::Stable));
        assert_eq!(a.pending_branch, Some(Branch::Development));
        // Re-picking what the folder is on just withdraws the proposal.
        let _ = a.update(Message::BranchSelected(Branch::Stable));
        assert_eq!(a.pending_branch, None);
    }

    #[test]
    fn closing_mid_job_asks_first() {
        assert!(Op::Updating.unsafe_to_close() && Op::Installing.unsafe_to_close() && Op::SwitchingBranch.unsafe_to_close());
        assert!(!Op::Idle.unsafe_to_close() && !Op::Scanning.unsafe_to_close());
        let mut a = app();
        a.op = Op::Updating;
        let _ = a.update(Message::CloseRequested(window::Id::unique()));
        assert!(a.is_armed(Arm::CloseWindow) && logged(&a, "Close again"));
    }

    #[test]
    fn reset_keys_arms_then_a_tab_change_disarms() {
        // First click only (the second would touch the real credentials file).
        let mut a = app();
        let _ = a.update(Message::ResetCredentials);
        assert!(a.is_armed(Arm::ResetKeys) && a.log_lines.is_empty());
        let _ = a.update(Message::TabSelected(Tab::Install));
        assert!(!a.is_armed(Arm::ResetKeys));
    }

    #[test]
    fn stop_signals_our_child_even_if_a_unit_was_detected_later() {
        let mut a = app();
        ours(&mut a);
        a.service = Some(ServiceInfo {
            active_state: "inactive".into(), enabled: true,
            sub_state: "dead".into(), working_dir: None, env_file: None,
        });
        let _ = a.update(Message::StopSapphire);
        assert!(a.sapphire_stopping && !a.streaming_journal, "not a systemctl stop");
    }

    #[test]
    fn a_stop_right_after_launch_is_a_double_click() {
        let mut a = app();
        let d = scratch("launch");
        std::fs::write(d.join("main.py"), "").unwrap();
        a.install_path = d.to_string_lossy().into_owned();
        let _ = a.update(Message::Launch);
        assert!(a.sapphire_running && a.holding_child && a.launched_at.is_some());
        assert_eq!(a.attached.as_ref().map(|i| (i.pid, i.origin)), Some((0, Origin::Launcher)));
        assert_eq!(a.tail_epoch, 0, "no tail until the log file is fresh");
        let _ = a.update(Message::StopSapphire);
        assert!(!a.sapphire_stopping && logged(&a, "still starting"));
        // Past the debounce but before the runner reported a pid: nothing to signal,
        // and no sweep (that would hit every sapphire-env python on the box).
        a.launched_at = None;
        let _ = a.update(Message::StopSapphire);
        assert!(!a.sapphire_stopping && a.stop_epoch == 0);
        // The runner reports its pid: now her (fresh) log is followed, and Stop works.
        let _ = a.update(Message::SapphireStarted(777));
        assert_eq!((a.attached.as_ref().map(|i| i.pid), a.tail_epoch), (Some(777), 1));
        let _ = a.update(Message::StopSapphire);
        assert!(a.sapphire_stopping && a.stop_epoch == 1);
    }

    #[test]
    fn stop_reason_is_logged_once_per_stop() {
        let mut a = app();
        ours(&mut a);
        a.install_path = fake_checkout("stop-reason").to_string_lossy().into_owned();
        let _ = a.update(Message::UpdateClicked);
        assert!(a.sapphire_stopping && matches!(a.after_stop, AfterStop::Update));
        assert_eq!(a.log_lines.iter().filter(|l| l.contains("Stopping Sapphire")).count(), 1);
        assert!(logged(&a, "for the update"));
    }

    #[test]
    fn launch_and_update_wait_for_each_other() {
        let mut a = app();
        a.op = Op::Updating;
        let _ = a.update(Message::Launch);
        assert!(!a.sapphire_running && logged(&a, "Wait for the update"));
        let before = a.selected_branch;
        let other = if before == Some(Branch::Stable) { Branch::Development } else { Branch::Stable };
        let _ = a.update(Message::BranchSelected(other));
        assert_eq!((a.selected_branch, a.pending_branch), (before, None), "no branch switch mid-update");
        let mut b = app();
        b.op = Op::Installing;
        let _ = b.update(Message::UpdateClicked);
        assert!(!(b.op == Op::Updating));
    }

    #[test]
    fn a_failed_checkout_puts_the_picker_back() {
        let mut a = app();
        let r = scratch("checkout-fail");
        let git = |args: &[&str]| std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args)
            .current_dir(&r).status().map(|s| s.success()).unwrap_or(false);
        if !(git(&["init", "-q", "-b", "main"]) && git(&["commit", "-q", "--allow-empty", "-m", "init"])) {
            eprintln!("skipped: git init failed");
            return;
        }
        a.install_path = r.to_string_lossy().into_owned();
        a.selected_branch = Some(Branch::Development);
        a.op = Op::SwitchingBranch;
        let _ = a.update(Message::SwitchBranchResult(Branch::Development, "error: pathspec 'dev' did not match".into(), false));
        assert_eq!(a.selected_branch, Some(Branch::Stable));
        assert_eq!(a.op, Op::Idle);
        assert!(a.branch_note.is_some(), "the failure is on screen, not only in the log");
    }

    #[test]
    fn a_failed_step_is_retried_not_skipped() {
        let mut a = app();
        for (step, st) in &mut a.steps {
            *st = match step {
                Step::Git => StepStatus::Found("ok".into()),
                Step::Conda => StepStatus::Failed("winget died".into()),
                Step::Done => StepStatus::NotStarted,
                _ => StepStatus::NotFound("todo".into()),
            };
        }
        let _ = a.update(Message::GoClicked);
        assert!((a.op == Op::Installing));
        let conda = a.steps.iter().find(|(s, _)| *s == Step::Conda).unwrap();
        assert_eq!(conda.1, StepStatus::Installing, "Go retries the failed step first");
    }

    #[test]
    fn service_refresh_respects_a_stop_in_flight() {
        let mut a = app();
        let up = ServiceInfo { active_state: "active".into(), enabled: true, sub_state: "running".into(), working_dir: None, env_file: None };
        a.service = Some(up.clone());
        a.sapphire_running = true;
        a.sapphire_stopping = true;
        let _ = a.update(Message::ServiceRefreshed(Some(up.clone())));
        assert!(a.sapphire_stopping && !a.streaming_journal, "a poll mid-stop changes nothing");
        a.sapphire_stopping = false;
        a.sapphire_running = false;
        let _ = a.update(Message::ServiceRefreshed(Some(up)));
        assert!(a.sapphire_running && a.streaming_journal, "she came up behind our back: follow her");
        let _ = a.update(Message::ServiceRefreshed(None));
        assert!(a.service.is_none());
    }

    #[cfg(not(windows))]
    #[test]
    fn desktop_exec_quotes_awkward_paths() {
        assert_eq!(desktop_exec("/home/k/Apps/My Tools/Sapphire.AppImage"), "\"/home/k/Apps/My Tools/Sapphire.AppImage\"");
        assert_eq!(desktop_exec(r#"/x/a"b$c`d\e%f"#), r#""/x/a\"b\$c\`d\\e%%f""#);
    }

    #[test]
    fn without_a_unit_the_startup_asks_whether_she_runs_anyway() {
        // Only the state is checked here; the Task it returns is inert until polled.
        let mut a = app();
        let _ = a.update(Message::ServiceDetected(None));
        assert!(a.service.is_none() && !a.sapphire_running);
    }

    #[test]
    fn a_unit_owns_her_only_while_it_runs_her() {
        let unit = |state: &str| ServiceInfo { active_state: state.into(), enabled: true, sub_state: "x".into(), working_dir: None, env_file: None };
        let her = Instance { pid: 9, since: String::new(), origin: Origin::Elsewhere, log: PathBuf::new() };
        // Unit running: a found instance is the unit's own.
        let mut a = app();
        a.service = Some(unit("active"));
        let _ = a.update(Message::InstanceFound(Some(her.clone())));
        assert!(a.attached.is_none());
        // Unit down, Sapphire up: she was started some other way. Stop reaches her by pid.
        let mut b = app();
        b.service = Some(unit("inactive"));
        let _ = b.update(Message::InstanceFound(Some(her.clone())));
        assert!(b.sapphire_running && b.attached.as_ref().is_some_and(|i| i.pid == 9));
        let _ = b.update(Message::ServiceRefreshed(Some(unit("inactive"))));
        assert!(b.sapphire_running, "the unit being down doesn't make her down");
        let _ = b.update(Message::StopSapphire);
        assert!(b.sapphire_stopping && b.stop_epoch == 1);
        // No pid: never the sweep on a box with a unit.
        let mut c = app();
        c.service = Some(unit("inactive"));
        let _ = c.update(Message::InstanceFound(Some(Instance { pid: 0, ..her.clone() })));
        assert!(c.attached.is_none());
        // She went away and the unit came up: back to the unit.
        let mut d = app();
        d.service = Some(unit("inactive"));
        let _ = d.update(Message::InstanceFound(Some(her)));
        let _ = d.update(Message::ServiceRefreshed(Some(unit("active"))));
        assert!(d.attached.is_none() && d.sapphire_running);
    }

    #[test]
    fn a_launch_that_loses_to_a_twin_attaches_to_the_winner() {
        let mut a = app();
        ours(&mut a);
        let _ = a.update(Message::SapphireExited(Ok(Some(43))));
        assert!(!a.sapphire_running && !a.holding_child && a.attached.is_none());
        let winner = Instance { pid: 5151, since: String::new(), origin: Origin::Elsewhere, log: PathBuf::from("/core.log") };
        let _ = a.update(Message::TwinLookup(Some(winner.clone())));
        assert!(a.sapphire_running && a.attached == Some(winner), "Stop is back");
        let mut b = app();
        ours(&mut b);
        let _ = b.update(Message::SapphireExited(Ok(Some(43))));
        let _ = b.update(Message::TwinLookup(None));
        assert!(!b.sapphire_running && logged(&b, "Couldn't find that Sapphire"));
    }
}
