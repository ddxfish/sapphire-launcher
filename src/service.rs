// systemd --user service detection + log/launch streams.
use crate::*;
use crate::platform::*;
use std::sync::Arc;
use tokio::io::BufReader;

// Service mode is Linux-only today (Windows detect returns None), so on Windows the
// fields the Autostart view reads are never read.
#[cfg_attr(windows, allow(dead_code))]
#[derive(Debug, Clone)]
pub(crate) struct ServiceInfo {
    pub(crate) active: bool,            // ActiveState == "active" (running now)
    pub(crate) active_state: String,    // raw ActiveState (active/failed/activating/inactive)
    pub(crate) enabled: bool,           // UnitFileState == "enabled" (autostart at sign-in)
    pub(crate) sub_state: String,
    pub(crate) working_dir: Option<String>,
    pub(crate) env_file: Option<String>,    // first EnvironmentFile= (what the env editor edits)
}

/// Detect a systemd --user `sapphire.service`. Returns None if no such unit
/// (or on Windows, or if systemctl is absent). This is what flips the launcher
/// into "service mode" — Launch/Stop drive the unit instead of spawning python.
#[cfg(not(windows))]
pub(crate) fn detect_sapphire_service() -> Option<ServiceInfo> {
    let out = std::process::Command::new("systemctl")
        .args([
            "--user", "show", "sapphire",
            "-p", "LoadState", "-p", "ActiveState", "-p", "SubState",
            "-p", "UnitFileState", "-p", "WorkingDirectory", "-p", "EnvironmentFiles",
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (mut load, mut active, mut sub, mut ufs, mut wd, mut envf) = ("", "", "", "", "", "");
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("LoadState=") { load = v; }
        else if let Some(v) = line.strip_prefix("ActiveState=") { active = v; }
        else if let Some(v) = line.strip_prefix("SubState=") { sub = v; }
        else if let Some(v) = line.strip_prefix("UnitFileState=") { ufs = v; }
        else if let Some(v) = line.strip_prefix("WorkingDirectory=") { wd = v; }
        else if let Some(v) = line.strip_prefix("EnvironmentFiles=") && envf.is_empty() { envf = v; }
    }
    if load != "loaded" {
        return None; // unit doesn't exist
    }
    Some(ServiceInfo {
        active: active == "active",
        active_state: active.to_string(),
        enabled: ufs == "enabled",
        sub_state: sub.to_string(),
        working_dir: if wd.is_empty() { None } else { Some(wd.to_string()) },
        env_file: env_file_from_show(envf),
    })
}

/// `EnvironmentFiles=/home/k/.config/sapphire/env (ignore_errors=no)` → the path.
/// A hand-written unit names whatever file it likes; the editor must edit THAT one.
#[cfg_attr(windows, allow(dead_code))]
pub(crate) fn env_file_from_show(value: &str) -> Option<String> {
    let path = value.split(" (ignore_errors=").next().unwrap_or(value).trim().trim_start_matches('-');
    (!path.is_empty()).then(|| path.to_string())
}

// Windows: stays None until the control seam (ServiceStart/Stop/Restart) and the
// logfile-tail follower are wired — returning Some would flip the app into service
// mode and route Launch through the still-systemctl ServiceStart, breaking launch.
// The schtasks-query / wrapper-file detection is drafted in install_service's notes.
#[cfg(windows)]
pub(crate) fn detect_sapphire_service() -> Option<ServiceInfo> {
    None
}

/// Is a systemd --user instance available? (No on Windows, non-systemd distros,
/// some containers/WSL.) Gates the "Enable service" button.
#[cfg(not(windows))]
pub(crate) fn systemd_user_available() -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "show-environment"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
#[cfg(windows)]
pub(crate) fn systemd_user_available() -> bool { false }

/// Create + enable the systemd --user unit so Sapphire starts at sign-in.
/// Runs the env python directly (no wrapper); references an optional EnvironmentFile
/// so the env-vars textarea can drop keys in later without touching the unit.
#[cfg(not(windows))]
pub(crate) async fn install_service(install_path: String) -> (String, bool) {
    let Some(py) = sapphire_env_python() else {
        return ("Couldn't find the Sapphire environment's Python — install Sapphire first.".to_string(), false);
    };
    let home = std::env::var("HOME").unwrap_or_default();
    let unit_dir = format!("{}/.config/systemd/user", home);
    let env_file = format!("{}/.config/sapphire/service.env", home);
    if let Err(e) = tokio::fs::create_dir_all(&unit_dir).await {
        return (format!("Couldn't create {}: {}", unit_dir, e), false);
    }
    // 43 = another Sapphire already holds this user/ (lock or port), 44 = config
    // error: a restart would just loop, so neither counts as a failure to retry.
    // 120 s stop timeout: longer than the runner's own 90 s child-kill, so systemd
    // never SIGKILLs a stop that is still winding down.
    let unit = format!(
        "[Unit]\n\
         Description=Sapphire AI\n\
         After=network.target\n\n\
         [Service]\n\
         Type=simple\n\
         WorkingDirectory={wd}\n\
         ExecStart={py} main.py\n\
         Restart=on-failure\n\
         RestartSec=3\n\
         RestartPreventExitStatus=43 44\n\
         TimeoutStopSec=120\n\
         Environment=PYTHONUNBUFFERED=1\n\
         EnvironmentFile=-{env}\n\n\
         [Install]\n\
         WantedBy=default.target\n",
        wd = install_path, py = py, env = env_file,
    );
    let unit_path = format!("{}/sapphire.service", unit_dir);
    if let Err(e) = tokio::fs::write(&unit_path, unit).await {
        return (format!("Couldn't write {}: {}", unit_path, e), false);
    }
    let _ = run_cmd_full_async("systemctl".into(), vec!["--user".into(), "daemon-reload".into()]).await;
    match run_cmd_full_async("systemctl".into(), vec!["--user".into(), "enable".into(), "--now".into(), "sapphire".into()]).await {
        Ok(_) => ("Service created and started. Sapphire will start when you sign in.".to_string(), true),
        Err(e) => (format!("Unit written, but enabling it failed: {}", e), false),
    }
}
/// Windows: enable autostart. Registers a per-user logon Scheduled Task that runs our
/// shim under the env's windowless pythonw (no console window), with the install dir
/// as the working directory. We use PowerShell `Register-ScheduledTask` (not bare
/// `schtasks.exe`) so we control the settings `schtasks` defaults wrong:
///   • trigger scoped to THIS user's logon (not "any user")
///   • runs on battery and doesn't stop on battery (laptop-friendly)
///   • no execution time limit (`schtasks` defaults to stop after 3 days)
/// Registration needs admin, so we elevate a generated `.ps1` via `Start-Process
/// -Verb RunAs` (one UAC prompt). The trigger/principal user is captured from OUR
/// (non-elevated) process, so it's correct even if UAC elevates a different admin.
#[cfg(windows)]
pub(crate) async fn install_service(install_path: String) -> (String, bool) {
    let Some(pyw) = sapphire_pythonw() else {
        return ("Couldn't find Sapphire's environment (pythonw.exe) — install Sapphire first.".to_string(), false);
    };
    let user = format!(
        "{}\\{}",
        std::env::var("USERDOMAIN").unwrap_or_default(),
        std::env::var("USERNAME").unwrap_or_default(),
    );

    // Write the launch shim the task runs (instead of `pythonw main.py` directly):
    // it starts her the way Launch does — detached, hidden, own process group, output
    // to our log file, handshake written. See platform::autostart_shim_path.
    let shim = autostart_shim_path();
    let log = launch_log_path();
    if let Some(parent) = shim.parent()
        && let Err(e) = tokio::fs::create_dir_all(parent).await {
            return (format!("Couldn't create the autostart folder: {}", e), false);
        }
    if let Err(e) = tokio::fs::write(&shim, shim_source(&install_path, &log.to_string_lossy(), &run_state_path().to_string_lossy())).await {
        return (format!("Couldn't write the autostart launcher: {}", e), false);
    }

    let esc = |s: &str| s.replace('\'', "''"); // PowerShell single-quote escaping

    // Registration script — normal PS single-quoted strings we control (dodges the
    // nested-ArgumentList quoting hell of passing this inline to an elevated shell).
    // The action runs pythonw on the shim; the shim path is wrapped in literal double
    // quotes inside the argument so a path with spaces stays a single argument. The
    // elevated window is hidden and its stderr can't be captured across the UAC
    // boundary, so the script writes its own error to a file we read back.
    let err_file = std::env::temp_dir().join("sapphire-enable.err");
    let _ = tokio::fs::remove_file(&err_file).await;
    let script = format!(
        "$ErrorActionPreference='Stop'\r\n\
         try {{\r\n\
         $a = New-ScheduledTaskAction -Execute '{pyw}' -Argument '\"{shim}\"' -WorkingDirectory '{cwd}'\r\n\
         $t = New-ScheduledTaskTrigger -AtLogOn -User '{user}'\r\n\
         $p = New-ScheduledTaskPrincipal -UserId '{user}' -LogonType Interactive\r\n\
         $s = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero) -StartWhenAvailable\r\n\
         Register-ScheduledTask -TaskName Sapphire -Action $a -Trigger $t -Principal $p -Settings $s -Force | Out-Null\r\n\
         }} catch {{ $_ | Out-String | Set-Content -Path '{err}' -Encoding UTF8; exit 1 }}\r\n",
        pyw = esc(&pyw), shim = esc(&shim.to_string_lossy()), cwd = esc(&install_path), user = esc(&user),
        err = esc(&err_file.to_string_lossy()),
    );
    let ps1 = std::env::temp_dir().join("sapphire-enable.ps1");
    // With a BOM: Windows PowerShell reads a BOM-less .ps1 as ANSI and mangles a
    // non-ASCII user name or path into a task that can never run.
    if let Err(e) = tokio::fs::write(&ps1, format!("\u{feff}{}", script)).await {
        return (format!("Couldn't write the setup script: {}", e), false);
    }
    // Elevate (UAC) and run the .ps1. Start-Process joins -ArgumentList with spaces
    // and no quoting, so the path carries its own double quotes (a user named
    // "Krem Bander" has a space in %TEMP%).
    let outer = format!(
        "$ErrorActionPreference='Stop'; try {{ $p = Start-Process powershell -Verb RunAs -WindowStyle Hidden -PassThru -Wait -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File','\"{}\"'; exit $p.ExitCode }} catch {{ exit 1223 }}",
        ps1.display().to_string().replace('\'', "''"),
    );
    match run_cmd_full_async("powershell".into(), vec!["-NoProfile".into(), "-Command".into(), outer]).await {
        Ok(_) => {
            // No-admin "enabled" marker (detection avoids querying the task).
            let marker = autostart_marker_path();
            if let Some(parent) = marker.parent() { let _ = tokio::fs::create_dir_all(parent).await; }
            let _ = tokio::fs::write(marker, "on").await;
            ("Autostart enabled — Sapphire will start when you sign in.".to_string(), true)
        }
        Err(_) => {
            let detail = tokio::fs::read_to_string(&err_file).await.ok()
                .map(|t| strip_bom(&t).trim().lines().next().unwrap_or("").to_string())
                .filter(|t| !t.is_empty());
            match detail {
                Some(d) => (format!("Couldn't enable autostart: {}", d), false),
                None => ("Couldn't enable autostart — the admin prompt may have been declined. Try again and choose Yes.".to_string(), false),
            }
        }
    }
}

/// Stop, disable, back up (.bak), and delete the systemd --user unit.
#[cfg(not(windows))]
pub(crate) async fn remove_service() -> (String, bool) {
    let home = std::env::var("HOME").unwrap_or_default();
    let unit_path = format!("{}/.config/systemd/user/sapphire.service", home);
    let _ = run_cmd_full_async("systemctl".into(), vec!["--user".into(), "disable".into(), "--now".into(), "sapphire".into()]).await;
    if tokio::fs::metadata(&unit_path).await.is_ok() {
        let _ = tokio::fs::copy(&unit_path, format!("{}.bak", unit_path)).await;
        if let Err(e) = tokio::fs::remove_file(&unit_path).await {
            return (format!("Disabled, but couldn't delete {}: {}", unit_path, e), false);
        }
    }
    let _ = run_cmd_full_async("systemctl".into(), vec!["--user".into(), "daemon-reload".into()]).await;
    ("Service stopped, disabled, and removed (kept a .bak copy).".to_string(), true)
}
/// Windows: disable autostart. Unregisters the logon task (admin → UAC) and clears our
/// "enabled" marker.
#[cfg(windows)]
pub(crate) async fn remove_service() -> (String, bool) {
    // Inner command: if the task exists, unregister it with -ErrorAction Stop so a real
    // failure becomes a non-zero exit (→ Err → marker kept). If it's already gone, do
    // nothing and exit 0 (idempotent success). This makes "removed" mean it's truly gone,
    // matching the marker-clear comment below — not just "UAC wasn't declined".
    let outer = "$ErrorActionPreference='Stop'; try { $p = Start-Process powershell -Verb RunAs -WindowStyle Hidden -PassThru -Wait -ArgumentList '-NoProfile','-Command','if (Get-ScheduledTask -TaskName Sapphire -ErrorAction SilentlyContinue) { Unregister-ScheduledTask -TaskName Sapphire -Confirm:$false -ErrorAction Stop }'; exit $p.ExitCode } catch { exit 1223 }".to_string();
    let result = run_cmd_full_async("powershell".into(), vec!["-NoProfile".into(), "-Command".into(), outer]).await;
    match result {
        Ok(_) => {
            // Only clear the "enabled" marker once the task is actually gone — a
            // declined UAC must NOT flip the UI to "off" while the task lives on.
            let _ = tokio::fs::remove_file(autostart_marker_path()).await;
            ("Autostart removed.".to_string(), true)
        }
        Err(_) => ("Couldn't remove autostart — the admin prompt may have been declined. Nothing was changed.".to_string(), false),
    }
}

// ── Windows autostart shim ─────────────────────────────────────────────────
// Platform-neutral string code (so the tests run everywhere); only the file I/O
// around it is Windows-only.

/// Bumped whenever the shim's shape changes; the header carries it so a launcher
/// can tell a stale shim from a current one and regenerate it.
/// v1 ran main.py in-process (died at import on core ≥ 2026-10-16), v2 fixed that,
/// v3 starts her detached like Launch does (python.exe, hidden console, own process
/// group) so the launcher can follow her log and stop her gracefully.
#[cfg_attr(not(windows), allow(dead_code))] // only the Windows shim repair calls these
pub(crate) const SHIM_VERSION: u32 = 3;

/// The autostart shim the logon task runs under pythonw (nothing flashes). It is
/// Launch in Python: truncate the log, then start the env's python.exe (next to the
/// pythonw running the shim) on main.py — hidden console, own process group (so
/// Stop's CTRL_BREAK has a group to send to), stdin closed, stdout+stderr appended
/// to the log, UTF-8 like Launch — write the `sapphire-run.json` handshake
/// atomically, and exit. The log is created first so a failure has somewhere to go;
/// a traceback lands there too. Absolute main.py path so `__file__` (the lock's
/// anchor) is absolute.
#[cfg_attr(not(windows), allow(dead_code))] // only the Windows shim repair calls these
pub(crate) fn shim_source(cwd: &str, log: &str, run: &str) -> String {
    // Python string literal: escape backslashes then double-quotes (handles spaces
    // and any path safely — avoids raw-string's can't-end-in-backslash gotcha).
    let py_lit = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let lines: Vec<String> = vec![
        format!("# sapphire-launcher autostart shim v{} (regenerated by the launcher; edits are overwritten)", SHIM_VERSION),
        "import json, os, subprocess, sys, time".into(),
        format!("_cwd = \"{}\"", py_lit(cwd)),
        format!("_log = \"{}\"", py_lit(log)),
        format!("_run = \"{}\"", py_lit(run)),
        "os.makedirs(os.path.dirname(_log), exist_ok=True)".into(),
        "open(_log, \"w\").close()".into(),
        "try:".into(),
        "    _env = dict(os.environ, PYTHONUTF8=\"1\", PYTHONIOENCODING=\"utf-8\")".into(),
        "    _py = os.path.join(os.path.dirname(sys.executable), \"python.exe\")".into(),
        "    with open(_log, \"ab\") as _f:".into(),
        "        _p = subprocess.Popen([_py, \"-u\", os.path.join(_cwd, \"main.py\")], cwd=_cwd, env=_env,".into(),
        "                              stdin=subprocess.DEVNULL, stdout=_f, stderr=subprocess.STDOUT,".into(),
        "                              creationflags=0x08000000 | 0x00000200)".into(),
        "    with open(_run + \".tmp\", \"w\", encoding=\"utf-8\") as _f:".into(),
        "        json.dump({\"pid\": _p.pid, \"log\": _log, \"by\": \"autostart\", \"started\": int(time.time())}, _f)".into(),
        "    os.replace(_run + \".tmp\", _run)".into(),
        "except Exception:".into(),
        "    import traceback".into(),
        "    with open(_log, \"a\", encoding=\"utf-8\") as _f:".into(),
        "        _f.write(\"[autostart shim] \" + traceback.format_exc())".into(),
    ];
    lines.join("\r\n") + "\r\n"
}

/// The version in a shim's header line (`# sapphire-launcher autostart shim vN …`);
/// None for v1, which had no header.
#[cfg_attr(not(windows), allow(dead_code))] // only the Windows shim repair calls these
pub(crate) fn shim_version(src: &str) -> Option<u32> {
    src.lines().next()
        .and_then(|l| l.trim().strip_prefix("# sapphire-launcher autostart shim v"))
        .and_then(|rest| rest.split_whitespace().next()?.parse::<u32>().ok())
}

/// Is this shim text the current shape?
#[cfg_attr(not(windows), allow(dead_code))] // only the Windows shim repair calls these
pub(crate) fn shim_is_current(src: &str) -> bool {
    shim_version(src).is_some_and(|v| v >= SHIM_VERSION)
}

/// The install folder a shim (any version) points at: `_cwd = "…"` now,
/// `os.chdir("…")` in v1. Python-literal escapes undone.
#[cfg_attr(not(windows), allow(dead_code))] // only the Windows shim repair calls these
pub(crate) fn shim_cwd(src: &str) -> Option<String> {
    let lit = src.lines().find_map(|l| {
        let l = l.trim();
        l.strip_prefix("_cwd = \"").or_else(|| l.strip_prefix("os.chdir(\""))
    })?;
    let mut out = String::new();
    let mut chars = lit.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push(chars.next()?),
            '"' => return Some(out),
            c => out.push(c),
        }
    }
    None
}

/// Startup check (Windows): if autostart is on but its shim predates the current
/// shape, rewrite it in place — same folder, same log, no admin needed (the logon
/// task points at the shim's path, which doesn't change). Returns a line for the
/// launcher log when something was done or is wrong; None when all is well.
#[cfg(windows)]
pub(crate) fn repair_autostart_shim(install_path: &str) -> Option<String> {
    if !autostart_marker_path().exists() {
        return None;
    }
    let shim = autostart_shim_path();
    let Ok(old) = std::fs::read_to_string(&shim) else {
        return Some("Autostart is turned on, but its launcher file is missing — turn autostart off and on again in the Autostart tab.".to_string());
    };
    if shim_is_current(&old) {
        return None;
    }
    let has_main = |d: &str| PathBuf::from(d).join("main.py").is_file();
    let cwd = shim_cwd(&old).filter(|d| has_main(d))
        .or_else(|| has_main(install_path).then(|| install_path.to_string()));
    let Some(cwd) = cwd else {
        return Some("Autostart is turned on, but points at a folder with no Sapphire in it — turn it off and on again once the path is right.".to_string());
    };
    let note = match shim_version(&old) {
        Some(2) => "Updated the autostart launcher: Sapphire now starts detached at sign-in, so this app can follow her log and stop her gracefully. Takes effect at your next sign-in.",
        _ => "Updated the autostart launcher: Sapphire changed how it starts (2026-10-16) and the old one couldn't start her. Takes effect at your next sign-in — hit Launch to start her now.",
    };
    match std::fs::write(&shim, shim_source(&cwd, &launch_log_path().to_string_lossy(), &run_state_path().to_string_lossy())) {
        Ok(()) => Some(note.to_string()),
        Err(e) => Some(format!("Autostart needs an update but the launcher file couldn't be written: {}", e)),
    }
}

// ── Linux unit health ──────────────────────────────────────────────────────
// A unit from before the stop contract (or a hand-written one) keeps working, but
// restarts a twin/config-error exit forever and gets SIGKILLed at 90 s on a slow
// stop. We read the EFFECTIVE properties from systemctl (drop-ins included) and
// offer to add what's missing to the unit file itself.

/// The unit properties that matter for health, as `systemctl show` reports them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(windows, allow(dead_code))] // only the Linux unit check calls these
pub(crate) struct UnitProps {
    pub(crate) restart: String,
    pub(crate) prevent_exit: Vec<String>,
    pub(crate) timeout_stop_secs: Option<u64>,
    pub(crate) exec_path: Option<String>,
    pub(crate) working_dir: Option<String>,
    pub(crate) fragment: Option<String>,
}

/// Parse `systemctl show` output (one `Key=value` per line). `ExecStart` comes as
/// `{ path=/x/python ; argv[]=… }`; USec properties come human-readable ("2min").
#[cfg_attr(windows, allow(dead_code))] // only the Linux unit check calls these
pub(crate) fn parse_unit_props(show: &str) -> UnitProps {
    let mut p = UnitProps::default();
    let some = |v: &str| if v.is_empty() { None } else { Some(v.to_string()) };
    for line in show.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        match k {
            "Restart" => p.restart = v.to_string(),
            "RestartPreventExitStatus" => p.prevent_exit = v.split_whitespace().map(str::to_string).collect(),
            "TimeoutStopUSec" => p.timeout_stop_secs = parse_systemd_duration(v),
            "WorkingDirectory" => p.working_dir = some(v.trim_start_matches('!')),
            "FragmentPath" => p.fragment = some(v),
            "ExecStart" => {
                p.exec_path = v.split_once("path=")
                    .map(|(_, rest)| rest.split(" ;").next().unwrap_or(rest).trim())
                    .and_then(some);
            }
            _ => {}
        }
    }
    p
}

/// systemd's human durations ("2min", "1min 30s", "90s", "infinity") → seconds.
#[cfg_attr(windows, allow(dead_code))] // only the Linux unit check calls these
pub(crate) fn parse_systemd_duration(s: &str) -> Option<u64> {
    let s = s.trim();
    if s == "infinity" { return Some(u64::MAX); }
    if s.is_empty() { return None; }
    let mut total = 0u64;
    for tok in s.split_whitespace() {
        let split = tok.find(|c: char| !c.is_ascii_digit() && c != '.')?;
        let (num, unit) = tok.split_at(split);
        let n: f64 = num.parse().ok()?;
        let mult = match unit {
            "us" | "µs" => 0.000_001, "ms" => 0.001, "s" | "sec" => 1.0,
            "min" | "m" => 60.0, "h" => 3600.0, "d" => 86400.0, "w" => 604800.0,
            _ => return None,
        };
        total += (n * mult) as u64;
    }
    Some(total)
}

/// What is wrong with the unit, split by remedy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(windows, allow(dead_code))] // only the Linux unit check calls these
pub(crate) struct UnitIssues {
    pub(crate) missing: Vec<&'static str>, // lines to add to [Service] (we can do it)
    pub(crate) broken: Vec<String>,        // only removing + re-creating the service fixes these
    pub(crate) fragment: Option<String>,   // the unit file we would patch
}

#[cfg_attr(windows, allow(dead_code))] // only the Linux unit check calls these
impl UnitIssues {
    pub(crate) fn is_empty(&self) -> bool { self.missing.is_empty() && self.broken.is_empty() }
}

/// Judge the unit. `python_ok` / `main_py_ok` are the filesystem answers for the
/// ExecStart interpreter and `<WorkingDirectory>/main.py`.
#[cfg_attr(windows, allow(dead_code))] // only the Linux unit check calls these
pub(crate) fn unit_issues(p: &UnitProps, python_ok: bool, main_py_ok: bool) -> UnitIssues {
    let mut out = UnitIssues { fragment: p.fragment.clone(), ..Default::default() };
    let has = |code: &str| p.prevent_exit.iter().any(|c| c == code);
    if p.restart != "no" && !(has("43") && has("44")) {
        out.missing.push("RestartPreventExitStatus=43 44");
    }
    if p.timeout_stop_secs.is_some_and(|t| t < 120) {
        out.missing.push("TimeoutStopSec=120");
    }
    if let Some(py) = &p.exec_path && !python_ok {
        out.broken.push(format!("It runs {}, which doesn't exist anymore (environment removed or moved).", py));
    }
    if let Some(wd) = &p.working_dir && !main_py_ok {
        out.broken.push(format!("Its folder {} has no main.py (Sapphire moved or was deleted).", wd));
    }
    out
}

/// Append `lines` at the end of the `[Service]` section (so a later-wins key like
/// TimeoutStopSec= overrides what is there), creating the section if absent.
#[cfg_attr(windows, allow(dead_code))] // only the Linux unit check calls these
pub(crate) fn add_service_lines(unit: &str, lines: &[&str]) -> String {
    let mut out: Vec<String> = unit.lines().map(str::to_string).collect();
    let start = out.iter().position(|l| l.trim().eq_ignore_ascii_case("[Service]"));
    let Some(start) = start else {
        if !out.is_empty() && !out.last().unwrap().trim().is_empty() { out.push(String::new()); }
        out.push("[Service]".to_string());
        out.extend(lines.iter().map(|l| l.to_string()));
        return out.join("\n") + "\n";
    };
    // The section ends at the next header; back up over trailing blank lines.
    let mut end = out[start + 1..].iter().position(|l| l.trim_start().starts_with('['))
        .map(|i| start + 1 + i).unwrap_or(out.len());
    while end > start + 1 && out[end - 1].trim().is_empty() { end -= 1; }
    for (i, l) in lines.iter().enumerate() {
        out.insert(end + i, l.to_string());
    }
    out.join("\n") + "\n"
}

/// Read the unit's effective properties and judge them. None = no unit or no issues.
#[cfg(not(windows))]
pub(crate) async fn check_unit() -> Option<UnitIssues> {
    let show = run_cmd_full_async("systemctl".into(), vec![
        "--user".into(), "show".into(), "sapphire".into(),
        "-p".into(), "LoadState".into(), "-p".into(), "Restart".into(),
        "-p".into(), "RestartPreventExitStatus".into(), "-p".into(), "TimeoutStopUSec".into(),
        "-p".into(), "ExecStart".into(), "-p".into(), "WorkingDirectory".into(),
        "-p".into(), "FragmentPath".into(),
    ]).await.ok()?;
    if !show.lines().any(|l| l == "LoadState=loaded") {
        return None;
    }
    let p = parse_unit_props(&show);
    let python_ok = p.exec_path.as_deref().is_none_or(|x| PathBuf::from(x).is_file());
    let main_py_ok = p.working_dir.as_deref().is_none_or(|w| PathBuf::from(w).join("main.py").is_file());
    let issues = unit_issues(&p, python_ok, main_py_ok);
    (!issues.is_empty()).then_some(issues)
}

/// Add the missing lines to the unit file (backup `.bak` first) and daemon-reload.
/// Only touches a unit inside the user's home — anything else is told, not edited.
#[cfg(not(windows))]
pub(crate) async fn patch_unit(fragment: String, lines: Vec<&'static str>) -> (String, bool) {
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() || !fragment.starts_with(&home) {
        return (format!("The unit lives at {} (outside your home), so add these lines to its [Service] section yourself: {}", fragment, lines.join(", ")), false);
    }
    let text = match tokio::fs::read_to_string(&fragment).await {
        Ok(t) => t,
        Err(e) => return (format!("Couldn't read {}: {}", fragment, e), false),
    };
    if let Err(e) = tokio::fs::copy(&fragment, format!("{}.bak", fragment)).await {
        return (format!("Couldn't back up {}: {}", fragment, e), false);
    }
    if let Err(e) = tokio::fs::write(&fragment, add_service_lines(&text, &lines)).await {
        return (format!("Couldn't write {}: {}", fragment, e), false);
    }
    match run_cmd_full_async("systemctl".into(), vec!["--user".into(), "daemon-reload".into()]).await {
        Ok(_) => (format!("Service file updated (added {}; backup at {}.bak).", lines.join(" and "), fragment), true),
        Err(e) => (format!("Service file updated, but daemon-reload failed: {}", e), false),
    }
}

/// Read the unit's EnvironmentFile (KEY=VALUE per line). Empty if absent.
#[cfg(not(windows))]
pub(crate) async fn read_env_file(path: String) -> String {
    tokio::fs::read_to_string(path).await.unwrap_or_default()
}

/// Write the unit's EnvironmentFile (chmod 600 — it can hold API keys).
#[cfg(not(windows))]
pub(crate) async fn write_env_file(path: String, content: String) -> bool {
    if let Some(dir) = std::path::Path::new(&path).parent()
        && tokio::fs::create_dir_all(dir).await.is_err()
    {
        return false;
    }
    if tokio::fs::write(&path, content).await.is_err() { return false; }
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    true
}

/// Run a command, streaming each output line into `sink` (drained to the launcher
/// log by the Tick handler). Spin-safe: a normal awaited future, not a Task::stream.
/// Returns Ok("") on success (lines already shown live) / Err(reason) on failure.
pub(crate) async fn run_cmd_streamed(
    program: String,
    args: Vec<String>,
    cwd: Option<String>,
    sink: Arc<std::sync::Mutex<Vec<String>>>,
) -> Result<String, String> {
    async fn pump(r: impl tokio::io::AsyncRead + Unpin, sink: Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::AsyncBufReadExt;
        let mut reader = BufReader::new(r);
        let mut buf = Vec::new();
        while reader.read_until(b'\n', &mut buf).await.unwrap_or(0) > 0 {
            while buf.last() == Some(&b'\n') || buf.last() == Some(&b'\r') { buf.pop(); }
            let mut s = String::from_utf8_lossy(&buf).into_owned();
            if let Some(i) = s.rfind('\r') { s = s[i + 1..].to_string(); }
            buf.clear();
            if let Ok(mut v) = sink.lock() { v.push(format!("  {}", strip_ansi(&s))); }
        }
    }

    // stdin is never inherited: ours is null (GUI app) or, if a Stop just made the
    // console trip in signal_stop, a dead handle — and inheriting a dead handle fails
    // the spawn with "The handle is invalid". Same rule for every child we start.
    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env("CONDA_PLUGINS_AUTO_ACCEPT_TOS", "yes"); // see run_cmd_full_async
    if let Some(d) = cwd { cmd.current_dir(d); }
    #[cfg(windows)]
    { cmd.creation_flags(CREATE_NO_WINDOW); }

    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let so = child.stdout.take().unwrap();
    let se = child.stderr.take().unwrap();
    let a = tokio::spawn(pump(so, sink.clone()));
    let b = tokio::spawn(pump(se, sink.clone()));
    let (_, _, status) = tokio::join!(a, b, child.wait());
    match status {
        Ok(s) if s.success() => Ok(String::new()),
        Ok(s) => Err(format!("exit code {}", s.code().unwrap_or(-1))),
        Err(e) => Err(e.to_string()),
    }
}

// ── Launch streaming ───────────────────────────────────────────────────────

/// Reader task: owns the journalctl child and pumps lines into a channel.
/// Uses lossy UTF-8 (download progress spam won't kill it) and collapses
/// carriage-return progress bars to their final value. Stops when the receiver
/// drops (subscription removed), which drops the child → `kill_on_drop`.
async fn journal_reader(tx: tokio::sync::mpsc::UnboundedSender<String>) {
    use tokio::io::AsyncBufReadExt;
    let mut cmd = tokio::process::Command::new("journalctl");
    cmd.args(["--user", "-u", "sapphire", "-f", "-n", "200", "-o", "cat"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(format!("Couldn't read service logs: {}", e));
            return;
        }
    };
    let Some(stdout) = child.stdout.take() else { return };
    let mut reader = BufReader::new(stdout);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        tokio::select! {
            _ = tx.closed() => break, // subscription gone → shut down (kills journalctl)
            r = reader.read_until(b'\n', &mut buf) => match r {
                Ok(0) => break, // EOF — journalctl exited
                Ok(_) => {
                    let mut s = String::from_utf8_lossy(&buf).into_owned();
                    while s.ends_with('\n') || s.ends_with('\r') { s.pop(); }
                    // Collapse \r progress bars to the latest segment.
                    if let Some(i) = s.rfind('\r') { s = s[i + 1..].to_string(); }
                    if tx.send(strip_ansi(&s)).is_err() { break; }
                }
                Err(_) => break,
            }
        }
    }
}

/// A `Subscription`-driven follower of `journalctl --user -u sapphire -f`.
/// As a Subscription (not Task::stream) iced parks it when idle. Each poll drains
/// *all* currently-queued lines into one `SapphireLines` batch → one redraw per
/// burst instead of one per line.
pub(crate) fn journal_log_stream() -> impl futures::Stream<Item = Message> {
    enum St {
        Start,
        Run(tokio::sync::mpsc::UnboundedReceiver<String>),
    }
    futures::stream::unfold(St::Start, |state| async move {
        let mut rx = match state {
            St::Start => {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                // Spawned here (inside the polled stream → in tokio runtime context).
                tokio::spawn(journal_reader(tx));
                rx
            }
            St::Run(rx) => rx,
        };
        let first = rx.recv().await?;
        let mut batch = vec![first];
        while let Ok(line) = rx.try_recv() {
            batch.push(line);
            if batch.len() >= 2000 { break; }
        }
        Some((Message::SapphireLines(batch), St::Run(rx)))
    })
}

// ── Launch (detached) ─────────────────────────────────────────────────────────

/// Start her detached: `python -u main.py` with stdout/stderr in `log` (no pipes —
/// a pipe whose reader has gone kills the writer), nothing killed when the Child or
/// the launcher goes away, her own session / process group. The launcher is an
/// assistant, not a parent: closing it leaves her running, and a later launcher
/// finds her through the handshake written to `run` (`find_instance`). Returns the
/// Child, its pid and the grace (ms) to give the log tail once she has exited.
fn spawn_detached(
    install_path: &str,
    log: &std::path::Path,
    run: &std::path::Path,
) -> Result<(tokio::process::Child, u32, u64), String> {
    use std::io::Write;
    let (program, mut args) = sapphire_python_cmd();
    let is_direct = args.is_empty();
    args.push("-u".to_string());
    args.push("main.py".to_string());

    // Her output file: truncated per Launch (she is not running — Launch is not
    // offered while she is), then opened for append: everyone who holds it writes
    // at the end, so a second writer could never leave holes in it.
    let what = |e: std::io::Error| format!("Couldn't write {}: {}", log.display(), e);
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent).map_err(what)?;
    }
    std::fs::File::create(log).map_err(what)?;
    let mut out = std::fs::OpenOptions::new().append(true).open(log).map_err(what)?;
    if is_direct {
        let _ = writeln!(out, "[Launcher] Using {}", program);
    }
    let err = out.try_clone().map_err(what)?;

    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(&args)
        .current_dir(install_path)
        .stdin(std::process::Stdio::null()) // never inherit ours: see run_cmd_streamed
        .stdout(std::process::Stdio::from(out))
        .stderr(std::process::Stdio::from(err))
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1")
        .kill_on_drop(false); // she outlives us by design
    #[cfg(windows)]
    {
        // Own process group (its pid = the group id) so Stop can send CTRL_BREAK to
        // the runner and everything it spawned; hidden console so nothing flashes —
        // and that console is hers, so it stays when the launcher closes.
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(unix)]
    {
        // Own session: hence her own process group with pid = pgid (what Stop's
        // killpg checks) and no controlling terminal, so a terminal the launcher was
        // started from cannot HUP her. setpgid is the fallback if setsid is refused.
        // SAFETY: setsid/setpgid are async-signal-safe and act on the child only.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    libc::setpgid(0, 0);
                }
                Ok(())
            });
        }
    }
    let child = cmd.spawn().map_err(|e| format!("Failed to start: {}", e))?;
    let pid = child.id().unwrap_or(0);
    // The handshake for a later launcher (and for find_instance on a core without
    // the lock file). Best effort: without it she still runs; we just can't claim her.
    let _ = write_run_state(run, &RunState { pid, log: log.to_path_buf(), origin: Origin::Launcher });
    // `conda run` puts a wrapper between us and her: a longer grace for her last lines.
    Ok((child, pid, if is_direct { 700 } else { 1500 }))
}

/// `Subscription`-driven: start her (on first poll, inside the tokio runtime) and
/// report `SapphireStarted(pid)`, then wait for the runner to end and report
/// `SapphireExited`. Holding the Child is what gets us the exit code; it has no say
/// over her life (no kill_on_drop, no pipes). Her lines reach the pane through
/// `tail_log_stream` on `log`, which is why the exit notice waits a moment. Lazy
/// like the journal follower, so building it in `subscription()` is cheap.
pub(crate) fn child_follower(install_path: String, log: PathBuf, run: PathBuf) -> impl futures::Stream<Item = Message> {
    enum St {
        Start(String, PathBuf, PathBuf),
        Running(tokio::process::Child, u64),
        Done,
    }
    futures::stream::unfold(St::Start(install_path, log, run), |st| async move {
        match st {
            St::Start(install, log, run) => match spawn_detached(&install, &log, &run) {
                Ok((child, pid, grace)) => Some((Message::SapphireStarted(pid), St::Running(child, grace))),
                Err(e) => Some((Message::SapphireExited(Err(e)), St::Done)),
            },
            St::Running(mut child, grace) => {
                let result = child.wait().await.map(|s| s.code()).map_err(|e| format!("Error: {}", e));
                tokio::time::sleep(std::time::Duration::from_millis(grace)).await;
                Some((Message::SapphireExited(result), St::Done))
            }
            St::Done => None,
        }
    })
}

// ── Following a log file ──────────────────────────────────────────────────────

/// `tail -F` for one file, driven by `tail_log_stream`. Opens the path afresh on
/// every poll — so core's midnight rotation (rename, then a new file) is followed by
/// name with no inode bookkeeping on either OS — and starts over when the file is
/// shorter than where it was (truncated by a new Launch, or just rotated). The one
/// blind spot: a replacement that already outgrew the old file within one poll; a
/// day's log replaced by 300 ms of a fresh one does not happen.
struct Tail {
    path: PathBuf,
    pos: u64,
    partial: Vec<u8>,      // bytes after the last newline seen
    primed: bool,          // the first sight shows the last lines, not the whole file
    prime_pending: bool,   // ...and that first batch is capped to LAST_LINES
    skip_to_newline: bool, // we started mid-line: drop up to the next newline
}

impl Tail {
    const LAST_LINES: usize = 200;
    const LAST_BYTES: u64 = 64 * 1024;

    fn new(path: PathBuf) -> Self {
        Tail { path, pos: 0, partial: Vec::new(), primed: false, prime_pending: false, skip_to_newline: false }
    }

    /// The complete lines that appeared since the last call (empty = nothing yet).
    fn poll(&mut self) -> Vec<String> {
        use std::io::{Read, Seek, SeekFrom};
        let mut lines = Vec::new();
        // Not there yet (she is booting) or mid-rotation: try again next time.
        let Ok(mut f) = std::fs::File::open(&self.path) else { return lines };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len < self.pos {
            self.pos = 0;
            self.partial.clear();
            self.skip_to_newline = false;
        }
        if !self.primed {
            self.primed = true;
            self.prime_pending = true;
            self.pos = len.saturating_sub(Self::LAST_BYTES);
            self.skip_to_newline = self.pos > 0;
        }
        if self.pos > 0 && f.seek(SeekFrom::Start(self.pos)).is_err() {
            return lines;
        }
        let mut buf = Vec::new();
        match f.read_to_end(&mut buf) {
            Ok(0) | Err(_) => return lines,
            Ok(n) => self.pos += n as u64,
        }
        self.partial.extend_from_slice(&buf);
        if self.skip_to_newline {
            match self.partial.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    self.partial.drain(..=i);
                    self.skip_to_newline = false;
                }
                None => {
                    self.partial.clear();
                    return lines;
                }
            }
        }
        let Some(last_nl) = self.partial.iter().rposition(|&b| b == b'\n') else {
            // A writer with no newlines at all: don't hoard forever.
            if self.partial.len() as u64 > Self::LAST_BYTES {
                lines.push(clean_line(&std::mem::take(&mut self.partial)));
            }
            return lines;
        };
        let complete: Vec<u8> = self.partial.drain(..=last_nl).collect();
        let mut parts: Vec<&[u8]> = complete.split(|&b| b == b'\n').collect();
        parts.pop(); // the empty piece after the final newline
        lines.extend(parts.into_iter().map(clean_line));
        if std::mem::take(&mut self.prime_pending) && lines.len() > Self::LAST_LINES {
            lines.drain(..lines.len() - Self::LAST_LINES);
        }
        lines
    }
}

/// One raw log line → what the pane shows: lossy UTF-8, ANSI colour stripped, a
/// `\r` progress bar collapsed to its last segment, the CR of a CRLF dropped.
fn clean_line(raw: &[u8]) -> String {
    let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
    let mut s = strip_ansi(&String::from_utf8_lossy(raw));
    if let Some(i) = s.rfind('\r') {
        s = s[i + 1..].to_string();
    }
    s
}

/// `Subscription`-driven follower of a log file: the last ~200 lines first, then
/// whatever gets appended, polled every 300 ms and batched into one `SapphireLines`
/// per poll (one redraw per burst). Used for her launch log (Launch, autostart) and
/// for core's own `sapphire.log` when she was started elsewhere; journalctl stays
/// the source in service mode. No inotify on purpose: one code path on both OSes,
/// and the file may not exist yet.
pub(crate) fn tail_log_stream(path: PathBuf) -> impl futures::Stream<Item = Message> {
    futures::stream::unfold(Tail::new(path), |mut tail| async move {
        loop {
            let lines = tail.poll();
            if !lines.is_empty() {
                return Some((Message::SapphireLines(lines), tail));
            }
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
    })
}


// ── Tests: the pure helpers (any OS) ──────────────────────────────────────────
#[cfg(test)]
mod pure_tests {
    use super::*;

    #[test]
    fn shim_round_trips_awkward_paths_and_starts_her_detached() {
        let cwd = r#"C:\Users\Krem's "lab"\sapphire"#;
        let log = r#"C:\Users\Krem's "lab"\AppData\Roaming\Sapphire\sapphire-launcher.log"#;
        let run = r#"C:\Users\Krem's "lab"\AppData\Roaming\Sapphire\sapphire-run.json"#;
        let src = shim_source(cwd, log, run);
        assert!(shim_is_current(&src));
        assert_eq!(shim_version(&src), Some(SHIM_VERSION));
        assert_eq!(shim_cwd(&src).as_deref(), Some(cwd));
        // Like Launch: python.exe (not pythonw), hidden, own process group, stdin
        // closed, output appended to the log, UTF-8, handshake written atomically.
        for needle in [
            "\"python.exe\"", "subprocess.Popen(", "creationflags=0x08000000 | 0x00000200",
            "stdin=subprocess.DEVNULL", "stdout=_f, stderr=subprocess.STDOUT", "open(_log, \"ab\")",
            "PYTHONUTF8=\"1\"", "\"by\": \"autostart\"", "os.replace(_run + \".tmp\", _run)",
            "os.path.join(_cwd, \"main.py\")", "traceback.format_exc()",
        ] {
            assert!(src.contains(needle), "missing {:?} in:\n{}", needle, src);
        }
        // The log is created before anything can fail: a bad folder still leaves a trace.
        assert!(src.find("open(_log, \"w\")").unwrap() < src.find("subprocess.Popen(").unwrap());
        assert!(src.contains(&format!("_run = \"{}\"", run.replace('\\', "\\\\").replace('"', "\\\""))));
    }

    #[test]
    fn older_shims_are_stale_but_their_folder_is_recoverable() {
        let v1 = "import runpy, sys, os\r\nos.chdir(\"C:\\\\Users\\\\krem\\\\sapphire\")\r\n_log = \"x\"\r\nrunpy.run_path(\"main.py\", run_name=\"__main__\")\r\n";
        assert!(!shim_is_current(v1));
        assert_eq!(shim_version(v1), None);
        assert_eq!(shim_cwd(v1).as_deref(), Some(r"C:\Users\krem\sapphire"));
        let v2 = "# sapphire-launcher autostart shim v2 (regenerated by the launcher; edits are overwritten)\r\nimport runpy, sys, os\r\n_cwd = \"C:\\\\Users\\\\krem\\\\sapphire\"\r\n";
        assert_eq!(shim_version(v2), Some(2));
        assert!(!shim_is_current(v2), "v2 ran her under pythonw: no console, no graceful Stop");
        assert_eq!(shim_cwd(v2).as_deref(), Some(r"C:\Users\krem\sapphire"));
        assert!(!shim_is_current(""));
        assert_eq!(shim_cwd("print(1)"), None);
        assert!(shim_is_current(&format!("# sapphire-launcher autostart shim v{} (newer)\n", SHIM_VERSION + 1)));
    }

    #[test]
    fn environment_file_is_read_from_systemctl_show() {
        assert_eq!(env_file_from_show("/home/k/.config/sapphire/env (ignore_errors=no)").as_deref(), Some("/home/k/.config/sapphire/env"));
        assert_eq!(env_file_from_show("-/home/k/.config/sapphire/service.env (ignore_errors=yes)").as_deref(), Some("/home/k/.config/sapphire/service.env"));
        assert_eq!(env_file_from_show(""), None);
    }

    #[test]
    fn systemd_durations_parse() {
        for (s, want) in [
            ("2min", Some(120)), ("1min 30s", Some(90)), ("90s", Some(90)), ("1h", Some(3600)),
            ("100ms", Some(0)), ("infinity", Some(u64::MAX)), ("1.5min", Some(90)),
            ("", None), ("soon", None), ("5 parsecs", None),
        ] {
            assert_eq!(parse_systemd_duration(s), want, "duration {:?}", s);
        }
    }

    /// Shaped exactly like `systemctl --user show sapphire -p …` output.
    const SHOW: &str = "Restart=on-failure\nTimeoutStopUSec=2min\nRestartPreventExitStatus=43 44\n\
        ExecStart={ path=/home/k/miniconda3/envs/sapphire/bin/python ; argv[]=/home/k/miniconda3/envs/sapphire/bin/python main.py ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }\n\
        WorkingDirectory=/home/k/sapphire\nLoadState=loaded\nFragmentPath=/home/k/.config/systemd/user/sapphire.service\n";

    #[test]
    fn unit_props_parse_from_systemctl_show() {
        let p = parse_unit_props(SHOW);
        assert_eq!(p, UnitProps {
            restart: "on-failure".into(),
            prevent_exit: vec!["43".into(), "44".into()],
            timeout_stop_secs: Some(120),
            exec_path: Some("/home/k/miniconda3/envs/sapphire/bin/python".into()),
            working_dir: Some("/home/k/sapphire".into()),
            fragment: Some("/home/k/.config/systemd/user/sapphire.service".into()),
        });
        assert!(unit_issues(&p, true, true).is_empty(), "a current unit has no issues");
    }

    #[test]
    fn old_and_broken_units_are_judged() {
        let old = parse_unit_props("Restart=on-failure\nTimeoutStopUSec=1min 30s\nRestartPreventExitStatus=\nExecStart={ path=/py ; argv[]=/py main.py }\nWorkingDirectory=/w\n");
        let i = unit_issues(&old, true, true);
        assert_eq!(i.missing, vec!["RestartPreventExitStatus=43 44", "TimeoutStopSec=120"]);
        assert!(i.broken.is_empty());
        let partial = parse_unit_props("Restart=always\nTimeoutStopUSec=3min\nRestartPreventExitStatus=43\n");
        assert_eq!(unit_issues(&partial, true, true).missing, vec!["RestartPreventExitStatus=43 44"]);
        let no_restart = parse_unit_props("Restart=no\nTimeoutStopUSec=infinity\nRestartPreventExitStatus=\n");
        assert!(unit_issues(&no_restart, true, true).is_empty(), "no restarts → nothing to prevent");
        let b = unit_issues(&old, false, false);
        assert_eq!(b.broken.len(), 2);
        assert!(b.broken[0].contains("/py") && b.broken[1].contains("/w"), "{:?}", b.broken);
    }

    #[test]
    fn service_lines_land_at_the_end_of_the_section() {
        let unit = "[Unit]\nDescription=x\n\n[Service]\nType=simple\nTimeoutStopSec=10\n\n[Install]\nWantedBy=default.target\n";
        let got = add_service_lines(unit, &["RestartPreventExitStatus=43 44", "TimeoutStopSec=120"]);
        assert_eq!(got, "[Unit]\nDescription=x\n\n[Service]\nType=simple\nTimeoutStopSec=10\nRestartPreventExitStatus=43 44\nTimeoutStopSec=120\n\n[Install]\nWantedBy=default.target\n");
        // Later wins in systemd, so ours must come after the existing TimeoutStopSec.
        assert!(got.rfind("TimeoutStopSec=120").unwrap() > got.find("TimeoutStopSec=10").unwrap());
        assert_eq!(add_service_lines("[Service]\nA=1", &["B=2"]), "[Service]\nA=1\nB=2\n");
        assert_eq!(add_service_lines("[Unit]\nDescription=x\n", &["B=2"]), "[Unit]\nDescription=x\n\n[Service]\nB=2\n");
    }
}

// ── Tests: the log tail (any OS) ──────────────────────────────────────────────
#[cfg(test)]
mod tail_tests {
    use super::*;
    use futures::StreamExt;
    use std::io::Write;
    use std::time::Duration;

    type Pinned<S> = std::pin::Pin<Box<S>>;

    async fn next_lines(s: &mut Pinned<impl futures::Stream<Item = Message>>) -> Vec<String> {
        match tokio::time::timeout(Duration::from_secs(5), s.next()).await {
            Ok(Some(Message::SapphireLines(l))) => l,
            Ok(Some(_)) => panic!("unexpected message"),
            Ok(None) => panic!("tail ended"),
            Err(_) => panic!("no lines within 5 s"),
        }
    }

    /// Batches until `n` lines have arrived (a big append may land in two polls).
    async fn lines_until(s: &mut Pinned<impl futures::Stream<Item = Message>>, n: usize) -> Vec<String> {
        let mut all = Vec::new();
        while all.len() < n {
            all.extend(next_lines(s).await);
        }
        all
    }

    fn append(path: &std::path::Path, bytes: &[u8]) {
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(bytes).unwrap();
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sapphire-launcher-test-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn tail_shows_the_end_then_follows_appends_truncation_and_rotation() {
        let dir = scratch("tail");
        let path = dir.join("sapphire.log");
        let body: String = (0..300).map(|i| format!("line {}\n", i)).collect();
        std::fs::write(&path, &body).unwrap();

        let mut s = Box::pin(tail_log_stream(path.clone()));
        let first = next_lines(&mut s).await;
        assert_eq!(first.len(), 200, "the last 200 lines, not all 300");
        assert_eq!((first[0].as_str(), first[199].as_str()), ("line 100", "line 299"));

        // Appended: CRLF, a colour code, a \r progress bar.
        append(&path, b"\x1b[31mred\x1b[0m\r\n10%\r50%\r100% done\n");
        assert_eq!(lines_until(&mut s, 2).await, vec!["red", "100% done"]);

        // Truncated and rewritten (a new Launch): from the top again.
        std::fs::write(&path, "fresh\n").unwrap();
        assert_eq!(next_lines(&mut s).await, vec!["fresh"]);

        // Rotated the way core does at midnight: the full file renamed away, a new
        // (smaller) one under the same name.
        append(&path, body.as_bytes());
        assert_eq!(lines_until(&mut s, 300).await.len(), 300);
        std::fs::rename(&path, dir.join("sapphire.log.2026-10-19")).unwrap();
        std::fs::write(&path, "after rotation\n").unwrap();
        assert_eq!(next_lines(&mut s).await, vec!["after rotation"]);

        // A partial line waits for its newline.
        append(&path, b"half");
        tokio::time::sleep(Duration::from_millis(700)).await;
        append(&path, b" and whole\n");
        assert_eq!(next_lines(&mut s).await, vec!["half and whole"]);
    }

    #[tokio::test]
    async fn tail_waits_for_a_file_that_is_not_there_yet() {
        let path = scratch("tail-late").join("late.log");
        let mut s = Box::pin(tail_log_stream(path.clone()));
        let p = path.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            std::fs::write(&p, "born\n").unwrap();
        });
        assert_eq!(next_lines(&mut s).await, vec!["born"]);
        assert_eq!(clean_line(b"plain\r"), "plain");
        assert_eq!(clean_line(b"\xff\xfebad utf8"), "\u{fffd}\u{fffd}bad utf8");
    }
}

// ── Tests (Linux dev box): detached launch, end to end ───────────────────────
// Uses the real sapphire env python to run a throwaway fake main.py that behaves
// like core's runner: writes the instance lock note, handles SIGTERM, prints. The
// whole chain is exercised: detached spawn in its own session, handshake written,
// pid reported, lines read back from the log file, find_instance agreeing with the
// Child, SIGTERM via signal_stop, exit code reported, last lines not lost, and the
// stale note believed by nobody afterwards.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::time::Duration;

    type Pinned<S> = std::pin::Pin<Box<S>>;

    /// A throwaway install dir with a fake `main.py`, plus a log and handshake path
    /// inside it (never the real config dir).
    fn fake_install(name: &str, body: &str) -> (String, PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("sapphire-launcher-test-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("main.py"), body).unwrap();
        (dir.to_string_lossy().into_owned(), dir.join("cfg").join("sapphire-launcher.log"), dir.join("cfg").join("sapphire-run.json"))
    }

    async fn next_msg(s: &mut Pinned<impl futures::Stream<Item = Message>>) -> Message {
        tokio::time::timeout(Duration::from_secs(15), s.next()).await.expect("follower timed out").expect("follower ended")
    }

    /// Tail batches until one holds `needle`; everything seen so far.
    async fn tail_until(t: &mut Pinned<impl futures::Stream<Item = Message>>, needle: &str) -> Vec<String> {
        let mut all = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(15), t.next()).await {
                Ok(Some(Message::SapphireLines(l))) => {
                    all.extend(l);
                    if all.iter().any(|x| x.contains(needle)) {
                        return all;
                    }
                }
                _ => panic!("tail never showed {:?}; got {:?}", needle, all),
            }
        }
    }

    /// Core's runner in miniature: the lock note, a SIGTERM handler, two prints.
    const RUNNER: &str = r#"
import os, signal, sys, time
os.makedirs("user", exist_ok=True)
with open("user/.instance.lock", "wb") as f:
    f.write(b"\n" + f"pid={os.getpid()} since=2026-10-20 08:01:02\n".encode())
stop = False
def h(sig, frame):
    global stop
    stop = True
signal.signal(signal.SIGTERM, h)
print("[Runner] fake starting", flush=True)
while not stop:
    time.sleep(0.05)
print("[Runner] fake stopping", flush=True)
time.sleep(0.3)
sys.exit(0)
"#;

    #[tokio::test]
    async fn detached_launch_is_found_followed_and_stopped_gracefully() {
        if sapphire_env_python().is_none() {
            eprintln!("skipped: no sapphire env python on this box");
            return;
        }
        let (dir, log, run) = fake_install("detached", RUNNER);
        let mut follower = Box::pin(child_follower(dir.clone(), log.clone(), run.clone()));
        let Message::SapphireStarted(pid) = next_msg(&mut follower).await else { panic!("expected SapphireStarted") };
        assert!(pid > 1);
        // Her own session: she leads her process group (what signal_group checks).
        assert_eq!(process_group_of(pid), Some(pid as i32), "runner is not a process-group leader");
        // The handshake is on disk before the pid is reported.
        assert_eq!(read_run_state(&run), Some(RunState { pid, log: log.clone(), origin: Origin::Launcher }));

        let mut tail = Box::pin(tail_log_stream(log.clone()));
        let lines = tail_until(&mut tail, "fake starting").await;
        assert!(lines.iter().any(|l| l.starts_with("[Launcher] Using ")), "{:?}", lines);

        // What a later launcher sees: the lock names her, the handshake claims her.
        let found = resolve_instance(read_lock_note(&dir).as_deref(), read_run_state(&run), pid_is_hers, &dir);
        assert_eq!(found, Some(Instance { pid, since: "2026-10-20 08:01:02".into(), origin: Origin::Launcher, log: log.clone() }));

        assert!(signal_stop(pid), "signal_stop could not send SIGTERM");
        assert!(matches!(next_msg(&mut follower).await, Message::SapphireExited(Ok(Some(0)))));
        tail_until(&mut tail, "fake stopping").await;

        // Gone: the note is stale, nobody is found, nothing can be signalled.
        assert!(!pid_is_hers(pid));
        assert_eq!(resolve_instance(read_lock_note(&dir).as_deref(), read_run_state(&run), pid_is_hers, &dir), None);
        assert!(!signal_stop(pid));
    }

    #[tokio::test]
    async fn twin_exit_code_is_passed_through_and_its_line_lands_in_the_log() {
        if sapphire_env_python().is_none() {
            eprintln!("skipped: no sapphire env python on this box");
            return;
        }
        let (dir, log, run) = fake_install(
            "twin",
            "import sys\nprint('[Runner] Another Sapphire is already running (fake)', flush=True)\nsys.exit(43)\n",
        );
        let mut follower = Box::pin(child_follower(dir, log.clone(), run));
        assert!(matches!(next_msg(&mut follower).await, Message::SapphireStarted(p) if p > 1));
        assert!(matches!(next_msg(&mut follower).await, Message::SapphireExited(Ok(Some(43)))));
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("Another Sapphire"), "runner line lost: {}", text);
    }
}
