// systemd --user service detection + log/launch streams.
use crate::*;
use crate::platform::*;
use std::sync::{Arc, atomic::{AtomicU32, Ordering}};
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
/// Windows: enable autostart. Registers a per-user logon Scheduled Task that runs the
/// env's windowless `pythonw main.py` directly (no console window), with the install
/// dir as the working directory. We use PowerShell `Register-ScheduledTask` (not bare
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

    // Write the launch shim the task runs (instead of `pythonw main.py` directly).
    // Under pythonw there's no console, so sys.stdout/stderr are None and Sapphire's
    // startup logging crashes the child → supervisor gives up → task exits 1. The shim
    // redirects output to a logfile first (valid handles, no crash) then runs main.py.
    // See platform::autostart_shim_path for the full why.
    let shim = autostart_shim_path();
    let log = autostart_log_path();
    if let Some(parent) = shim.parent()
        && let Err(e) = tokio::fs::create_dir_all(parent).await {
            return (format!("Couldn't create the autostart folder: {}", e), false);
        }
    if let Err(e) = tokio::fs::write(&shim, shim_source(&install_path, &log.to_string_lossy())).await {
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
#[cfg_attr(not(windows), allow(dead_code))] // only the Windows shim repair calls these
pub(crate) const SHIM_VERSION: u32 = 2;

/// The autostart shim the logon task runs under pythonw. Why each line is there:
/// the log first (pythonw has no console — without real handles Sapphire's first
/// print crashes; and if the chdir below fails, the error has somewhere to go),
/// "w" so each sign-in starts a fresh log (main.py's own restarts within one session
/// still accumulate), then the install dir on sys.path: core's runner does
/// `from core.single_instance import …` unconditionally (2026-10-16), and runpy does
/// NOT put a script's folder on the path — without this line autostart dies at
/// import. Absolute main.py path so `__file__` (the lock's anchor) is absolute too.
#[cfg_attr(not(windows), allow(dead_code))] // only the Windows shim repair calls these
pub(crate) fn shim_source(cwd: &str, log: &str) -> String {
    // Python string literal: escape backslashes then double-quotes (handles spaces
    // and any path safely — avoids raw-string's can't-end-in-backslash gotcha).
    let py_lit = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        "# sapphire-launcher autostart shim v{v} (regenerated by the launcher; edits are overwritten)\r\n\
         import runpy, sys, os\r\n\
         _cwd = \"{cwd}\"\r\n\
         _log = \"{log}\"\r\n\
         os.makedirs(os.path.dirname(_log), exist_ok=True)\r\n\
         sys.stdout = sys.stderr = open(_log, \"w\", buffering=1, encoding=\"utf-8\", errors=\"replace\")\r\n\
         os.chdir(_cwd)\r\n\
         sys.path.insert(0, _cwd)\r\n\
         runpy.run_path(os.path.join(_cwd, \"main.py\"), run_name=\"__main__\")\r\n",
        v = SHIM_VERSION, cwd = py_lit(cwd), log = py_lit(log),
    )
}

/// Is this shim text the current shape? (Header line carries the version.)
#[cfg_attr(not(windows), allow(dead_code))] // only the Windows shim repair calls these
pub(crate) fn shim_is_current(src: &str) -> bool {
    src.lines().next()
        .and_then(|l| l.trim().strip_prefix("# sapphire-launcher autostart shim v"))
        .and_then(|rest| rest.split_whitespace().next()?.parse::<u32>().ok())
        .is_some_and(|v| v >= SHIM_VERSION)
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
    match std::fs::write(&shim, shim_source(&cwd, &autostart_log_path().to_string_lossy())) {
        Ok(()) => Some("Updated the autostart launcher: Sapphire changed how it starts (2026-10-16) and the old one couldn't start her. Takes effect at your next sign-in — hit Launch to start her now.".to_string()),
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

    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(&args).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped())
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

/// One message from the spawned Sapphire process: a log line, or the final exit
/// notice — Ok(exit code; None = killed by a signal) or Err(why it never started).
/// The wording lives in main.rs, which knows whether WE asked her to stop.
enum SpawnItem {
    Line(String),
    Exited(Result<Option<i32>, String>),
}

/// Spawn `python -u main.py` and pump its stdout+stderr (lossy UTF-8, ANSI-stripped,
/// `\r` progress bars collapsed) as `SpawnItem::Line`s into `tx`, then one final
/// `SpawnItem::Exited`. Note: this task owns the child, NOT the subscription — so
/// dropping the follower does not kill her. `kill_on_drop` only fires when the whole
/// runtime goes away (launcher exit/crash); the Stop flow in main.rs signals first and
/// only falls back to `kill_process_tree`.
async fn spawn_reader(
    install_path: String,
    pid_store: Arc<AtomicU32>,
    tx: tokio::sync::mpsc::UnboundedSender<SpawnItem>,
) {
    // Resolve the env's python directly (avoids conda-run buffering) or conda run.
    let (program, mut args) = sapphire_python_cmd();
    let is_direct = args.is_empty();
    args.push("-u".to_string());
    args.push("main.py".to_string());

    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(&args)
        .current_dir(&install_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1")
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        // Own process group (its pid = the group id) so Stop can send CTRL_BREAK to
        // the runner and everything it spawned; hidden console so nothing flashes.
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(unix)]
    {
        // Own process group so Stop can signal the whole tree (child workers).
        cmd.process_group(0);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(SpawnItem::Exited(Err(format!("Failed to start: {}", e))));
            return;
        }
    };

    // Store PID for the Stop button.
    if let Some(pid) = child.id() {
        pid_store.store(pid, Ordering::Relaxed);
    }

    if is_direct {
        let _ = tx.send(SpawnItem::Line(format!("Using {}", program)));
    }

    async fn pump(
        r: impl tokio::io::AsyncRead + Unpin,
        tx: tokio::sync::mpsc::UnboundedSender<SpawnItem>,
    ) {
        use tokio::io::AsyncBufReadExt;
        let mut reader = BufReader::new(r);
        let mut buf = Vec::new();
        while reader.read_until(b'\n', &mut buf).await.unwrap_or(0) > 0 {
            while buf.last() == Some(&b'\n') || buf.last() == Some(&b'\r') { buf.pop(); }
            let mut s = strip_ansi(&String::from_utf8_lossy(&buf));
            if let Some(i) = s.rfind('\r') { s = s[i + 1..].to_string(); }
            buf.clear();
            if tx.send(SpawnItem::Line(s)).is_err() { break; }
        }
    }

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let a = tokio::spawn(pump(stdout, tx.clone()));
    let b = tokio::spawn(pump(stderr, tx.clone()));
    // Report the exit as soon as the runner itself is gone. The pumps only end once
    // every holder of the pipes is gone (sapphire.py, Kokoro inherit them), so give
    // them a short grace to flush the last teardown lines — but never let a lingering
    // grandchild delay the exit notice.
    let status = child.wait().await;
    let grace = if is_direct { 2 } else { 10 }; // `conda run` adds a wrapper between us and her
    let _ = tokio::time::timeout(std::time::Duration::from_secs(grace), async {
        let _ = a.await;
        let _ = b.await;
    })
    .await;
    let result = match status {
        Ok(s) => Ok(s.code()),
        Err(e) => Err(format!("Error: {}", e)),
    };
    let _ = tx.send(SpawnItem::Exited(result));
}

/// `Subscription`-driven follower of a spawned Sapphire process. **Lazy** — the
/// process starts on first poll (inside the tokio runtime), so building the stream
/// repeatedly in `subscription()` is cheap and iced parks it when idle. That's the
/// fix for the `Task::stream` redraw-spin (idle ~55% CPU). Each poll drains all
/// queued lines into one `SapphireLines` batch (one redraw per burst), then emits a
/// final `SapphireExited`.
pub(crate) fn spawn_log_stream(install_path: String, pid_store: Arc<AtomicU32>) -> impl futures::Stream<Item = Message> {
    enum St {
        Start,
        Run(tokio::sync::mpsc::UnboundedReceiver<SpawnItem>),
        ExitPending(Result<Option<i32>, String>),
        Done,
    }
    futures::stream::unfold(St::Start, move |state| {
        let install_path = install_path.clone();
        let pid_store = pid_store.clone();
        async move {
            let mut rx = match state {
                St::Start => {
                    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                    // Spawned here (inside the polled stream → tokio runtime context).
                    tokio::spawn(spawn_reader(install_path, pid_store, tx));
                    rx
                }
                St::Run(rx) => rx,
                St::ExitPending(msg) => return Some((Message::SapphireExited(msg), St::Done)),
                St::Done => return None,
            };
            let first = rx.recv().await?;
            let mut batch = Vec::new();
            let mut exit = None;
            match first {
                SpawnItem::Line(l) => batch.push(l),
                SpawnItem::Exited(m) => exit = Some(m),
            }
            if exit.is_none() {
                loop {
                    match rx.try_recv() {
                        Ok(SpawnItem::Line(l)) => {
                            batch.push(l);
                            if batch.len() >= 2000 { break; }
                        }
                        Ok(SpawnItem::Exited(m)) => { exit = Some(m); break; }
                        Err(_) => break,
                    }
                }
            }
            // One Message per step: if a poll caught both lines and the exit, emit the
            // lines now and the exit next (St::ExitPending).
            match exit {
                Some(m) if batch.is_empty() => Some((Message::SapphireExited(m), St::Done)),
                Some(m) => Some((Message::SapphireLines(batch), St::ExitPending(m))),
                None => Some((Message::SapphireLines(batch), St::Run(rx))),
            }
        }
    })
}


// ── Tests: the pure helpers (any OS) ──────────────────────────────────────────
#[cfg(test)]
mod pure_tests {
    use super::*;

    #[test]
    fn shim_round_trips_awkward_paths_and_is_current() {
        let cwd = r#"C:\Users\Krem's "lab"\sapphire"#;
        let log = r#"C:\Users\Krem's "lab"\AppData\Roaming\Sapphire\sapphire-launcher.log"#;
        let src = shim_source(cwd, log);
        assert!(shim_is_current(&src));
        assert_eq!(shim_cwd(&src).as_deref(), Some(cwd));
        assert!(src.contains("sys.path.insert(0, _cwd)"), "{}", src);
        assert!(src.contains("open(_log, \"w\""), "{}", src);
        assert!(src.contains("os.path.join(_cwd, \"main.py\")"), "{}", src);
        // The log is opened before chdir: a bad folder still leaves a trace.
        assert!(src.find("open(_log").unwrap() < src.find("os.chdir(").unwrap());
    }

    #[test]
    fn v1_shim_is_stale_but_its_folder_is_recoverable() {
        let v1 = "import runpy, sys, os\r\nos.chdir(\"C:\\\\Users\\\\krem\\\\sapphire\")\r\n_log = \"x\"\r\nrunpy.run_path(\"main.py\", run_name=\"__main__\")\r\n";
        assert!(!shim_is_current(v1));
        assert_eq!(shim_cwd(v1).as_deref(), Some(r"C:\Users\krem\sapphire"));
        assert!(!shim_is_current(""));
        assert_eq!(shim_cwd("print(1)"), None);
        assert!(!shim_is_current("# sapphire-launcher autostart shim v1 (old)\n"));
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

// ── Tests (Linux dev box): the spawn follower + graceful stop, end to end ─────
// Uses the real sapphire env python to run a throwaway fake main.py, so the whole
// chain is exercised: spawn in its own process group, pid stored, lines pumped,
// SIGTERM via signal_stop, exit code reported, last teardown lines not lost.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::time::Duration;

    /// A throwaway install dir holding a fake `main.py` with the given body.
    fn fake_install(name: &str, body: &str) -> String {
        let dir = std::env::temp_dir()
            .join(format!("sapphire-launcher-test-{}-{}", std::process::id(), name));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("main.py"), body).unwrap();
        dir.to_string_lossy().into_owned()
    }

    /// Drive the follower until it reports the exit. `on_line` sees every log line
    /// (the test reacts to the fake runner's "starting" line from there).
    async fn run_until_exit(
        stream: impl futures::Stream<Item = Message>,
        mut on_line: impl FnMut(&str),
    ) -> (Vec<String>, Result<Option<i32>, String>) {
        futures::pin_mut!(stream);
        let mut lines = Vec::new();
        while let Some(msg) = stream.next().await {
            match msg {
                Message::SapphireLines(batch) => {
                    for l in &batch { on_line(l); }
                    lines.extend(batch);
                }
                Message::SapphireExited(r) => return (lines, r),
                _ => {}
            }
        }
        panic!("follower ended without an exit notice");
    }

    #[tokio::test]
    async fn graceful_stop_exits_zero_and_keeps_last_lines() {
        if sapphire_env_python().is_none() {
            eprintln!("skipped: no sapphire env python on this box");
            return;
        }
        let dir = fake_install("graceful", r#"
import signal, sys, time
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
"#);
        let pid = Arc::new(AtomicU32::new(0));
        let p = pid.clone();
        let mut signaled = false;
        let fut = run_until_exit(spawn_log_stream(dir, pid.clone()), move |line| {
            // Signal only once the handler is installed (the fake says so).
            if !signaled && line.contains("fake starting") {
                signaled = true;
                let pid = p.load(Ordering::Relaxed);
                assert!(pid > 1, "pid not stored before the first line");
                // The runner must lead its own process group — signal_group refuses
                // anything else, and this is what keeps the signal off the session.
                assert_eq!(process_group_of(pid), Some(pid as i32), "runner is not a process-group leader");
                assert!(signal_stop(pid), "signal_stop could not send SIGTERM");
            }
        });
        let (lines, exit) = tokio::time::timeout(Duration::from_secs(15), fut)
            .await
            .expect("follower did not report an exit in time");
        assert_eq!(exit, Ok(Some(0)));
        assert!(lines.iter().any(|l| l.contains("fake stopping")), "teardown line lost: {:?}", lines);
    }

    #[tokio::test]
    async fn twin_exit_code_is_passed_through() {
        if sapphire_env_python().is_none() {
            eprintln!("skipped: no sapphire env python on this box");
            return;
        }
        let dir = fake_install(
            "twin",
            "import sys\nprint('[Runner] Another Sapphire is already running (fake)', flush=True)\nsys.exit(43)\n",
        );
        let pid = Arc::new(AtomicU32::new(0));
        let (lines, exit) = tokio::time::timeout(
            Duration::from_secs(15),
            run_until_exit(spawn_log_stream(dir, pid), |_| {}),
        )
        .await
        .expect("follower did not report an exit in time");
        assert_eq!(exit, Ok(Some(43)));
        assert!(lines.iter().any(|l| l.contains("Another Sapphire")), "runner line lost: {:?}", lines);
    }
}
