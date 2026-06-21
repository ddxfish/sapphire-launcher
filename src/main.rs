#![windows_subsystem = "windows"]

use iced::widget::{scrollable, text_editor};
use iced::{color, Subscription, Task, Theme};
use iced::window;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex, atomic::{AtomicU32, Ordering}};
use std::time::Instant;

mod ui;
mod service;
mod platform;
use crate::platform::*;
use crate::service::*;



// ── Config persistence ─────────────────────────────────────────────────────

fn config_path() -> PathBuf {
    app_config_dir().join("launcher.json")
}

fn load_config() -> (String, Branch) {
    let default_path = default_install_path();

    let path = config_path();
    let install_path = match std::fs::read_to_string(&path) {
        Ok(content) => {
            content
                .split("\"install_path\"")
                .nth(1)
                .and_then(|s| s.split('"').nth(1))
                .map(|s| s.replace("\\\\", "\\"))
                .unwrap_or(default_path)
        }
        Err(_) => default_path,
    };

    // Detect actual branch from the repo instead of trusting config
    let branch = detect_git_branch(&install_path);

    (install_path, branch)
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
    let path = config_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let branch_str = match branch {
        Branch::Development => "Development",
        Branch::Stable => "Stable",
    };
    let escaped_path = install_path.replace('\\', "\\\\");
    let json = format!(
        "{{\n  \"install_path\": \"{}\",\n  \"branch\": \"{}\"\n}}\n",
        escaped_path, branch_str
    );
    let _ = std::fs::write(&path, json);
}

fn main() -> iced::Result {
    // Linux: register the app with the desktop (apps menu / dock icon).
    #[cfg(not(windows))]
    install_desktop_entry();

    let icon = load_window_icon();

    let win_settings = window::Settings {
        size: iced::Size::new(700.0, 500.0),
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

    iced::application("Sapphire Launcher", App::update, App::view)
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
        .run_with(|| {
            let app = App::default();
            let path = app.install_path.clone();
            let branch = app.selected_branch.unwrap_or(Branch::Stable);
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
                Task::perform(
                    async {
                        run_cmd_full_async("curl".into(), vec![
                            "-sk".into(), "--max-time".into(), "3".into(),
                            "https://localhost:8073/api/health".into(),
                        ]).await.map(|b| b.contains("ok")).unwrap_or(false)
                    },
                    |running| Message::LaunchPreCheck(running, false),
                ),
                // Check for updates
                Task::perform(
                    check_for_updates(path, branch),
                    Message::UpdatesAvailable,
                ),
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
    let exe_str = exe.display().to_string();

    let desktop_path = format!("{}/.local/share/applications/sapphire-launcher.desktop", home);
    // Skip if we've already installed for this exact binary location.
    if let Ok(existing) = std::fs::read_to_string(&desktop_path) {
        if existing.contains(&format!("Exec={}", exe_str)) {
            return;
        }
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

// ── Step tracking ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Git,
    Conda,
    CondaInit,
    PythonEnv,
    Clone,
    Deps,
    Done,
}

const ALL_STEPS: &[Step] = &[
    Step::Git,
    Step::Conda,
    Step::CondaInit,
    Step::PythonEnv,
    Step::Clone,
    Step::Deps,
    Step::Done,
];

#[derive(Debug, Clone, PartialEq, Eq)]
enum StepStatus {
    NotStarted,
    Checking,
    Found(String),    // already installed — friendly detail
    NotFound(String), // not installed — what we'll do
    Installing,
    Done(String),
    Failed(String),
}

impl StepStatus {
    fn indicator(&self, tick: usize) -> &str {
        const SPINNER: &[&str] = &["/", "-", "\\", "|"];
        match self {
            StepStatus::NotStarted => "-",
            StepStatus::Checking | StepStatus::Installing => SPINNER[tick % SPINNER.len()],
            StepStatus::Found(_) => "+",
            StepStatus::NotFound(_) => "?",
            StepStatus::Done(_) => "+",
            StepStatus::Failed(_) => "x",
        }
    }

    fn color(&self) -> iced::Color {
        match self {
            StepStatus::NotStarted => color!(0x585b70),
            StepStatus::Checking | StepStatus::Installing => color!(0x3d85c6),
            StepStatus::Found(_) | StepStatus::Done(_) => color!(0x4caf50),
            StepStatus::NotFound(_) => color!(0xf9e154),
            StepStatus::Failed(_) => color!(0xe74c3c),
        }
    }

    fn detail(&self) -> Option<&str> {
        match self {
            StepStatus::Found(s)
            | StepStatus::NotFound(s)
            | StepStatus::Done(s)
            | StepStatus::Failed(s) => Some(s.as_str()),
            _ => None,
        }
    }
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

        Self {
            install_path,
            selected_branch: Some(branch),
            active_tab: Tab::default(),
            log_visible: true,
            log_lines: vec!["Ready.".to_string()],
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
    LaunchPreCheck(bool, bool),   // (already_running, user_initiated)
    SapphireLine(String),   // a single line of output from sapphire
    SapphireLines(Vec<String>), // a coalesced batch of log lines (burst-safe)
    SapphireExited(String), // process ended
    SapphireStopConfirmed,  // process is actually dead
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
    ServiceRestart,
    ServiceEnable,
    ServiceDisable,
    ServiceActionResult(String, bool),
    ServiceRefreshed(Option<ServiceInfo>),
    SystemdChecked(bool),
    ServiceInstall,
    ServiceInstallResult(String, bool),
    ServiceRemoveClick,
    ServiceRemoveResult(String, bool),
    EnvLoaded(String),
    EnvEdit(text_editor::Action),
    EnvSaveRestart,
    EnvSaved(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TsCheck {
    SapphireRunning,
    WebUi,
    DepsHealth,
    Plugins,
    Gpu,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TsStatus {
    NotChecked,
    Checking,
    Ok(String),
    Problem(String),
    Fixing,
    Fixed(String),
}

// ── Async detection logic ──────────────────────────────────────────────────

/// Run a command on a background thread so it doesn't freeze the UI.
const CREATE_NO_WINDOW: u32 = 0x08000000;


async fn run_cmd_async(program: String, args: Vec<String>) -> Result<String, String> {
    let result: Result<Result<String, String>, _> = tokio::task::spawn_blocking(move || {
        let mut cmd = Command::new(&program);
        cmd.args(&args);
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        cmd.output()
            .map(|out| {
                let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                if out.status.success() {
                    Ok(stdout)
                } else if stderr.is_empty() {
                    Err(format!("Exit code {}", out.status.code().unwrap_or(-1)))
                } else {
                    Err(stderr)
                }
            })
            .unwrap_or_else(|e| Err(e.to_string()))
    })
    .await;
    result.map_err(|e| e.to_string())?
}

// These return (Step, StepStatus) after checking the system
#[cfg(windows)]
async fn check_git() -> (Step, StepStatus) {
    match run_cmd_async(find_git(), vec!["--version".into()]).await {
        Ok(ver) => (Step::Git, StepStatus::Found(ver)),
        Err(_) => (Step::Git, StepStatus::NotFound("Git not found — we'll install it".to_string())),
    }
}

/// Linux: the "Git" step covers all system prerequisites — git (to clone) plus
/// libportaudio (Sapphire's audio at runtime). Both must be present, even if git
/// alone already is, so we check both here.
#[cfg(not(windows))]
async fn check_git() -> (Step, StepStatus) {
    let git_ok = run_cmd_async(find_git(), vec!["--version".into()]).await.is_ok();
    let audio_ok = run_cmd_async(
        "sh".into(),
        vec!["-c".into(), "ldconfig -p 2>/dev/null | grep -qi portaudio".into()],
    ).await.is_ok();
    // curl is used by the launcher's health/troubleshoot checks (not preinstalled on a fresh box).
    let curl_ok = run_cmd_async(
        "sh".into(),
        vec!["-c".into(), "command -v curl >/dev/null 2>&1".into()],
    ).await.is_ok();

    if git_ok && audio_ok && curl_ok {
        return (Step::Git, StepStatus::Found("git, audio libraries, and curl present".to_string()));
    }
    let mut missing = Vec::new();
    if !git_ok { missing.push("git"); }
    if !audio_ok { missing.push("audio libs (libportaudio)"); }
    if !curl_ok { missing.push("curl"); }
    (
        Step::Git,
        StepStatus::NotFound(format!(
            "Missing {} — Go opens your system's own password prompt to install",
            missing.join(", ")
        )),
    )
}

async fn check_conda() -> (Step, StepStatus) {
    match run_cmd_async(find_conda(), vec!["--version".into()]).await {
        Ok(ver) => (Step::Conda, StepStatus::Found(ver)),
        Err(_) => {
            #[cfg(windows)]
            let paths = {
                let home = std::env::var("USERPROFILE").unwrap_or_default();
                [
                    format!("{}\\miniconda3\\Scripts\\conda.exe", home),
                    format!("{}\\anaconda3\\Scripts\\conda.exe", home),
                    format!("{}\\Miniconda3\\Scripts\\conda.exe", home),
                    format!("{}\\Anaconda3\\Scripts\\conda.exe", home),
                ]
            };
            #[cfg(not(windows))]
            let paths = {
                let home = std::env::var("HOME").unwrap_or_default();
                [
                    format!("{}/miniconda3/bin/conda", home),
                    format!("{}/anaconda3/bin/conda", home),
                    "/opt/miniconda3/bin/conda".to_string(),
                    "/opt/anaconda3/bin/conda".to_string(),
                ]
            };
            for p in &paths {
                if PathBuf::from(p).exists() {
                    return (
                        Step::Conda,
                        StepStatus::Found(format!("Found at {}", p)),
                    );
                }
            }
            (
                Step::Conda,
                StepStatus::NotFound("Miniconda not found — we'll install it".to_string()),
            )
        }
    }
}

async fn check_conda_init() -> (Step, StepStatus) {
    match run_cmd_async(find_conda(), vec!["info".into(), "--json".into()]).await {
        Ok(_) => (
            Step::CondaInit,
            StepStatus::Found("Conda is initialized".to_string()),
        ),
        Err(_) => (
            Step::CondaInit,
            StepStatus::NotFound("Will run conda init after install".to_string()),
        ),
    }
}

async fn check_python_env() -> (Step, StepStatus) {
    match run_cmd_async(find_conda(), vec!["env".into(), "list".into()]).await {
        Ok(output) => {
            if output.lines().any(|l| l.starts_with("sapphire ") || l.starts_with("sapphire\t")) {
                (
                    Step::PythonEnv,
                    StepStatus::Found("'sapphire' environment exists".to_string()),
                )
            } else {
                (
                    Step::PythonEnv,
                    StepStatus::NotFound("Will create 'sapphire' Python 3.11 environment".to_string()),
                )
            }
        }
        Err(_) => (
            Step::PythonEnv,
            StepStatus::NotFound("Need conda first, then we'll create the environment".to_string()),
        ),
    }
}

async fn check_clone(install_path: String) -> (Step, StepStatus) {
    let repo_path = PathBuf::from(&install_path);
    let git_dir = repo_path.join(".git");

    if git_dir.exists() {
        (
            Step::Clone,
            StepStatus::Found(format!("Repo already at {}", install_path)),
        )
    } else if repo_path.exists() {
        (
            Step::Clone,
            StepStatus::NotFound(format!("Folder exists but no git repo — will clone into {}", install_path)),
        )
    } else {
        (
            Step::Clone,
            StepStatus::NotFound(format!("Will clone Sapphire into {}", install_path)),
        )
    }
}

async fn check_deps(install_path: String) -> (Step, StepStatus) {
    let req_file = PathBuf::from(&install_path).join("requirements.txt");
    if !req_file.exists() {
        return (Step::Deps, StepStatus::NotFound("Will install after cloning".to_string()));
    }

    // requirements.txt existing isn't enough — the env (and its packages) may be
    // gone (e.g. after removing the conda env). Probe the env's pip for the first
    // real package to confirm deps are actually installed.
    let first_pkg = tokio::fs::read_to_string(&req_file).await.ok().and_then(|c| {
        c.lines()
            .map(|l| l.trim())
            .find(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with('-'))
            .map(|l| l.split(&['>', '<', '=', '!', '[', ';', ' '][..]).next().unwrap_or(l).trim().to_string())
    });

    let Some(pkg) = first_pkg.filter(|p| !p.is_empty()) else {
        return (Step::Deps, StepStatus::Found("requirements.txt is empty — nothing to install".to_string()));
    };

    let (program, mut args) = find_conda_pip();
    args.extend(["show".into(), pkg.clone()]);
    match run_cmd_full_async(program, args).await {
        Ok(_) => (Step::Deps, StepStatus::Found("Dependencies installed".to_string())),
        Err(_) => (Step::Deps, StepStatus::NotFound(format!("Dependencies missing (no '{}') — will install", pkg))),
    }
}

// ── Async install logic ────────────────────────────────────────────────────

/// Run a command on a background thread, returning combined stdout+stderr for logging
async fn run_cmd_full_async(program: String, args: Vec<String>) -> Result<String, String> {
    let result: Result<Result<String, String>, _> = tokio::task::spawn_blocking(move || {
        let mut cmd = Command::new(&program);
        cmd.args(&args);
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        match cmd.output() {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                let combined = format!("{}{}", stdout, stderr).trim().to_string();
                if out.status.success() {
                    Ok(combined)
                } else {
                    Err(combined)
                }
            }
            Err(e) => Err(e.to_string()),
        }
    })
    .await;
    result.map_err(|e| e.to_string())?
}

#[cfg(windows)]
async fn install_git() -> (Step, StepStatus, String) {
    match run_cmd_full_async("winget".into(), vec!["install".into(), "Git.Git".into(), "--accept-source-agreements".into(), "--accept-package-agreements".into()]).await {
        Ok(out) => (Step::Git, StepStatus::Done("Git installed! You may need to restart the launcher for PATH to update.".to_string()), out),
        Err(e) => {
            if e.contains("already installed") {
                (Step::Git, StepStatus::Done("Git is already installed".to_string()), e)
            } else {
                (Step::Git, StepStatus::Failed(format!("Couldn't install Git — try installing it manually. {}", e)), e)
            }
        }
    }
}

/// Pick the package-manager install of Sapphire's system prerequisites from
/// /etc/os-release: git (to clone) + portaudio (audio runtime) + python dev
/// headers. Returns (pkexec args, human-readable manual command).
/// Empty args = unknown distro. Package names per the Sapphire docs (libportaudio2).
#[cfg(not(windows))]
fn system_packages_plan() -> (Vec<String>, String) {
    let osr = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let mut id = String::new();
    let mut id_like = String::new();
    for line in osr.lines() {
        if let Some(v) = line.strip_prefix("ID=") { id = v.trim_matches('"').to_lowercase(); }
        else if let Some(v) = line.strip_prefix("ID_LIKE=") { id_like = v.trim_matches('"').to_lowercase(); }
    }
    let hay = format!("{} {}", id, id_like);
    let v = |parts: &[&str]| parts.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    if hay.contains("debian") || hay.contains("ubuntu") {
        (v(&["apt-get", "install", "-y", "git", "libportaudio2", "python3-dev", "curl"]),
         "sudo apt install git libportaudio2 python3-dev curl".into())
    } else if hay.contains("fedora") || hay.contains("rhel") || hay.contains("centos") {
        (v(&["dnf", "install", "-y", "git", "portaudio", "python3-devel", "curl"]),
         "sudo dnf install git portaudio python3-devel curl".into())
    } else if hay.contains("arch") {
        (v(&["pacman", "-S", "--noconfirm", "git", "portaudio", "curl"]),
         "sudo pacman -S git portaudio curl".into())
    } else if hay.contains("suse") {
        (v(&["zypper", "install", "-y", "git", "libportaudio2", "python3-devel", "curl"]),
         "sudo zypper install git libportaudio2 python3-devel curl".into())
    } else {
        (vec![], "install git, portaudio, python dev headers, and curl with your package manager".into())
    }
}

#[cfg(not(windows))]
async fn install_git() -> (Step, StepStatus, String) {
    let (pm_args, manual) = system_packages_plan();
    if pm_args.is_empty() {
        return (
            Step::Git,
            StepStatus::Failed(format!("Couldn't detect your package manager. Run this yourself, then Re-scan:  {}", manual)),
            String::new(),
        );
    }
    // pkexec hands off to polkit — the desktop's OWN password dialog, not ours.
    match run_cmd_full_async("pkexec".into(), pm_args).await {
        Ok(out) => (Step::Git, StepStatus::Done("System packages installed (git, audio libs).".to_string()), out),
        Err(e) => (
            Step::Git,
            StepStatus::Failed(format!("Couldn't install system packages automatically. Run this in a terminal, then Re-scan:  {}", manual)),
            e,
        ),
    }
}

#[cfg(windows)]
async fn install_conda() -> (Step, StepStatus, String) {
    match run_cmd_full_async("winget".into(), vec!["install".into(), "Anaconda.Miniconda3".into(), "--accept-source-agreements".into(), "--accept-package-agreements".into()]).await {
        Ok(out) => (Step::Conda, StepStatus::Done("Miniconda installed! You may need to restart the launcher for PATH to update.".to_string()), out),
        Err(e) => {
            if e.contains("already installed") {
                (Step::Conda, StepStatus::Done("Miniconda is already installed".to_string()), e)
            } else {
                (Step::Conda, StepStatus::Failed(format!("Couldn't install Miniconda — try installing it manually. {}", e)), e)
            }
        }
    }
}

/// Linux Miniconda install: download the official installer and run it unattended
/// into ~/miniconda3 (no root, identical across distros).
#[cfg(not(windows))]
async fn install_conda() -> (Step, StepStatus, String) {
    let arch = run_cmd_async("uname".into(), vec!["-m".into()]).await.unwrap_or_default();
    let installer_arch = match arch.trim() {
        "aarch64" | "arm64" => "aarch64",
        _ => "x86_64",
    };
    let url = format!("https://repo.anaconda.com/miniconda/Miniconda3-latest-Linux-{}.sh", installer_arch);
    let tmp = std::env::temp_dir().join("miniconda-installer.sh").to_string_lossy().to_string();

    // Download with curl, fall back to wget.
    let dl = match run_cmd_full_async("curl".into(), vec!["-fsSL".into(), "-o".into(), tmp.clone(), url.clone()]).await {
        Ok(o) => Ok(o),
        Err(_) => run_cmd_full_async("wget".into(), vec!["-qO".into(), tmp.clone(), url.clone()]).await,
    };
    if let Err(e) = dl {
        return (Step::Conda, StepStatus::Failed(format!("Couldn't download the Miniconda installer (need curl or wget): {}", e)), e);
    }

    let home = std::env::var("HOME").unwrap_or_default();
    let prefix = format!("{}/miniconda3", home);
    match run_cmd_full_async("bash".into(), vec![tmp, "-b".into(), "-p".into(), prefix]).await {
        Ok(out) => (Step::Conda, StepStatus::Done("Miniconda installed into ~/miniconda3".to_string()), out),
        Err(e) => {
            if e.contains("already exists") {
                (Step::Conda, StepStatus::Done("Miniconda already installed".to_string()), e)
            } else {
                (Step::Conda, StepStatus::Failed(format!("Miniconda install failed: {}", e)), e)
            }
        }
    }
}

async fn install_conda_init() -> (Step, StepStatus, String) {
    let conda = find_conda();
    // Accept Anaconda ToS for the default channels (required since 2025-07-15).
    for ch in [
        "https://repo.anaconda.com/pkgs/main",
        "https://repo.anaconda.com/pkgs/r",
        "https://repo.anaconda.com/pkgs/msys2",
    ] {
        let _ = run_cmd_full_async(conda.clone(), vec![
            "tos".into(), "accept".into(), "--override-channels".into(),
            "--channel".into(), ch.to_string(),
        ]).await;
    }

    match run_cmd_full_async(conda, vec!["init".into()]).await {
        Ok(out) => (Step::CondaInit, StepStatus::Done("Conda initialized".to_string()), out),
        Err(e) => (Step::CondaInit, StepStatus::Failed(format!("Conda init had a problem: {}", e)), e),
    }
}

async fn install_python_env() -> (Step, StepStatus, String) {
    match run_cmd_full_async(find_conda(), vec!["create".into(), "-n".into(), "sapphire".into(), "python=3.11".into(), "-y".into()]).await {
        Ok(out) => (Step::PythonEnv, StepStatus::Done("Python 3.11 environment 'sapphire' created".to_string()), out),
        Err(e) => {
            if e.contains("already exists") {
                (Step::PythonEnv, StepStatus::Done("Environment 'sapphire' already exists".to_string()), e)
            } else {
                (Step::PythonEnv, StepStatus::Failed(format!("Couldn't create Python environment: {}", e)), e)
            }
        }
    }
}

async fn install_clone(install_path: String, branch: String) -> (Step, StepStatus, String) {
    let git_branch = match branch.as_str() {
        "Development" => "dev",
        _ => "main",
    };

    let repo_path = PathBuf::from(&install_path);

    // If folder exists but isn't a repo, check if it's empty
    if repo_path.exists() && !repo_path.join(".git").exists() {
        let is_empty = std::fs::read_dir(&repo_path)
            .map(|mut d| d.next().is_none())
            .unwrap_or(false);
        if !is_empty {
            return (
                Step::Clone,
                StepStatus::Failed(format!(
                    "Folder {} already exists and has files in it. Move or rename it first.",
                    install_path
                )),
                String::new(),
            );
        }
        // Empty folder — git clone works fine into empty dirs
    }

    // If already a repo, just make sure we're on the right branch
    if repo_path.join(".git").exists() {
        let _ = run_cmd_full_async(find_git(), vec!["-C".into(), install_path.clone(), "checkout".into(), git_branch.to_string()]).await;
        return (
            Step::Clone,
            StepStatus::Done(format!("Repo exists, switched to {} branch", git_branch)),
            String::new(),
        );
    }

    match run_cmd_full_async(
        find_git(),
        vec![
            "clone".into(),
            "--branch".into(),
            git_branch.to_string(),
            "https://github.com/ddxfish/sapphire.git".into(),
            install_path.clone(),
        ],
    ).await {
        Ok(out) => (
            Step::Clone,
            StepStatus::Done(format!("Cloned Sapphire ({}) into {}", git_branch, install_path)),
            out,
        ),
        Err(e) => (
            Step::Clone,
            StepStatus::Failed(format!("Clone failed: {}", e)),
            e,
        ),
    }
}

async fn install_deps(install_path: String, sink: Arc<std::sync::Mutex<Vec<String>>>) -> (Step, StepStatus, String) {
    let req_file = PathBuf::from(&install_path).join("requirements.txt");

    if !req_file.exists() {
        return (
            Step::Deps,
            StepStatus::Failed("requirements.txt not found — clone may have failed".to_string()),
            String::new(),
        );
    }

    // Run pip inside the conda env. Stream output live so a multi-GB download
    // (torch &c.) shows progress instead of looking frozen.
    let (program, mut base_args) = find_conda_pip();
    base_args.extend(["install".into(), "-r".into(), req_file.to_string_lossy().to_string()]);
    match run_cmd_streamed(program, base_args, None, sink).await {
        // lines were already streamed live → empty log_output to avoid double-printing
        Ok(_) => (Step::Deps, StepStatus::Done("Dependencies installed".to_string()), String::new()),
        Err(e) => (Step::Deps, StepStatus::Failed(format!("pip install failed: {}", e)), String::new()),
    }
}

// ── Async uninstall logic ───────────────────────────────────────────────────

async fn uninstall_conda_env() -> (String, bool) {
    match run_cmd_full_async(find_conda(), vec!["remove".into(), "-n".into(), "sapphire".into(), "--all".into(), "-y".into()]).await {
        Ok(out) => (format!("Removed conda environment 'sapphire'.\n{}", out), true),
        Err(e) => {
            if e.contains("does not exist") || e.contains("not found") {
                ("Conda environment 'sapphire' doesn't exist — nothing to remove.".to_string(), true)
            } else {
                (format!("Failed to remove conda environment: {}", e), false)
            }
        }
    }
}

async fn uninstall_delete_folder(install_path: String) -> (String, bool) {
    let path = install_path.clone();
    tokio::task::spawn_blocking(move || {
        let p = PathBuf::from(&path);
        if !p.exists() {
            return (format!("Folder {} doesn't exist — nothing to delete.", path), true);
        }
        match std::fs::remove_dir_all(&p) {
            Ok(()) => (format!("Deleted {}", path), true),
            Err(e) => (format!("Couldn't delete {}: {}", path, e), false),
        }
    })
    .await
    .unwrap_or_else(|e| (format!("Task failed: {}", e), false))
}

async fn uninstall_delete_userdata(install_path: String) -> (String, bool) {
    let path = install_path.clone();
    tokio::task::spawn_blocking(move || {
        let user_dir = PathBuf::from(&path).join("user");
        if !user_dir.exists() {
            return (format!("No user folder found at {}\\user — nothing to delete.", path), true);
        }
        match std::fs::remove_dir_all(&user_dir) {
            Ok(()) => (format!("Deleted {}\\user — your settings and data are gone.", path), true),
            Err(e) => (format!("Couldn't delete {}\\user: {}", path, e), false),
        }
    })
    .await
    .unwrap_or_else(|e| (format!("Task failed: {}", e), false))
}

// ── Update check ───────────────────────────────────────────────────────────

async fn check_for_updates(install_path: String, branch: Branch) -> Option<u32> {
    let git_dir = PathBuf::from(&install_path).join(".git");
    if !git_dir.exists() { return None; }

    let git_branch = match branch {
        Branch::Development => "dev",
        Branch::Stable => "main",
    };

    // Fetch latest from remote
    let _ = run_cmd_full_async(find_git(), vec![
        "-C".into(), install_path.clone(), "fetch".into(), "origin".into(),
    ]).await;

    // Count commits behind
    match run_cmd_full_async(find_git(), vec![
        "-C".into(), install_path,
        "rev-list".into(), "--count".into(),
        format!("HEAD..origin/{}", git_branch),
    ]).await {
        Ok(count) => count.trim().parse::<u32>().ok(),
        Err(_) => None,
    }
}

// ── Update helpers ─────────────────────────────────────────────────────────

async fn git_stash_and_pull(install_path: String) -> (String, bool) {
    // Check for local changes
    let dirty = match run_cmd_full_async(find_git(), vec![
        "-C".into(), install_path.clone(), "status".into(), "--porcelain".into(),
    ]).await {
        Ok(out) => !out.trim().is_empty(),
        Err(_) => false,
    };

    if dirty {
        // Stash local changes
        match run_cmd_full_async(find_git(), vec![
            "-C".into(), install_path.clone(), "stash".into(), "push".into(),
            "-m".into(), "sapphire-launcher auto-stash before update".into(),
        ]).await {
            Ok(_) => {}
            Err(e) => return (format!("Couldn't stash local changes: {}", e), false),
        }
    }

    // Pull
    let result = match run_cmd_full_async(find_git(), vec![
        "-C".into(), install_path.clone(), "pull".into(),
    ]).await {
        Ok(out) => {
            let summary = out.lines().next().unwrap_or(&out).to_string();
            if dirty {
                (format!("pull: {} (your local changes were stashed — run 'git stash pop' to restore)", summary), true)
            } else {
                (format!("pull: {}", summary), true)
            }
        }
        Err(e) => (format!("pull failed: {}", e), false),
    };

    result
}

// ── Troubleshoot checks ────────────────────────────────────────────────────

async fn ts_check_running() -> (TsCheck, TsStatus) {
    match run_cmd_full_async("curl".into(), vec![
        "-sk".into(), "--max-time".into(), "5".into(),
        "https://localhost:8073/api/health".into(),
    ]).await {
        Ok(body) if body.contains("ok") => (TsCheck::SapphireRunning, TsStatus::Ok("Sapphire is responding".into())),
        Ok(_) => (TsCheck::SapphireRunning, TsStatus::Problem("Sapphire responded but health check failed".into())),
        Err(_) => (TsCheck::SapphireRunning, TsStatus::Problem("Can't reach Sapphire — is it running?".into())),
    }
}

async fn ts_check_webui() -> (TsCheck, TsStatus) {
    match run_cmd_full_async("curl".into(), vec![
        "-sk".into(), "--max-time".into(), "5".into(),
        "-o".into(), null_device().into(),
        "-w".into(), "%{http_code}".into(),
        "-L".into(), "https://localhost:8073/".into(),
    ]).await {
        Ok(code) => {
            let code = code.trim().to_string();
            // curl output may have extra text before the code — grab last 3 chars
            let status = code.chars().rev().take(3).collect::<String>().chars().rev().collect::<String>();
            if status == "200" {
                (TsCheck::WebUi, TsStatus::Ok("Web UI is loading fine".into()))
            } else if status == "500" {
                (TsCheck::WebUi, TsStatus::Problem("Web UI returns error 500 — likely a package version issue".into()))
            } else {
                (TsCheck::WebUi, TsStatus::Problem(format!("Web UI returned HTTP {}", status)))
            }
        }
        Err(_) => (TsCheck::WebUi, TsStatus::Problem("Can't reach web UI — is Sapphire running?".into())),
    }
}

async fn ts_check_deps() -> (TsCheck, TsStatus) {
    let (program, mut args) = find_conda_pip();
    args.push("check".into());
    match run_cmd_full_async(program, args).await {
        Ok(output) if output.contains("No broken requirements") => {
            (TsCheck::DepsHealth, TsStatus::Ok("All dependencies look good".into()))
        }
        Ok(output) => {
            let problems: Vec<&str> = output.lines().take(5).collect();
            (TsCheck::DepsHealth, TsStatus::Problem(format!("Broken packages:\n{}", problems.join("\n"))))
        }
        Err(e) => {
            // pip check returns non-zero when deps are broken — that's the actual output
            if e.contains("has requirement") || e.contains("not installed") {
                let problems: Vec<&str> = e.lines().take(5).collect();
                (TsCheck::DepsHealth, TsStatus::Problem(format!("Broken packages:\n{}", problems.join("\n"))))
            } else {
                (TsCheck::DepsHealth, TsStatus::Problem(format!("Couldn't check: {}", e)))
            }
        }
    }
}

/// Find git executable on disk (Linux: rely on PATH).
#[cfg(not(windows))]
fn find_git() -> String {
    if let Ok(out) = std::process::Command::new("which").arg("git").output() {
        if out.status.success() {
            let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !p.is_empty() && PathBuf::from(&p).exists() {
                return p;
            }
        }
    }
    "git".into()
}

/// Find git executable on disk
#[cfg(windows)]
fn find_git() -> String {
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    let common_paths = [
        "C:\\Program Files\\Git\\cmd\\git.exe".to_string(),
        "C:\\Program Files (x86)\\Git\\cmd\\git.exe".to_string(),
        "C:\\Program Files\\Git\\bin\\git.exe".to_string(),
        format!("{}\\AppData\\Local\\Programs\\Git\\cmd\\git.exe", home),
        format!("{}\\scoop\\shims\\git.exe", home),
        format!("{}\\AppData\\Local\\GitHubDesktop\\app\\resources\\app\\git\\cmd\\git.exe", home),
    ];
    for p in &common_paths {
        if PathBuf::from(p).exists() {
            return p.clone();
        }
    }
    // Try refreshing PATH from registry and searching there
    if let Ok(output) = hidden_cmd("cmd")
        .args(["/c", "where", "git"])
        .output()
    {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout);
            if let Some(first_line) = path.lines().next() {
                let p = first_line.trim();
                if !p.is_empty() && PathBuf::from(p).exists() {
                    return p.to_string();
                }
            }
        }
    }
    "git".into() // final fallback
}

/// Find conda executable on disk
#[cfg(windows)]
fn find_conda() -> String {
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    let conda_paths = [
        format!("{}\\miniconda3\\Scripts\\conda.exe", home),
        format!("{}\\Miniconda3\\Scripts\\conda.exe", home),
        format!("{}\\anaconda3\\Scripts\\conda.exe", home),
        format!("{}\\Anaconda3\\Scripts\\conda.exe", home),
    ];
    for p in &conda_paths {
        if PathBuf::from(p).exists() {
            return p.clone();
        }
    }
    "conda".into() // fallback to PATH
}

/// Find conda executable on disk (Linux locations, then PATH).
#[cfg(not(windows))]
fn find_conda() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let conda_paths = [
        format!("{}/miniconda3/bin/conda", home),
        format!("{}/anaconda3/bin/conda", home),
        "/opt/miniconda3/bin/conda".to_string(),
        "/opt/anaconda3/bin/conda".to_string(),
    ];
    for p in &conda_paths {
        if PathBuf::from(p).exists() {
            return p.clone();
        }
    }
    if let Ok(out) = std::process::Command::new("which").arg("conda").output() {
        if out.status.success() {
            let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !p.is_empty() && PathBuf::from(&p).exists() {
                return p;
            }
        }
    }
    "conda".into() // fallback to PATH
}

#[cfg(windows)]
fn find_conda_pip() -> (String, Vec<String>) {
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    let pip_paths = [
        format!("{}\\miniconda3\\envs\\sapphire\\Scripts\\pip.exe", home),
        format!("{}\\Miniconda3\\envs\\sapphire\\Scripts\\pip.exe", home),
        format!("{}\\anaconda3\\envs\\sapphire\\Scripts\\pip.exe", home),
    ];
    for p in &pip_paths {
        if PathBuf::from(p).exists() {
            return (p.clone(), vec![]);
        }
    }
    // Fallback to conda run
    (find_conda(), vec!["run".into(), "-n".into(), "sapphire".into(), "pip".into()])
}

/// Find pip in the conda sapphire env directly (Linux), else `conda run`.
#[cfg(not(windows))]
fn find_conda_pip() -> (String, Vec<String>) {
    let home = std::env::var("HOME").unwrap_or_default();
    let pip_paths = [
        format!("{}/miniconda3/envs/sapphire/bin/pip", home),
        format!("{}/anaconda3/envs/sapphire/bin/pip", home),
    ];
    for p in &pip_paths {
        if PathBuf::from(p).exists() {
            return (p.clone(), vec![]);
        }
    }
    // Fallback to conda run
    (find_conda(), vec!["run".into(), "-n".into(), "sapphire".into(), "pip".into()])
}

async fn ts_fix_deps(install_path: String) -> (TsCheck, String, bool) {
    let req = PathBuf::from(&install_path).join("requirements.txt");
    let (program, mut base_args) = find_conda_pip();
    base_args.extend(["install".into(), "-r".into(), req.to_string_lossy().to_string()]);
    match run_cmd_full_async(program, base_args).await {
        Ok(out) => (TsCheck::DepsHealth, format!("Repaired! Restart Sapphire to apply."), true),
        Err(e) => (TsCheck::DepsHealth, format!("Repair failed: {}", e), false),
    }
}

async fn ts_check_gpu() -> (TsCheck, TsStatus) {
    match run_cmd_async("nvidia-smi".into(), vec![
        "--query-gpu=name,memory.total".into(), "--format=csv,noheader,nounits".into(),
    ]).await {
        Ok(output) => {
            let info = output.trim().to_string();
            if let Some((name, mem)) = info.split_once(',') {
                let mem_gb = mem.trim().parse::<f64>().unwrap_or(0.0) / 1024.0;
                (TsCheck::Gpu, TsStatus::Ok(format!("{} ({:.0} GB) — voice features will use GPU", name.trim(), mem_gb)))
            } else {
                (TsCheck::Gpu, TsStatus::Ok(format!("NVIDIA GPU found: {}", info)))
            }
        }
        Err(_) => (TsCheck::Gpu, TsStatus::Ok("No NVIDIA GPU found — voice features will use CPU (still works, just slower)".into())),
    }
}

async fn ts_check_plugins(install_path: String) -> (TsCheck, TsStatus) {
    let install_dir = PathBuf::from(&install_path).join("install");
    if !install_dir.exists() {
        return (TsCheck::Plugins, TsStatus::Ok("No optional features found".into()));
    }

    let (pip_program, pip_base) = find_conda_pip();

    // Find all requirements-*.txt files
    let mut results: Vec<String> = Vec::new();
    let mut any_missing = false;

    let features = [
        ("requirements-stt.txt", "Speech-to-Text"),
        ("requirements-tts.txt", "Text-to-Speech"),
        ("requirements-wakeword.txt", "Wake Word"),
    ];

    for (file, label) in &features {
        let req_path = install_dir.join(file);
        if !req_path.exists() { continue; }

        // Read the requirements file and check each package
        let contents = match tokio::fs::read_to_string(&req_path).await {
            Ok(c) => c,
            Err(_) => continue,
        };

        let mut all_installed = true;
        let mut missing = Vec::new();

        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { continue; }
            // Extract package name (before any >= or == specifier)
            let pkg_name = line.split(&['>', '<', '=', '!', '['][..]).next().unwrap_or(line).trim();

            let mut args = pip_base.clone();
            args.extend(["show".into(), pkg_name.into()]);
            match run_cmd_full_async(pip_program.clone(), args).await {
                Ok(_) => {} // installed
                Err(_) => {
                    all_installed = false;
                    missing.push(pkg_name.to_string());
                }
            }
        }

        if all_installed {
            results.push(format!("{}: installed", label));
        } else {
            any_missing = true;
            results.push(format!("{}: missing {}", label, missing.join(", ")));
        }
    }

    let summary = results.join(" | ");
    if any_missing {
        (TsCheck::Plugins, TsStatus::Problem(summary))
    } else if results.is_empty() {
        (TsCheck::Plugins, TsStatus::Ok("No optional features found".into()))
    } else {
        (TsCheck::Plugins, TsStatus::Ok(summary))
    }
}

async fn ts_fix_plugins(install_path: String) -> (TsCheck, String, bool) {
    let install_dir = PathBuf::from(&install_path).join("install");
    let (pip_program, pip_base) = find_conda_pip();

    let features = ["requirements-stt.txt", "requirements-tts.txt", "requirements-wakeword.txt"];
    let mut any_fail = false;

    for file in &features {
        let req_path = install_dir.join(file);
        if !req_path.exists() { continue; }

        let mut args = pip_base.clone();
        args.extend(["install".into(), "-r".into(), req_path.to_string_lossy().to_string()]);
        if let Err(e) = run_cmd_full_async(pip_program.clone(), args).await {
            any_fail = true;
            return (TsCheck::Plugins, format!("Failed on {}: {}", file, e), false);
        }
    }

    if any_fail {
        (TsCheck::Plugins, "Some features failed to install.".into(), false)
    } else {
        (TsCheck::Plugins, "All optional features installed. Restart Sapphire to apply.".into(), true)
    }
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

                // Stop sapphire if running before switching
                if self.sapphire_running || self.sapphire_stopping {
                    self.log_lines.push("Stopping Sapphire for branch switch...".to_string());
                    self.kill_sapphire_processes();
                    self.sapphire_stopping = true;
                }

                let path = self.install_path.clone();
                let git_branch = match branch {
                    Branch::Development => "dev",
                    Branch::Stable => "main",
                };
                self.log_lines.push(format!("Switching to {} branch...", git_branch));
                let b = git_branch.to_string();
                Task::perform(
                    async move {
                        // Wait for sapphire to die if it was running
                        for _ in 0..20 {
                            let still_up = run_cmd_full_async("curl".into(), vec![
                                "-sk".into(), "--max-time".into(), "2".into(),
                                "https://localhost:8073/api/health".into(),
                            ]).await.map(|r| r.contains("ok")).unwrap_or(false);
                            if !still_up { break; }
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        }
                        run_cmd_full_async(find_git(), vec!["-C".into(), path, "checkout".into(), b.clone()]).await
                            .map(|out| (format!("Switched to {}: {}", b, out), true))
                            .unwrap_or_else(|e| (format!("Branch switch failed: {}", e), false))
                    },
                    |(msg, ok)| Message::SwitchBranchResult(msg, ok),
                )
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

                // Check if sapphire is already running (from a previous session/cmd window)
                self.log_lines.push("Checking if Sapphire is already running...".to_string());
                Task::perform(
                    async {
                        run_cmd_full_async("curl".into(), vec![
                            "-sk".into(), "--max-time".into(), "3".into(),
                            "https://localhost:8073/api/health".into(),
                        ]).await.map(|b| b.contains("ok")).unwrap_or(false)
                    },
                    |running| Message::LaunchPreCheck(running, true),
                )
            }
            Message::LaunchPreCheck(already_running, user_initiated) => {
                // Service mode: running state + logs are owned by ServiceDetected.
                if self.service.is_some() {
                    self.sapphire_running = already_running;
                    if user_initiated {
                        self.active_tab = Tab::Running;
                    }
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
                    // Only switch to Running tab if user clicked Launch
                    if user_initiated {
                        self.active_tab = Tab::Running;
                    }
                    Task::none()
                } else if user_initiated {
                    // Not running, user clicked Launch — start it
                    self.sapphire_running = true;
                    self.sapphire_log.clear();
                    self.active_tab = Tab::Running;
                    self.log_lines.push("Launching Sapphire...".to_string());
                    let path = self.install_path.clone();
                    let pid = self.sapphire_pid.clone();
                    Task::stream(launch_sapphire_stream(path, pid))
                } else {
                    // Startup check — not running, nothing to do
                    Task::none()
                }
            }
            Message::StopSapphire => {
                // Service mode: stop the unit (and refresh status) instead of pkill.
                if self.service.is_some() {
                    return self.update(Message::ServiceStop);
                }
                self.sapphire_stopping = true;
                self.log_lines.push("Stopping Sapphire...".to_string());
                self.kill_sapphire_processes();

                // Poll until health endpoint stops responding
                Task::perform(async {
                    for _ in 0..20 {
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        let still_up = run_cmd_full_async("curl".into(), vec![
                            "-sk".into(), "--max-time".into(), "2".into(),
                            "https://localhost:8073/api/health".into(),
                        ]).await.map(|b| b.contains("ok")).unwrap_or(false);
                        if !still_up {
                            return;
                        }
                    }
                }, |_| Message::SapphireStopConfirmed)
            }
            Message::OpenBrowser => {
                open_url("https://localhost:8073");
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
                Task::perform(
                    check_for_updates(path, branch),
                    Message::UpdatesAvailable,
                )
            }
            Message::UpdatesAvailable(count) => {
                self.checking_updates = false;
                self.updates_available = count;
                Task::none()
            }
            Message::UpdateClicked => {
                if self.sapphire_running || self.sapphire_stopping {
                    // Auto-stop sapphire, then update after it's dead
                    self.log_lines.push("[update] Stopping Sapphire first...".to_string());
                    self.kill_sapphire_processes();
                    self.sapphire_stopping = true;
                    // Poll until dead, then trigger update
                    return Task::perform(async {
                        for _ in 0..20 {
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            let still_up = run_cmd_full_async("curl".into(), vec![
                                "-sk".into(), "--max-time".into(), "2".into(),
                                "https://localhost:8073/api/health".into(),
                            ]).await.map(|b| b.contains("ok")).unwrap_or(false);
                            if !still_up { return; }
                        }
                    }, |_| Message::UpdateAfterStop);
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
                        let (program, mut args) = find_conda_pip();
                        let req = PathBuf::from(&self.install_path).join("requirements.txt");
                        args.extend(["install".into(), "-r".into(), req.to_string_lossy().to_string()]);
                        return Task::perform(async move {
                            match run_cmd_full_async(program, args).await {
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
            Message::SapphireLine(line) => {
                self.sapphire_log.push(line);
                self.cap_sapphire_log();
                Task::none()
            }
            Message::SapphireLines(lines) => {
                self.sapphire_log.extend(lines);
                self.cap_sapphire_log();
                Task::none()
            }
            Message::SapphireExited(msg) => {
                self.sapphire_running = false;
                self.sapphire_stopping = false;
                self.sapphire_log.push(format!("--- {} ---", msg));
                self.log_lines.push(msg);
                Task::none()
            }
            Message::SapphireStopConfirmed => {
                self.sapphire_running = false;
                self.sapphire_stopping = false;
                self.log_lines.push("Sapphire stopped.".to_string());
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
                Task::batch(vec![
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
                ])
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

                Task::batch(vec![
                    Task::perform(ts_check_running(), |(c, s)| Message::TroubleshootResult(c, s)),
                    Task::perform(ts_check_webui(), |(c, s)| Message::TroubleshootResult(c, s)),
                    Task::perform(ts_check_deps(), |(c, s)| Message::TroubleshootResult(c, s)),
                    Task::perform(ts_check_plugins(path), |(c, s)| Message::TroubleshootResult(c, s)),
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
                self.log_lines.push("[service] Creating systemd --user unit...".to_string());
                let path = self.install_path.clone();
                Task::perform(install_service(path), |(m, ok)| Message::ServiceInstallResult(m, ok))
            }
            Message::ServiceInstallResult(msg, ok) => {
                self.log_lines.push(format!("[service] {}", msg));
                if ok {
                    self.sapphire_running = true;
                    self.streaming_journal = true;
                    self.journal_epoch += 1;
                    self.sapphire_log.clear();
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
                    self.sapphire_running = false;
                    self.sapphire_stopping = false;
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
            Message::EnvEdit(action) => {
                self.env_content.perform(action);
                Task::none()
            }
            Message::EnvSaveRestart => {
                self.log_lines.push("[service] Saving environment, then restarting...".to_string());
                Task::perform(write_env_file(self.env_content.text()), Message::EnvSaved)
            }
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

    /// Kill sapphire — the systemd unit in service mode, else the process tree.
    /// (The journalctl follower is a Subscription; it dies when streaming_journal
    /// goes false and iced drops it.)
    fn kill_sapphire_processes(&self) {
        if self.service.is_some() {
            let _ = hidden_cmd("systemctl").args(["--user", "stop", "sapphire"]).output();
            return;
        }
        let pid = self.sapphire_pid.load(Ordering::Relaxed);
        kill_process_tree(pid);
        if pid > 0 {
            self.sapphire_pid.store(0, Ordering::Relaxed);
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

        Subscription::batch(subs)
    }

}

// ── Helpers ────────────────────────────────────────────────────────────────

fn step_label(step: Step) -> &'static str {
    match step {
        #[cfg(windows)]
        Step::Git => "Check for Git",
        #[cfg(not(windows))]
        Step::Git => "Check system packages (git, audio, curl)",
        Step::Conda => "Check for Miniconda",
        Step::CondaInit => "Initialize conda",
        Step::PythonEnv => "Create Python environment",
        Step::Clone => "Clone Sapphire",
        Step::Deps => "Install dependencies",
        Step::Done => "Done!",
    }
}

/// Strip ANSI escape sequences from a string
fn ts_label(check: TsCheck) -> &'static str {
    match check {
        TsCheck::SapphireRunning => "Sapphire responding",
        TsCheck::WebUi => "Web UI loading",
        TsCheck::DepsHealth => "Package health",
        TsCheck::Plugins => "Optional features",
        TsCheck::Gpu => "GPU",
    }
}

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
