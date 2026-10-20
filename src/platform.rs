// Platform seam: all OS divergence (paths, exec, browser/file open, process kill).
use std::path::PathBuf;
use std::process::Command;

// ── Platform seam ───────────────────────────────────────────────────────────
// All OS divergence lives here. Callers stay OS-agnostic. Windows branches
// preserve existing behavior byte-for-byte; the Linux branches are the new path.
// (run_cmd_* exec helpers and find_git / find_conda / find_conda_pip live at the
// bottom, also #[cfg]-split.)

/// Where a conda install might live, most likely first. Everything that looks for
/// conda itself or for a file inside the sapphire env goes through this one list —
/// a new location (miniforge, a machine-wide ProgramData install) is added here once.
fn conda_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    #[cfg(windows)]
    {
        let bases = [
            std::env::var("USERPROFILE").unwrap_or_default(),
            std::env::var("LOCALAPPDATA").unwrap_or_default(),
            std::env::var("ProgramData").unwrap_or_else(|_| "C:\\ProgramData".into()),
        ];
        for base in bases.iter().filter(|b| !b.is_empty()) {
            for name in ["miniconda3", "Miniconda3", "anaconda3", "Anaconda3", "miniforge3", "Miniforge3", "mambaforge"] {
                roots.push(PathBuf::from(base).join(name));
            }
        }
    }
    #[cfg(not(windows))]
    {
        let home = std::env::var("HOME").unwrap_or_default();
        for name in ["miniconda3", "anaconda3", "miniforge3", "mambaforge"] {
            roots.push(PathBuf::from(&home).join(name));
        }
        for p in ["/opt/miniconda3", "/opt/anaconda3", "/opt/conda", "/opt/miniforge3", "/usr/local/miniconda3"] {
            roots.push(PathBuf::from(p));
        }
    }
    roots
}

/// A file inside the sapphire env (`<root>/envs/sapphire/<rel>`), from the first conda
/// root that has it. None = no env anywhere we know of.
fn sapphire_env_file(rel: &str) -> Option<String> {
    conda_roots()
        .into_iter()
        .map(|r| r.join("envs").join("sapphire").join(rel))
        .find(|p| p.exists())
        .map(|p| p.to_string_lossy().into_owned())
}

/// Absolute path to the sapphire env's python (for a systemd ExecStart). None if
/// the env isn't there. Linux only (the service tab is Linux for now).
#[cfg(not(windows))]
pub(crate) fn sapphire_env_python() -> Option<String> {
    sapphire_env_file("bin/python")
}

/// Windows: the env's windowless interpreter (pythonw.exe) for the autostart wrapper —
/// no console flashes at logon. None if the env isn't there.
#[cfg(windows)]
pub(crate) fn sapphire_pythonw() -> Option<String> {
    sapphire_env_file("pythonw.exe")
}

/// `conda run` prefix for the sapphire env, when the env's own binaries can't be
/// found. `--no-capture-output`: conda run otherwise swallows the child's output
/// until it exits — no live log, no streamed pip.
fn conda_run(tool: &str) -> (String, Vec<String>) {
    (find_conda(), vec!["run".into(), "--no-capture-output".into(), "-n".into(), "sapphire".into(), tool.into()])
}

/// Windows: our no-admin "autostart is on" marker. Written when the logon task is
/// registered, deleted on removal — detection reads this instead of querying the task
/// (sidesteps the schtasks query-permission question). Lives in our config dir.
#[cfg(windows)]
pub(crate) fn autostart_marker_path() -> PathBuf {
    app_config_dir().join("autostart.on")
}

/// Windows: the autostart launch shim the logon task runs (via pythonw). Why a shim
/// instead of `pythonw main.py` directly: under pythonw there is NO console, so
/// `sys.stdout`/`sys.stderr` are None — Sapphire's first startup log write throws,
/// main.py's supervisor retries, then gives up after 5 crashes (~17s) and the task
/// exits 1. The shim redirects stdout/stderr to a real logfile before running main.py,
/// which gives pythonw valid handles (no crash) AND captures logs for later tailing.
/// Still windowless — it's pythonw, no cmd.exe console flash.
#[cfg(windows)]
pub(crate) fn autostart_shim_path() -> PathBuf {
    app_config_dir().join("autostart_shim.py")
}

/// Windows: the logfile the autostart shim redirects Sapphire's output into.
#[cfg(windows)]
pub(crate) fn autostart_log_path() -> PathBuf {
    app_config_dir().join("sapphire-launcher.log")
}

/// Base config dir, shared with Sapphire. Mirrors core/setup.py::get_config_dir():
/// Windows %APPDATA%\Sapphire, Linux $XDG_CONFIG_HOME/sapphire or ~/.config/sapphire.
pub(crate) fn app_config_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Ok(appdata) = std::env::var("APPDATA")
            && !appdata.is_empty()
        {
            return PathBuf::from(appdata).join("Sapphire");
        }
        PathBuf::from(std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into()))
            .join("AppData").join("Roaming").join("Sapphire")
    }
    #[cfg(not(windows))]
    {
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
            && !xdg.is_empty()
        {
            return PathBuf::from(xdg).join("sapphire");
        }
        PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
            .join(".config").join("sapphire")
    }
}

/// Default Sapphire install location: ~/sapphire (home folder, not root).
pub(crate) fn default_install_path() -> String {
    #[cfg(windows)]
    {
        let home = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_else(|_| "C:\\".to_string());
        format!("{}\\sapphire", home)
    }
    #[cfg(not(windows))]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        format!("{}/sapphire", home)
    }
}

/// Resolve how to run Sapphire's Python: the env's interpreter directly if we can
/// find it, else fall back to `conda run`. Returns (program, args-before-script).
pub(crate) fn sapphire_python_cmd() -> (String, Vec<String>) {
    let rel = if cfg!(windows) { "python.exe" } else { "bin/python" };
    match sapphire_env_file(rel) {
        Some(p) => (p, vec![]),
        None => conda_run("python"),
    }
}

/// Start a child we never talk to again, with none of our stdio. A plain `spawn()`
/// inherits all three standard handles, and a GUI app's are null or — after
/// `signal_stop`'s console trip — dead, which fails the spawn outright.
fn fire(cmd: &mut Command) {
    use std::process::Stdio;
    let _ = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
}

/// Open a URL in the user's default browser.
pub(crate) fn open_url(url: &str) {
    #[cfg(windows)]
    fire(hidden_cmd("cmd").args(["/c", "start", url]));
    #[cfg(not(windows))]
    fire(hidden_cmd("xdg-open").arg(url));
}

/// Open a file in the user's default viewer/editor.
pub(crate) fn open_file(path: &std::path::Path) {
    #[cfg(windows)]
    fire(hidden_cmd("notepad").arg(path));
    #[cfg(not(windows))]
    fire(hidden_cmd("xdg-open").arg(path));
}

/// Platform null device for discarding command output.
pub(crate) fn null_device() -> &'static str {
    #[cfg(windows)]
    { "NUL" }
    #[cfg(not(windows))]
    { "/dev/null" }
}

/// Linux: the process group id of `pid` from /proc, if the process exists.
#[cfg(not(windows))]
pub(crate) fn process_group_of(pid: u32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    // The comm field can hold spaces and parens; everything after the last ')' is
    // fixed-format: state ppid pgrp ...
    let rest = stat.rsplit_once(')')?.1;
    rest.split_whitespace().nth(2)?.parse().ok()
}

/// Send `sig` to the process group led by `pid` — only if `pid` really is the leader
/// of its own group (we spawn Sapphire with process_group(0), so it is). Direct
/// syscall on purpose: NEVER shell out to /usr/bin/kill for this. procps-ng 4.x
/// parses `kill -TERM -1522700` as kill(-1, SIGTERM), i.e. every process the user
/// owns — it took Krem's whole desktop session down on 2026-10-19.
#[cfg(not(windows))]
fn signal_group(pid: u32, sig: libc::c_int) -> bool {
    if pid <= 1 || process_group_of(pid) != Some(pid as i32) {
        return false;
    }
    // SAFETY: killpg is a plain syscall wrapper; pid was just verified as a group
    // leader > 1, so this can only reach that group.
    unsafe { libc::killpg(pid as libc::pid_t, sig) == 0 }
}

/// Ask Sapphire to stop gracefully (core tears down in ~8 s, worst case ~100 s).
/// Windows: CTRL_BREAK to the runner's process group — it is spawned with
/// CREATE_NEW_PROCESS_GROUP so its pid is the group id, and main.py/sapphire.py
/// handle SIGBREAK as a stop. The launcher is a GUI app with no console of its own,
/// so it briefly attaches to the runner's hidden console to send the event, and puts
/// its standard handles back afterwards: AttachConsole repoints stdin/stdout/stderr
/// at that console and FreeConsole closes them without restoring the old values, so
/// without the restore every later child that inherits a handle (tokio's spawn
/// inherits stdin) would fail at birth with "The handle is invalid (os error 6)" —
/// pip after an update-stop, the next Launch.
/// Linux: SIGTERM to the process group. Returns false if the signal could not be
/// sent; the caller then falls back to `kill_process_tree`.
pub(crate) fn signal_stop(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::{
            AttachConsole, FreeConsole, GenerateConsoleCtrlEvent, GetStdHandle,
            SetConsoleCtrlHandler, SetStdHandle, CTRL_BREAK_EVENT, STD_ERROR_HANDLE,
            STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        };
        // SAFETY: plain Win32 calls against our own process; the only pointer argument
        // is the null handler (None), which is the documented "ignore Ctrl+C" form.
        unsafe {
            let std_ids = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE];
            let saved = std_ids.map(|id| GetStdHandle(id));
            FreeConsole();
            let mut sent = false;
            if AttachConsole(pid) != 0 {
                SetConsoleCtrlHandler(None, 1);
                sent = GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) != 0;
                FreeConsole();
                SetConsoleCtrlHandler(None, 0);
            }
            for (id, h) in std_ids.into_iter().zip(saved) {
                SetStdHandle(id, h);
            }
            sent
        }
    }
    #[cfg(not(windows))]
    {
        signal_group(pid, libc::SIGTERM)
    }
}

/// Hard stop — the fallback when `signal_stop` can't be sent or Sapphire ignores it,
/// and the only option for an instance we didn't spawn (pid 0: sweep only).
/// Windows: taskkill the tree + sweep env pythons. Linux: SIGKILL the process group
/// (we spawn Sapphire in its own group; SIGTERM already went unanswered) + a SIGTERM
/// sweep. The sweep hits EVERY sapphire-env python (a user's own script too), which
/// is why it is not the first move.
pub(crate) fn kill_process_tree(pid: u32) {
    #[cfg(windows)]
    {
        // If we spawned it, the stored PID gives a fast, complete tree kill.
        if pid > 0 {
            let _ = hidden_cmd("taskkill").args(["/F", "/T", "/PID", &pid.to_string()]).output();
        }
        // Sweep every Sapphire-env interpreter — BOTH python.exe AND pythonw.exe. The
        // autostart task runs pythonw.exe (windowless); the old python.exe-only sweep
        // missed it, so Stop did nothing for task-started instances. Matching by an
        // env-path substring also covers miniconda3/anaconda3/Anaconda3 variants. This
        // kills main.py too, so its supervisor can't relaunch sapphire.py. Uses CIM, not
        // the deprecated wmic (removed-by-default on Win11 24H2+).
        let ps = "Get-CimInstance Win32_Process -Filter \"Name='python.exe' OR Name='pythonw.exe'\" | Where-Object { $_.ExecutablePath -like '*\\envs\\sapphire\\*' } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }";
        let _ = hidden_cmd("powershell").args(["-NoProfile", "-Command", ps]).output();
    }
    #[cfg(not(windows))]
    {
        if pid > 0 {
            // Whole process group (Sapphire is spawned into its own group at launch),
            // child STT/TTS workers too. Direct syscall — see signal_group.
            signal_group(pid, libc::SIGKILL);
        }
        // Sweep any stray python running the sapphire env (e.g. started outside us).
        let _ = hidden_cmd("pkill").args(["-TERM", "-f", "envs/sapphire/bin/python"]).output();
    }
}

/// Create a Command that won't flash a console window on Windows
pub(crate) fn hidden_cmd(program: &str) -> Command {
    #[allow(unused_mut)] // mutated on Windows only
    let mut cmd = Command::new(program);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

// ── Exec helpers (moved from main.rs) ─────────────────────────────────────

#[cfg(windows)]
pub(crate) const CREATE_NO_WINDOW: u32 = 0x08000000;
/// Windows: the spawned runner becomes its own process group (pid = group id) so
/// Stop can send CTRL_BREAK to it and everything it spawned.
#[cfg(windows)]
pub(crate) const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;


/// Run a command on a background thread so it doesn't freeze the UI.
pub(crate) async fn run_cmd_async(program: String, args: Vec<String>) -> Result<String, String> {
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




/// Run a command on a background thread, returning combined stdout+stderr for logging.
/// Conda's terms-of-service gate (2025-07) would otherwise stall `conda create` on a
/// pre-existing conda with no terminal to say yes in; the variable is harmless elsewhere.
pub(crate) async fn run_cmd_full_async(program: String, args: Vec<String>) -> Result<String, String> {
    let result: Result<Result<String, String>, _> = tokio::task::spawn_blocking(move || {
        let mut cmd = Command::new(&program);
        cmd.args(&args).env("CONDA_PLUGINS_AUTO_ACCEPT_TOS", "yes");
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


/// Find git executable on disk (Linux: rely on PATH).
#[cfg(not(windows))]
pub(crate) fn find_git() -> String {
    if let Ok(out) = std::process::Command::new("which").arg("git").output()
        && out.status.success()
    {
        let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !p.is_empty() && PathBuf::from(&p).exists() {
            return p;
        }
    }
    "git".into()
}

/// Find git executable on disk
#[cfg(windows)]
pub(crate) fn find_git() -> String {
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
        && output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout);
            if let Some(first_line) = path.lines().next() {
                let p = first_line.trim();
                if !p.is_empty() && PathBuf::from(p).exists() {
                    return p.to_string();
                }
            }
        }
    "git".into() // final fallback
}

/// The conda executable: a known install root first, then PATH (`where`/`which`),
/// else the bare name and hope.
pub(crate) fn find_conda() -> String {
    let rel = if cfg!(windows) { "Scripts\\conda.exe" } else { "bin/conda" };
    if let Some(p) = conda_roots().into_iter().map(|r| r.join(rel)).find(|p| p.exists()) {
        return p.to_string_lossy().into_owned();
    }
    #[cfg(windows)]
    let probe = hidden_cmd("cmd").args(["/c", "where", "conda"]).output();
    #[cfg(not(windows))]
    let probe = hidden_cmd("which").arg("conda").output();
    if let Ok(out) = probe
        && out.status.success()
    {
        let p = String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("").trim().to_string();
        if !p.is_empty() && PathBuf::from(&p).exists() {
            return p;
        }
    }
    "conda".into()
}

/// pip for the sapphire env: the env's own pip directly, else `conda run`.
pub(crate) fn find_conda_pip() -> (String, Vec<String>) {
    let rel = if cfg!(windows) { "Scripts\\pip.exe" } else { "bin/pip" };
    match sapphire_env_file(rel) {
        Some(p) => (p, vec![]),
        None => conda_run("pip"),
    }
}
