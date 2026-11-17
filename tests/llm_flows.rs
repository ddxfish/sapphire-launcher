//! The local LLM's real flows, end to end: the real binary, headless, in a scratch
//! home of its own. Offline and with no GPU: a folder of fixtures stands in for
//! GitHub and Hugging Face, and the binary itself answers as a stand-in llama-server
//! (both debug-build seams, see "Test seams" in src/llm.rs). What unit tests can't
//! reach is here: processes that outlive each other, files they share, signals.
#![cfg(target_os = "linux")] // /proc, and curl dying with its worker, are Linux

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const EXE: &str = env!("CARGO_BIN_EXE_sapphire-launcher");
/// `LLAMA_PIN` in src/llm.rs: the release a first install asks for.
const PIN: &str = "b12198";
const MODEL: &str = "tiny-Q4_K_M.gguf";

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// One scratch computer: a home folder, fixtures to download from, and two ports.
struct Scratch {
    root: PathBuf,
    web_port: u16,
    model: Vec<u8>,
    /// Where downloads come from: the fixtures folder, or a `Slowly` server's address.
    from: String,
}

impl Scratch {
    fn new(name: &str) -> Scratch {
        let root = std::env::temp_dir().join(format!("sl-flows-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&root);
        let (llm_port, web_port) = (free_port(), free_port());
        let cfg = root.join("home/.config/sapphire");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join("llm.json"), format!(r#"{{"port": {}, "ctx": 4096}}"#, llm_port)).unwrap();

        // The engine: each release file is a small archive. The server one holds a
        // link named llama-server to this very binary, which answers as the engine.
        let gh = root.join(format!("fixtures/gh/{}", PIN));
        std::fs::create_dir_all(&gh).unwrap();
        let pack = |name: &str, fill: &dyn Fn(&Path)| -> u64 {
            let stage = root.join("stage").join(name);
            std::fs::create_dir_all(&stage).unwrap();
            fill(&stage);
            let out = gh.join(name);
            assert!(Command::new("tar").arg("-czf").arg(&out).arg("-C").arg(&stage).arg(".").status().unwrap().success());
            std::fs::metadata(&out).unwrap().len()
        };
        let server = |d: &Path| std::os::unix::fs::symlink(EXE, d.join("llama-server")).unwrap();
        let runtime = |d: &Path| std::fs::write(d.join("libstandin.so"), b"x").unwrap();
        let assets: Vec<serde_json::Value> = [
            ("llama-b1-bin-ubuntu-cuda-13.5-x64.tar.gz", &server as &dyn Fn(&Path)),
            ("cudart-llama-b1-bin-ubuntu-cuda-13.5-x64.tar.gz", &runtime),
            ("llama-b1-bin-ubuntu-vulkan-x64.tar.gz", &server),
            ("llama-b1-bin-ubuntu-x64.tar.gz", &server),
        ]
        .into_iter()
        .map(|(name, fill)| serde_json::json!({"name": name, "size": pack(name, fill)}))
        .collect();
        std::fs::write(gh.join("release.json"), serde_json::json!({"assets": assets}).to_string()).unwrap();

        // The model: 2 MB with a pattern, so a resumed download can be checked byte for byte.
        let hf = root.join("fixtures/hf/test/tiny-GGUF");
        std::fs::create_dir_all(&hf).unwrap();
        let model: Vec<u8> = (0..2_000_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(hf.join(MODEL), &model).unwrap();
        std::fs::write(hf.join("api.json"), serde_json::json!({"siblings": [{"rfilename": MODEL, "size": model.len()}, {"rfilename": "README.md", "size": 3}]}).to_string()).unwrap();

        // A Sapphire install as the watcher sees one: a core with no lock, asked at her port.
        let her = root.join("home/sapphire");
        std::fs::create_dir_all(her.join(".git")).unwrap();
        std::fs::create_dir_all(her.join("core")).unwrap();
        std::fs::create_dir_all(her.join("user")).unwrap();
        for f in ["main.py", "sapphire.py", "core/setup.py"] {
            std::fs::write(her.join(f), b"").unwrap();
        }
        std::fs::write(her.join("user/settings.json"), format!(r#"{{"server": {{"WEB_UI_PORT": {}}}}}"#, web_port)).unwrap();
        let from = root.join("fixtures").display().to_string();
        Scratch { root, web_port, model, from }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(EXE);
        c.args(args)
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("home/.config"))
            .env("XDG_DATA_HOME", self.root.join("home/.local/share"))
            .env("SAPPHIRE_LLM_FIXTURES", &self.from)
            .env_remove("APPIMAGE")
            .env_remove("SAPPHIRE_LLM_WATCH_SECS")
            .stdin(Stdio::null());
        c
    }

    /// One CLI call: (exit code, the JSON it printed).
    fn run(&self, c: &mut Command) -> (i32, serde_json::Value) {
        let out = c.output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        (out.status.code().unwrap_or(-1), serde_json::from_str(text.trim()).unwrap_or_else(|_| panic!("not JSON: {:?} / {:?}", text, String::from_utf8_lossy(&out.stderr))))
    }

    fn cli(&self, args: &[&str]) -> serde_json::Value {
        self.run(&mut self.cmd(args)).1
    }

    fn status(&self) -> serde_json::Value {
        self.cli(&["--llm-status"])
    }

    /// Poll the status until `ok` says so; panic with the last one after `secs`.
    fn wait(&self, what: &str, secs: u64, ok: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
        let until = Instant::now() + Duration::from_secs(secs);
        loop {
            let st = self.status();
            if ok(&st) {
                return st;
            }
            assert!(Instant::now() < until, "waited {} s for {}: phase={} state={} job={} last_job={}", secs, what, st["phase"], st["state"], st["job"], st["last_job"]);
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    fn config(&self, name: &str) -> PathBuf {
        self.root.join("home/.config/sapphire").join(name)
    }

    fn models(&self) -> PathBuf {
        self.root.join("home/.local/share/sapphire/llm/gguf")
    }

    fn files_in_models(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(self.models()).into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
        v.sort();
        v
    }

    /// Engine and model in, nothing running.
    fn set_up(&self) {
        assert_eq!(self.cli(&["--llm-download", "test/tiny-GGUF"])["ok"], true);
        let st = self.wait("the download to finish", 60, |s| s["job"].is_null() && s["last_job"]["state"] != "interrupted" && !s["last_job"].is_null());
        assert_eq!(st["last_job"]["state"], "done", "{}", st);
        assert_eq!(st["phase"], "stopped", "{}", st);
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Whatever a failed test left running in this scratch home.
        let _ = self.cmd(&["--llm-cancel"]).output();
        let _ = self.cmd(&["--stop-llm"]).output();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A stand-in for a running Sapphire: something answering on her web port.
struct Her {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Her {
    fn up(port: u16) -> Her {
        let listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                if let Ok((mut s, _)) = listener.accept() {
                    let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        Her { stop, thread: Some(thread) }
    }
}

impl Drop for Her {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The fixtures over HTTP, with the model served slowly while `slow` is set, so a
/// download lasts long enough to cancel or kill. It answers Range requests, which
/// is how curl resumes.
struct Slowly {
    port: u16,
    slow: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    /// The largest offset a request asked to continue from (0 = nobody resumed).
    resumed_from: Arc<std::sync::atomic::AtomicUsize>,
}

impl Slowly {
    fn serve(root: PathBuf) -> Slowly {
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let (slow, stop) = (Arc::new(AtomicBool::new(true)), Arc::new(AtomicBool::new(false)));
        let (slow2, stop2) = (slow.clone(), stop.clone());
        let resumed_from = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let resumed2 = resumed_from.clone();
        std::thread::spawn(move || {
            while !stop2.load(Ordering::Relaxed) {
                let Ok((mut conn, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                };
                let (root, slow, stop, resumed) = (root.clone(), slow2.clone(), stop2.clone(), resumed2.clone());
                std::thread::spawn(move || {
                    conn.set_nonblocking(false).unwrap();
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") && conn.read(&mut byte).unwrap_or(0) == 1 {
                        head.push(byte[0]);
                    }
                    let head = String::from_utf8_lossy(&head).to_string();
                    let path = head.split_whitespace().nth(1).unwrap_or("/").trim_start_matches('/').to_string();
                    let from: usize = head
                        .lines()
                        .find_map(|l| l.to_lowercase().strip_prefix("range: bytes=").map(|r| r.trim_end_matches('-').trim().parse().unwrap_or(0)))
                        .unwrap_or(0);
                    resumed.fetch_max(from, Ordering::Relaxed);
                    let Ok(body) = std::fs::read(root.join(&path)) else {
                        let _ = conn.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        return;
                    };
                    if from >= body.len() {
                        let _ = conn.write_all(b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        return;
                    }
                    let rest = &body[from..];
                    let status = if from > 0 { format!("206 Partial Content\r\nContent-Range: bytes {}-{}/{}", from, body.len() - 1, body.len()) } else { "200 OK".to_string() };
                    if conn.write_all(format!("HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", status, rest.len()).as_bytes()).is_err() {
                        return;
                    }
                    for chunk in rest.chunks(16 * 1024) {
                        if stop.load(Ordering::Relaxed) || conn.write_all(chunk).is_err() {
                            return;
                        }
                        if slow.load(Ordering::Relaxed) && path.ends_with(".gguf") {
                            std::thread::sleep(Duration::from_millis(60)); // about 270 KB/s
                        }
                    }
                });
            }
        });
        Slowly { port, slow, stop, resumed_from }
    }
}

impl Drop for Slowly {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// End a child for good, whatever state the test is in.
struct Reaped(Child);
impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn a_fresh_home_has_nothing_and_says_so() {
    let s = Scratch::new("fresh");
    let st = s.status();
    assert_eq!((st["phase"].as_str(), st["set_up"].as_bool(), st["state"].as_str()), (Some("none"), Some(false), Some("off")), "{}", st);
    assert!(st["job"].is_null() && st["last_job"].is_null() && st["running"].is_null());
    assert_eq!(st["sapphire_running"], false);
    // Asking for what can't be done is an error in JSON, exit 1, and changes nothing.
    let (code, out) = s.run(&mut s.cmd(&["--start-llm"]));
    assert!(code == 1 && out["ok"] == false, "{}", out);
    let (code, out) = s.run(&mut s.cmd(&["--llm-model", "nothing-here"]));
    assert!(code == 1 && out["error"].as_str().unwrap().contains("nothing-here"), "{}", out);
    let (code, _) = s.run(&mut s.cmd(&["--llm-cancel"]));
    assert_eq!(code, 1);
    let (code, out) = s.run(&mut s.cmd(&["--llm-status", "--stop-llm"]));
    assert!(code == 1 && out["error"].as_str().unwrap().contains("One switch at a time"), "{}", out);
    assert_eq!(s.status()["phase"], "none");
}

#[test]
fn install_download_start_restart_stop() {
    let s = Scratch::new("happy");
    s.set_up();
    let st = s.status();
    assert_eq!(st["set_up"], true);
    assert!(st["models"].as_array().unwrap().iter().any(|m| m["path"].as_str().unwrap().ends_with(MODEL) && m["default"] == true), "{}", st);
    assert_eq!(s.files_in_models(), [MODEL], "no part file, no note of where it came from, left behind");
    assert_eq!(std::fs::read(s.models().join(MODEL)).unwrap(), s.model);
    assert_eq!(st["settings"]["on"], true, "a first download with nothing chosen switches it on");
    // A second start is the same server.
    let started = s.cli(&["--start-llm"]);
    assert_eq!(started["ok"], true, "{}", started);
    let up = s.wait("the server to answer", 20, |s| s["phase"] == "running");
    assert_eq!(up["running"]["pid"], started["pid"]);
    assert!(s.config("llm-run.json").exists());
    assert_eq!(s.cli(&["--start-llm"])["pid"], started["pid"], "starting what runs is a no-op");
    // Restart: a new process on the same address.
    let again = s.cli(&["--restart-llm"]);
    assert!(again["ok"] == true && again["pid"] != started["pid"] && again["base_url"] == started["base_url"], "{}", again);
    s.wait("it to answer again", 20, |s| s["phase"] == "running");
    // Stop: the process itself is gone (not just our note of it), the handshake
    // too, and stopping what is down is fine.
    let pid = again["pid"].as_u64().unwrap();
    assert!(Path::new(&format!("/proc/{}", pid)).exists());
    assert_eq!(s.cli(&["--stop-llm"])["ok"], true);
    let st = s.status();
    assert!(st["phase"] == "stopped" && st["running"].is_null() && !s.config("llm-run.json").exists(), "{}", st);
    assert!(!Path::new(&format!("/proc/{}", pid)).exists(), "the server process is still there");
    assert!(!Path::new(&format!("/proc/{}", started["pid"].as_u64().unwrap())).exists(), "the one before the restart is still there");
    assert_eq!(s.cli(&["--stop-llm"])["ok"], true);
}

#[test]
fn cancel_deletes_the_part_and_a_killed_download_resumes_byte_for_byte() {
    let mut s = Scratch::new("cancel-resume");
    // Over HTTP, the model at about 270 KB/s: time enough to cancel, and to kill.
    let server = Slowly::serve(s.root.join("fixtures"));
    s.from = format!("http://127.0.0.1:{}", server.port);
    let slow = |args: &[&str]| s.cli(args);
    let downloading = |st: &serde_json::Value| st["job"]["engine"] == false && st["job"]["done"].as_u64().unwrap_or(0) > 100_000;

    // Cancel: the worker ends curl and deletes what it had.
    assert_eq!(slow(&["--llm-download", "test/tiny-GGUF"])["ok"], true);
    let (code, twice) = s.run(&mut s.cmd(&["--llm-download", "test/tiny-GGUF"]));
    assert!(code == 1 && twice["error"].as_str().unwrap().contains("already running"), "{}", twice);
    s.wait("the model to be coming down", 30, downloading);
    assert!(s.files_in_models().contains(&format!("{}.part", MODEL)));
    assert_eq!(s.cli(&["--llm-cancel"])["ok"], true);
    let st = s.wait("the cancel to land", 10, |st| st["job"].is_null());
    assert_eq!(st["last_job"]["state"], "cancelled", "{}", st);
    assert_eq!(s.files_in_models(), Vec::<String>::new(), "cancel leaves nothing");
    assert_eq!(st["phase"], "none", "the engine is in, but with no model nothing is set up: {}", st);

    // A worker killed outright (a crash, a power cut): its curl dies with it, the
    // part stays, and the job reads as interrupted.
    assert_eq!(slow(&["--llm-download", "test/tiny-GGUF"])["ok"], true);
    s.wait("the model to be coming down again", 30, downloading);
    let report: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(s.config("llm-job.json")).unwrap()).unwrap();
    let pid = report["pid"].as_i64().unwrap() as i32;
    let runs = std::fs::read_link(format!("/proc/{}/exe", pid)).unwrap();
    assert!(pid > 1 && runs == std::fs::canonicalize(EXE).unwrap(), "the pid in the report is our worker: {:?}", runs);
    // SAFETY: a plain syscall on a positive pid just checked to be this test's own worker.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    let st = s.wait("the dead worker to be seen", 10, |st| st["job"].is_null());
    assert_eq!(st["last_job"]["state"], "interrupted", "{}", st);
    let part = s.models().join(format!("{}.part", MODEL));
    std::thread::sleep(Duration::from_millis(700));
    let kept = std::fs::metadata(&part).unwrap().len();
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(std::fs::metadata(&part).unwrap().len(), kept, "nothing is still writing the part");
    assert!(kept > 100_000 && (kept as usize) < s.model.len());

    // The same download again carries on from there, at full speed this time, and
    // the file is the model, byte for byte.
    server.slow.store(false, Ordering::Relaxed);
    assert_eq!(s.cli(&["--llm-download", "test/tiny-GGUF"])["ok"], true);
    let st = s.wait("the resumed download to finish", 30, |st| st["job"].is_null() && !st["last_job"].is_null() && st["last_job"]["state"] != "interrupted");
    assert_eq!(st["last_job"]["state"], "done", "{}", st);
    // Carried on, not started over: the server was asked for the rest only.
    let from = server.resumed_from.load(Ordering::Relaxed);
    assert!(from as u64 >= kept && from < s.model.len(), "resumed from byte {} with {} kept", from, kept);
    assert_eq!(std::fs::read(s.models().join(MODEL)).unwrap(), s.model);
    assert_eq!(s.files_in_models(), [MODEL]);
    assert_eq!(st["phase"], "stopped");
}

#[test]
fn the_watcher_starts_it_with_her_and_stops_it_with_her() {
    let s = Scratch::new("watcher");
    s.set_up();
    let watch = || {
        let mut c = s.cmd(&["--llm-watch"]);
        c.env("SAPPHIRE_LLM_WATCH_SECS", "1").env("SAPPHIRE_LLM_GRACE_SECS", "3").stdout(Stdio::null()).stderr(Stdio::null());
        c.spawn().unwrap()
    };
    let mut watcher = Reaped(watch());
    // One watcher: a second finds the lock taken and leaves at once. (Once the first
    // has had a moment to take it.)
    std::thread::sleep(Duration::from_millis(800));
    let mut second = Reaped(watch());
    let until = Instant::now() + Duration::from_secs(5);
    while second.0.try_wait().unwrap().is_none() {
        assert!(Instant::now() < until, "a second watcher stayed");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(second.0.try_wait().unwrap().unwrap().code(), Some(0));
    // She is down: nothing is started for nobody.
    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(s.status()["phase"], "stopped");

    // She comes up: so does her LLM.
    let her = Her::up(s.web_port);
    let up = s.wait("the watcher to start it", 15, |st| st["phase"] == "running");
    assert_eq!(up["sapphire_running"], true);
    // Stop is respected while she runs: it stays down...
    assert_eq!(s.cli(&["--stop-llm"])["ok"], true);
    std::thread::sleep(Duration::from_millis(3500));
    assert_eq!(s.status()["phase"], "stopped", "held");
    // ...until someone starts it again.
    assert_eq!(s.cli(&["--start-llm"])["ok"], true);
    s.wait("it to answer", 15, |st| st["phase"] == "running");
    std::thread::sleep(Duration::from_millis(2500)); // a round sees the two together

    // She goes down: it goes with her, process and all.
    let pid = s.status()["running"]["pid"].as_u64().unwrap();
    drop(her);
    s.wait("the watcher to stop it", 15, |st| st["phase"] == "stopped");
    assert!(!Path::new(&format!("/proc/{}", pid)).exists(), "the watcher's stop left the server running");
    // Started on its own while she is down: left alone.
    assert_eq!(s.cli(&["--start-llm"])["ok"], true);
    s.wait("it to answer", 15, |st| st["phase"] == "running");
    std::thread::sleep(Duration::from_millis(3500));
    assert_eq!(s.status()["phase"], "running", "an LLM someone started on its own is theirs");
    // The same server, but noted as started for her (what Launch does), and she
    // never comes up: once she has had her time it is taken down.
    let pid = s.status()["running"]["pid"].as_u64().unwrap();
    std::fs::write(s.config("llm-for-her"), pid.to_string()).unwrap();
    s.wait("the watcher to stop the one started for her", 15, |st| st["phase"] == "stopped");
    assert!(!Path::new(&format!("/proc/{}", pid)).exists(), "the server started for her outlived her");

    // Switched off (the dropdown's None): the watcher ends by itself.
    let cfg = s.config("llm.json");
    let mut v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
    assert!(v["off"] == false && v["model"].as_str().unwrap().ends_with(MODEL));
    v["off"] = true.into();
    std::fs::write(&cfg, v.to_string()).unwrap();
    let until = Instant::now() + Duration::from_secs(10);
    while watcher.0.try_wait().unwrap().is_none() {
        assert!(Instant::now() < until, "the watcher did not end by itself");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(watcher.0.try_wait().unwrap().unwrap().code(), Some(0));
}
