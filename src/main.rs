use iced::widget::{
    button, column, container, horizontal_space, pick_list, row, scrollable, text,
    text_input, Column,
};
use iced::{color, Element, Font, Length, Padding, Task, Theme};
use iced::window;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, atomic::{AtomicU32, Ordering}};
use tokio::io::{AsyncBufReadExt, BufReader};

fn main() -> iced::Result {
    let icon = load_window_icon();

    let win_settings = window::Settings {
        size: iced::Size::new(700.0, 500.0),
        icon,
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
        .window(win_settings)
        .run_with(|| {
            let app = App::default();
            let task = Task::perform(
                async {
                    run_cmd_full_async("curl".into(), vec![
                        "-sk".into(), "--max-time".into(), "3".into(),
                        "https://localhost:8073/api/health".into(),
                    ]).await.map(|b| b.contains("ok")).unwrap_or(false)
                },
                Message::LaunchPreCheck,
            );
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
    fn indicator(&self) -> &str {
        match self {
            StepStatus::NotStarted => "○",
            StepStatus::Checking => "◌",
            StepStatus::Found(_) => "●",
            StepStatus::NotFound(_) => "◎",
            StepStatus::Installing => "◌",
            StepStatus::Done(_) => "●",
            StepStatus::Failed(_) => "✕",
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
    updating: bool,
    update_status: Vec<(String, StepStatus)>, // label, status
    // Troubleshoot
    ts_checks: Vec<(TsCheck, TsStatus)>,
    ts_running: bool,
    // Sapphire process
    sapphire_running: bool,
    sapphire_stopping: bool,
    sapphire_log: Vec<String>,
    sapphire_pid: Arc<AtomicU32>,
}

impl Default for App {
    fn default() -> Self {
        let home = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_else(|_| "C:\\".to_string());

        Self {
            install_path: format!("{}\\sapphire", home),
            selected_branch: Some(Branch::Stable),
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
            updating: false,
            update_status: vec![
                ("Pull latest changes".into(), StepStatus::NotStarted),
                ("Update dependencies".into(), StepStatus::NotStarted),
            ],
            ts_checks: vec![
                (TsCheck::SapphireRunning, TsStatus::NotChecked),
                (TsCheck::WebUi, TsStatus::NotChecked),
                (TsCheck::Starlette, TsStatus::NotChecked),
                (TsCheck::DepsHealth, TsStatus::NotChecked),
                (TsCheck::Plugins, TsStatus::NotChecked),
            ],
            ts_running: false,
            confirm_remove_env: false,
            confirm_delete_folder: false,
            confirm_delete_userdata: false,
            sapphire_running: false,
            sapphire_stopping: false,
            sapphire_log: Vec::new(),
            sapphire_pid: Arc::new(AtomicU32::new(0)),
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
    LaunchPreCheck(bool),   // true = sapphire already running
    SapphireLine(String),   // a line of output from sapphire
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
    UpdateClicked,
    UpdateAfterStop,
    UpdateStepDone(String, bool), // message, success — chains to next step
    // Troubleshoot
    TroubleshootCheck,
    TroubleshootResult(TsCheck, TsStatus),
    TroubleshootFix(TsCheck),
    TroubleshootFixResult(TsCheck, String, bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TsCheck {
    SapphireRunning,
    WebUi,
    Starlette,
    DepsHealth,
    Plugins,
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
async fn run_cmd_async(program: String, args: Vec<String>) -> Result<String, String> {
    let result: Result<Result<String, String>, _> = tokio::task::spawn_blocking(move || {
        Command::new(&program)
            .args(&args)
            .output()
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
async fn check_git() -> (Step, StepStatus) {
    match run_cmd_async("git".into(), vec!["--version".into()]).await {
        Ok(ver) => (Step::Git, StepStatus::Found(ver)),
        Err(_) => (
            Step::Git,
            StepStatus::NotFound("Git not found — we'll install it".to_string()),
        ),
    }
}

async fn check_conda() -> (Step, StepStatus) {
    match run_cmd_async("conda".into(), vec!["--version".into()]).await {
        Ok(ver) => (Step::Conda, StepStatus::Found(ver)),
        Err(_) => {
            let home = std::env::var("USERPROFILE").unwrap_or_default();
            let paths = [
                format!("{}\\miniconda3\\Scripts\\conda.exe", home),
                format!("{}\\anaconda3\\Scripts\\conda.exe", home),
                format!("{}\\Miniconda3\\Scripts\\conda.exe", home),
                format!("{}\\Anaconda3\\Scripts\\conda.exe", home),
            ];
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
    match run_cmd_async("conda".into(), vec!["info".into(), "--json".into()]).await {
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
    match run_cmd_async("conda".into(), vec!["env".into(), "list".into()]).await {
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
    if req_file.exists() {
        (
            Step::Deps,
            StepStatus::Found("requirements.txt found — will verify deps".to_string()),
        )
    } else {
        (
            Step::Deps,
            StepStatus::NotFound("Will install after cloning".to_string()),
        )
    }
}

// ── Async install logic ────────────────────────────────────────────────────

/// Run a command on a background thread, returning combined stdout+stderr for logging
async fn run_cmd_full_async(program: String, args: Vec<String>) -> Result<String, String> {
    let result: Result<Result<String, String>, _> = tokio::task::spawn_blocking(move || {
        match Command::new(&program).args(&args).output() {
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

async fn install_conda_init() -> (Step, StepStatus, String) {
    match run_cmd_full_async("conda".into(), vec!["init".into()]).await {
        Ok(out) => (Step::CondaInit, StepStatus::Done("Conda initialized".to_string()), out),
        Err(e) => (Step::CondaInit, StepStatus::Failed(format!("Conda init had a problem: {}", e)), e),
    }
}

async fn install_python_env() -> (Step, StepStatus, String) {
    match run_cmd_full_async("conda".into(), vec!["create".into(), "-n".into(), "sapphire".into(), "python=3.11".into(), "-y".into()]).await {
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

    // If folder exists but isn't a repo, bail — don't blow away their stuff
    if repo_path.exists() && !repo_path.join(".git").exists() {
        return (
            Step::Clone,
            StepStatus::Failed(format!(
                "Folder {} already exists but isn't a Sapphire repo. Move or rename it first.",
                install_path
            )),
            String::new(),
        );
    }

    // If already a repo, just make sure we're on the right branch
    if repo_path.join(".git").exists() {
        let _ = run_cmd_full_async("git".into(), vec!["-C".into(), install_path.clone(), "checkout".into(), git_branch.to_string()]).await;
        return (
            Step::Clone,
            StepStatus::Done(format!("Repo exists, switched to {} branch", git_branch)),
            String::new(),
        );
    }

    match run_cmd_full_async(
        "git".into(),
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

async fn install_deps(install_path: String) -> (Step, StepStatus, String) {
    let req_file = PathBuf::from(&install_path).join("requirements.txt");

    if !req_file.exists() {
        return (
            Step::Deps,
            StepStatus::Failed("requirements.txt not found — clone may have failed".to_string()),
            String::new(),
        );
    }

    // We need to run pip inside the conda env.
    // Use conda run to execute pip in the sapphire environment.
    match run_cmd_full_async(
        "conda".into(),
        vec![
            "run".into(),
            "-n".into(),
            "sapphire".into(),
            "pip".into(),
            "install".into(),
            "-r".into(),
            req_file.to_string_lossy().to_string(),
        ],
    ).await {
        Ok(out) => (
            Step::Deps,
            StepStatus::Done("Dependencies installed".to_string()),
            out,
        ),
        Err(e) => (
            Step::Deps,
            StepStatus::Failed(format!("pip install failed: {}", e)),
            e,
        ),
    }
}

// ── Async uninstall logic ───────────────────────────────────────────────────

async fn uninstall_conda_env() -> (String, bool) {
    match run_cmd_full_async("conda".into(), vec!["remove".into(), "-n".into(), "sapphire".into(), "--all".into(), "-y".into()]).await {
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
        "-o".into(), "NUL".into(),
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

async fn ts_check_starlette() -> (TsCheck, TsStatus) {
    let (program, mut args) = find_conda_pip();
    args.extend(["show".into(), "starlette".into()]);
    match run_cmd_full_async(program, args).await {
        Ok(output) => {
            if let Some(ver_line) = output.lines().find(|l| l.starts_with("Version:")) {
                let ver = ver_line.replace("Version:", "").trim().to_string();
                if ver == "0.52.1" {
                    (TsCheck::Starlette, TsStatus::Ok(format!("starlette {} (correct)", ver)))
                } else {
                    (TsCheck::Starlette, TsStatus::Problem(format!("starlette {} installed, needs 0.52.1", ver)))
                }
            } else {
                (TsCheck::Starlette, TsStatus::Problem("starlette not installed".into()))
            }
        }
        Err(e) => (TsCheck::Starlette, TsStatus::Problem(format!("Couldn't check: {}", e))),
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

/// Find pip in the conda sapphire env directly
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
    ("conda".into(), vec!["run".into(), "-n".into(), "sapphire".into(), "pip".into()])
}

async fn ts_fix_starlette() -> (TsCheck, String, bool) {
    let (program, mut base_args) = find_conda_pip();
    base_args.extend(["install".into(), "starlette==0.52.1".into()]);
    match run_cmd_full_async(program, base_args).await {
        Ok(out) => (TsCheck::Starlette, format!("Fixed! Restart Sapphire to apply."), true),
        Err(e) => (TsCheck::Starlette, format!("Fix failed: {}", e), false),
    }
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

// ── Launch streaming ───────────────────────────────────────────────────────

fn launch_sapphire_stream(install_path: String, pid_store: Arc<AtomicU32>) -> impl futures::Stream<Item = Message> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

    tokio::task::spawn(async move {
        // Find the conda env's python directly — avoids conda run buffering issues
        let home = std::env::var("USERPROFILE").unwrap_or_default();
        let conda_python_paths = [
            format!("{}\\miniconda3\\envs\\sapphire\\python.exe", home),
            format!("{}\\Miniconda3\\envs\\sapphire\\python.exe", home),
            format!("{}\\anaconda3\\envs\\sapphire\\python.exe", home),
            format!("{}\\Anaconda3\\envs\\sapphire\\python.exe", home),
        ];

        let python_exe = conda_python_paths
            .iter()
            .find(|p| PathBuf::from(p).exists())
            .cloned();

        let (program, args) = if let Some(ref py) = python_exe {
            (py.as_str(), vec!["-u", "main.py"])
        } else {
            // Fallback to conda run
            ("conda", vec!["run", "-n", "sapphire", "python", "-u", "main.py"])
        };

        let mut cmd = tokio::process::Command::new(program);
        cmd.args(&args)
            .current_dir(&install_path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .env("PYTHONIOENCODING", "utf-8")
            .env("PYTHONUTF8", "1");

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(Message::SapphireExited(format!("Failed to start: {}", e)));
                return;
            }
        };

        // Store PID for stop button
        if let Some(pid) = child.id() {
            pid_store.store(pid, Ordering::Relaxed);
        }

        if let Some(ref py) = python_exe {
            let _ = tx.send(Message::SapphireLine(format!("Using {}", py)));
        }

        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let tx_out = tx.clone();
        let tx_err = tx.clone();

        // Read stdout — buffered line reading with lossy UTF-8
        let stdout_task = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut buf = Vec::new();
            use tokio::io::AsyncBufReadExt;
            while reader.read_until(b'\n', &mut buf).await.unwrap_or(0) > 0 {
                // Strip \r\n
                while buf.last() == Some(&b'\n') || buf.last() == Some(&b'\r') {
                    buf.pop();
                }
                let line = strip_ansi(&String::from_utf8_lossy(&buf));
                buf.clear();
                if tx_out.send(Message::SapphireLine(line)).is_err() {
                    break;
                }
            }
        });

        // Read stderr
        let stderr_task = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut buf = Vec::new();
            use tokio::io::AsyncBufReadExt;
            while reader.read_until(b'\n', &mut buf).await.unwrap_or(0) > 0 {
                while buf.last() == Some(&b'\n') || buf.last() == Some(&b'\r') {
                    buf.pop();
                }
                let line = strip_ansi(&String::from_utf8_lossy(&buf));
                buf.clear();
                if tx_err.send(Message::SapphireLine(line)).is_err() {
                    break;
                }
            }
        });

        // Wait for everything concurrently — don't deadlock
        let (_, _, status) = tokio::join!(stdout_task, stderr_task, child.wait());

        let msg = match status {
            Ok(s) if s.success() => "Sapphire exited.".to_string(),
            Ok(s) if s.code() == Some(42) => "Sapphire is restarting...".to_string(),
            Ok(s) => format!("Sapphire exited (code {}).", s.code().unwrap_or(-1)),
            Err(e) => format!("Error: {}", e),
        };
        let _ = tx.send(Message::SapphireExited(msg));
    });

    tokio_stream::wrappers::UnboundedReceiverStream::new(rx)
}

// ── Update ─────────────────────────────────────────────────────────────────

impl App {
    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::PathChanged(path) => {
                self.install_path = path;
                Task::none()
            }
            Message::BranchSelected(branch) => {
                self.selected_branch = Some(branch);
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
                        run_cmd_full_async("git".into(), vec!["-C".into(), path, "checkout".into(), b.clone()]).await
                            .map(|out| (format!("Switched to {}: {}", b, out), true))
                            .unwrap_or_else(|e| (format!("Branch switch failed: {}", e), false))
                    },
                    |(msg, ok)| Message::SwitchBranchResult(msg, ok),
                )
            }
            Message::BrowsePath => {
                // TODO: native file dialog
                Task::none()
            }
            Message::Launch => {
                if self.sapphire_running {
                    self.log_lines.push("Sapphire is already running.".to_string());
                    return Task::none();
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
                    Message::LaunchPreCheck,
                )
            }
            Message::LaunchPreCheck(already_running) => {
                if already_running {
                    // Sapphire is already up — we can't capture its logs but we can show status
                    self.sapphire_running = true;
                    self.sapphire_log.clear();
                    self.sapphire_log.push("Sapphire is already running (started outside this app).".into());
                    self.sapphire_log.push("".into());
                    self.sapphire_log.push("No live logs available for this session.".into());
                    self.sapphire_log.push("To see logs: click Stop, then Launch again.".into());
                    self.active_tab = Tab::Running;
                    self.log_lines.push("Sapphire already running.".to_string());
                    Task::none()
                } else {
                    // Not running, launch it
                    self.sapphire_running = true;
                    self.sapphire_log.clear();
                    self.active_tab = Tab::Running;
                    self.log_lines.push("Launching Sapphire...".to_string());
                    let path = self.install_path.clone();
                    let pid = self.sapphire_pid.clone();
                    Task::stream(launch_sapphire_stream(path, pid))
                }
            }
            Message::StopSapphire => {
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
                let _ = Command::new("cmd")
                    .args(["/c", "start", "https://localhost:8073"])
                    .spawn();
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
                        run_cmd_full_async("git".into(), vec!["-C".into(), path, "checkout".into(), b.clone()]).await
                            .map(|out| (format!("Switched to {}: {}", b, out), true))
                            .unwrap_or_else(|e| (format!("Branch switch failed: {}", e), false))
                    },
                    |(msg, ok)| Message::SwitchBranchResult(msg, ok),
                )
            }
            Message::SwitchBranchResult(msg, _ok) => {
                self.sapphire_running = false;
                self.sapphire_stopping = false;
                self.log_lines.push(msg);
                Task::none()
            }

            // ── Update flow ───────────────────────────────────────────
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
                Task::perform(async move {
                    match run_cmd_full_async("git".into(), vec!["-C".into(), path, "pull".into()]).await {
                        Ok(out) => (format!("pull: {}", out.lines().next().unwrap_or(&out)), true),
                        Err(e) => (format!("pull failed: {}", e), false),
                    }
                }, |(msg, ok)| Message::UpdateStepDone(msg, ok))
            }
            Message::UpdateAfterStop => {
                self.sapphire_running = false;
                self.sapphire_stopping = false;
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
                Task::perform(async move {
                    match run_cmd_full_async("git".into(), vec!["-C".into(), path, "pull".into()]).await {
                        Ok(out) => (format!("pull: {}", out.lines().next().unwrap_or(&out)), true),
                        Err(e) => (format!("pull failed: {}", e), false),
                    }
                }, |(msg, ok)| Message::UpdateStepDone(msg, ok))
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
                Task::none()
            }
            Message::SapphireLine(line) => {
                self.sapphire_log.push(line);
                // Cap at 5000 lines to prevent memory bloat
                if self.sapphire_log.len() > 5000 {
                    self.sapphire_log.drain(..1000);
                }
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

                self.log_lines.push("Starting install...".to_string());

                // Find the first step that needs work
                self.kick_next_install()
            }

            Message::InstallStepResult(step, status, log_output) => {
                let label = step_label(step);

                // Log the output
                if !log_output.is_empty() {
                    for line in log_output.lines() {
                        self.log_lines.push(format!("  {}", line));
                    }
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
                let log_text = self.sapphire_log.join("\n");
                // Use clip.exe on Windows, xclip on Linux
                let _ = std::process::Command::new("clip")
                    .stdin(std::process::Stdio::piped())
                    .spawn()
                    .and_then(|mut child| {
                        use std::io::Write;
                        if let Some(ref mut stdin) = child.stdin {
                            stdin.write_all(log_text.as_bytes())?;
                        }
                        child.wait()
                    });
                self.log_lines.push("Copied to clipboard.".to_string());
                Task::none()
            }
            Message::OpenRunLog => {
                // Write log to temp file and open in Notepad for selection/copying
                let log_text = self.sapphire_log.join("\n");
                let log_path = std::env::temp_dir().join("sapphire_log.txt");
                if std::fs::write(&log_path, &log_text).is_ok() {
                    let _ = Command::new("notepad").arg(&log_path).spawn();
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
                let key_path = PathBuf::from(std::env::var("APPDATA").unwrap_or_default())
                    .join("Sapphire").join("secret_key");
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
                let cred_path = PathBuf::from(std::env::var("APPDATA").unwrap_or_default())
                    .join("Sapphire").join("credentials.json");
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
                    Task::perform(ts_check_starlette(), |(c, s)| Message::TroubleshootResult(c, s)),
                    Task::perform(ts_check_deps(), |(c, s)| Message::TroubleshootResult(c, s)),
                    Task::perform(ts_check_plugins(path), |(c, s)| Message::TroubleshootResult(c, s)),
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
                    TsCheck::Starlette => Task::perform(ts_fix_starlette(), |(c, msg, ok)| {
                        Message::TroubleshootFixResult(c, msg, ok)
                    }),
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
        }
    }

    /// Find the next step that needs installing and fire it off.
    /// Steps that are already Found/Done get skipped.
    /// Kill all sapphire python processes
    fn kill_sapphire_processes(&self) {
        let pid = self.sapphire_pid.load(Ordering::Relaxed);
        if pid > 0 {
            let _ = Command::new("taskkill")
                .args(["/F", "/T", "/PID", &pid.to_string()])
                .output();
            self.sapphire_pid.store(0, Ordering::Relaxed);
        }
        let home = std::env::var("USERPROFILE").unwrap_or_default();
        let env_path = format!("{}\\miniconda3\\envs\\sapphire", home);
        if let Ok(output) = Command::new("wmic")
            .args(["process", "where", &format!("name='python.exe' and executablepath like '%{}%'", env_path.replace('\\', "\\\\")),
                "get", "processid"])
            .output()
        {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                if let Ok(pid) = line.trim().parse::<u32>() {
                    let _ = Command::new("taskkill")
                        .args(["/F", "/PID", &pid.to_string()])
                        .output();
                }
            }
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
            Step::Deps => Task::perform(install_deps(path), |(s, st, log)| {
                Message::InstallStepResult(s, st, log)
            }),
            Step::Done => unreachable!(),
        }
    }

    // ── View ───────────────────────────────────────────────────────────────

    fn view(&self) -> Element<Message> {
        column![
            self.view_header(),
            self.view_tab_bar(),
            self.view_tab_content(),
            self.view_log_panel(),
        ]
        .spacing(0)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }

    // ── Header bar ─────────────────────────────────────────────────────────

    fn view_header(&self) -> Element<Message> {
        let path_input = text_input("C:\\Users\\You\\Sapphire", &self.install_path)
            .on_input(Message::PathChanged)
            .width(Length::FillPortion(3));

        let browse_btn = button("Browse").on_press(Message::BrowsePath);

        let branch_picker = pick_list(
            BRANCHES,
            self.selected_branch,
            Message::BranchSelected,
        )
        .placeholder("Branch...");

        let mut header = row![
            path_input,
            browse_btn,
            horizontal_space(),
            branch_picker,
        ]
        .spacing(8)
        .padding(8)
        .align_y(iced::Alignment::Center);

        if self.sapphire_stopping {
            let stopping_btn = button(text("Stopping...").font(Font {
                weight: iced::font::Weight::Bold,
                ..Font::DEFAULT
            }))
            .style(button::secondary);

            header = header.push(stopping_btn);
        } else if self.sapphire_running {
            let open_btn = button(text("Open Browser").font(Font {
                weight: iced::font::Weight::Bold,
                ..Font::DEFAULT
            }))
            .on_press(Message::OpenBrowser)
            .style(button::primary);

            let stop_btn = button(text("Stop").font(Font {
                weight: iced::font::Weight::Bold,
                ..Font::DEFAULT
            }))
            .on_press(Message::StopSapphire)
            .style(button::danger);

            header = header.push(open_btn).push(stop_btn);
        } else {
            let launch_btn = button(text("Launch").font(Font {
                weight: iced::font::Weight::Bold,
                ..Font::DEFAULT
            }))
            .on_press(Message::Launch)
            .style(button::success);

            header = header.push(launch_btn);
        }

        container(header)
            .width(Length::Fill)
            .style(|_theme| container::Style {
                background: Some(iced::Background::Color(color!(0x181825))),
                ..Default::default()
            })
            .into()
    }

    // ── Tab bar ────────────────────────────────────────────────────────────

    fn view_tab_bar(&self) -> Element<Message> {
        let running_label = if self.sapphire_running { "Running •" } else { "Running" };
        let tabs = row![
            self.tab_button("Install", Tab::Install),
            self.tab_button("Update", Tab::Update),
            self.tab_button("Uninstall", Tab::Uninstall),
            self.tab_button("Troubleshoot", Tab::Troubleshoot),
            self.tab_button(running_label, Tab::Running),
        ]
        .spacing(0);

        container(tabs)
            .width(Length::Fill)
            .style(|_theme| container::Style {
                background: Some(iced::Background::Color(color!(0x11111b))),
                ..Default::default()
            })
            .into()
    }

    fn tab_button<'a>(&self, label: &'a str, tab: Tab) -> Element<'a, Message> {
        let is_active = self.active_tab == tab;

        let btn = button(text(label))
            .on_press(Message::TabSelected(tab))
            .padding([8, 20]);

        if is_active {
            container(btn.style(button::primary))
                .style(|_theme| container::Style {
                    border: iced::Border {
                        color: color!(0x3d85c6),
                        width: 0.0,
                        radius: 0.into(),
                    },
                    background: Some(iced::Background::Color(color!(0x1e1e2e))),
                    ..Default::default()
                })
                .into()
        } else {
            container(btn.style(button::text)).into()
        }
    }

    // ── Tab content ────────────────────────────────────────────────────────

    fn view_tab_content(&self) -> Element<Message> {
        // Running tab gets its own layout with fixed toolbar + scrollable log
        if self.active_tab == Tab::Running {
            return self.view_running_tab();
        }

        let content: Element<Message> = match self.active_tab {
            Tab::Install => self.view_install_tab(),
            Tab::Update => self.view_update_tab(),
            Tab::Uninstall => self.view_uninstall_tab(),
            Tab::Troubleshoot => self.view_troubleshoot_tab(),
            Tab::Running => unreachable!(),
        };

        container(
            scrollable(
                container(content).width(Length::Fill)
            )
            .height(Length::Fill)
            .width(Length::Fill),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(Padding { top: 10.0, right: 16.0, bottom: 6.0, left: 16.0 })
        .into()
    }

    fn view_install_tab(&self) -> Element<Message> {
        let mut steps_col = Column::new().spacing(6);

        for (step, status) in &self.steps {
            let indicator = text(status.indicator())
                .size(16)
                .color(status.color())
                .width(20);

            let label = text(step_label(*step)).size(15);

            let mut row_items = row![indicator, label].spacing(10).align_y(iced::Alignment::Center);

            // Show detail text if we have it
            if let Some(detail) = status.detail() {
                row_items = row_items.push(
                    text(format!("— {}", detail))
                        .size(12)
                        .color(color!(0x7f849c)),
                );
            }

            steps_col = steps_col.push(row_items);
        }

        // Action buttons
        let has_not_found = self
            .steps
            .iter()
            .any(|(s, st)| *s != Step::Done && matches!(st, StepStatus::NotFound(_)));

        let all_not_started = self.steps.iter().all(|(_, st)| *st == StepStatus::NotStarted);

        let mut buttons_row = row![].spacing(10);

        // Scan button
        let scan_label = if all_not_started { "Scan System" } else { "Re-scan" };
        let scan_btn = button(text(scan_label))
            .on_press_maybe(if self.scanning || self.installing {
                None
            } else {
                Some(Message::ScanClicked)
            })
            .style(button::primary);
        buttons_row = buttons_row.push(scan_btn);

        // Go button — only if scan found stuff to install
        if has_not_found {
            let go_btn = button(
                text("Go — Install Missing").font(Font {
                    weight: iced::font::Weight::Bold,
                    ..Font::DEFAULT
                }),
            )
            .on_press_maybe(if self.scanning || self.installing {
                None
            } else {
                Some(Message::GoClicked)
            })
            .style(button::success);
            buttons_row = buttons_row.push(go_btn);
        }

        column![
            text("Install Sapphire").size(18).font(Font {
                weight: iced::font::Weight::Bold,
                ..Font::DEFAULT
            }),
            steps_col,
            buttons_row,
        ]
        .spacing(8)
        .padding(Padding { top: 0.0, right: 0.0, bottom: 12.0, left: 0.0 })
        .into()
    }

    fn view_update_tab(&self) -> Element<Message> {
        let mut steps_col = Column::new().spacing(6);

        for (label, status) in &self.update_status {
            let indicator = text(status.indicator())
                .size(16)
                .color(status.color())
                .width(20);

            let label_text = text(label.as_str()).size(14);

            let mut row_items = row![indicator, label_text].spacing(10).align_y(iced::Alignment::Center);

            if let Some(detail) = status.detail() {
                row_items = row_items.push(
                    text(format!("— {}", detail)).size(11).color(color!(0x7f849c)),
                );
            }

            steps_col = steps_col.push(row_items);
        }

        let update_label = if self.updating {
            "Updating..."
        } else if self.sapphire_running || self.sapphire_stopping {
            "Stop & Update"
        } else {
            "Update"
        };

        let update_btn = button(text(update_label))
            .on_press_maybe(if self.updating || self.sapphire_stopping {
                None
            } else {
                Some(Message::UpdateClicked)
            })
            .style(button::primary);

        column![
            text("Update Sapphire").size(18).font(Font {
                weight: iced::font::Weight::Bold,
                ..Font::DEFAULT
            }),
            steps_col,
            update_btn,
        ]
        .spacing(10)
        .into()
    }

    fn view_uninstall_tab(&self) -> Element<Message> {
        let busy = self.uninstalling;

        // ═══════════════════════════════════════════════════════════
        // Quick Resets — safe, non-destructive to the install
        // ═══════════════════════════════════════════════════════════

        let reset_pw_btn = button(text("Reset Password").size(13))
            .on_press(Message::ResetPassword)
            .style(button::primary)
            .padding([4, 12]);

        let reset_creds_btn = button(text("Reset API Keys").size(13))
            .on_press(Message::ResetCredentials)
            .style(button::primary)
            .padding([4, 12]);

        let resets_section = column![
            text("Quick Resets").size(16).font(Font {
                weight: iced::font::Weight::Bold,
                ..Font::DEFAULT
            }),
            row![
                column![
                    reset_pw_btn,
                    text("Forgot your password? This clears it so Sapphire asks for a new one.")
                        .size(11).color(color!(0x7f849c)),
                ].spacing(3).width(Length::FillPortion(1)),
                column![
                    reset_creds_btn,
                    text("Clears saved API keys (Claude, OpenAI, etc). You'll re-enter them in Sapphire.")
                        .size(11).color(color!(0x7f849c)),
                ].spacing(3).width(Length::FillPortion(1)),
            ].spacing(16),
        ].spacing(8);

        // ═══════════════════════════════════════════════════════════
        // Danger Zone — destructive actions
        // ═══════════════════════════════════════════════════════════

        // Remove conda env
        let env_label = if self.confirm_remove_env { "YES, remove it" } else { "Remove conda env" };
        let remove_env_btn = button(text(env_label).size(13))
            .on_press_maybe(if busy { None } else { Some(Message::UninstallCondaEnvClick) })
            .style(button::danger)
            .padding([4, 12]);
        let env_desc = if self.confirm_remove_env {
            text("Click again to confirm.").size(11).color(color!(0xe74c3c))
        } else {
            text("Deletes the 'sapphire' Python environment and all packages.").size(11).color(color!(0x7f849c))
        };

        // Delete user data
        let ud_label = if self.confirm_delete_userdata { "YES, delete user data" } else { "Delete user data" };
        let delete_ud_btn = button(text(ud_label).size(13))
            .on_press_maybe(if busy { None } else { Some(Message::UninstallDeleteUserdataClick) })
            .style(button::danger)
            .padding([4, 12]);
        let ud_desc = if self.confirm_delete_userdata {
            text("Click again to confirm.").size(11).color(color!(0xe74c3c))
        } else {
            text("Removes sapphire/user/ — your settings and personal data.").size(11).color(color!(0x7f849c))
        };

        // Delete everything
        let folder_label = if self.confirm_delete_folder { "YES, delete everything" } else { "Delete Sapphire folder" };
        let delete_folder_btn = button(text(folder_label).size(13))
            .on_press_maybe(if busy { None } else { Some(Message::UninstallDeleteFolderClick) })
            .style(button::danger)
            .padding([4, 12]);
        let folder_desc = if self.confirm_delete_folder {
            text("FINAL WARNING. Everything will be permanently deleted.").size(11).color(color!(0xe74c3c))
        } else {
            text(format!("Nukes {} — code, settings, everything. Cannot be undone.", self.install_path))
                .size(11).color(color!(0x7f849c))
        };

        let danger_section = column![
            text("Danger Zone").size(16).font(Font {
                weight: iced::font::Weight::Bold,
                ..Font::DEFAULT
            }).color(color!(0xe74c3c)),
            text("These actions are destructive. Won't touch Git or Miniconda.").size(11).color(color!(0x7f849c)),
            column![remove_env_btn, env_desc].spacing(2),
            column![delete_ud_btn, ud_desc].spacing(2),
            column![delete_folder_btn, folder_desc].spacing(2),
        ].spacing(8);

        // Divider between sections
        let divider = container(text(""))
            .width(Length::Fill)
            .height(1)
            .style(|_theme| container::Style {
                background: Some(iced::Background::Color(color!(0x313244))),
                ..Default::default()
            });

        column![
            resets_section,
            divider,
            danger_section,
        ]
        .spacing(12)
        .into()
    }

    fn view_troubleshoot_tab(&self) -> Element<Message> {
        let mut checks_col = Column::new().spacing(8);

        for (check, status) in &self.ts_checks {
            let (indicator, color) = match status {
                TsStatus::NotChecked => ("○", color!(0x585b70)),
                TsStatus::Checking | TsStatus::Fixing => ("◌", color!(0x3d85c6)),
                TsStatus::Ok(_) | TsStatus::Fixed(_) => ("●", color!(0x4caf50)),
                TsStatus::Problem(_) => ("●", color!(0xe74c3c)),
            };

            let label_text = ts_label(*check);
            let detail = match status {
                TsStatus::Ok(s) | TsStatus::Problem(s) | TsStatus::Fixed(s) => Some(s.as_str()),
                TsStatus::Checking => Some("checking..."),
                TsStatus::Fixing => Some("fixing..."),
                TsStatus::NotChecked => None,
            };

            let mut check_row = row![
                text(indicator).size(14).color(color).width(18),
                text(label_text).size(14),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center);

            if let Some(d) = detail {
                check_row = check_row.push(
                    text(format!("— {}", d)).size(11).color(color!(0x7f849c))
                );
            }

            // Add Fix button for fixable problems
            let is_fixable = matches!(
                (check, status),
                (TsCheck::Starlette, TsStatus::Problem(_))
                    | (TsCheck::DepsHealth, TsStatus::Problem(_))
                    | (TsCheck::Plugins, TsStatus::Problem(_))
            );
            let fix_label = if *check == TsCheck::Plugins { "Install" } else { "Fix" };
            if is_fixable {
                check_row = check_row.push(horizontal_space());
                check_row = check_row.push(
                    button(text(fix_label).size(11))
                        .on_press(Message::TroubleshootFix(*check))
                        .style(button::success)
                        .padding([2, 10])
                );
            }

            checks_col = checks_col.push(check_row);
        }

        let check_btn = button(text("Check Sapphire"))
            .on_press_maybe(if self.ts_running { None } else { Some(Message::TroubleshootCheck) })
            .style(button::primary);

        column![
            text("Troubleshoot").size(18).font(Font {
                weight: iced::font::Weight::Bold,
                ..Font::DEFAULT
            }),
            checks_col,
            check_btn,
        ]
        .spacing(10)
        .into()
    }

    fn view_running_tab(&self) -> Element<Message> {
        let status_text = if self.sapphire_running {
            text("Sapphire is running").size(13).color(color!(0x4caf50))
        } else if self.sapphire_log.is_empty() {
            text("Hit Launch to start Sapphire.").size(13).color(color!(0x7f849c))
        } else {
            text("Sapphire stopped").size(13).color(color!(0x7f849c))
        };

        let copy_btn = button(text("Copy").size(11))
            .on_press_maybe(if self.sapphire_log.is_empty() { None } else { Some(Message::CopyRunLog) })
            .style(button::secondary)
            .padding([2, 8]);

        let open_btn = button(text("Open in Notepad").size(11))
            .on_press_maybe(if self.sapphire_log.is_empty() { None } else { Some(Message::OpenRunLog) })
            .style(button::secondary)
            .padding([2, 8]);

        let bottom_btn = button(text("↓ Bottom").size(11))
            .on_press(Message::ScrollRunLog)
            .style(button::secondary)
            .padding([2, 8]);

        let toolbar = row![status_text, horizontal_space(), copy_btn, open_btn, bottom_btn]
            .spacing(6)
            .align_y(iced::Alignment::Center);

        let log_text = if self.sapphire_log.is_empty() {
            "Waiting...".to_string()
        } else {
            self.sapphire_log.join("\n")
        };

        let log_scroll = scrollable(
            container(
                text(log_text).size(12).font(Font::MONOSPACE),
            )
            .width(Length::Fill)
            .padding(6),
        )
        .id(scrollable::Id::new("run-log"))
        .width(Length::Fill)
        .height(Length::Fill);

        let log_area = container(log_scroll)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_theme| container::Style {
                background: Some(iced::Background::Color(color!(0x11111b))),
                ..Default::default()
            });

        // Return the full layout — toolbar is fixed, log scrolls independently
        container(
            column![toolbar, log_area].spacing(4)
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(Padding { top: 8.0, right: 12.0, bottom: 4.0, left: 12.0 })
        .into()
    }

    // ── Log panel ──────────────────────────────────────────────────────────

    fn view_log_panel(&self) -> Element<Message> {
        let toggle = button(if self.log_visible { "▼ Log" } else { "▶ Log" })
            .on_press(Message::ToggleLog)
            .style(button::text)
            .padding([2, 8]);

        let mut panel = column![toggle].spacing(2).width(Length::Fill);

        if self.log_visible {
            let log_text = self.log_lines.join("\n");
            let log_area = container(
                scrollable(
                    container(
                        text(log_text)
                            .size(12)
                            .font(Font::MONOSPACE),
                    )
                    .width(Length::Fill)
                    .padding(6),
                )
                .anchor_bottom()
                .width(Length::Fill)
                .height(100),
            )
            .width(Length::Fill)
            .style(|_theme| container::Style {
                background: Some(iced::Background::Color(color!(0x11111b))),
                border: iced::Border {
                    color: color!(0x313244),
                    width: 1.0,
                    radius: 0.into(),
                },
                ..Default::default()
            });
            panel = panel.push(log_area);
        }

        container(panel)
            .width(Length::Fill)
            .padding(Padding { top: 8.0, right: 0.0, bottom: 0.0, left: 0.0 })
            .into()
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn step_label(step: Step) -> &'static str {
    match step {
        Step::Git => "Check for Git",
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
        TsCheck::Starlette => "Starlette version",
        TsCheck::DepsHealth => "Package health",
        TsCheck::Plugins => "Optional features",
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
