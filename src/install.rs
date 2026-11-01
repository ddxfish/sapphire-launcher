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
    // ldconfig lives in /sbin, which a Debian user's PATH lacks — try it by full path,
    // then look for the library file itself.
    let audio_ok = run_cmd_async(
        "sh".into(),
        vec!["-c".into(), "{ /sbin/ldconfig -p 2>/dev/null || ldconfig -p 2>/dev/null; } | grep -qi portaudio \
             || ls /usr/lib/*/libportaudio.so.2 /usr/lib64/libportaudio.so.2 /usr/lib/libportaudio.so.2 >/dev/null 2>&1".into()],
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
    let conda = find_conda();
    match run_cmd_async(conda.clone(), vec!["--version".into()]).await {
        Ok(ver) => (Step::Conda, StepStatus::Found(ver)),
        // The binary is there but won't answer (a half-finished install, a broken base
        // env): still "found" — Go's installer would only say "already installed".
        Err(_) if conda != "conda" => (Step::Conda, StepStatus::Found(format!("Found at {}", conda))),
        Err(_) => (Step::Conda, StepStatus::NotFound("Miniconda not found — we'll install it".to_string())),
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

/// What a folder holds. Every action that touches a folder decides from this one
/// answer, so a user's own project is never updated, switched or deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InstallKind {
    Nothing,    // no folder, or an empty one: a clone can go here
    Sapphire,   // a full checkout
    Unfinished, // our own clone, cut short: it has our remote but not her files
    Other,      // somebody else's files
}

/// The launcher's clone URL, as git records it in `.git/config`. With the `.git` it
/// can't match the launcher's own repo (`ddxfish/sapphire-launcher.git`).
const SAPPHIRE_REMOTE: &str = "ddxfish/sapphire.git";

pub(crate) fn install_kind(path: &str) -> InstallKind {
    let p = PathBuf::from(path);
    if !p.exists() {
        return InstallKind::Nothing;
    }
    if std::fs::read_dir(&p).is_ok_and(|mut d| d.next().is_none()) {
        return InstallKind::Nothing;
    }
    let git = p.join(".git");
    if git.exists() && ["main.py", "sapphire.py", "core/setup.py"].iter().all(|m| p.join(m).is_file()) {
        return InstallKind::Sapphire;
    }
    if std::fs::read_to_string(git.join("config")).is_ok_and(|c| c.contains(SAPPHIRE_REMOTE)) {
        return InstallKind::Unfinished;
    }
    InstallKind::Other
}

/// Why Sapphire can't be cloned into this folder. Asked at scan time and again by
/// Go, before any slow step runs.
pub(crate) fn clone_blocker(path: &str) -> Option<String> {
    match install_kind(path) {
        InstallKind::Other => Some(format!("{} already has other files in it. Pick an empty folder, or the folder where Sapphire is already installed.", path)),
        InstallKind::Unfinished => Some(format!("{} holds an unfinished download of Sapphire. Delete it from the Uninstall tab, then Re-scan.", path)),
        _ => None,
    }
}

pub(crate) async fn check_clone(install_path: String) -> (Step, StepStatus) {
    if let Some(why) = clone_blocker(&install_path) {
        return (Step::Clone, StepStatus::Failed(why));
    }
    match install_kind(&install_path) {
        InstallKind::Sapphire => (Step::Clone, StepStatus::Found(format!("Sapphire is at {}", install_path))),
        _ => (Step::Clone, StepStatus::NotFound(format!("Will download Sapphire into {}", install_path))),
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

/// `pip install <args…>`: `-r <file>` or bare specifiers, output streamed live into
/// `sink` (a multi-GB torch download must show progress, not a frozen spinner). pip
/// skips what is already satisfied, so callers need not pre-filter.
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

/// Runs in her env: every requirement that applies to this machine, checked against
/// what is installed, version included. No network. `packaging` comes from pip when
/// it isn't installed in its own right.
const REQUIREMENTS_CHECK: &str = r##"
import sys
try:
    from packaging.requirements import Requirement
except ImportError:
    from pip._vendor.packaging.requirements import Requirement
from importlib.metadata import version, PackageNotFoundError
for line in open(sys.argv[1], encoding="utf-8"):
    line = line.split(" #")[0].strip()
    if not line or line[0] in "#-":
        continue
    try:
        r = Requirement(line)
        if r.marker and not r.marker.evaluate():
            continue
        v = version(r.name)
        if r.specifier and not r.specifier.contains(v, prereleases=True):
            print(r.name + " (needs " + str(r.specifier) + ", has " + v + ")")
    except PackageNotFoundError:
        print(r.name)
    except Exception:
        pass
"##;

/// The requirements this env doesn't satisfy. Err = the env couldn't answer at all.
pub(crate) async fn requirements_missing(req_file: &std::path::Path) -> Result<Vec<String>, String> {
    let (program, mut args) = sapphire_python_cmd();
    args.extend(["-c".to_string(), REQUIREMENTS_CHECK.to_string(), req_file.to_string_lossy().into_owned()]);
    let out = run_cmd_async(program, args).await?;
    Ok(out.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect())
}

/// `a, b, c and 4 more`.
pub(crate) fn name_some(items: &[String]) -> String {
    let shown = items.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
    if items.len() > 3 { format!("{} and {} more", shown, items.len() - 3) } else { shown }
}

pub(crate) async fn check_deps(install_path: String) -> (Step, StepStatus) {
    let req_file = PathBuf::from(&install_path).join("requirements.txt");
    if !req_file.exists() {
        return (Step::Deps, StepStatus::NotFound("Will install after downloading Sapphire".to_string()));
    }
    match requirements_missing(&req_file).await {
        Ok(missing) if missing.is_empty() => (Step::Deps, StepStatus::Found("Dependencies installed".to_string())),
        Ok(missing) => (Step::Deps, StepStatus::NotFound(format!("Needs installing: {}", name_some(&missing)))),
        // A missing or broken env: installing is the way forward either way.
        Err(_) => (Step::Deps, StepStatus::NotFound("Will install them into Sapphire's Python environment".to_string())),
    }
}

// ── Plain-English failures ────────────────────────────────────────────────

/// What a tool's output means for a person, when we recognise it. The raw text stays
/// in the log for support.
pub(crate) fn explain_failure(output: &str) -> Option<&'static str> {
    let o = output.to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| o.contains(n));
    if has(&["no space left", "errno 28", "not enough space", "disk full"]) {
        Some("Your disk is full. Free up some space, then try again.")
    } else if has(&["being used by another process", "winerror 32", "device or resource busy"]) {
        Some("A file is in use. Close other programs using this folder, then try again.")
    } else if has(&["filename too long", "file name too long", "winerror 206", "path too long"]) {
        Some("The folder path is too long. Pick a shorter folder, then try again.")
    } else if has(&["permission denied", "access is denied", "winerror 5", "errno 13"]) {
        Some("Permission was denied. Your antivirus may be blocking it, or this folder may be protected. Try a folder in your home directory.")
    } else if has(&["sslerror", "ssl certificate", "ssl_error", "certificate verify", "tls handshake"]) {
        Some("A secure connection was blocked. A VPN, proxy or antivirus may be in the way.")
    } else if has(&[
        "could not resolve", "failed to connect", "timed out", "name resolution", "network is unreachable",
        "unable to access", "condahttperror", "connection broken", "getaddrinfo", "connection reset",
    ]) {
        Some("Couldn't reach the internet. Check your connection, then try again.")
    } else {
        None
    }
}

/// `what` plus the cause, or a pointer to the log when we don't recognise it.
pub(crate) fn plain_failure(what: &str, raw: &str) -> String {
    format!("{} {}", what, explain_failure(raw).unwrap_or("The details are in the log below."))
}

/// A failed pip's error is only its exit code; the reason is in what it streamed.
pub(crate) fn pip_failure(what: &str, sink: &Arc<std::sync::Mutex<Vec<String>>>, err: &str) -> String {
    let output = sink.lock().map(|v| v.join("\n")).unwrap_or_default();
    plain_failure(what, &format!("{}\n{}", output, err))
}

// ── winget ────────────────────────────────────────────────────────────────

#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WingetOutcome {
    Installed,
    AlreadyThere,
    RestartNeeded,
    Cancelled,
    NoNetwork,
    DiskFull,
    Failed,
}

/// winget's documented exit codes: numbers, because its text is translated.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn winget_outcome(code: i32) -> WingetOutcome {
    match code as u32 {
        0 => WingetOutcome::Installed,
        0x8A15002B | 0x8A150061 | 0x8A15010D => WingetOutcome::AlreadyThere,
        0x8A150109..=0x8A15010B => WingetOutcome::RestartNeeded,
        0x8A15010C => WingetOutcome::Cancelled,
        0x8A150107 | 0x8A150008 => WingetOutcome::NoNetwork,
        0x8A150105 => WingetOutcome::DiskFull,
        _ => WingetOutcome::Failed,
    }
}

/// Install `id` with winget and report in plain English. `manual` is where to get it
/// by hand. The output always goes to the log.
#[cfg(windows)]
async fn winget_install(step: Step, id: &str, name: &str, manual: &str) -> (Step, StepStatus, String) {
    let args = ["install", "--exact", "--id", id, "--source", "winget", "--accept-source-agreements", "--accept-package-agreements"]
        .iter().map(|a| a.to_string()).collect();
    let by_hand = format!("Or install {} yourself from {}, then click Re-scan.", name, manual);
    let (code, out) = match run_cmd_code("winget".into(), args).await {
        Ok(r) => r,
        Err(e) => return (step, StepStatus::Failed(format!("Windows' app installer (winget) isn't available here. Install {} from {}, then click Re-scan.", name, manual)), e),
    };
    let status = match winget_outcome(code) {
        WingetOutcome::Installed => StepStatus::Done(format!("{} installed", name)),
        WingetOutcome::AlreadyThere => StepStatus::Done(format!("{} is already installed", name)),
        WingetOutcome::RestartNeeded => StepStatus::Failed(format!("Restart your PC to finish installing {}, then open this app again.", name)),
        WingetOutcome::Cancelled => StepStatus::Failed(format!("The {} install was cancelled. Click Retry when you're ready.", name)),
        WingetOutcome::NoNetwork => StepStatus::Failed(format!("Couldn't download {}. Check your connection, then click Retry. {}", name, by_hand)),
        WingetOutcome::DiskFull => StepStatus::Failed(format!("Your disk is full. Free up some space, then click Retry to install {}.", name)),
        WingetOutcome::Failed => StepStatus::Failed(format!("Couldn't install {}. {}", name, by_hand)),
    };
    (step, status, out)
}

#[cfg(windows)]
type StepCheck = fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = (Step, StepStatus)> + Send>>;

/// One install step: skip it if a Retry finds the tool is there now (the user installed
/// it by hand), and never call it done until the tool is really found.
#[cfg(windows)]
async fn winget_step(step: Step, id: &str, name: &str, manual: &str, check: StepCheck) -> (Step, StepStatus, String) {
    if matches!(check().await.1, StepStatus::Found(_)) {
        return (step, StepStatus::Done(format!("{} is already installed", name)), String::new());
    }
    let (s, status, out) = winget_install(step, id, name, manual).await;
    if matches!(status, StepStatus::Done(_)) && !matches!(check().await.1, StepStatus::Found(_)) {
        return (s, StepStatus::Failed(format!("{} finished installing, but this app can't find it. Close and reopen this app, then click Re-scan.", name)), out);
    }
    (s, status, out)
}

#[cfg(windows)]
pub(crate) async fn install_git() -> (Step, StepStatus, String) {
    winget_step(Step::Git, "Git.Git", "Git", "https://git-scm.com/download/win", || Box::pin(check_git())).await
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

/// Windows-only: install the VC++ redistributable via winget.
#[cfg(windows)]
pub(crate) async fn install_vcredist() -> (Step, StepStatus, String) {
    winget_step(Step::VcRedist, "Microsoft.VCRedist.2015+.x64", "the Visual C++ runtime", "https://aka.ms/vc14/vc_redist.x64.exe", || Box::pin(check_vcredist())).await
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
    winget_step(Step::Conda, "Anaconda.Miniconda3", "Miniconda", "https://docs.anaconda.com/miniconda/", || Box::pin(check_conda())).await
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
        return (Step::Conda, StepStatus::Failed(plain_failure("Couldn't download Miniconda.", &e)), e);
    }

    let home = std::env::var("HOME").unwrap_or_default();
    let prefix = format!("{}/miniconda3", home);
    match run_cmd_full_async("bash".into(), vec![tmp, "-b".into(), "-p".into(), prefix]).await {
        Ok(out) => (Step::Conda, StepStatus::Done("Miniconda installed into ~/miniconda3".to_string()), out),
        Err(e) => {
            if e.contains("already exists") {
                (Step::Conda, StepStatus::Done("Miniconda already installed".to_string()), e)
            } else {
                (Step::Conda, StepStatus::Failed(plain_failure("Miniconda didn't install.", &e)), e)
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
        Err(e) => (Step::CondaInit, StepStatus::Failed(plain_failure("Python setup hit a snag.", &e)), e),
    }
}

pub(crate) async fn install_python_env() -> (Step, StepStatus, String) {
    match run_cmd_full_async(find_conda(), vec!["create".into(), "-n".into(), "sapphire".into(), "python=3.11".into(), "-y".into()]).await {
        Ok(out) => (Step::PythonEnv, StepStatus::Done("Python 3.11 environment 'sapphire' created".to_string()), out),
        Err(e) => {
            if e.contains("already exists") {
                (Step::PythonEnv, StepStatus::Done("Environment 'sapphire' already exists".to_string()), e)
            } else {
                (Step::PythonEnv, StepStatus::Failed(plain_failure("Couldn't set up Sapphire's Python environment.", &e)), e)
            }
        }
    }
}

pub(crate) async fn install_clone(install_path: String, branch: String) -> (Step, StepStatus, String) {
    let git_branch = match branch.as_str() {
        "Development" => "dev",
        _ => "main",
    };

    if let Some(why) = clone_blocker(&install_path) {
        return (Step::Clone, StepStatus::Failed(why), String::new());
    }
    if install_kind(&install_path) == InstallKind::Sapphire {
        return (Step::Clone, StepStatus::Done("Sapphire is already here".to_string()), String::new());
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
            StepStatus::Failed(plain_failure("Couldn't download Sapphire.", &e)),
            e,
        ),
    }
}

pub(crate) async fn install_deps(install_path: String, sink: Arc<std::sync::Mutex<Vec<String>>>) -> (Step, StepStatus, String) {
    let req_file = PathBuf::from(&install_path).join("requirements.txt");

    if !req_file.exists() {
        return (
            Step::Deps,
            StepStatus::Failed("Sapphire's files aren't complete in this folder. Pick another folder, or delete this one and try again.".to_string()),
            String::new(),
        );
    }

    // Stream output live so a multi-GB download (torch &c.) shows progress.
    match pip_install_streamed(vec!["-r".into(), req_file.to_string_lossy().to_string()], sink.clone()).await {
        // lines were already streamed live → empty log_output to avoid double-printing
        Ok(_) => (Step::Deps, StepStatus::Done("Dependencies installed".to_string()), String::new()),
        Err(e) => (Step::Deps, StepStatus::Failed(pip_failure("Installing Sapphire's packages failed.", &sink, &e)), String::new()),
    }
}


// ── Async uninstall logic ───────────────────────────────────────────────────

/// Does this folder hold a Sapphire checkout? Update and branch switch need this.
pub(crate) fn looks_like_install(path: &str) -> bool {
    install_kind(path) == InstallKind::Sapphire
}

/// May the delete actions touch this folder? A checkout, or our own unfinished one.
pub(crate) fn can_delete(path: &str) -> bool {
    matches!(install_kind(path), InstallKind::Sapphire | InstallKind::Unfinished)
}

/// Rename `p` aside, then delete the renamed copy. The rename is what a locked file
/// refuses (on Windows), and it refuses before anything is lost. A deletion that fails
/// halfway leaves only a `.deleting` folder, never a half-eaten install.
fn delete_by_rename(p: &std::path::Path) -> Result<(), String> {
    let name = p.file_name().ok_or_else(|| format!("{} can't be deleted.", p.display()))?;
    let aside = p.with_file_name(format!("{}.deleting-{}", name.to_string_lossy(), now_secs()));
    std::fs::rename(p, &aside).map_err(|e| {
        format!("Nothing was deleted: {} {}", explain_failure(&e.to_string()).unwrap_or("Close anything using this folder, then try again."), e)
    })?;
    std::fs::remove_dir_all(&aside).map_err(|e| {
        format!("Some files couldn't be removed. What's left is in {}, and it's safe to delete by hand. ({})", aside.display(), e)
    })
}

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
        if !can_delete(&path) {
            return (format!("{} doesn't look like a Sapphire install — not deleting it.", path), false);
        }
        match delete_by_rename(&p) {
            Ok(()) => (format!("Deleted {}", path), true),
            Err(e) => (e, false),
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
            return (format!("No user folder found at {} — nothing to delete.", user_dir.display()), true);
        }
        if !can_delete(&path) {
            return (format!("{} doesn't look like a Sapphire install — not touching its user folder.", path), false);
        }
        match delete_by_rename(&user_dir) {
            Ok(()) => (format!("Deleted {} — your settings and data are gone.", user_dir.display()), true),
            Err(e) => (e, false),
        }
    })
    .await
    .unwrap_or_else(|e| (format!("Task failed: {}", e), false))
}


// ── Update check ───────────────────────────────────────────────────────────

pub(crate) async fn check_for_updates(install_path: String, branch: Option<Branch>) -> Option<u32> {
    if !looks_like_install(&install_path) { return None; }

    let git_branch = match branch? {
        Branch::Development => "dev",
        Branch::Stable => "main",
    };

    // Fetch latest from remote. Offline (or a dead network) must read as "couldn't
    // check", never as "up to date" against a stale origin/ — and a hung fetch must
    // not leave the tab on "Checking..." forever.
    let fetch = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        run_cmd_full_async(find_git(), vec!["-C".into(), install_path.clone(), "fetch".into(), "origin".into()]),
    ).await;
    if !matches!(fetch, Ok(Ok(_))) {
        return None;
    }

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


// ── Launcher update check (GitHub releases) ───────────────────────────────

pub(crate) const LAUNCHER_REPO: &str = "ddxfish/sapphire-launcher";
/// While the launcher is pre-1.0 every release is a beta, so beta users should hear
/// about the next beta. Flip to false at 1.0 to announce stable releases only.
const INCLUDE_PRERELEASES: bool = true;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LauncherRelease {
    pub(crate) version: String, // tag without the leading v, e.g. "0.9.7"
    pub(crate) url: String,     // the release page (assets + notes)
}

/// Outcome of the launcher's own update check.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum LauncherCheck {
    #[default]
    Pending,
    UpToDate,
    Available(LauncherRelease),
    Failed, // offline, rate-limited, or an answer we couldn't read
}

/// `v0.9.7`, `0.9.7-beta`, `v1.0` → (major, minor, patch); anything else → None.
pub(crate) fn parse_version(tag: &str) -> Option<(u32, u32, u32)> {
    let core = tag.trim().trim_start_matches(['v', 'V']);
    let core = core.split(['-', '+', ' ']).next().unwrap_or(core);
    let mut parts = core.split('.').map(|p| p.parse::<u32>());
    let major = parts.next()?.ok()?;
    let minor = parts.next().unwrap_or(Ok(0)).ok()?;
    let patch = parts.next().unwrap_or(Ok(0)).ok()?;
    Some((major, minor, patch))
}

/// The newest non-draft release in a GitHub `/releases` listing that is newer than
/// `local`. Drafts never count; pre-releases per INCLUDE_PRERELEASES. Tags that are
/// not versions are skipped; a body that is not a release list → None.
pub(crate) fn newer_release(json: &str, local: &str) -> Option<LauncherRelease> {
    let local_v = parse_version(local)?;
    let list: Vec<serde_json::Value> = serde_json::from_str(json).ok()?;
    let flag = |r: &serde_json::Value, key: &str| r.get(key).and_then(|b| b.as_bool()).unwrap_or(false);
    list.iter()
        .filter(|r| !flag(r, "draft") && (INCLUDE_PRERELEASES || !flag(r, "prerelease")))
        .filter_map(|r| {
            let tag = r.get("tag_name")?.as_str()?;
            let url = r.get("html_url")?.as_str()?.to_string();
            let rel = LauncherRelease { version: tag.trim_start_matches(['v', 'V']).to_string(), url };
            Some((parse_version(tag)?, !flag(r, "prerelease"), rel))
        })
        .filter(|(v, _, _)| *v > local_v)
        // Same version as beta and final: the final wins the tie.
        .max_by_key(|(v, is_final, _)| (*v, *is_final))
        .map(|(_, _, rel)| rel)
}

/// One small GitHub API call (at startup and on the Update tab's check): is there a
/// newer launcher than this build? Unauthenticated (60 calls/hour/IP is plenty);
/// GitHub wants a User-Agent. Fails silent offline or when rate-limited.
pub(crate) async fn check_launcher_release() -> LauncherCheck {
    let url = format!("https://api.github.com/repos/{}/releases?per_page=10", LAUNCHER_REPO);
    let body = match run_cmd_full_async("curl".into(), vec![
        "-sL".into(), "--max-time".into(), "8".into(),
        "-H".into(), "Accept: application/vnd.github+json".into(),
        "-H".into(), format!("User-Agent: sapphire-launcher/{}", env!("CARGO_PKG_VERSION")),
        url,
    ]).await {
        Ok(b) => b,
        Err(_) => return LauncherCheck::Failed,
    };
    match newer_release(&body, env!("CARGO_PKG_VERSION")) {
        Some(rel) => LauncherCheck::Available(rel),
        // No newer one — but only "up to date" if we actually read a release list.
        None if serde_json::from_str::<Vec<serde_json::Value>>(&body).is_ok() => LauncherCheck::UpToDate,
        None => LauncherCheck::Failed,
    }
}

// ── Update helpers ─────────────────────────────────────────────────────────

/// git, in one folder. stdout only on success: a warning on stderr must not read as
/// a file listing.
async fn git_in(path: &str, args: &[&str]) -> Result<String, String> {
    let mut all = vec!["-C".to_string(), path.to_string()];
    all.extend(args.iter().map(|a| a.to_string()));
    run_cmd_async(find_git(), all).await
}

const STASH_LABEL: &str = "sapphire-launcher: your edits from before an update";

/// Pull the latest code, as one transaction around the user's edits to tracked files:
/// put them aside, pull, and put them back, whether the pull worked or not. If they
/// can't be combined with the new code, the update stands, the tree is left clean so
/// Sapphire can run, and the edits stay safe in the stash. Untracked files are never
/// touched: a pull only trips on one the update itself would overwrite, and says so.
pub(crate) async fn update_code(path: String, sink: Arc<std::sync::Mutex<Vec<String>>>) -> (String, bool) {
    let log = |line: String| { if let Ok(mut v) = sink.lock() { v.push(line); } };
    if git_in(&path, &["symbolic-ref", "-q", "HEAD"]).await.is_err() {
        return ("This install isn't on a branch, so there's nothing to update from. Pick Stable or Development at the top first.".to_string(), false);
    }
    let dirty = match git_in(&path, &["status", "--porcelain", "--untracked-files=no"]).await {
        Ok(out) => !out.trim().is_empty(),
        Err(e) => {
            log(e.clone());
            return (plain_failure("Couldn't read this install's state, so nothing was changed.", &e), false);
        }
    };
    if dirty {
        // A fresh Windows box has no git identity, and a stash is a commit.
        let stashed = git_in(&path, &["-c", "user.name=Sapphire Launcher", "-c", "user.email=launcher@localhost",
            "stash", "push", "-m", STASH_LABEL]).await;
        if let Err(e) = stashed {
            log(e.clone());
            return (plain_failure("Couldn't set your edited files aside, so nothing was changed.", &e), false);
        }
    }

    let pulled = run_cmd_full_async(find_git(), vec!["-C".into(), path.clone(), "pull".into(), "--ff-only".into()]).await;
    if let Err(e) = &pulled {
        log(e.clone());
    }
    let restored = if dirty { restore_edits(&path, &log).await } else { Ok(()) };

    match (pulled, restored) {
        (Ok(out), Ok(())) => {
            let done = if out.contains("Already up to date") { "Already up to date." } else { "Downloaded the latest code." };
            (format!("{}{}", done, if dirty { " Your edited files are still in place." } else { "" }), true)
        }
        (Ok(_), Err(why)) => (format!("Downloaded the latest code. {}", why), true),
        (Err(e), restored) => {
            let cause = if e.contains("Not possible to fast-forward") || e.contains("divergent") {
                "This copy has its own commits that the update can't merge automatically."
            } else if e.contains("would be overwritten") {
                "The update would overwrite files you added yourself. Move them, then try again."
            } else {
                explain_failure(&e).unwrap_or("The details are in the log below.")
            };
            let kept = match restored {
                Ok(()) if dirty => " Your edited files are still in place.",
                Ok(()) => "",
                Err(_) => " Your edited files are saved in the git stash.",
            };
            (format!("Couldn't download the update. {}{}", cause, kept), false)
        }
    }
}

/// Put stashed edits back. On a conflict: the tree was clean before the pop, so
/// resetting loses nothing, and git keeps the stash when a pop conflicts.
async fn restore_edits(path: &str, log: &impl Fn(String)) -> Result<(), String> {
    match git_in(path, &["stash", "pop"]).await {
        Ok(_) => Ok(()),
        Err(e) => {
            log(e);
            if let Err(e) = git_in(path, &["reset", "--hard", "-q", "HEAD"]).await {
                log(e);
            }
            Err("Your edited files clashed with the new code, so they weren't put back. They're saved in the git stash.".to_string())
        }
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

    #[test]
    fn version_tags_parse() {
        for (tag, want) in [
            ("v0.9.6", Some((0, 9, 6))),
            ("0.9.6", Some((0, 9, 6))),
            ("v1.0", Some((1, 0, 0))),
            ("V2", Some((2, 0, 0))),
            ("v0.9.8-beta", Some((0, 9, 8))),
            (" v0.9.8+build7 ", Some((0, 9, 8))),
            ("nightly", None),
            ("v0.9.x", None),
            ("", None),
        ] {
            assert_eq!(parse_version(tag), want, "tag {:?}", tag);
        }
    }

    #[test]
    fn newer_release_picks_the_newest_usable_one() {
        let listing = r#"[
            {"tag_name":"v2.0.0","draft":true,"prerelease":false,"html_url":"https://x/draft"},
            {"tag_name":"v0.9.8-beta","draft":false,"prerelease":true,"html_url":"https://x/beta"},
            {"tag_name":"v0.9.7","draft":false,"prerelease":false,"html_url":"https://x/stable"},
            {"tag_name":"nightly","draft":false,"prerelease":true,"html_url":"https://x/junk"},
            {"tag_name":"v0.9.5","draft":false,"prerelease":false,"html_url":"https://x/old"}
        ]"#;
        let got = newer_release(listing, "0.9.6").expect("something newer than 0.9.6");
        assert_eq!(got, LauncherRelease { version: "0.9.8-beta".into(), url: "https://x/beta".into() });
        assert_eq!(newer_release(listing, "0.9.8"), None, "equal is not newer");
        let tie = r#"[
            {"tag_name":"v0.9.9-beta","draft":false,"prerelease":true,"html_url":"https://x/beta9"},
            {"tag_name":"v0.9.9","draft":false,"prerelease":false,"html_url":"https://x/final9"}
        ]"#;
        assert_eq!(newer_release(tie, "0.9.6").unwrap().url, "https://x/final9", "final beats beta at the same version");
        assert_eq!(newer_release(listing, "1.0.0"), None, "drafts never count");
        assert_eq!(newer_release("not json", "0.9.6"), None);
        assert_eq!(newer_release(r#"{"message":"API rate limit exceeded"}"#, "0.9.6"), None);
        assert_eq!(newer_release("[]", "0.9.6"), None);
    }

    /// Hits the real GitHub API: run by hand with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "network"]
    async fn live_github_release_check_answers() {
        let check = check_launcher_release().await;
        assert_ne!(check, LauncherCheck::Failed, "GitHub listing unreadable or offline");
        assert_ne!(check, LauncherCheck::Pending);
    }

    #[tokio::test]
    async fn delete_actions_refuse_a_folder_that_is_not_an_install() {
        let d = std::env::temp_dir().join(format!("sapphire-launcher-test-{}-notinstall", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("user")).unwrap();
        std::fs::write(d.join("user").join("precious.txt"), "docs").unwrap();
        let path = d.to_string_lossy().into_owned();
        assert!(!looks_like_install(&path));
        let (msg, ok) = uninstall_delete_folder(path.clone()).await;
        assert!(!ok && msg.contains("doesn't look like a Sapphire install"), "{}", msg);
        let (msg, ok) = uninstall_delete_userdata(path.clone()).await;
        assert!(!ok && msg.contains("doesn't look like a Sapphire install"), "{}", msg);
        assert!(d.join("user").join("precious.txt").exists(), "nothing was deleted");
        // A real-looking install is deleted.
        sapphire_files(&d);
        std::fs::create_dir_all(d.join(".git")).unwrap();
        assert!(looks_like_install(&path));
        let (_, ok) = uninstall_delete_userdata(path.clone()).await;
        assert!(ok && !d.join("user").exists());
        let (_, ok) = uninstall_delete_folder(path).await;
        assert!(ok && !d.exists());
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

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sapphire-launcher-install-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn sapphire_files(d: &std::path::Path) {
        std::fs::create_dir_all(d.join("core")).unwrap();
        for f in ["main.py", "sapphire.py", "core/setup.py"] {
            std::fs::write(d.join(f), "").unwrap();
        }
    }

    #[test]
    fn a_folder_is_identified_by_what_it_holds() {
        let root = scratch("kind");
        let s = |p: &std::path::Path| p.to_string_lossy().into_owned();
        assert_eq!(install_kind(&s(&root.join("nope"))), InstallKind::Nothing);
        let empty = root.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(install_kind(&s(&empty)), InstallKind::Nothing);
        // Someone's own Python project.
        let mine = root.join("mine");
        std::fs::create_dir_all(mine.join(".git")).unwrap();
        std::fs::write(mine.join("main.py"), "").unwrap();
        assert_eq!(install_kind(&s(&mine)), InstallKind::Other);
        assert!(!can_delete(&s(&mine)) && !looks_like_install(&s(&mine)));
        assert!(clone_blocker(&s(&mine)).is_some());
        // The launcher's own repo has "ddxfish/sapphire" in its remote, and isn't her.
        std::fs::write(mine.join(".git/config"), "url = https://github.com/ddxfish/sapphire-launcher.git\n").unwrap();
        assert_eq!(install_kind(&s(&mine)), InstallKind::Other);
        // Our clone, cut short.
        let cut = root.join("cut");
        std::fs::create_dir_all(cut.join(".git")).unwrap();
        std::fs::write(cut.join(".git/config"), "url = https://github.com/ddxfish/sapphire.git\n").unwrap();
        assert_eq!(install_kind(&s(&cut)), InstallKind::Unfinished);
        assert!(can_delete(&s(&cut)) && !looks_like_install(&s(&cut)));
        // A real checkout, fork or not.
        let real = root.join("real");
        std::fs::create_dir_all(real.join(".git")).unwrap();
        sapphire_files(&real);
        assert_eq!(install_kind(&s(&real)), InstallKind::Sapphire);
        assert!(clone_blocker(&s(&real)).is_none());
    }

    #[tokio::test]
    async fn delete_refuses_others_and_leaves_nothing_behind() {
        let root = scratch("delete");
        let s = |p: &std::path::Path| p.to_string_lossy().into_owned();
        let mine = root.join("mine");
        std::fs::create_dir_all(mine.join(".git")).unwrap();
        std::fs::write(mine.join("main.py"), "keep me").unwrap();
        assert!(!uninstall_delete_folder(s(&mine)).await.1);
        assert!(mine.join("main.py").exists(), "not ours: untouched");
        let real = root.join("real");
        std::fs::create_dir_all(real.join(".git")).unwrap();
        std::fs::create_dir_all(real.join("user")).unwrap();
        std::fs::write(real.join("user/chat.db"), "x").unwrap();
        sapphire_files(&real);
        assert!(uninstall_delete_userdata(s(&real)).await.1);
        assert!(!real.join("user").exists() && real.join("main.py").exists());
        assert!(uninstall_delete_folder(s(&real)).await.1);
        let left: Vec<_> = std::fs::read_dir(&root).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left, vec![std::ffi::OsString::from("mine")], "no .deleting folders left");
    }

    /// A bare "GitHub", a checkout of it with one tracked file, and a way to push news.
    fn git_setup(name: &str) -> Option<(PathBuf, PathBuf)> {
        let root = scratch(name);
        let work = root.join("work");
        let ok = git(&root, &["init", "-q", "--bare", "-b", "main", "remote.git"])
            && git(&root, &["clone", "-q", "remote.git", "work"])
            && { std::fs::write(work.join("app.py"), "one\ntwo\nthree\n").unwrap(); true }
            && git(&work, &["checkout", "-q", "-b", "main"])
            && git(&work, &["add", "."]) && git(&work, &["commit", "-qm", "first"]) && git(&work, &["push", "-q", "-u", "origin", "main"]);
        ok.then_some((root, work))
    }

    fn git(dir: &std::path::Path, args: &[&str]) -> bool {
        std::process::Command::new("git").args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "init.defaultBranch=main"]).args(args)
            .current_dir(dir).output().is_ok_and(|o| o.status.success())
    }

    /// Land a commit on the remote that `work` hasn't got.
    fn upstream_change(root: &std::path::Path, file: &str, text: &str) {
        let other = root.join("other");
        if !other.exists() {
            assert!(git(root, &["clone", "-q", "remote.git", "other"]));
        }
        assert!(git(&other, &["pull", "-q"]));
        std::fs::write(other.join(file), text).unwrap();
        assert!(git(&other, &["add", "."]) && git(&other, &["commit", "-qm", "news"]) && git(&other, &["push", "-q"]));
    }

    fn stashes(work: &std::path::Path) -> usize {
        let out = std::process::Command::new("git").args(["stash", "list"]).current_dir(work).output().unwrap();
        String::from_utf8_lossy(&out.stdout).lines().count()
    }

    fn sink() -> Arc<std::sync::Mutex<Vec<String>>> {
        Arc::new(std::sync::Mutex::new(Vec::new()))
    }

    #[tokio::test]
    async fn update_keeps_edits_when_the_pull_works() {
        let Some((root, work)) = git_setup("up-ok") else { return };
        upstream_change(&root, "new.py", "news\n");
        std::fs::write(work.join("app.py"), "one\nmine\nthree\n").unwrap();
        let (msg, ok) = update_code(work.to_string_lossy().into_owned(), sink()).await;
        assert!(ok && msg.contains("still in place"), "{}", msg);
        assert!(work.join("new.py").exists(), "the update landed");
        assert!(std::fs::read_to_string(work.join("app.py")).unwrap().contains("mine"), "the edit survived");
        assert_eq!(stashes(&work), 0);
    }

    #[tokio::test]
    async fn update_keeps_edits_when_the_pull_fails() {
        let Some((root, work)) = git_setup("up-fail") else { return };
        std::fs::write(work.join("app.py"), "one\nmine\nthree\n").unwrap();
        std::fs::remove_dir_all(root.join("remote.git")).unwrap(); // GitHub's down
        let (msg, ok) = update_code(work.to_string_lossy().into_owned(), sink()).await;
        assert!(!ok && msg.contains("still in place"), "{}", msg);
        assert!(std::fs::read_to_string(work.join("app.py")).unwrap().contains("mine"), "the edit is back");
        assert_eq!(stashes(&work), 0);
    }

    #[tokio::test]
    async fn update_with_clashing_edits_lands_clean_and_keeps_them_in_the_stash() {
        let Some((root, work)) = git_setup("up-clash") else { return };
        upstream_change(&root, "app.py", "one\ntheirs\nthree\n");
        std::fs::write(work.join("app.py"), "one\nmine\nthree\n").unwrap();
        let (msg, ok) = update_code(work.to_string_lossy().into_owned(), sink()).await;
        assert!(ok && msg.contains("saved in the git stash"), "{}", msg);
        let text = std::fs::read_to_string(work.join("app.py")).unwrap();
        assert!(text.contains("theirs") && !text.contains("<<<<"), "the new code, no conflict markers: {}", text);
        assert_eq!(stashes(&work), 1, "the edit is safe");
    }

    #[tokio::test]
    async fn update_with_only_untracked_files_claims_nothing_and_touches_nothing() {
        let Some((root, work)) = git_setup("up-untracked") else { return };
        upstream_change(&root, "new.py", "news\n");
        std::fs::write(work.join("my-notes.txt"), "mine").unwrap();
        let (msg, ok) = update_code(work.to_string_lossy().into_owned(), sink()).await;
        assert!(ok && !msg.contains("edited"), "{}", msg);
        assert!(work.join("my-notes.txt").exists());
        assert_eq!(stashes(&work), 0);
    }

    #[tokio::test]
    async fn update_refuses_a_detached_install_in_plain_words() {
        let Some((_root, work)) = git_setup("up-detached") else { return };
        assert!(git(&work, &["checkout", "-q", "--detach"]));
        let (msg, ok) = update_code(work.to_string_lossy().into_owned(), sink()).await;
        assert!(!ok && msg.contains("isn't on a branch"), "{}", msg);
    }

    #[test]
    fn failures_are_told_in_plain_words() {
        for (raw, want) in [
            ("fatal: unable to access 'https://x': Could not resolve host: github.com", "internet"),
            ("CondaHTTPError: HTTP 000 CONNECTION FAILED", "internet"),
            ("ERROR: Could not install packages due to an OSError: [Errno 28] No space left on device", "disk is full"),
            ("[WinError 32] The process cannot access the file because it is being used by another process", "in use"),
            ("error: unable to create file: Filename too long", "too long"),
            ("fatal: could not create work tree dir: Permission denied", "Permission"),
            ("SSLError(SSLCertVerificationError('certificate verify failed'))", "secure connection"),
        ] {
            assert!(explain_failure(raw).is_some_and(|e| e.contains(want)), "{} → {:?}", raw, explain_failure(raw));
        }
        // A package with ssl in its name, in a long pip log, is not a blocked connection.
        assert_eq!(explain_failure("Collecting pyopenssl\nERROR: No matching distribution for x"), None);
        assert!(plain_failure("It broke.", "???").contains("log"));
    }

    #[test]
    fn winget_is_read_by_its_exit_code() {
        // The signed decimals winget's own table lists: what a Windows exit code is.
        assert_eq!(winget_outcome(0), WingetOutcome::Installed);
        assert_eq!(winget_outcome(-1978335135), WingetOutcome::AlreadyThere);
        assert_eq!(winget_outcome(-1978335189), WingetOutcome::AlreadyThere);
        assert_eq!(winget_outcome(-1978334967), WingetOutcome::RestartNeeded);
        assert_eq!(winget_outcome(-1978334964), WingetOutcome::Cancelled);
        assert_eq!(winget_outcome(-1978334969), WingetOutcome::NoNetwork);
        assert_eq!(winget_outcome(-1978334971), WingetOutcome::DiskFull);
        assert_eq!(winget_outcome(-1978335212), WingetOutcome::Failed);
    }

    #[tokio::test]
    async fn requirements_are_checked_in_full_with_versions_and_markers() {
        if !sapphire_python_cmd().1.is_empty() {
            eprintln!("skipped: no sapphire env python on this box");
            return;
        }
        let d = scratch("reqs");
        let f = d.join("requirements.txt");
        std::fs::write(&f, "# comment\n\npip>=1  # fine\npip<1\nno-such-package-xyz>=1\nalso-missing; python_version < \"3\"\n--extra-index-url https://x\n").unwrap();
        let got = requirements_missing(&f).await.unwrap();
        assert_eq!(got.len(), 2, "{:?}", got);
        assert!(got[0].starts_with("pip (needs"), "{:?}", got);
        assert_eq!(got[1], "no-such-package-xyz");
        assert_eq!(name_some(&["a".into(), "b".into(), "c".into(), "d".into(), "e".into()]), "a, b, c and 2 more");
    }
}
