// Install / update / uninstall: the step model, detection checks, installers,
// git pull and the uninstall actions. Pure shell-driving async fns; the App
// message loop in main.rs sequences them.
use crate::*;
use crate::platform::*;

// ── Step tracking ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    Git,
    Conda,
    CondaInit,
    PythonEnv,
    Clone,
    // Windows-only: Sapphire's native wheels (numpy/torch/…) need the VC++ runtime.
    // Runs before Deps. A first-class step (like Git/Conda), gated off other OSes.
    #[cfg(windows)]
    VcRedist,
    Deps,
    Done,
}

pub(crate) const ALL_STEPS: &[Step] = &[
    Step::Git,
    Step::Conda,
    Step::CondaInit,
    Step::PythonEnv,
    Step::Clone,
    #[cfg(windows)]
    Step::VcRedist,
    Step::Deps,
    Step::Done,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StepStatus {
    NotStarted,
    Checking,
    Found(String),    // already installed — friendly detail
    NotFound(String), // not installed — what we'll do
    Installing,
    Done(String),
    Failed(String),
}

impl StepStatus {
    pub(crate) fn indicator(&self, tick: usize) -> &str {
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

    pub(crate) fn color(&self) -> iced::Color {
        match self {
            StepStatus::NotStarted => color!(0x585b70),
            StepStatus::Checking | StepStatus::Installing => color!(0x3d85c6),
            StepStatus::Found(_) | StepStatus::Done(_) => color!(0x4caf50),
            StepStatus::NotFound(_) => color!(0xf9e154),
            StepStatus::Failed(_) => color!(0xe74c3c),
        }
    }

    pub(crate) fn detail(&self) -> Option<&str> {
        match self {
            StepStatus::Found(s)
            | StepStatus::NotFound(s)
            | StepStatus::Done(s)
            | StepStatus::Failed(s) => Some(s.as_str()),
            _ => None,
        }
    }
}


// These return (Step, StepStatus) after checking the system
#[cfg(windows)]
pub(crate) async fn check_git() -> (Step, StepStatus) {
    match run_cmd_async(find_git(), vec!["--version".into()]).await {
        Ok(ver) => (Step::Git, StepStatus::Found(ver)),
        Err(_) => (Step::Git, StepStatus::NotFound("Git not found — we'll install it".to_string())),
    }
}

/// Linux: the "Git" step covers all system prerequisites — git (to clone) plus
/// libportaudio (Sapphire's audio at runtime). Both must be present, even if git
/// alone already is, so we check both here.
#[cfg(not(windows))]
pub(crate) async fn check_git() -> (Step, StepStatus) {
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

pub(crate) async fn check_conda() -> (Step, StepStatus) {
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

pub(crate) async fn check_conda_init() -> (Step, StepStatus) {
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

pub(crate) async fn check_python_env() -> (Step, StepStatus) {
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

pub(crate) async fn check_clone(install_path: String) -> (Step, StepStatus) {
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

// ── pip (the one place that talks to the env's pip) ───────────────────────

/// Bare package name from a requirements/specifier line: `torch>=2.0.0` → `torch`,
/// `pkg[extra]==1; python_version<"3.12"` → `pkg`. None for blanks, comments and pip
/// options (`-r other.txt`, `--index-url`). Mirrors Sapphire's own
/// re.split(r'[><=!~\[]', spec) in plugin_loader.py.
pub(crate) fn requirement_name(line: &str) -> Option<String> {
    let l = line.trim();
    if l.is_empty() || l.starts_with('#') || l.starts_with('-') {
        return None;
    }
    let name = l.split(&['>', '<', '=', '!', '~', '[', ';', ' '][..]).next().unwrap_or(l).trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// The specifier lines of a requirements file (comments/options/blanks dropped).
/// Missing or unreadable file → empty.
pub(crate) async fn read_requirements(path: &std::path::Path) -> Vec<String> {
    tokio::fs::read_to_string(path).await.unwrap_or_default()
        .lines()
        .filter(|l| requirement_name(l).is_some())
        .map(|l| l.trim().to_string())
        .collect()
}

/// Run the env's pip with `args` (direct pip.exe / bin/pip, else `conda run -n sapphire pip`).
pub(crate) async fn pip_run(args: Vec<String>) -> Result<String, String> {
    let (program, mut base) = find_conda_pip();
    base.extend(args);
    run_cmd_full_async(program, base).await
}

/// `pip install <args…>`: `-r <file>` or bare specifiers. pip skips what is already
/// satisfied, so callers need not pre-filter.
pub(crate) async fn pip_install(args: Vec<String>) -> Result<String, String> {
    let mut a = vec!["install".to_string()];
    a.extend(args);
    pip_run(a).await
}

/// Same, streaming output live into `sink` (the install tab's multi-GB torch download
/// must show progress instead of looking frozen).
pub(crate) async fn pip_install_streamed(args: Vec<String>, sink: Arc<std::sync::Mutex<Vec<String>>>) -> Result<String, String> {
    let (program, mut base) = find_conda_pip();
    base.push("install".into());
    base.extend(args);
    run_cmd_streamed(program, base, None, sink).await
}

/// `pip show` each specifier's package; returns the specifiers whose package is NOT
/// installed (lines that are not specifiers are skipped).
pub(crate) async fn pip_missing(specs: &[String]) -> Vec<String> {
    let mut missing = Vec::new();
    for spec in specs {
        let Some(pkg) = requirement_name(spec) else { continue };
        if pip_run(vec!["show".into(), pkg]).await.is_err() {
            missing.push(spec.clone());
        }
    }
    missing
}

pub(crate) async fn check_deps(install_path: String) -> (Step, StepStatus) {
    let req_file = PathBuf::from(&install_path).join("requirements.txt");
    if !req_file.exists() {
        return (Step::Deps, StepStatus::NotFound("Will install after cloning".to_string()));
    }

    // requirements.txt existing isn't enough — the env (and its packages) may be
    // gone (e.g. after removing the conda env). Probe the env's pip for the first
    // real package to confirm deps are actually installed.
    let specs = read_requirements(&req_file).await;
    let Some(first) = specs.first() else {
        return (Step::Deps, StepStatus::Found("requirements.txt is empty — nothing to install".to_string()));
    };
    let pkg = requirement_name(first).unwrap_or_default();
    if pip_missing(&specs[..1]).await.is_empty() {
        (Step::Deps, StepStatus::Found("Dependencies installed".to_string()))
    } else {
        (Step::Deps, StepStatus::NotFound(format!("Dependencies missing (no '{}') — will install", pkg)))
    }
}


#[cfg(windows)]
pub(crate) async fn install_git() -> (Step, StepStatus, String) {
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

/// Windows-only: is the VC++ 2015-2022 x64 runtime present? Cheap presence check via
/// the registry key the redist writes (both the native and WOW6432Node views). We
/// don't version-match — Go's installer is idempotent — this just lets an already-set-up
/// box show green instead of re-running winget. A miss here is harmless (Go no-ops).
#[cfg(windows)]
pub(crate) async fn check_vcredist() -> (Step, StepStatus) {
    for key in [
        "HKLM\\SOFTWARE\\Microsoft\\VisualStudio\\14.0\\VC\\Runtimes\\x64",
        "HKLM\\SOFTWARE\\WOW6432Node\\Microsoft\\VisualStudio\\14.0\\VC\\Runtimes\\x64",
    ] {
        if let Ok(out) = run_cmd_async("reg".into(), vec![
            "query".into(), key.into(), "/v".into(), "Installed".into(),
        ]).await
            && out.contains("0x1") {
                return (Step::VcRedist, StepStatus::Found("Visual C++ runtime is installed".to_string()));
            }
    }
    (Step::VcRedist, StepStatus::NotFound("We'll install the Visual C++ runtime — Sapphire's components need it".to_string()))
}

/// Windows-only: install the VC++ redistributable via winget. Idempotent — no-ops if a
/// same/newer version is already present (same pattern as the git/conda steps).
#[cfg(windows)]
pub(crate) async fn install_vcredist() -> (Step, StepStatus, String) {
    match run_cmd_full_async("winget".into(), vec![
        "install".into(), "Microsoft.VCRedist.2015+.x64".into(),
        "--accept-source-agreements".into(), "--accept-package-agreements".into(),
    ]).await {
        Ok(out) => (Step::VcRedist, StepStatus::Done("Visual C++ runtime installed".to_string()), out),
        Err(e) => {
            if e.contains("already installed") || e.contains("No newer") || e.contains("up to date") {
                (Step::VcRedist, StepStatus::Done("Visual C++ runtime is already installed".to_string()), e)
            } else {
                (Step::VcRedist, StepStatus::Failed(format!("Couldn't install the Visual C++ runtime — you can get it from https://aka.ms/vc14/vc_redist.x64.exe. {}", e)), e)
            }
        }
    }
}

/// Pick the package-manager install of Sapphire's system prerequisites from
/// /etc/os-release: git (to clone) + portaudio (audio runtime) + python dev
/// headers. Returns (pkexec args, human-readable manual command).
/// Empty args = unknown distro. Package names per the Sapphire docs (libportaudio2).
#[cfg(not(windows))]
pub(crate) fn system_packages_plan() -> (Vec<String>, String) {
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
pub(crate) async fn install_git() -> (Step, StepStatus, String) {
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
pub(crate) async fn install_conda() -> (Step, StepStatus, String) {
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
pub(crate) async fn install_conda() -> (Step, StepStatus, String) {
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

pub(crate) async fn install_conda_init() -> (Step, StepStatus, String) {
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

pub(crate) async fn install_python_env() -> (Step, StepStatus, String) {
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

pub(crate) async fn install_clone(install_path: String, branch: String) -> (Step, StepStatus, String) {
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

pub(crate) async fn install_deps(install_path: String, sink: Arc<std::sync::Mutex<Vec<String>>>) -> (Step, StepStatus, String) {
    let req_file = PathBuf::from(&install_path).join("requirements.txt");

    if !req_file.exists() {
        return (
            Step::Deps,
            StepStatus::Failed("requirements.txt not found — clone may have failed".to_string()),
            String::new(),
        );
    }

    // Stream output live so a multi-GB download (torch &c.) shows progress.
    match pip_install_streamed(vec!["-r".into(), req_file.to_string_lossy().to_string()], sink).await {
        // lines were already streamed live → empty log_output to avoid double-printing
        Ok(_) => (Step::Deps, StepStatus::Done("Dependencies installed".to_string()), String::new()),
        Err(e) => (Step::Deps, StepStatus::Failed(format!("pip install failed: {}", e)), String::new()),
    }
}


// ── Async uninstall logic ───────────────────────────────────────────────────

pub(crate) async fn uninstall_conda_env() -> (String, bool) {
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

pub(crate) async fn uninstall_delete_folder(install_path: String) -> (String, bool) {
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

pub(crate) async fn uninstall_delete_userdata(install_path: String) -> (String, bool) {
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

pub(crate) async fn check_for_updates(install_path: String, branch: Branch) -> Option<u32> {
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

pub(crate) async fn git_stash_and_pull(install_path: String) -> (String, bool) {
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
    match run_cmd_full_async(find_git(), vec![
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
    }
}


pub(crate) fn step_label(step: Step) -> &'static str {
    match step {
        #[cfg(windows)]
        Step::Git => "Check for Git",
        #[cfg(not(windows))]
        Step::Git => "Check system packages (git, audio, curl)",
        Step::Conda => "Check for Miniconda",
        Step::CondaInit => "Initialize conda",
        Step::PythonEnv => "Create Python environment",
        Step::Clone => "Clone Sapphire",
        #[cfg(windows)]
        Step::VcRedist => "Check Visual C++ runtime",
        Step::Deps => "Install dependencies",
        Step::Done => "Done!",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requirement_names() {
        for (line, want) in [
            ("torch>=2.0.0", Some("torch")),
            ("  numpy == 1.26  ", Some("numpy")),
            ("pkg[extra]==1", Some("pkg")),
            ("uvicorn[standard]>=0.30; python_version>='3.9'", Some("uvicorn")),
            ("starlette~=0.52", Some("starlette")),
            ("name @ https://example.invalid/x.whl", Some("name")),
            ("# comment", None),
            ("", None),
            ("   ", None),
            ("-r other.txt", None),
            ("--index-url https://x", None),
        ] {
            assert_eq!(requirement_name(line).as_deref(), want, "line {:?}", line);
        }
    }

    #[tokio::test]
    async fn read_requirements_keeps_only_specifiers() {
        let d = std::env::temp_dir().join(format!("sapphire-launcher-test-{}-reqs", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("requirements.txt");
        std::fs::write(&f, "# deps\n-r base.txt\n\ntorch>=2.0.0\n  fastapi \nuvicorn[standard]>=0.30\n").unwrap();
        assert_eq!(read_requirements(&f).await, vec!["torch>=2.0.0", "fastapi", "uvicorn[standard]>=0.30"]);
        assert!(read_requirements(&d.join("missing.txt")).await.is_empty());
    }
}
