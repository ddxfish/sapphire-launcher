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

/// Windows: the autostart launch shim the logon task runs (via pythonw, so nothing
/// flashes at sign-in). The shim does what Launch does: starts the env's python.exe
/// on main.py, hidden, in its own process group, with her output going to
/// `launch_log_path()`, and leaves the `run_state_path()` handshake — so an
/// autostarted Sapphire is one the launcher can follow and stop like its own.
/// (v1/v2 ran main.py in-process under pythonw: no console, so no graceful Stop.)
#[cfg(windows)]
pub(crate) fn autostart_shim_path() -> PathBuf {
    app_config_dir().join("autostart_shim.py")
}

// ── Finding her ─────────────────────────────────────────────────────────────
// The launcher starts Sapphire detached and is not her parent. To know whether she
// is running, who started her and where her output goes, it reads two small files:
// core's own instance lock (`user/.instance.lock`, written by her runner since
// 2026-10-16) and our handshake (`sapphire-run.json`, written by Launch and by the
// Windows autostart shim). Both name a pid, and a pid is believed only after it is
// checked to be alive AND a sapphire-env python (`pid_is_hers`): notes outlive her,
// and a reboot can hand the number to anything.

/// Where Launch (and the Windows autostart shim) send her console output: the
/// runner's own lines plus the console copy of her log. Core keeps writing its own
/// `user/logs/sapphire.log` regardless.
pub(crate) fn launch_log_path() -> PathBuf {
    app_config_dir().join("sapphire-launcher.log")
}

/// The handshake left for a later launcher: "this pid's output is in this file".
pub(crate) fn run_state_path() -> PathBuf {
    app_config_dir().join("sapphire-run.json")
}

/// Core's own log (TimedRotatingFileHandler, rotated at midnight): the live source
/// for an instance started elsewhere (a terminal, an old shim).
pub(crate) fn core_log_path(install: &str) -> PathBuf {
    PathBuf::from(install).join("user").join("logs").join("sapphire.log")
}

/// Core's instance lock. Absent = never run, or a core from before the lock.
pub(crate) fn lock_path(install: &str) -> PathBuf {
    PathBuf::from(install).join("user").join(".instance.lock")
}

/// Who started the Sapphire we found: decides the wording and where her log is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    Launcher,
    Autostart,
    Elsewhere,
}

/// A running Sapphire: her runner's pid (0 = answering on the web port but no pid
/// to be had — an old core started elsewhere), core's "since" note when there is
/// one, who started her, and the file her output goes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Instance {
    pub(crate) pid: u32,
    pub(crate) since: String,
    pub(crate) origin: Origin,
    pub(crate) log: PathBuf,
}

impl Instance {
    /// Status words for the Log tab and the launcher log.
    pub(crate) fn describe(&self) -> String {
        let who = match self.origin {
            Origin::Launcher => "started by this app".to_string(),
            Origin::Autostart => "started at sign-in".to_string(),
            Origin::Elsewhere if self.pid > 0 => format!("started elsewhere, pid {}", self.pid),
            Origin::Elsewhere => "started outside this app".to_string(),
        };
        if self.since.is_empty() {
            format!("Sapphire is running ({})", who)
        } else {
            format!("Sapphire is running ({}, since {})", who, self.since)
        }
    }
}

/// What Launch and the shim write to `run_state_path()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunState {
    pub(crate) pid: u32,
    pub(crate) log: PathBuf,
    pub(crate) origin: Origin,
}

/// Write the handshake atomically (tmp + rename): a reader never sees half a file.
pub(crate) fn write_run_state(path: &std::path::Path, st: &RunState) -> std::io::Result<()> {
    let by = match st.origin {
        Origin::Launcher => "launcher",
        Origin::Autostart => "autostart",
        Origin::Elsewhere => "elsewhere",
    };
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let json = serde_json::json!({ "pid": st.pid, "log": st.log.to_string_lossy(), "by": by, "started": started });
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, format!("{:#}\n", json))?;
    std::fs::rename(&tmp, path)
}

/// Read the handshake. Forgiving: anything odd is "no handshake".
pub(crate) fn read_run_state(path: &std::path::Path) -> Option<RunState> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
    let pid = u32::try_from(v.get("pid")?.as_u64()?).ok().filter(|p| *p > 0)?;
    let log = PathBuf::from(v.get("log")?.as_str()?);
    let origin = match v.get("by").and_then(|b| b.as_str()) {
        Some("autostart") => Origin::Autostart,
        _ => Origin::Launcher,
    };
    Some(RunState { pid, log, origin })
}

/// Core's lock note, read the way core itself reads it: from byte 1. Byte 0 carries
/// the kernel lock, and Windows refuses to let another process read a locked byte.
/// None = no file.
pub(crate) fn read_lock_note(install: &str) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(lock_path(install)).ok()?;
    f.seek(SeekFrom::Start(1)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// `pid=N since=YYYY-MM-DD HH:MM:SS` → (N, "YYYY-MM-DD HH:MM:SS"). The note is
/// whatever core wrote last; it is still there after she exits.
pub(crate) fn parse_lock_note(note: &str) -> Option<(u32, String)> {
    let words: Vec<&str> = note.split_whitespace().collect();
    let pid = words.iter().find_map(|w| w.strip_prefix("pid=")?.parse::<u32>().ok()).filter(|p| *p > 0)?;
    let since = words.iter().position(|w| w.starts_with("since=")).map(|i| {
        let mut s = words[i]["since=".len()..].to_string();
        if let Some(t) = words.get(i + 1)
            && t.contains(':')
        {
            s.push(' ');
            s.push_str(t); // the date and the time are two words
        }
        s
    }).unwrap_or_default();
    Some((pid, since))
}

/// The pure heart of `find_instance`: core's note, our handshake, and an "is this
/// pid alive and hers" oracle (a closure, so the tests need no real processes).
pub(crate) fn resolve_instance(
    note: Option<&str>,
    run: Option<RunState>,
    hers: impl Fn(u32) -> bool,
    install: &str,
) -> Option<Instance> {
    if let Some((pid, since)) = note.and_then(parse_lock_note)
        && hers(pid)
    {
        // Her runner holds the lock. Our handshake naming the same pid means her
        // output is in our file; otherwise she was started elsewhere.
        let (origin, log) = match run {
            Some(r) if r.pid == pid => (r.origin, r.log),
            _ => (Origin::Elsewhere, core_log_path(install)),
        };
        return Some(Instance { pid, since, origin, log });
    }
    // No lock (a core from before 2026-10-16 — Stable, as of this writing) or a
    // stale one: the handshake alone still finds a Sapphire Launch or the shim started.
    let r = run?;
    hers(r.pid).then(|| Instance { pid: r.pid, since: String::new(), origin: r.origin, log: r.log })
}

/// Is a Sapphire running on this install? Reads two small files and checks a pid;
/// never spawns a process, so it is cheap enough to poll. None = nobody we can
/// vouch for (an old core started elsewhere is still possible; the caller's web
/// port probe covers that).
pub(crate) fn find_instance(install: &str) -> Option<Instance> {
    resolve_instance(read_lock_note(install).as_deref(), read_run_state(&run_state_path()), pid_is_hers, install)
}

/// Is `pid` alive and a sapphire-env python running her? Linux: /proc gives the
/// executable (a gone or zombie pid has no exe link) and the command line. The
/// identity half is what makes a recycled pid harmless: nothing that fails it is
/// ever signalled.
#[cfg(not(windows))]
pub(crate) fn pid_is_hers(pid: u32) -> bool {
    if pid <= 1 {
        return false;
    }
    let Ok(exe) = std::fs::read_link(format!("/proc/{}/exe", pid)) else { return false };
    if !exe.to_string_lossy().contains("/envs/sapphire/") {
        return false;
    }
    let Ok(cmd) = std::fs::read(format!("/proc/{}/cmdline", pid)) else { return false };
    let cmd = String::from_utf8_lossy(&cmd);
    cmd.contains("main.py") || cmd.contains("sapphire.py")
}

/// Is `pid` alive and a sapphire-env python? Windows: open the process for a look
/// (no tasklist/CIM spawn), check it has not exited, read its image path. A pid we
/// may not open (she runs elevated or as another user) is believed on the lock's
/// word — the most we can know — and Stop says so when it cannot reach her.
#[cfg(windows)]
pub(crate) fn pid_is_hers(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ACCESS_DENIED, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    if pid <= 4 {
        return false; // the idle/system pseudo-pids
    }
    // SAFETY: plain Win32 calls; the handle is closed on every path and the name
    // buffer's length is passed in and out exactly as the API requires.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return GetLastError() == ERROR_ACCESS_DENIED;
        }
        let mut code = 0u32;
        let alive = GetExitCodeProcess(h, &mut code) != 0 && code == STILL_ACTIVE as u32;
        let mut name = [0u16; 1024];
        let mut len = name.len() as u32;
        let named = QueryFullProcessImageNameW(h, 0, name.as_mut_ptr(), &mut len) != 0;
        CloseHandle(h);
        if !alive {
            return false;
        }
        if !named {
            return true; // no name to check: the lock's word stands
        }
        String::from_utf16_lossy(&name[..len as usize]).to_ascii_lowercase().contains("\\envs\\sapphire\\")
    }
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

/// Send `sig` to `pid` alone — only if it is alive and hers (`pid_is_hers`). For a
/// runner started by hand inside someone else's group: it forwards SIGTERM to
/// sapphire.py itself. Same no-/usr/bin/kill rule as `signal_group`.
#[cfg(not(windows))]
fn signal_one(pid: u32, sig: libc::c_int) -> bool {
    if !pid_is_hers(pid) {
        return false;
    }
    // SAFETY: plain syscall; pid was just verified alive and a sapphire-env python.
    unsafe { libc::kill(pid as libc::pid_t, sig) == 0 }
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
        // Her whole group when she leads one (we start her that way), else the runner
        // alone (started by hand); never a pid that is not provably hers.
        signal_group(pid, libc::SIGTERM) || signal_one(pid, libc::SIGTERM)
    }
}

/// Hard stop — the fallback when `signal_stop` can't be sent or Sapphire ignores it.
/// With a pid: her tree / process group, precisely. Without one (pid 0: an old core
/// started elsewhere, nothing to name): sweep every sapphire-env python — a user's
/// own script in that env too, which is why the sweep is the last resort only.
pub(crate) fn kill_process_tree(pid: u32) {
    #[cfg(windows)]
    {
        if pid > 0 {
            let _ = hidden_cmd("taskkill").args(["/F", "/T", "/PID", &pid.to_string()]).output();
            return;
        }
        // Sweep every Sapphire-env interpreter — BOTH python.exe AND pythonw.exe (an
        // old autostart shim runs her under pythonw). Matching by an env-path substring
        // covers miniconda3/anaconda3/Anaconda3 variants. This kills main.py too, so its
        // supervisor can't relaunch sapphire.py. Uses CIM, not the deprecated wmic
        // (removed-by-default on Win11 24H2+).
        let ps = "Get-CimInstance Win32_Process -Filter \"Name='python.exe' OR Name='pythonw.exe'\" | Where-Object { $_.ExecutablePath -like '*\\envs\\sapphire\\*' } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }";
        let _ = hidden_cmd("powershell").args(["-NoProfile", "-Command", ps]).output();
    }
    #[cfg(not(windows))]
    {
        if pid > 0 {
            // Her whole group (she leads one when we started her), child STT/TTS
            // workers too; the runner alone otherwise. Direct syscalls — see signal_group.
            if !signal_group(pid, libc::SIGKILL) {
                signal_one(pid, libc::SIGKILL);
            }
            return;
        }
        // Sweep any stray python running the sapphire env (started outside us, no pid known).
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

// ── Tests: the pure parts of finding her (any OS) ─────────────────────────────
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
    fn lock_note_parses_what_core_writes() {
        // Core's exact shape as seen from byte 1 (byte 0 is the '\n' that carries the lock).
        assert_eq!(parse_lock_note("pid=4242 since=2026-10-20 08:01:02\n"), Some((4242, "2026-10-20 08:01:02".into())));
        assert_eq!(parse_lock_note("\r\n pid=7 since=2026-10-20 08:01:02 \r\n"), Some((7, "2026-10-20 08:01:02".into())));
        assert_eq!(parse_lock_note("pid=9"), Some((9, String::new())));
        for bad in ["", "\n", "pid=0 since=x", "pid=abc", "since=2026-10-20 08:01:02", "garbage", "id=12 since=x"] {
            assert_eq!(parse_lock_note(bad), None, "{:?}", bad);
        }
    }

    #[test]
    fn lock_note_is_read_from_byte_one() {
        let d = scratch("lock");
        let install = d.to_string_lossy().into_owned();
        assert_eq!(read_lock_note(&install), None, "no file");
        std::fs::create_dir_all(d.join("user")).unwrap();
        std::fs::write(lock_path(&install), b"\npid=4242 since=2026-10-20 08:01:02\n").unwrap();
        assert_eq!(parse_lock_note(&read_lock_note(&install).unwrap()), Some((4242, "2026-10-20 08:01:02".into())));
    }

    #[test]
    fn run_state_round_trips() {
        let p = scratch("run").join("deep").join("sapphire-run.json");
        let st = RunState {
            pid: 31337,
            log: PathBuf::from(r#"C:\Users\Krem's "lab"\AppData\Roaming\Sapphire\sapphire-launcher.log"#),
            origin: Origin::Autostart,
        };
        write_run_state(&p, &st).unwrap();
        assert_eq!(read_run_state(&p), Some(st));
        assert!(!p.with_extension("json.tmp").exists(), "the tmp file was renamed away");
        std::fs::write(&p, "{broken").unwrap();
        assert_eq!(read_run_state(&p), None);
        std::fs::write(&p, r#"{"pid": 0, "log": "x"}"#).unwrap();
        assert_eq!(read_run_state(&p), None);
        // What the Python shim writes (json.dump: different key order, no BOM).
        std::fs::write(&p, r#"{"pid": 5, "log": "C:\\x\\sapphire-launcher.log", "by": "autostart", "started": 1}"#).unwrap();
        let r = read_run_state(&p).unwrap();
        assert_eq!((r.pid, r.origin, r.log.to_string_lossy().as_ref()), (5, Origin::Autostart, r"C:\x\sapphire-launcher.log"));
    }

    #[test]
    fn instance_resolution_believes_only_live_pids() {
        let note = Some("pid=100 since=2026-10-20 08:01:02\n");
        let ours = || Some(RunState { pid: 100, log: PathBuf::from("/cfg/sapphire-launcher.log"), origin: Origin::Launcher });
        let core_log = core_log_path("/inst");
        // Lock and handshake agree: ours, our log, core's since.
        let i = resolve_instance(note, ours(), |p| p == 100, "/inst").unwrap();
        assert_eq!(i, Instance { pid: 100, since: "2026-10-20 08:01:02".into(), origin: Origin::Launcher, log: PathBuf::from("/cfg/sapphire-launcher.log") });
        // Lock only (a terminal start): elsewhere, core's log.
        let i = resolve_instance(note, None, |p| p == 100, "/inst").unwrap();
        assert_eq!((i.origin, &i.log), (Origin::Elsewhere, &core_log));
        // A handshake left by an older run names another pid: the lock wins.
        let old = Some(RunState { pid: 55, log: PathBuf::from("/cfg/old.log"), origin: Origin::Launcher });
        let i = resolve_instance(note, old, |p| p == 100, "/inst").unwrap();
        assert_eq!((i.pid, i.origin), (100, Origin::Elsewhere));
        // Stale note (pid dead, or recycled by something that is not her): nobody.
        assert_eq!(resolve_instance(note, ours(), |_| false, "/inst"), None);
        // No lock file (a core before the lock), but our handshake's pid is alive and hers.
        let i = resolve_instance(None, ours(), |p| p == 100, "/inst").unwrap();
        assert_eq!((i.pid, i.origin, i.since.as_str()), (100, Origin::Launcher, ""));
        assert_eq!(resolve_instance(None, ours(), |_| false, "/inst"), None);
        assert_eq!(resolve_instance(None, None, |_| true, "/inst"), None);
        // The words.
        assert_eq!(i.describe(), "Sapphire is running (started by this app)");
        let e = Instance { pid: 100, since: "2026-10-20 08:01:02".into(), origin: Origin::Elsewhere, log: core_log };
        assert_eq!(e.describe(), "Sapphire is running (started elsewhere, pid 100, since 2026-10-20 08:01:02)");
        let a = Instance { pid: 3, since: String::new(), origin: Origin::Autostart, log: PathBuf::new() };
        assert_eq!(a.describe(), "Sapphire is running (started at sign-in)");
        let z = Instance { pid: 0, since: String::new(), origin: Origin::Elsewhere, log: PathBuf::new() };
        assert_eq!(z.describe(), "Sapphire is running (started outside this app)");
    }

    #[test]
    fn pid_identity_rejects_us_and_the_dead() {
        // This test binary is alive, but it is not a sapphire-env python.
        assert!(!pid_is_hers(std::process::id()));
        assert!(!pid_is_hers(0));
        assert!(!pid_is_hers(u32::MAX - 1)); // nobody
    }
}
