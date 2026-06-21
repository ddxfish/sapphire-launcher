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

