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

/// Where conda itself says the sapphire env is, once it has been asked (every scan
/// asks). conda does not always put an env under its own root: a machine-wide
/// install the user can't write to gets its envs in the user's `.conda` folder.
static ENV_PREFIX: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// Remember conda's answer (None = it has no such env).
pub(crate) fn set_env_prefix(prefix: Option<PathBuf>) {
    if let Ok(mut p) = ENV_PREFIX.lock() {
        *p = prefix;
    }
}

/// `conda env list --json` → the folder of the env called sapphire.
pub(crate) fn conda_env_prefix(json: &str) -> Option<PathBuf> {
    let v: serde_json::Value = serde_json::from_str(&json[json.find('{')?..]).ok()?;
    v.get("envs")?.as_array()?.iter().filter_map(|e| e.as_str()).map(PathBuf::from)
        .find(|p| p.file_name().is_some_and(|n| n.eq_ignore_ascii_case("sapphire")))
}

/// Every folder the sapphire env could be in, most trusted first: conda's own
/// answer, each known root's `envs`, then conda's per-user fallback.
fn sapphire_env_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = ENV_PREFIX.lock().ok().and_then(|p| p.clone()).into_iter().collect();
    dirs.extend(conda_roots().into_iter().map(|r| r.join("envs").join("sapphire")));
    let home = std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).unwrap_or_default();
    if !home.is_empty() {
        dirs.push(PathBuf::from(home).join(".conda").join("envs").join("sapphire"));
    }
    dirs
}

/// A file inside the sapphire env, from the first place that has it. None = no env
/// anywhere we know of. One answer for python, pip, pythonw and the unit's ExecStart.
fn sapphire_env_file(rel: &str) -> Option<String> {
    sapphire_env_dirs().into_iter().map(|d| d.join(rel)).find(|p| p.exists()).map(|p| p.to_string_lossy().into_owned())
}

/// The env's folder, if its python is there.
pub(crate) fn sapphire_env_dir() -> Option<PathBuf> {
    let rel = if cfg!(windows) { "python.exe" } else { "bin/python" };
    sapphire_env_dirs().into_iter().find(|d| d.join(rel).exists())
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
// core's own instance lock (`<config>/locks/instance-<key>.lock` since 2026-10-27,
// `user/.instance.lock` before that) and our handshake (`sapphire-run.json`, written by Launch and by the
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
    // Tests never read the real one: on a computer with a live Sapphire it says she
    // is running from her real folder, and every test that asks "is she running
    // somewhere else?" about its made-up folder would be told yes.
    #[cfg(test)]
    return std::env::temp_dir().join(format!("sl-test-{}-sapphire-run.json", std::process::id()));
    #[cfg(not(test))]
    app_config_dir().join("sapphire-run.json")
}

/// Core's own log (TimedRotatingFileHandler, rotated at midnight): the live source
/// for an instance started elsewhere (a terminal, an old shim).
pub(crate) fn core_log_path(install: &str) -> PathBuf {
    PathBuf::from(install).join("user").join("logs").join("sapphire.log")
}

/// Where a runner from before 2026-10-27 keeps its lock. One can still be up after an
/// update; core deletes this file as soon as a newer runner takes the lock.
pub(crate) fn legacy_lock_path(install: &str) -> PathBuf {
    PathBuf::from(install).join("user").join(".instance.lock")
}

/// What Python's `os.path.normcase` does on Windows, after `realpath`'s prefix strip:
/// no `\\\\?\\` prefix, backslashes, lower case.
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_normcase(path: &str) -> String {
    let plain = match (path.strip_prefix(r"\\?\UNC\"), path.strip_prefix(r"\\?\")) {
        (Some(unc), _) => format!(r"\\{}", unc),
        (_, Some(p)) => p.to_string(),
        _ => path.to_string(),
    };
    plain.replace('/', "\\").to_lowercase()
}

/// A path the way core spells it: `normcase(realpath(path))`. None = it does not exist.
fn core_spelling(path: &std::path::Path) -> Option<String> {
    let real = std::fs::canonicalize(path).ok()?.to_string_lossy().into_owned();
    #[cfg(windows)]
    return Some(windows_normcase(&real));
    #[cfg(not(windows))]
    Some(real)
}

/// Core's lock key: the first 16 hex digits of the sha256 of that spelling.
/// MUST match `core/single_instance.py::lock_path`; the test vectors pin it.
fn lock_key(spelled_user_dir: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(spelled_user_dir.as_bytes())[..8].iter().map(|b| format!("{:02x}", b)).collect()
}

/// Core's instance lock for this install. None = no user/ folder, so she never ran here.
pub(crate) fn instance_lock_path(install: &str) -> Option<PathBuf> {
    let user = core_spelling(&PathBuf::from(install).join("user"))?;
    Some(app_config_dir().join("locks").join(format!("instance-{}.lock", lock_key(&user))))
}

/// Does this install have a lock file at all? No = a core from before the lock.
pub(crate) fn has_lock_file(install: &str) -> bool {
    legacy_lock_path(install).exists() || instance_lock_path(install).is_some_and(|p| p.exists())
}

/// Do two paths name the same install folder?
pub(crate) fn same_install(a: &str, b: &str) -> bool {
    a == b || match (core_spelling(a.as_ref()), core_spelling(b.as_ref())) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
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
    pub(crate) install: Option<String>, // None = written by a shim before v4
    pub(crate) started: u64,            // unix seconds; 0 = unknown
}

/// Seconds since the epoch, for the handshake.
pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Rename, patiently: on Windows a scanner, a sync client or another reader can
/// hold the target for a moment, and that moment must not lose the write.
pub(crate) fn rename_retry(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    let mut last = std::fs::rename(from, to);
    for _ in 0..5 {
        if last.is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        last = std::fs::rename(from, to);
    }
    last
}

/// The one way a file of ours is written: whole or not at all. `private` = it can
/// hold secrets (API keys), so it is never readable by anyone else, not even for
/// the moment between being written and being renamed into place.
pub(crate) fn write_atomic(path: &std::path::Path, text: &str, private: bool) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // A tmp name of this write's own (process, then a count within it): two
    // writers sharing one would rename a half-written file into place.
    static NTH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nth = NTH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = PathBuf::from(format!("{}.{}-{}.tmp", path.display(), std::process::id(), nth));
    let _ = std::fs::remove_file(&tmp); // a leftover (a recycled pid's) would keep its old permissions
    let mut open = std::fs::OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt;
        open.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = private; // a Windows profile folder is already the user's alone
    let done = open.open(&tmp).and_then(|mut f| f.write_all(text.as_bytes())).and_then(|_| rename_retry(&tmp, path));
    if done.is_err() {
        let _ = std::fs::remove_file(&tmp); // never left lying about
    }
    done
}

/// Where the systemd --user unit lives (`$XDG_CONFIG_HOME/systemd/user`, as systemd
/// itself reads it), and the file its EnvironmentFile= names. One answer for the
/// writer, the remover and the LLM watcher's look at the unit.
#[cfg(not(windows))]
pub(crate) fn unit_path() -> PathBuf {
    app_config_dir().with_file_name("systemd").join("user").join("sapphire.service")
}
#[cfg(not(windows))]
pub(crate) fn service_env_path() -> PathBuf {
    app_config_dir().join("service.env")
}

/// Written before an update changes anything and removed once its packages are in:
/// a launcher opening with this file finds new code on old packages.
pub(crate) fn update_marker_path() -> PathBuf {
    app_config_dir().join("update-unfinished")
}

/// Write the marker for this install. False = it couldn't be written, and a crash
/// from here on would leave no trace that an update stopped halfway.
pub(crate) fn mark_update_unfinished(install: &str) -> bool {
    let _ = std::fs::create_dir_all(app_config_dir());
    std::fs::write(update_marker_path(), install).is_ok()
}

/// Did an update on this install stop partway?
pub(crate) fn update_unfinished(marker: &std::path::Path, install: &str) -> bool {
    std::fs::read_to_string(marker).is_ok_and(|p| same_install(p.trim(), install))
}

/// When this machine booted (unix seconds). None = unknown.
fn boot_time() -> Option<u64> {
    #[cfg(windows)]
    {
        // SAFETY: GetTickCount64 takes nothing and cannot fail.
        let up = unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() } / 1000;
        now_secs().checked_sub(up)
    }
    #[cfg(not(windows))]
    {
        let stat = std::fs::read_to_string("/proc/stat").ok()?;
        stat.lines().find_map(|l| l.strip_prefix("btime ")?.trim().parse().ok())
    }
}

/// A handshake from before this boot names a pid from a previous life. The slack
/// covers clock jitter between the two readings.
pub(crate) fn handshake_is_stale(started: u64, boot: Option<u64>) -> bool {
    boot.is_some_and(|b| started > 0 && started + 5 < b)
}

/// Write the handshake atomically (tmp + rename): a reader never sees half a file.
pub(crate) fn write_run_state(path: &std::path::Path, st: &RunState) -> std::io::Result<()> {
    let by = match st.origin {
        Origin::Launcher => "launcher",
        Origin::Autostart => "autostart",
        Origin::Elsewhere => "elsewhere",
    };
    let json = serde_json::json!({ "pid": st.pid, "log": st.log.to_string_lossy(), "by": by, "started": st.started, "install": st.install });
    write_atomic(path, &format!("{:#}\n", json), false)
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
    let install = v.get("install").and_then(|i| i.as_str()).map(str::to_string);
    let started = v.get("started").and_then(|s| s.as_u64()).unwrap_or(0);
    Some(RunState { pid, log, origin, install, started })
}

/// Core's lock note, read the way core itself reads it: from byte 1. Byte 0 carries
/// the kernel lock, and Windows refuses to let another process read a locked byte.
/// None = no file.
pub(crate) fn read_lock_note(path: &std::path::Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
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
    // stale one: the handshake alone still finds a Sapphire Launch or the shim started
    // — on this install: a handshake from another checkout is not about this one.
    let r = run.filter(|r| r.install.as_deref().is_none_or(|i| same_install(i, install)))?;
    hers(r.pid).then(|| Instance { pid: r.pid, since: String::new(), origin: r.origin, log: r.log })
}

/// Is a Sapphire running on this install? Reads two small files and checks a pid;
/// never spawns a process, so it is cheap enough to poll. None = nobody we can
/// vouch for (an old core started elsewhere is still possible; the caller's web
/// port probe covers that).
pub(crate) fn find_instance(install: &str) -> Option<Instance> {
    let locks: Vec<PathBuf> = instance_lock_path(install).into_iter().chain([legacy_lock_path(install)]).collect();
    // A pid we may not look at counts only while a lock's byte is really held: after
    // a reboot a stale note's number can belong to a system process.
    let hers = |pid: u32| pid_look(pid).unwrap_or_else(|| locks.iter().any(|l| lock_is_held(l)));
    // The first note naming a live her (a stale new lock must not hide an old runner).
    let note = locks.iter().filter_map(|l| read_lock_note(l))
        .find(|n| parse_lock_note(n).is_some_and(|(pid, _)| hers(pid)));
    let run = read_run_state(&run_state_path()).filter(|r| !handshake_is_stale(r.started, boot_time()));
    resolve_instance(note.as_deref(), run, hers, install)
}

/// Is the lock's byte 0 held? Windows refuses a read of a byte another process has
/// locked (ERROR_LOCK_VIOLATION), so this asks without ever taking the lock, which
/// could make a booting Sapphire exit 43.
#[cfg(windows)]
pub(crate) fn lock_is_held(path: &std::path::Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else { return false };
    f.read(&mut [0u8; 1]).is_err_and(|e| e.raw_os_error() == Some(33))
}
/// Linux never answers "can't tell" (see `pid_look`), so this is never asked there.
#[cfg(not(windows))]
pub(crate) fn lock_is_held(_path: &std::path::Path) -> bool {
    false
}

/// Alive and hers, or possibly so: what a Stop in progress must keep waiting on, and
/// the check before any signal.
pub(crate) fn pid_is_hers(pid: u32) -> bool {
    pid_look(pid) != Some(false)
}

/// Is `pid` alive and a sapphire-env python running her? Linux: /proc gives the
/// executable (a gone or zombie pid has no exe link) and the command line. The
/// identity half is what makes a recycled pid harmless: nothing that fails it is
/// ever signalled.
#[cfg(not(windows))]
pub(crate) fn pid_look(pid: u32) -> Option<bool> {
    if pid <= 1 {
        return Some(false);
    }
    let Ok(exe) = std::fs::read_link(format!("/proc/{}/exe", pid)) else { return Some(false) };
    if !exe.to_string_lossy().contains("/envs/sapphire/") {
        return Some(false);
    }
    let Ok(cmd) = std::fs::read(format!("/proc/{}/cmdline", pid)) else { return Some(false) };
    let cmd = String::from_utf8_lossy(&cmd);
    Some(cmd.contains("main.py") || cmd.contains("sapphire.py"))
}

/// Is `pid` alive and a sapphire-env python? Windows: open the process for a look
/// (no tasklist/CIM spawn), check it has not exited, read its image path. A pid we
/// may not open or name (she runs elevated, or it is a system process now) is None:
/// we can't tell, and the caller decides what that means.
#[cfg(windows)]
pub(crate) fn pid_look(pid: u32) -> Option<bool> {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ACCESS_DENIED, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    if pid <= 4 {
        return Some(false); // the idle/system pseudo-pids
    }
    // SAFETY: plain Win32 calls; the handle is closed on every path and the name
    // buffer's length is passed in and out exactly as the API requires.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return (GetLastError() != ERROR_ACCESS_DENIED).then_some(false);
        }
        let mut code = 0u32;
        let alive = GetExitCodeProcess(h, &mut code) != 0 && code == STILL_ACTIVE as u32;
        let mut name = [0u16; 1024];
        let mut len = name.len() as u32;
        let named = QueryFullProcessImageNameW(h, 0, name.as_mut_ptr(), &mut len) != 0;
        CloseHandle(h);
        if !alive {
            return Some(false);
        }
        if !named {
            return None;
        }
        Some(String::from_utf16_lossy(&name[..len as usize]).to_ascii_lowercase().contains("\\envs\\sapphire\\"))
    }
}

/// The executable `pid` runs, if it is alive and we may look. The identity check for
/// the local LLM server (its binary is ours, under the per-user `llm/bin/`).
#[cfg(not(windows))]
pub(crate) fn pid_exe(pid: u32) -> Option<PathBuf> {
    if pid <= 1 {
        return None;
    }
    // A zombie or gone pid has no exe link.
    std::fs::read_link(format!("/proc/{}/exe", pid)).ok()
}

#[cfg(windows)]
pub(crate) fn pid_exe(pid: u32) -> Option<PathBuf> {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    if pid <= 4 {
        return None;
    }
    // SAFETY: as in pid_look — the handle is closed on every path, the buffer length
    // is passed in and out as the API requires.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let mut code = 0u32;
        let alive = GetExitCodeProcess(h, &mut code) != 0 && code == STILL_ACTIVE as u32;
        let mut name = [0u16; 1024];
        let mut len = name.len() as u32;
        let named = QueryFullProcessImageNameW(h, 0, name.as_mut_ptr(), &mut len) != 0;
        CloseHandle(h);
        (alive && named).then(|| PathBuf::from(String::from_utf16_lossy(&name[..len as usize])))
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

/// Per-user data dir, for big files that belong to no one install (the local LLM's
/// engine and models): Windows %LOCALAPPDATA%\Sapphire (never roaming), Linux
/// $XDG_DATA_HOME/sapphire or ~/.local/share/sapphire.
pub(crate) fn app_data_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Ok(local) = std::env::var("LOCALAPPDATA")
            && !local.is_empty()
        {
            return PathBuf::from(local).join("Sapphire");
        }
        PathBuf::from(std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into()))
            .join("AppData").join("Local").join("Sapphire")
    }
    #[cfg(not(windows))]
    {
        if let Ok(xdg) = std::env::var("XDG_DATA_HOME")
            && !xdg.is_empty()
        {
            return PathBuf::from(xdg).join("sapphire");
        }
        PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
            .join(".local").join("share").join("sapphire")
    }
}

/// Bytes free for this user on the drive holding `dir` (None = couldn't tell).
#[cfg(unix)]
pub(crate) fn free_bytes(dir: &std::path::Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: `c` is a valid NUL-terminated path and `st` is a zeroed out-parameter.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // The field widths differ between unixes; on x86-64 Linux both are already u64.
    #[allow(clippy::unnecessary_cast)]
    (unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0).then(|| st.f_bavail as u64 * st.f_frsize as u64)
}

#[cfg(windows)]
pub(crate) fn free_bytes(dir: &std::path::Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut free = 0u64;
    // SAFETY: `wide` is NUL-terminated; the two unused out-parameters may be null.
    let ok = unsafe { windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) };
    (ok != 0).then_some(free)
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
        // The console and the std handles belong to the whole process: two stops
        // overlapping here would detach each other's console and restore dead handles.
        static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _turn = ONE_AT_A_TIME.lock();
        // While attached, this process is on her console too. Ctrl+Break goes to her
        // process group, but a pid that leads no group (she was started by hand in a
        // terminal, or through `conda run`) may be treated as "everyone on this
        // console", and the default answer to Ctrl+Break is to exit: the launcher
        // would close on its own Stop. So the launcher answers every console event
        // itself with "handled". The null handler would only cover Ctrl+C.
        unsafe extern "system" fn handled(_event: u32) -> i32 {
            1
        }
        // SAFETY: plain Win32 calls against our own process; the only pointer argument
        // is `handled`, a function that lives as long as the program.
        unsafe {
            let std_ids = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE];
            let saved = std_ids.map(|id| GetStdHandle(id));
            FreeConsole();
            let mut sent = false;
            if AttachConsole(pid) != 0 {
                // Off, then on: registered exactly once however many stops there
                // were, and left on (the event arrives on its own thread, perhaps
                // after this function has returned).
                SetConsoleCtrlHandler(Some(handled), 0);
                SetConsoleCtrlHandler(Some(handled), 1);
                sent = GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) != 0;
                FreeConsole();
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

/// Hard stop of a pid we can name — the fallback when `signal_stop` can't be sent
/// or Sapphire ignores it: her tree / process group, precisely. Pid 0 names nobody
/// and does nothing here; with no pid to name, the caller asks for `sweep_sapphire`.
pub(crate) fn kill_process_tree(pid: u32) {
    if pid == 0 {
        return;
    }
    #[cfg(windows)]
    {
        let _ = hidden_cmd("taskkill").args(["/F", "/T", "/PID", &pid.to_string()]).output();
    }
    #[cfg(not(windows))]
    {
        // Her whole group (she leads one when we started her), child STT/TTS
        // workers too; the runner alone otherwise. Direct syscalls — see signal_group.
        if !signal_group(pid, libc::SIGKILL) {
            signal_one(pid, libc::SIGKILL);
        }
    }
}

/// Every process that is her, by the one test used everywhere (`pid_look`: her
/// env's python, running main.py or sapphire.py), read from /proc. Never this
/// process, never pid 1, and never another user's (their /proc entries don't open).
#[cfg(not(windows))]
pub(crate) fn her_pids() -> Vec<u32> {
    let me = std::process::id();
    let Ok(procs) = std::fs::read_dir("/proc") else { return Vec::new() };
    procs
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| *pid > 1 && *pid != me && pid_look(*pid) == Some(true))
        .collect()
}

/// Stop a Sapphire nobody can name (an old core started elsewhere, found on the web
/// port only), the last resort. `force` = she ignored the polite ask.
///
/// Linux: each of `her_pids`, one direct signal each, and `signal_one` checks the
/// pid again at the moment of sending. Not `pkill -f <pattern>`: that signalled
/// anything whose command line merely contained her env's path (an editor on a file
/// in it, a `watch`, a debugger) and could never escalate.
/// Windows: every python in her env by its image path, a user's own script in that
/// env too, which is why the window asks first.
pub(crate) fn sweep_sapphire(force: bool) {
    #[cfg(windows)]
    {
        let _ = force; // Stop-Process is the only tool here, and it is always a hard stop
        // BOTH python.exe AND pythonw.exe (an old autostart shim runs her under
        // pythonw). Matching by an env-path substring covers the miniconda3/anaconda3
        // variants. This ends main.py too, so its supervisor can't relaunch
        // sapphire.py. CIM, not the deprecated wmic (gone by default on Win11 24H2+).
        let ps = "Get-CimInstance Win32_Process -Filter \"Name='python.exe' OR Name='pythonw.exe'\" | Where-Object { $_.ExecutablePath -like '*\\envs\\sapphire\\*' } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }";
        let _ = hidden_cmd("powershell").args(["-NoProfile", "-Command", ps]).output();
    }
    #[cfg(not(windows))]
    for pid in her_pids() {
        signal_one(pid, if force { libc::SIGKILL } else { libc::SIGTERM });
    }
}

/// Windows shows its own error boxes for a program that can't load a DLL or that
/// crashes, and the program waits behind the box until someone clicks it. For
/// something started hidden (llama-server, a download's curl) that is a popup from
/// nowhere and a job that never ends. This turns them off for this process, and
/// every child inherits the setting: the failure comes back as an exit code, which
/// is said in the launcher's own words.
pub(crate) fn no_error_boxes() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Diagnostics::Debug::{SetErrorMode, SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SEM_NOOPENFILEERRORBOX};
        // SAFETY: takes flags, returns the old ones; cannot fail.
        unsafe {
            SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX);
        }
    }
}

/// Make this process's standard handles non-inheritable (Windows). A headless run
/// started with its output captured would otherwise pass the caller's pipe on to
/// every child it starts (handles are inherited wholesale there), and the caller
/// would wait for llama-server or a download to end before seeing end-of-output.
pub(crate) fn no_inherit_std() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT};
        use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
        // SAFETY: plain Win32 calls on this process's own handles; a null or invalid
        // handle makes the call fail harmlessly.
        unsafe {
            for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
                SetHandleInformation(GetStdHandle(id), HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

/// Hard-stop one process, but only if its pid still runs a program called `name`
/// (the curl a dead download worker left behind). Never a group, never pid 0 or 1:
/// a direct syscall on a positive pid whose program was just checked.
pub(crate) fn kill_if_named(pid: u32, name: &str) -> bool {
    let Some(target) = i32::try_from(pid).ok().filter(|p| *p > 1) else { return false };
    let named = pid_exe(pid).is_some_and(|e| e.file_stem().is_some_and(|s| s.to_string_lossy().eq_ignore_ascii_case(name)));
    if !named {
        return false;
    }
    #[cfg(windows)]
    {
        let _ = target;
        hidden_cmd("taskkill").args(["/F", "/PID", &pid.to_string()]).output().is_ok_and(|o| o.status.success())
    }
    #[cfg(not(windows))]
    // SAFETY: plain syscall; `target` is > 1 and was just verified to run `name`.
    unsafe {
        libc::kill(target, libc::SIGKILL) == 0
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


/// What a command did. `code` is None when it was ended (its deadline, a signal).
#[derive(Debug, Default)]
pub(crate) struct Ran {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// Its deadline passed and it was ended: `stdout`/`stderr` hold what it had said.
    pub timed_out: bool,
}

impl Ran {
    pub(crate) fn ok(&self) -> bool {
        self.code == Some(0)
    }

    /// stdout then stderr, as one text for the log.
    pub(crate) fn both(&self) -> String {
        format!("{}{}", self.stdout, self.stderr).trim().to_string()
    }
}

/// The one way a command is run to completion: off the UI thread, no window, stdin
/// closed, and never left waiting on a prompt nobody can see. Err = it couldn't be
/// started.
///
/// The child is ours to end: when `limit` passes it is killed, and so it is when
/// the caller stops waiting (a `tokio::time::timeout` round this future, a dropped
/// Task). A timeout that walks away from a live `python` leaves it holding the
/// env's files, which is how a later pip fails on Windows.
pub(crate) async fn run(program: &str, args: &[String], limit: Option<std::time::Duration>) -> Result<Ran, String> {
    use tokio::io::AsyncReadExt;
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Conda's terms-of-service gate (2025-07) would stall `conda create` with no
        // terminal to say yes in; a git credentials prompt would hang forever.
        .env("CONDA_PLUGINS_AUTO_ACCEPT_TOS", "yes")
        .env("GIT_TERMINAL_PROMPT", "0")
        // Tools answer in English: a few of their sentences are still read here
        // (git's "would be overwritten"), and the log is what support is shown.
        // Messages only; the text encoding stays the user's.
        .env("LC_MESSAGES", "C")
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
        if let Some(path) = path_with_git() {
            cmd.env("PATH", path);
        }
    }
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let (mut so, mut se) = (child.stdout.take(), child.stderr.take());
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let work = async {
        let read_out = async {
            if let Some(s) = so.as_mut() {
                let _ = s.read_to_end(&mut out).await;
            }
        };
        let read_err = async {
            if let Some(s) = se.as_mut() {
                let _ = s.read_to_end(&mut err).await;
            }
        };
        tokio::join!(read_out, read_err, child.wait()).2
    };
    let status = match limit {
        Some(d) => tokio::time::timeout(d, work).await.ok(),
        None => Some(work.await),
    };
    let timed_out = status.is_none();
    if timed_out {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    let code = match status {
        Some(Ok(s)) => s.code(),
        Some(Err(e)) => return Err(e.to_string()),
        None => None,
    };
    Ok(Ran { code, stdout: String::from_utf8_lossy(&out).into_owned(), stderr: String::from_utf8_lossy(&err).into_owned(), timed_out })
}

/// A command's stdout when it succeeds; else its stderr (or its exit code).
pub(crate) async fn run_cmd_async(program: String, args: Vec<String>) -> Result<String, String> {
    let ran = run(&program, &args, None).await?;
    let stderr = ran.stderr.trim();
    if ran.ok() {
        Ok(ran.stdout.trim().to_string())
    } else if stderr.is_empty() {
        Err(format!("Exit code {}", ran.code.unwrap_or(-1)))
    } else {
        Err(stderr.to_string())
    }
}

/// A command's stdout+stderr, for the log: Ok when it succeeded, Err when it didn't.
pub(crate) async fn run_cmd_full_async(program: String, args: Vec<String>) -> Result<String, String> {
    let ran = run(&program, &args, None).await?;
    if ran.ok() { Ok(ran.both()) } else { Err(ran.both()) }
}

/// Is anything listening on this local port? A plain connect, no process: what the
/// idle polls ask first, so a closed port costs nothing more.
pub(crate) fn port_open(port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(300)).is_ok()
}

/// `GET http://127.0.0.1:<port><path>`, in-process, and the reply's body. None =
/// nothing answered within `secs`. Plain HTTP only (the LLM's own server); enough
/// of the protocol for a small JSON reply, which is all that is asked of it.
pub(crate) fn local_get(port: u16, path: &str, secs: u64) -> Option<String> {
    use std::io::{Read, Write};
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let limit = std::time::Duration::from_secs(secs);
    let mut stream = std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(300)).ok()?;
    stream.set_read_timeout(Some(limit)).ok()?;
    stream.set_write_timeout(Some(limit)).ok()?;
    // One write, so the request leaves as one piece: a server that answers and
    // closes after its first read would otherwise reset the connection under us.
    let request = format!("GET {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAccept: application/json\r\nConnection: close\r\n\r\n", path, port);
    stream.write_all(request.as_bytes()).ok()?;
    let mut reply = Vec::new();
    // A reply cut short by the timeout still counts for what arrived.
    let _ = stream.take(1 << 20).read_to_end(&mut reply);
    let reply = String::from_utf8_lossy(&reply);
    let (_, body) = reply.split_once("\r\n\r\n")?;
    Some(body.to_string())
}

// ── Graphics cards ─────────────────────────────────────────────────────────

/// One NVIDIA card, as nvidia-smi describes it. The one reader for the torch
/// step, the LLM's build choice and the Troubleshoot line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Card {
    pub name: String,
    /// Total memory in MiB (0 = the driver wouldn't say).
    pub vram_mib: u64,
    /// The driver's major version: 581 of "581.15".
    pub driver: u32,
    /// Compute capability x10: 75 for 7.5 (None = a driver too old to report it).
    pub cap10: Option<u32>,
}

/// What nvidia-smi is asked, newest first: an older driver rejects `compute_cap`.
const CARD_ASKS: [&str; 2] = ["name,memory.total,driver_version,compute_cap", "name,memory.total,driver_version"];

/// One line per card from either ask in `CARD_ASKS` (csv, no header, no units).
/// A row counts only when its driver version reads as one, so nvidia-smi's own
/// complaints ("has failed because...", "not a valid field") are never a card.
pub(crate) fn parse_cards(out: &str) -> Vec<Card> {
    out.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(',').map(str::trim).collect();
            if !(3..=4).contains(&f.len()) || f[0].is_empty() {
                return None;
            }
            let driver = f[2].split('.').next()?.parse().ok()?;
            let cap10 = f.get(3).and_then(|c| {
                let (maj, min) = c.split_once('.')?;
                Some(maj.parse::<u32>().ok()? * 10 + min.parse::<u32>().ok()?.min(9))
            });
            Some(Card { name: f[0].to_string(), vram_mib: f[1].parse().unwrap_or(0), driver, cap10 })
        })
        .collect()
}

/// Every NVIDIA card nvidia-smi can see; empty = none, no driver, or a driver that
/// didn't answer. Each ask gets fifteen seconds and is then ended, and a wedged
/// driver is not asked twice.
pub(crate) async fn nvidia_cards() -> Vec<Card> {
    for fields in CARD_ASKS {
        let args = [format!("--query-gpu={}", fields), "--format=csv,noheader,nounits".to_string()];
        match run("nvidia-smi", &args, Some(std::time::Duration::from_secs(15))).await {
            Ok(ran) if ran.timed_out => break,
            Ok(ran) if ran.ok() => {
                let cards = parse_cards(&ran.stdout);
                if !cards.is_empty() {
                    return cards;
                }
            }
            _ => {}
        }
    }
    Vec::new()
}

/// The card everything is chosen for: the newest generation, then the most memory,
/// whatever slot it sits in.
pub(crate) fn best_card(cards: &[Card]) -> Option<&Card> {
    cards.iter().max_by_key(|c| (c.cap10.unwrap_or(0), c.vram_mib))
}

/// Where git is, remembered once found: this is asked before every git call, and
/// looking means a process (`which`, `where`) each time. Not found is not
/// remembered: Go may install it a minute from now.
pub(crate) fn find_git() -> String {
    static FOUND: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    if let Ok(found) = FOUND.lock()
        && let Some(p) = found.as_ref().filter(|p| PathBuf::from(p).exists())
    {
        return p.clone();
    }
    let p = locate_git();
    if p != "git"
        && let Ok(mut found) = FOUND.lock()
    {
        *found = Some(p.clone());
    }
    p
}

/// Windows: the PATH to give a child, with Git's folder in it. None = it is there
/// already, or there is no git to add. winget installs Git during this very
/// session and our own PATH is from before that, so a child that looks git up by
/// name (pip, for a plugin fetched with `git+https`; Sapphire herself) would not
/// find the git that is now there.
#[cfg(windows)]
pub(crate) fn path_with_git() -> Option<std::ffi::OsString> {
    let git = PathBuf::from(find_git());
    let dir = git.parent().filter(|d| d.is_absolute())?.to_path_buf();
    let path = std::env::var_os("PATH").unwrap_or_default();
    if std::env::split_paths(&path).any(|p| p.as_os_str().eq_ignore_ascii_case(dir.as_os_str())) {
        return None;
    }
    std::env::join_paths(std::iter::once(dir).chain(std::env::split_paths(&path))).ok()
}

/// Find git executable on disk (Linux: rely on PATH).
#[cfg(not(windows))]
fn locate_git() -> String {
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
fn locate_git() -> String {
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

    #[cfg(unix)]
    #[tokio::test]
    async fn a_command_past_its_deadline_is_ended_and_keeps_what_it_said() {
        let limit = Some(std::time::Duration::from_millis(400));
        let started = std::time::Instant::now();
        let ran = run("sh", &["-c".to_string(), "echo $$; exec sleep 30".to_string()], limit).await.unwrap();
        assert!(ran.timed_out && ran.code.is_none() && !ran.ok());
        assert!(started.elapsed().as_secs() < 10, "it came back at the deadline, not at the sleep's end");
        let pid: u32 = ran.stdout.trim().parse().expect("what it printed before the deadline is kept");
        assert!(!std::path::Path::new(&format!("/proc/{}", pid)).exists(), "and it is gone, not left running");
        // The same when the caller is the one who stops waiting.
        let note = std::env::temp_dir().join(format!("sapphire-launcher-test-{}-runner.pid", std::process::id()));
        let _ = std::fs::remove_file(&note);
        let script = format!("echo $$ > '{}'; exec sleep 30", note.display());
        let waited = tokio::time::timeout(std::time::Duration::from_millis(400), run_cmd_full_async("sh".into(), vec!["-c".into(), script])).await;
        assert!(waited.is_err());
        let pid: u32 = std::fs::read_to_string(&note).unwrap().trim().parse().unwrap();
        for _ in 0..50 {
            if !std::path::Path::new(&format!("/proc/{}/cmdline", pid)).exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        }
        let left = std::fs::read_to_string(format!("/proc/{}/stat", pid)).unwrap_or_default();
        assert!(left.is_empty() || left.contains(") Z "), "a dropped wait ends its child too: {}", left);
        // And the plain shapes.
        assert_eq!(run_cmd_async("sh".into(), vec!["-c".into(), "echo out; echo err >&2".into()]).await, Ok("out".to_string()));
        assert_eq!(run_cmd_async("sh".into(), vec!["-c".into(), "echo err >&2; exit 3".into()]).await, Err("err".to_string()));
        assert_eq!(run_cmd_async("sh".into(), vec!["-c".into(), "exit 3".into()]).await, Err("Exit code 3".to_string()));
        assert_eq!(run_cmd_full_async("sh".into(), vec!["-c".into(), "echo out; echo err >&2; exit 1".into()]).await, Err("out\nerr".to_string()));
        assert!(run("no-such-program-anywhere", &[], None).await.is_err());
    }

    #[test]
    fn conda_says_where_the_env_is() {
        // Wherever conda put it: its own root, or the user's .conda when the root isn't theirs.
        let listed = r#"{"envs": ["/opt/conda", "/opt/conda/envs/other", "/home/k/.conda/envs/sapphire"]}"#;
        assert_eq!(conda_env_prefix(listed), Some(PathBuf::from("/home/k/.conda/envs/sapphire")));
        // A warning before the JSON, a path with a space and an accent, capitals.
        let odd = "WARNING: something\n{\"envs\": [\"/home/José Núñez/miniconda3\", \"/home/José Núñez/miniconda3/envs/Sapphire\"]}";
        assert_eq!(conda_env_prefix(odd), Some(PathBuf::from("/home/José Núñez/miniconda3/envs/Sapphire")));
        assert_eq!(conda_env_prefix(r#"{"envs": ["/opt/conda", "/opt/conda/envs/sapphire-old"]}"#), None);
        assert_eq!(conda_env_prefix("conda: command not found"), None);
        assert_eq!(conda_env_prefix(""), None);
    }

    #[test]
    fn a_file_is_written_whole_and_a_private_one_is_born_private() {
        let dir = std::env::temp_dir().join(format!("sapphire-launcher-test-{}-atomic", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("deep").join("keys.env");
        write_atomic(&file, "A=1\n", true).unwrap();
        write_atomic(&file, "A=2\n", true).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "A=2\n");
        let left: Vec<_> = std::fs::read_dir(file.parent().unwrap()).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left.len(), 1, "no tmp left behind: {:?}", left);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
            let open = dir.join("open.json");
            write_atomic(&open, "{}", false).unwrap();
            assert_ne!(std::fs::metadata(&open).unwrap().permissions().mode() & 0o077, 0, "an ordinary file keeps the usual permissions");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// NEVER run plainly: on a box with a live Sapphire this would stop her. It is
    /// for a dry run with the kill syscalls blocked, to read what would be sent:
    /// `strace -f -e trace=kill,tgkill,tkill,pidfd_send_signal -e inject=kill,tgkill,tkill,pidfd_send_signal:error=EPERM <test binary> --ignored --exact platform::tests::sweep_dry_run_under_strace_only`
    #[cfg(unix)]
    #[test]
    #[ignore = "signals real processes: strace-inject only"]
    fn sweep_dry_run_under_strace_only() {
        assert_eq!(std::env::var("SAPPHIRE_SWEEP_DRY_RUN").as_deref(), Ok("kill-is-blocked"), "refusing: see the comment above");
        eprintln!("her_pids = {:?}", her_pids());
        sweep_sapphire(false);
        sweep_sapphire(true);
    }

    #[cfg(unix)]
    #[test]
    fn the_sweep_names_only_her_own_processes() {
        // Read-only: who WOULD be swept. Never this test, never init, and nothing
        // that merely mentions her env's path on its command line.
        let dir = std::env::temp_dir().join(format!("sapphire-launcher-test-{}-sweep", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut mention = std::process::Command::new("sleep").args(["30", "/home/x/miniconda3/envs/sapphire/bin/python", "main.py"])
            .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
            .spawn().unwrap();
        let swept = her_pids();
        let _ = mention.kill();
        let _ = mention.wait();
        assert!(!swept.contains(&mention.id()), "its command line names her python, but it is `sleep`");
        assert!(!swept.contains(&std::process::id()) && !swept.contains(&1));
        for pid in swept {
            assert!(pid_look(pid).unwrap_or(true), "only pids that pass the identity test (or went away since)");
        }
    }

    #[test]
    fn nvidia_smi_rows_become_cards() {
        // A current driver: name, memory, driver, capability.
        let two = "NVIDIA GeForce GTX 1080, 8192, 581.15, 6.1\nNVIDIA GeForce RTX 5090, 32607, 581.15, 12.0\n";
        let cards = parse_cards(two);
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[1], Card { name: "NVIDIA GeForce RTX 5090".into(), vram_mib: 32607, driver: 581, cap10: Some(120) });
        // The newest card wins, whatever slot it is in; memory breaks a tie.
        assert_eq!(best_card(&cards).map(|c| c.name.as_str()), Some("NVIDIA GeForce RTX 5090"));
        let twins = parse_cards("RTX 3060, 12288, 581.15, 8.6\nRTX 3090, 24576, 581.15, 8.6\nRTX 3060, 12288, 581.15, 8.6");
        assert_eq!(best_card(&twins).map(|c| c.name.as_str()), Some("RTX 3090"));
        assert_eq!(parse_cards("NVIDIA GeForce RTX 2060, 6144, 560.94, 7.5")[0].cap10, Some(75));
        // An older driver has no compute_cap column: still a card, capability unknown.
        assert_eq!(parse_cards("NVIDIA GeForce GTX 970, 4096, 472.12"), vec![Card { name: "NVIDIA GeForce GTX 970".into(), vram_mib: 4096, driver: 472, cap10: None }]);
        // Memory the driver won't give doesn't lose the card.
        assert_eq!(parse_cards("Tesla T4, [N/A], 550.54, 7.5")[0].vram_mib, 0);
        // nvidia-smi's own complaints are never a card.
        for noise in [
            "",
            "No devices were found",
            "Field \"compute_cap\" is not a valid field to query.",
            "NVIDIA-SMI has failed because it couldn't communicate with the NVIDIA driver. Make sure that the latest NVIDIA driver is installed and running.",
            "Unable to determine the device handle for GPU0000:01:00.0: Unknown Error, try again, later",
        ] {
            assert_eq!(parse_cards(noise), vec![], "{}", noise);
        }
        assert_eq!(best_card(&[]), None);
    }

    #[test]
    fn kill_if_named_only_ever_touches_the_named_program() {
        // This test process is not curl; pid 0 and 1 are never anyone's to end.
        assert!(!kill_if_named(std::process::id(), "curl"));
        assert!(!kill_if_named(0, "curl") && !kill_if_named(1, "systemd") && !kill_if_named(u32::MAX, "curl"));
    }

    #[test]
    fn data_dir_is_per_user_and_not_the_config_dir() {
        let d = app_data_dir();
        assert!(d.ends_with(if cfg!(windows) { "Sapphire" } else { "sapphire" }), "{}", d.display());
        assert_ne!(d, app_config_dir());
    }

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
        let lock = legacy_lock_path(&install);
        assert_eq!(read_lock_note(&lock), None, "no file");
        std::fs::create_dir_all(d.join("user")).unwrap();
        std::fs::write(&lock, b"\npid=4242 since=2026-10-20 08:01:02\n").unwrap();
        assert_eq!(parse_lock_note(&read_lock_note(&lock).unwrap()), Some((4242, "2026-10-20 08:01:02".into())));
    }

    #[test]
    fn lock_key_matches_core() {
        // hashlib.sha256(path.encode()).hexdigest()[:16], the formula in
        // core/single_instance.py. The first is a real lock file core wrote on Krem's box.
        assert_eq!(lock_key("/home/bander/sapphire/sapphire/user"), "2e3c5a5b5ad02f2f");
        assert_eq!(lock_key(&windows_normcase(r"C:\Users\Krem Bander\sapphire\user")), "344c39081385b5fc");
        assert_eq!(windows_normcase(r"\\?\C:\Users/X"), r"c:\users\x");
        assert_eq!(windows_normcase(r"\\?\UNC\Box\Share"), r"\\box\share");
    }

    #[test]
    fn the_lock_is_in_the_config_dir_named_by_the_user_folder() {
        let d = scratch("lockpath");
        let install = d.to_string_lossy().into_owned();
        assert_eq!(instance_lock_path(&install), None, "no user/ folder yet");
        std::fs::create_dir_all(d.join("user")).unwrap();
        let p = instance_lock_path(&install).unwrap();
        assert!(p.starts_with(app_config_dir().join("locks")), "{:?}", p);
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("instance-") && name.ends_with(".lock") && name.len() == "instance-.lock".len() + 16, "{}", name);
    }

    #[test]
    fn a_handshake_from_before_this_boot_is_stale() {
        assert!(handshake_is_stale(1000, Some(2000)));
        assert!(!handshake_is_stale(2000, Some(1000)));
        assert!(!handshake_is_stale(998, Some(1000)), "clock jitter");
        assert!(!handshake_is_stale(0, Some(1000)), "no timestamp: the pid check decides");
        assert!(!handshake_is_stale(1000, None));
        let boot = boot_time().unwrap();
        assert!(boot > 0 && boot <= now_secs());
    }

    #[test]
    fn a_handshake_for_another_install_is_not_about_this_one() {
        let st = |install: Option<&str>| Some(RunState { pid: 7, log: PathBuf::from("/l"), origin: Origin::Launcher, install: install.map(str::to_string), started: 1 });
        assert_eq!(resolve_instance(None, st(Some("/inst/a")), |_| true, "/inst/b"), None);
        assert!(resolve_instance(None, st(Some("/inst/a")), |_| true, "/inst/a").is_some());
        assert!(resolve_instance(None, st(None), |_| true, "/inst/b").is_some(), "a v3 shim names no install");
    }

    /// Real Windows only: the probe sees another handle's lock on byte 0
    /// and the note after it stays readable.
    #[cfg(windows)]
    #[test]
    fn a_locked_byte_reads_as_held_without_taking_it() {
        use std::os::windows::io::AsRawHandle;
        unsafe extern "system" {
            fn LockFile(h: *mut core::ffi::c_void, lo: u32, hi: u32, nlo: u32, nhi: u32) -> i32;
        }
        // Wine keeps LockFile between handles but does not make reads honor it.
        if std::path::Path::new(r"C:\windows\system32\winemenubuilder.exe").exists() {
            eprintln!("skipped: Wine does not enforce byte locks on reads");
            return;
        }
        let p = scratch("held").join("x.lock");
        std::fs::write(&p, b"\npid=4242 since=2026-10-20 08:01:02\n").unwrap();
        assert!(!lock_is_held(&p), "nobody holds it");
        let f = std::fs::OpenOptions::new().read(true).write(true).open(&p).unwrap();
        assert!(unsafe { LockFile(f.as_raw_handle(), 0, 0, 1, 0) } != 0);
        assert!(lock_is_held(&p));
        assert_eq!(parse_lock_note(&read_lock_note(&p).unwrap()).map(|n| n.0), Some(4242));
        drop(f);
        assert!(!lock_is_held(&p));
    }

    #[test]
    fn run_state_round_trips() {
        let p = scratch("run").join("deep").join("sapphire-run.json");
        let st = RunState {
            pid: 31337,
            log: PathBuf::from(r#"C:\Users\Krem's "lab"\AppData\Roaming\Sapphire\sapphire-launcher.log"#),
            origin: Origin::Autostart,
            install: Some(r"C:\Users\Krem\sapphire".into()),
            started: 1234,
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
        let ours = || Some(RunState { pid: 100, log: PathBuf::from("/cfg/sapphire-launcher.log"), origin: Origin::Launcher, install: Some("/inst".into()), started: 1 });
        let core_log = core_log_path("/inst");
        // Lock and handshake agree: ours, our log, core's since.
        let i = resolve_instance(note, ours(), |p| p == 100, "/inst").unwrap();
        assert_eq!(i, Instance { pid: 100, since: "2026-10-20 08:01:02".into(), origin: Origin::Launcher, log: PathBuf::from("/cfg/sapphire-launcher.log") });
        // Lock only (a terminal start): elsewhere, core's log.
        let i = resolve_instance(note, None, |p| p == 100, "/inst").unwrap();
        assert_eq!((i.origin, &i.log), (Origin::Elsewhere, &core_log));
        // A handshake left by an older run names another pid: the lock wins.
        let old = Some(RunState { pid: 55, log: PathBuf::from("/cfg/old.log"), origin: Origin::Launcher, install: None, started: 1 });
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
