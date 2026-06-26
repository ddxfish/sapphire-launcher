// systemd --user service detection + log/launch streams.
use crate::*;
use crate::platform::*;
use std::sync::{Arc, atomic::{AtomicU32, Ordering}};
use tokio::io::BufReader;

#[derive(Debug, Clone)]
pub(crate) struct ServiceInfo {
    pub(crate) active: bool,            // ActiveState == "active" (running now)
    pub(crate) active_state: String,    // raw ActiveState (active/failed/activating/inactive)
    pub(crate) enabled: bool,           // UnitFileState == "enabled" (autostart at sign-in)
    pub(crate) sub_state: String,
    pub(crate) working_dir: Option<String>,
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
            "-p", "UnitFileState", "-p", "WorkingDirectory",
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (mut load, mut active, mut sub, mut ufs, mut wd) = ("", "", "", "", "");
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("LoadState=") { load = v; }
        else if let Some(v) = line.strip_prefix("ActiveState=") { active = v; }
        else if let Some(v) = line.strip_prefix("SubState=") { sub = v; }
        else if let Some(v) = line.strip_prefix("UnitFileState=") { ufs = v; }
        else if let Some(v) = line.strip_prefix("WorkingDirectory=") { wd = v; }
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
    })
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
    if let Some(parent) = shim.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            return (format!("Couldn't create the autostart folder: {}", e), false);
        }
    }
    // Poor-man's log rotation: truncate the logfile on each Enable so it can't grow
    // unboundedly across re-enables. The shim still opens it in append mode, so logs
    // accumulate within a single logon session (across main.py's crash-restarts).
    let _ = tokio::fs::write(&log, "").await;
    // Python string literal: escape backslashes then double-quotes (handles spaces and
    // any path safely — avoids raw-string's can't-end-in-backslash gotcha).
    let py_lit = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let shim_src = format!(
        "import runpy, sys, os\r\n\
         os.chdir(\"{cwd}\")\r\n\
         _log = \"{log}\"\r\n\
         os.makedirs(os.path.dirname(_log), exist_ok=True)\r\n\
         sys.stdout = sys.stderr = open(_log, \"a\", buffering=1, encoding=\"utf-8\", errors=\"replace\")\r\n\
         runpy.run_path(\"main.py\", run_name=\"__main__\")\r\n",
        cwd = py_lit(&install_path),
        log = py_lit(&log.to_string_lossy()),
    );
    if let Err(e) = tokio::fs::write(&shim, shim_src).await {
        return (format!("Couldn't write the autostart launcher: {}", e), false);
    }

    let esc = |s: &str| s.replace('\'', "''"); // PowerShell single-quote escaping

    // Registration script — normal PS single-quoted strings we control (dodges the
    // nested-ArgumentList quoting hell of passing this inline to an elevated shell).
    // The action runs pythonw on the shim; the shim path is wrapped in literal double
    // quotes inside the argument so a path with spaces stays a single argument.
    let script = format!(
        "$ErrorActionPreference='Stop'\r\n\
         $a = New-ScheduledTaskAction -Execute '{pyw}' -Argument '\"{shim}\"' -WorkingDirectory '{cwd}'\r\n\
         $t = New-ScheduledTaskTrigger -AtLogOn -User '{user}'\r\n\
         $p = New-ScheduledTaskPrincipal -UserId '{user}' -LogonType Interactive\r\n\
         $s = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero) -StartWhenAvailable\r\n\
         Register-ScheduledTask -TaskName Sapphire -Action $a -Trigger $t -Principal $p -Settings $s -Force | Out-Null\r\n",
        pyw = esc(&pyw), shim = esc(&shim.to_string_lossy()), cwd = esc(&install_path), user = esc(&user),
    );
    let ps1 = std::env::temp_dir().join("sapphire-enable.ps1");
    if let Err(e) = tokio::fs::write(&ps1, script).await {
        return (format!("Couldn't write the setup script: {}", e), false);
    }
    // Elevate (UAC) and run the .ps1.
    let outer = format!(
        "$ErrorActionPreference='Stop'; try {{ $p = Start-Process powershell -Verb RunAs -WindowStyle Hidden -PassThru -Wait -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File','{}'; exit $p.ExitCode }} catch {{ exit 1223 }}",
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
        Err(_) => ("Couldn't enable autostart — the admin prompt may have been declined. Try again and choose Yes.".to_string(), false),
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
    let outer = "$ErrorActionPreference='Stop'; try { $p = Start-Process powershell -Verb RunAs -WindowStyle Hidden -PassThru -Wait -ArgumentList '-NoProfile','-Command','Unregister-ScheduledTask -TaskName Sapphire -Confirm:$false'; exit $p.ExitCode } catch { exit 1223 }".to_string();
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

/// Read the service EnvironmentFile (KEY=VALUE per line). Empty if absent.
#[cfg(not(windows))]
pub(crate) async fn read_env_file() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    tokio::fs::read_to_string(format!("{}/.config/sapphire/service.env", home)).await.unwrap_or_default()
}
#[cfg(windows)]
pub(crate) async fn read_env_file() -> String { String::new() }

/// Write the service EnvironmentFile (chmod 600 — it can hold API keys).
#[cfg(not(windows))]
pub(crate) async fn write_env_file(content: String) -> bool {
    let home = std::env::var("HOME").unwrap_or_default();
    let dir = format!("{}/.config/sapphire", home);
    if tokio::fs::create_dir_all(&dir).await.is_err() { return false; }
    let path = format!("{}/service.env", dir);
    if tokio::fs::write(&path, content).await.is_err() { return false; }
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    true
}
#[cfg(windows)]
pub(crate) async fn write_env_file(_content: String) -> bool { false }

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
    cmd.args(&args).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
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

/// One message from the spawned Sapphire process: a log line, or the final exit notice.
enum SpawnItem {
    Line(String),
    Exited(String),
}

/// Spawn `python -u main.py` and pump its stdout+stderr (lossy UTF-8, ANSI-stripped,
/// `\r` progress bars collapsed) as `SpawnItem::Line`s into `tx`, then one final
/// `SpawnItem::Exited`. `kill_on_drop` means dropping the follower subscription also
/// tears the process down (the explicit Stop sweep reaps the child workers).
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
    { cmd.creation_flags(CREATE_NO_WINDOW); }
    #[cfg(unix)]
    {
        // Own process group so Stop can kill the whole tree (child workers).
        cmd.process_group(0);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(SpawnItem::Exited(format!("Failed to start: {}", e)));
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
    let (_, _, status) = tokio::join!(a, b, child.wait());

    let msg = match status {
        Ok(s) if s.success() => "Sapphire exited.".to_string(),
        Ok(s) if s.code() == Some(42) => "Sapphire is restarting...".to_string(),
        Ok(s) => format!("Sapphire exited (code {}).", s.code().unwrap_or(-1)),
        Err(e) => format!("Error: {}", e),
    };
    let _ = tx.send(SpawnItem::Exited(msg));
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
        ExitPending(String),
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

