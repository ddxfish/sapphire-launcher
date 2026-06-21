// systemd --user service detection + log/launch streams.
use crate::*;
use crate::platform::*;
use std::sync::{Arc, atomic::{AtomicU32, Ordering}};
use tokio::io::BufReader;

#[derive(Debug, Clone)]
pub(crate) struct ServiceInfo {
    pub(crate) active: bool,
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
            "-p", "LoadState", "-p", "ActiveState",
            "-p", "SubState", "-p", "WorkingDirectory",
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (mut load, mut active, mut sub, mut wd) = ("", "", "", "");
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("LoadState=") { load = v; }
        else if let Some(v) = line.strip_prefix("ActiveState=") { active = v; }
        else if let Some(v) = line.strip_prefix("SubState=") { sub = v; }
        else if let Some(v) = line.strip_prefix("WorkingDirectory=") { wd = v; }
    }
    if load != "loaded" {
        return None; // unit doesn't exist
    }
    Some(ServiceInfo {
        active: active == "active",
        sub_state: sub.to_string(),
        working_dir: if wd.is_empty() { None } else { Some(wd.to_string()) },
    })
}

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
#[cfg(windows)]
pub(crate) async fn install_service(_install_path: String) -> (String, bool) {
    ("Service install isn't available on Windows yet.".to_string(), false)
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
#[cfg(windows)]
pub(crate) async fn remove_service() -> (String, bool) {
    ("No service to remove on Windows yet.".to_string(), false)
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

pub(crate) fn launch_sapphire_stream(install_path: String, pid_store: Arc<AtomicU32>) -> impl futures::Stream<Item = Message> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

    tokio::task::spawn(async move {
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
            .env("PYTHONUTF8", "1");
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        #[cfg(unix)]
        {
            // Own process group so Stop can kill the whole tree (child workers).
            cmd.process_group(0);
        }

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

        if is_direct {
            let _ = tx.send(Message::SapphireLine(format!("Using {}", program)));
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

