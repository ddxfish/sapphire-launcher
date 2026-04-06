use iced::widget::{
    button, column, container, horizontal_space, pick_list, row, scrollable, text, text_input,
    Column,
};
use iced::{color, Element, Font, Length, Padding, Task, Theme};
use iced::window;
use std::path::PathBuf;
use std::process::Command;
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
        .run()
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
    // Sapphire process
    sapphire_running: bool,
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
            confirm_remove_env: false,
            confirm_delete_folder: false,
            confirm_delete_userdata: false,
            sapphire_running: false,
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
    TabSelected(Tab),
    ToggleLog,
    // Install flow
    ScanClicked,
    GoClicked,
    StepResult(Step, StepStatus),
    InstallStepResult(Step, StepStatus, String), // step, status, log output
    Log(String),
    // Launch
    SapphireLine(String),   // a line of output from sapphire
    SapphireExited(String), // process ended
    // Uninstall flow (two-click confirmation)
    UninstallCondaEnvClick,   // first click → confirm, second click → go
    UninstallDeleteFolderClick,
    UninstallDeleteUserdataClick,
    UninstallResult(String, bool), // message, success
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

// ── Launch streaming ───────────────────────────────────────────────────────

fn launch_sapphire_stream(install_path: String) -> impl futures::Stream<Item = Message> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

    tokio::task::spawn(async move {
        let mut child = match tokio::process::Command::new("conda")
            .args(["run", "-n", "sapphire", "python", "-u", "main.py"])
            .current_dir(&install_path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(Message::SapphireExited(format!("Failed to start: {}", e)));
                return;
            }
        };

        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let tx_out = tx.clone();
        let tx_err = tx.clone();

        // Read stdout in one task
        let stdout_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if tx_out.send(Message::SapphireLine(line)).is_err() {
                    break;
                }
            }
        });

        // Read stderr in another
        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if tx_err.send(Message::SapphireLine(line)).is_err() {
                    break;
                }
            }
        });

        let _ = stdout_task.await;
        let _ = stderr_task.await;

        let status = child.wait().await;
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
                Task::none()
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

                self.sapphire_running = true;

                self.log_lines.push("Launching Sapphire...".to_string());

                let path = self.install_path.clone();
                Task::stream(launch_sapphire_stream(path))
            }
            Message::SapphireLine(line) => {
                self.log_lines.push(line);
                Task::none()
            }
            Message::SapphireExited(msg) => {
                self.sapphire_running = false;
                self.log_lines.push(msg);
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

            Message::Log(line) => {
                self.log_lines.push(line);
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
        }
    }

    /// Find the next step that needs installing and fire it off.
    /// Steps that are already Found/Done get skipped.
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

        let launch_label = if self.sapphire_running { "Running..." } else { "Launch" };
        let launch_btn = button(
            text(launch_label).font(Font {
                weight: iced::font::Weight::Bold,
                ..Font::DEFAULT
            }),
        )
        .on_press_maybe(if self.sapphire_running { None } else { Some(Message::Launch) })
        .style(button::success);

        let header = row![
            path_input,
            browse_btn,
            horizontal_space(),
            branch_picker,
            launch_btn,
        ]
        .spacing(8)
        .padding(8)
        .align_y(iced::Alignment::Center);

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
        let tabs = row![
            self.tab_button("Install", Tab::Install),
            self.tab_button("Update", Tab::Update),
            self.tab_button("Uninstall", Tab::Uninstall),
            self.tab_button("Troubleshoot", Tab::Troubleshoot),
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
        let content: Element<Message> = match self.active_tab {
            Tab::Install => self.view_install_tab(),
            Tab::Update => self.view_update_tab(),
            Tab::Uninstall => self.view_uninstall_tab(),
            Tab::Troubleshoot => self.view_troubleshoot_tab(),
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
        column![
            text("Update Sapphire").size(18).font(Font {
                weight: iced::font::Weight::Bold,
                ..Font::DEFAULT
            }),
            text("Pull the latest changes and update dependencies.").size(13),
        ]
        .spacing(6)
        .into()
    }

    fn view_uninstall_tab(&self) -> Element<Message> {
        let busy = self.uninstalling;

        // ── Remove conda env ──
        let env_label = if self.confirm_remove_env {
            "YES, remove the conda environment"
        } else {
            "Remove conda environment"
        };
        let remove_env_btn = button(text(env_label))
            .on_press_maybe(if busy { None } else { Some(Message::UninstallCondaEnvClick) })
            .style(button::danger);

        let env_warning = if self.confirm_remove_env {
            text("Click again to confirm. This deletes all Python packages in the 'sapphire' env.")
                .size(12)
                .color(color!(0xe74c3c))
        } else {
            text("Removes the 'sapphire' Python environment and all its packages.")
                .size(12)
                .color(color!(0x7f849c))
        };

        // ── Delete user data ──
        let userdata_label = if self.confirm_delete_userdata {
            "YES, delete my settings and user data"
        } else {
            "Reset user data"
        };
        let delete_userdata_btn = button(text(userdata_label))
            .on_press_maybe(if busy { None } else { Some(Message::UninstallDeleteUserdataClick) })
            .style(button::danger);

        let userdata_warning = if self.confirm_delete_userdata {
            text("Click again to confirm. Your settings, API keys, and personal data will be permanently deleted.")
                .size(12)
                .color(color!(0xe74c3c))
        } else {
            text("Deletes your user/ folder — settings, API keys, and personal data. Sapphire itself stays installed.")
                .size(12)
                .color(color!(0x7f849c))
        };

        // ── Delete entire folder ──
        let folder_label = if self.confirm_delete_folder {
            format!("YES, permanently delete {}", self.install_path)
        } else {
            format!("Delete entire Sapphire folder")
        };
        let delete_folder_btn = button(text(folder_label))
            .on_press_maybe(if busy { None } else { Some(Message::UninstallDeleteFolderClick) })
            .style(button::danger);

        let folder_warning = if self.confirm_delete_folder {
            text("FINAL WARNING: Click again to permanently delete everything. This cannot be undone.")
                .size(12)
                .color(color!(0xe74c3c))
        } else {
            text(format!("Nukes {} and everything in it. Code, settings, all of it. Cannot be undone.", self.install_path))
                .size(12)
                .color(color!(0xe74c3c))
        };

        column![
            text("Uninstall").size(18).font(Font {
                weight: iced::font::Weight::Bold,
                ..Font::DEFAULT
            }),
            text("Won't touch Git or Miniconda — only Sapphire-specific stuff.").size(12).color(color!(0x7f849c)),
            Column::new()
                .spacing(14)
                .padding(Padding { top: 8.0, right: 0.0, bottom: 0.0, left: 0.0 })
                .push(column![remove_env_btn, env_warning].spacing(3))
                .push(column![delete_userdata_btn, userdata_warning].spacing(3))
                .push(column![delete_folder_btn, folder_warning].spacing(3)),
        ]
        .spacing(6)
        .into()
    }

    fn view_troubleshoot_tab(&self) -> Element<Message> {
        column![
            text("Troubleshoot").size(18).font(Font {
                weight: iced::font::Weight::Bold,
                ..Font::DEFAULT
            }),
            text("Quick fixes for common issues.").size(13),
        ]
        .spacing(6)
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
