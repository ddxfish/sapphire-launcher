// Local LLM: mainline llama.cpp `llama-server`, one model at a time, run detached
// for Sapphire the same way she is: started by pid, found by a handshake file whose
// pid is identity-checked, stopped by pid. Binaries and models live in the per-user
// data folder (`app_data_dir()/llm/`), not in any one install: the LLM is Sapphire's
// sibling. See plans/local-llm.md and plans/llm-easy-mode.md.
use crate::platform::*;
use std::path::{Path, PathBuf};

/// The llama.cpp build a fresh install gets. Releases land several times a day;
/// bump this deliberately (Advanced → Update moves past it per machine).
pub(crate) const LLAMA_PIN: &str = "b12198";
const LLAMA_REPO: &str = "ggml-org/llama.cpp";
pub(crate) const LLM_DEFAULT_PORT: u16 = 8642;
/// 0 = Auto: the most of AUTO_CTX that keeps the model whole on the card (see auto_ctx).
pub(crate) const DEFAULT_CTX: u32 = 0;
pub(crate) const AUTO_CTX: &[u32] = &[65536, 32768, 16384];
pub(crate) const CTX_CHOICES: &[u32] = &[0, 4096, 8192, 16384, 32768, 65536, 131072];
/// VRAM left for Sapphire's own voice stack (~2.5 GB measured) and the desktop.
const VRAM_RESERVE_MIB: u64 = 3500;
/// RAM left for the OS, Sapphire and the browser.
const RAM_RESERVE_MIB: u64 = 4096;
#[cfg(windows)]
pub(crate) const SERVER_EXE: &str = "llama-server.exe";
#[cfg(not(windows))]
pub(crate) const SERVER_EXE: &str = "llama-server";

// ── The curated list ───────────────────────────────────────────────────────

pub(crate) struct Model {
    pub name: &'static str,
    pub repo: &'static str,
    pub file: &'static str,
    pub bytes: u64,
    /// Rough KV cache per 8K of context. A guess per family; `--fit` does the real
    /// sizing at load, this only colours the list.
    pub kv8k_mib: u64,
    pub blurb: &'static str,
    /// Can be the recommendation (an untested model never is).
    pub pick: bool,
}

/// Ordered weakest → strongest: the recommendation is the last pickable one that fits.
pub(crate) const MODELS: &[Model] = &[
    Model { name: "Qwen3.5 4B", repo: "unsloth/Qwen3.5-4B-GGUF", file: "Qwen3.5-4B-Q4_K_M.gguf", bytes: 2_700_000_000, kv8k_mib: 300, blurb: "Small and quick. Great with tools for its size.", pick: true },
    Model { name: "Gemma 4 E4B", repo: "unsloth/gemma-4-E4B-it-GGUF", file: "gemma-4-E4B-it-Q4_K_M.gguf", bytes: 5_000_000_000, kv8k_mib: 300, blurb: "Light and friendly, good at everyday chat.", pick: true },
    Model { name: "Qwen3.5 9B", repo: "unsloth/Qwen3.5-9B-GGUF", file: "Qwen3.5-9B-Q4_K_M.gguf", bytes: 5_700_000_000, kv8k_mib: 300, blurb: "Solid all-rounder for mid-range cards.", pick: true },
    Model { name: "Gemma 4 12B", repo: "unsloth/gemma-4-12b-it-GGUF", file: "gemma-4-12b-it-Q4_K_M.gguf", bytes: 7_121_861_440, kv8k_mib: 500, blurb: "A favourite for chat: warm, and good with words.", pick: false },
    Model { name: "Qwen3.6 27B", repo: "unsloth/Qwen3.6-27B-GGUF", file: "Qwen3.6-27B-Q4_K_M.gguf", bytes: 16_800_000_000, kv8k_mib: 1000, blurb: "Warmer, more personality.", pick: true },
    Model { name: "Qwen3.8 27B", repo: "unsloth/Qwen3.8-27B-GGUF", file: "Qwen3.8-27B-UD-Q4_K_M.gguf", bytes: 16_500_000_000, kv8k_mib: 1000, blurb: "The smartest here. Best reasoning and tools.", pick: true },
    Model { name: "Qwen3.6 35B-A3B", repo: "unsloth/Qwen3.6-35B-A3B-GGUF", file: "Qwen3.6-35B-A3B-UD-Q4_K_M.gguf", bytes: 22_100_000_000, kv8k_mib: 700, blurb: "Big and still quick, even when it spills out of VRAM.", pick: false },
];

pub(crate) fn curated(file: &str) -> Option<&'static Model> {
    MODELS.iter().find(|m| m.file == file)
}

// ── Where things live ──────────────────────────────────────────────────────

/// The engine, its download scratch and (by default) the models.
pub(crate) fn llm_root() -> PathBuf {
    // Tests get a scratch root: never the real engine and models.
    #[cfg(test)]
    return std::env::temp_dir().join(format!("sl-test-llm-root-{}", std::process::id()));
    #[cfg(not(test))]
    app_data_dir().join("llm")
}
/// Where downloads go and the list looks: the folder you chose, else the default.
pub(crate) fn gguf_dir(cfg: &LlmConfig) -> PathBuf {
    cfg.models_dir.clone().unwrap_or_else(|| llm_root().join("gguf"))
}
/// The small files controllers share: settings, the job report, its cancel flag.
fn state_file(name: &str) -> PathBuf {
    // Tests never read or write the real ones.
    #[cfg(test)]
    return std::env::temp_dir().join(format!("sl-test-{}-{}", std::process::id(), name));
    #[cfg(not(test))]
    app_config_dir().join(name)
}
fn config_path() -> PathBuf {
    state_file("llm.json")
}
pub(crate) fn job_path() -> PathBuf {
    state_file("llm-job.json")
}
fn cancel_path() -> PathBuf {
    state_file("llm-job.cancel")
}
/// (changed, size) of llm.json: another launcher, the worker or Sapphire's CLI call
/// may have written it since this one read it.
pub(crate) fn config_stamp() -> Option<(std::time::SystemTime, u64)> {
    // Tests share one scratch file between threads; they keep what is in memory.
    if cfg!(test) {
        return None;
    }
    let m = std::fs::metadata(config_path()).ok()?;
    Some((m.modified().ok()?, m.len()))
}
/// The handshake: written at every start. Sapphire reads it to find her local brain.
pub(crate) fn run_path() -> PathBuf {
    state_file("llm-run.json")
}
pub(crate) fn log_path() -> PathBuf {
    state_file("llm.log")
}

/// Is a lock file held by a live process? (The OS frees it when the holder dies.)
/// (Asking takes the lock for an instant, which is why the real takers, `take_lock`,
/// try a few times before they believe someone else has it.)
fn lock_taken(name: &str) -> bool {
    let Ok(f) = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(state_file(name)) else { return false };
    match f.try_lock() {
        Ok(()) => {
            // Let go at once, and say so: Windows may otherwise keep a lock on a
            // closed handle for a while.
            let _ = f.unlock();
            false
        }
        Err(_) => true,
    }
}
/// Take a lock for this process's life. None = someone else really holds it.
fn take_lock(name: &str) -> Option<std::fs::File> {
    let path = state_file(name);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path).ok()?;
    for _ in 0..6 {
        if f.try_lock().is_ok() {
            return Some(f);
        }
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
    None
}
pub(crate) fn watcher_running() -> bool {
    lock_taken("llm-watch.lock")
}
pub(crate) fn worker_running() -> bool {
    lock_taken("llm-job.lock")
}

// ── Settings (llm.json) ────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Build {
    Cuda,
    Vulkan,
    Cpu,
}

impl Build {
    pub(crate) fn slug(self) -> &'static str {
        match self {
            Build::Cuda => "cuda",
            Build::Vulkan => "vulkan",
            Build::Cpu => "cpu",
        }
    }
    pub(crate) fn words(self) -> &'static str {
        match self {
            Build::Cuda => "CUDA",
            Build::Vulkan => "Vulkan",
            Build::Cpu => "CPU",
        }
    }
    fn from_slug(s: &str) -> Option<Build> {
        [Build::Cuda, Build::Vulkan, Build::Cpu].into_iter().find(|b| b.slug() == s)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LlmConfig {
    pub engine: Option<PathBuf>, // the llama-server binary
    pub tag: String,             // its release, e.g. b12198
    pub build: Option<Build>,
    pub model: Option<PathBuf>,
    pub ctx: u32,
    pub gpu: bool,
    pub port: u16,
    /// Listen on every network interface (another PC runs Sapphire), not just this one.
    pub lan: bool,
    /// Private mode: no server log at all. llama.cpp's error lines can quote a chat.
    pub private: bool,
    /// .gguf files of yours, used where they are (LM Studio's folder, say).
    pub files: Vec<PathBuf>,
    /// Models folder of your choosing (a bigger drive); None = the default.
    pub models_dir: Option<PathBuf>,
    /// Switched off by choice (the Install dropdown's None). The model, the engine
    /// and every other setting stay as they are: off is its own fact, not "no model",
    /// so looking around the LLM tab or a download landing can't switch it back on.
    pub off: bool,
    /// "Only run with Sapphire": up when she is, down when she is.
    pub with_sapphire: bool,
    /// "Start the LLM when I sign in", for running it on its own. Saved here and not
    /// only as the sign-in entry: while "Only run with Sapphire" is ticked the entry
    /// belongs to the watcher, and this choice has to outlive that.
    pub at_sign_in: bool,
    /// Where this launcher is, refreshed each time its window opens: the exe
    /// Sapphire calls with the `--llm-*` switches.
    pub launcher: Option<PathBuf>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        LlmConfig { engine: None, tag: String::new(), build: None, model: None, ctx: DEFAULT_CTX, gpu: true, port: LLM_DEFAULT_PORT, lan: false, private: false, files: vec![], models_dir: None, off: false, with_sapphire: true, at_sign_in: false, launcher: None }
    }
}

impl LlmConfig {
    /// The settings, or None when the file is there but can't be read or isn't
    /// whole right now (mid-write, a scanner holding it). No file = defaults. A
    /// caller must never mistake "unreadable" for "empty" and save that.
    pub(crate) fn read() -> Option<Self> {
        match std::fs::read_to_string(config_path()) {
            Ok(text) => Self::parse_checked(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(Self::default()),
            Err(_) => None,
        }
    }

    /// Read, change, save: how every headless writer touches the settings. False =
    /// they couldn't be read (after a few tries) or written, and nothing changed.
    pub(crate) fn update(change: impl FnOnce(&mut LlmConfig)) -> bool {
        let Some(mut cfg) = Self::read_patiently() else { return false };
        change(&mut cfg);
        cfg.save()
    }

    /// `read`, given half a second to get past a writer or a scanner. How every
    /// headless path gets the settings it is about to ACT on: None means stop and
    /// say so, never carry on with defaults (the wrong folder, the wrong port, an
    /// engine fetched twice).
    pub(crate) fn read_patiently() -> Option<Self> {
        for _ in 0..10 {
            if let Some(cfg) = Self::read() {
                return Some(cfg);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        None
    }

    #[cfg(test)]
    pub(crate) fn parse(text: &str) -> Self {
        Self::parse_checked(text).unwrap_or_default()
    }

    /// None = not a JSON object at all.
    pub(crate) fn parse_checked(text: &str) -> Option<Self> {
        let d = LlmConfig::default();
        let v = serde_json::from_str::<serde_json::Value>(text.trim_start_matches('\u{feff}')).ok().filter(|v| v.is_object())?;
        let path = |k: &str| v.get(k).and_then(|x| x.as_str()).filter(|s| !s.is_empty()).map(PathBuf::from);
        Some(LlmConfig {
            engine: path("engine"),
            tag: v.get("tag").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            build: v.get("build").and_then(|x| x.as_str()).and_then(Build::from_slug),
            model: path("model"),
            ctx: v.get("ctx").and_then(|x| x.as_u64()).map(|n| n as u32).filter(|n| *n == 0 || *n >= 512).unwrap_or(d.ctx),
            gpu: v.get("gpu").and_then(|x| x.as_bool()).unwrap_or(true),
            port: v.get("port").and_then(|x| x.as_u64()).and_then(|n| u16::try_from(n).ok()).filter(|p| *p > 0).unwrap_or(d.port),
            lan: v.get("lan").and_then(|x| x.as_bool()).unwrap_or(false),
            private: v.get("private").and_then(|x| x.as_bool()).unwrap_or(false),
            files: v.get("files").and_then(|x| x.as_array()).map(|a| a.iter().filter_map(|f| f.as_str()).map(PathBuf::from).collect()).unwrap_or_default(),
            models_dir: path("models_dir"),
            off: v.get("off").and_then(|x| x.as_bool()).unwrap_or(false),
            with_sapphire: v.get("with_sapphire").and_then(|x| x.as_bool()).unwrap_or(true),
            at_sign_in: v.get("at_sign_in").and_then(|x| x.as_bool()).unwrap_or(false),
            launcher: path("launcher"),
        })
    }

    /// This config with Auto context settled for its model on this hardware.
    pub(crate) fn resolved(&self, hw: Option<&Hw>) -> LlmConfig {
        let mut c = self.clone();
        if c.ctx == 0 {
            c.ctx = match (hw, &c.model) {
                (Some(hw), Some(m)) => {
                    let (bytes, kv) = model_size(m);
                    auto_ctx(bytes, kv, hw, c.gpu)
                }
                _ => 32768,
            };
        }
        c
    }

    /// llm.json as it is written (core reads `launcher`, `with_sapphire`, `engine`, `model`).
    pub(crate) fn to_json(&self) -> serde_json::Value {
        let s = |p: &Option<PathBuf>| p.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
        serde_json::json!({
            "engine": s(&self.engine), "tag": self.tag, "build": self.build.map(|b| b.slug()).unwrap_or(""),
            "model": s(&self.model), "ctx": self.ctx, "gpu": self.gpu, "port": self.port, "lan": self.lan, "private": self.private,
            "files": self.files.iter().map(|f| f.display().to_string()).collect::<Vec<_>>(),
            "models_dir": s(&self.models_dir), "off": self.off, "with_sapphire": self.with_sapphire, "at_sign_in": self.at_sign_in, "launcher": s(&self.launcher),
        })
    }

    /// False = it couldn't be written.
    pub(crate) fn save(&self) -> bool {
        write_atomic(&config_path(), &serde_json::to_string_pretty(&self.to_json()).unwrap_or_default(), false).is_ok()
    }

    /// The LLM is switched on: a model is chosen and nobody chose None since.
    pub(crate) fn is_on(&self) -> bool {
        !self.off && self.model.is_some()
    }

    /// The binary is set and still there.
    pub(crate) fn engine_ok(&self) -> bool {
        self.engine.as_ref().is_some_and(|e| e.exists())
    }
}

/// What `/v1/models` calls the model: the curated name, or the file stem.
pub(crate) fn model_alias(model: &Path) -> String {
    let file = model.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
    match curated(&file) {
        Some(m) => m.name.to_lowercase().replace(' ', "-"),
        None => file.trim_end_matches(".gguf").to_string(),
    }
}

pub(crate) fn model_label(model: &Path) -> String {
    let file = model.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
    curated(&file).map(|m| m.name.to_string()).unwrap_or(file)
}

pub(crate) fn ctx_words(ctx: u32) -> String {
    if ctx.is_multiple_of(1024) { format!("{}K", ctx / 1024) } else { ctx.to_string() }
}

/// "16k", "16K", "16384" → 16384. Anything under 512 tokens is not a context.
pub(crate) fn parse_ctx(s: &str) -> Option<u32> {
    let s = s.trim().to_lowercase();
    let n = match s.strip_suffix('k') {
        Some(k) => k.trim().parse::<u32>().ok()?.checked_mul(1024)?,
        None => s.parse::<u32>().ok()?,
    };
    (512..=1_048_576).contains(&n).then_some(n)
}

// ── The handshake (llm-run.json) ───────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LlmRun {
    pub pid: u32,
    pub port: u16,
    pub exe: PathBuf,
    pub model: PathBuf,
    pub alias: String,
    pub ctx: u32,
    pub gpu: bool,
    pub lan: bool,
    pub private: bool,
}

impl LlmRun {
    fn to_json(&self) -> String {
        let v = serde_json::json!({
            "pid": self.pid, "port": self.port, "exe": self.exe.display().to_string(),
            "model": self.model.display().to_string(), "alias": self.alias, "ctx": self.ctx, "gpu": self.gpu,
            "base_url": format!("http://127.0.0.1:{}/v1", self.port), "started": now_secs(),
            "lan": self.lan, "private": self.private, "lan_url": self.lan.then(|| lan_ip().map(|ip| format!("http://{}:{}/v1", ip, self.port))).flatten(),
        });
        serde_json::to_string_pretty(&v).unwrap_or_default()
    }

    pub(crate) fn parse(text: &str) -> Option<LlmRun> {
        let v: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
        Some(LlmRun {
            pid: u32::try_from(v.get("pid")?.as_u64()?).ok()?,
            port: u16::try_from(v.get("port")?.as_u64()?).ok()?,
            exe: PathBuf::from(s("exe")?),
            model: PathBuf::from(s("model")?),
            alias: s("alias").unwrap_or_default(),
            ctx: v.get("ctx").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
            gpu: v.get("gpu").and_then(|x| x.as_bool()).unwrap_or(true),
            lan: v.get("lan").and_then(|x| x.as_bool()).unwrap_or(false),
            private: v.get("private").and_then(|x| x.as_bool()).unwrap_or(false),
        })
    }

    /// Same model, context, device and listening as `cfg` asks for now.
    pub(crate) fn matches(&self, cfg: &LlmConfig) -> bool {
        cfg.model.as_ref() == Some(&self.model) && cfg.ctx == self.ctx && cfg.gpu == self.gpu && cfg.port == self.port && cfg.lan == self.lan && cfg.private == self.private
    }

    /// The address Sapphire uses: this computer's, or its network address when shared.
    pub(crate) fn url(&self) -> String {
        let host = if self.lan { lan_ip().map(|ip| ip.to_string()).unwrap_or_else(|| "this-pc".into()) } else { "127.0.0.1".into() };
        format!("http://{}:{}/v1", host, self.port)
    }
}

/// This computer's address on the local network: the source address the OS would
/// route outward from. UDP "connect" sends nothing; no internet needed beyond a route.
pub(crate) fn lan_ip() -> Option<std::net::IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?; // TEST-NET-1: never actually reached
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_loopback()).then_some(ip)
}

/// Paths compared the way the OS does: Windows ignores case and the `\\?\` prefix.
fn same_exe(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        let s = p.display().to_string();
        let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
        // Linux names a running program whose file was replaced "… (deleted)".
        let s = s.strip_suffix(" (deleted)").unwrap_or(s).to_string();
        if cfg!(windows) { s.to_lowercase() } else { s }
    };
    norm(a) == norm(b)
}

/// The running server, if the handshake names a live process running its binary.
/// A recycled pid runs some other program and is never taken for it.
pub(crate) fn find_running() -> Option<LlmRun> {
    let run = LlmRun::parse(&std::fs::read_to_string(run_path()).ok()?)?;
    let exe = pid_exe(run.pid)?;
    same_exe(&exe, &run.exe).then_some(run)
}

// ── Hardware ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Hw {
    pub gpu_name: Option<String>,
    pub vram_mib: u64,
    pub ram_mib: u64,
    /// NVIDIA only: (compute capability ×10, driver major). Capability 0 = a driver
    /// too old to report it, which is also too old for the CUDA build.
    pub nvidia: Option<(u32, u32)>,
    pub vulkan_ok: bool,
}

/// `llama-server --list-devices` → (name, total MiB) per device:
/// `  Vulkan0: NVIDIA GeForce RTX 5090 (32853 MiB, 26998 MiB free)`.
pub(crate) fn parse_devices(out: &str) -> Vec<(String, u64)> {
    out.lines()
        .filter_map(|l| {
            let (_, rest) = l.trim().split_once(": ")?;
            let open = rest.rfind(" (")?;
            let total = rest[open + 2..].split_whitespace().next()?.parse().ok()?;
            Some((rest[..open].to_string(), total))
        })
        .collect()
}

#[cfg(not(windows))]
fn ram_mib() -> u64 {
    let info = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    info.lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))
        .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
        .map_or(0, |kb| kb / 1024)
}

#[cfg(windows)]
fn ram_mib() -> u64 {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: the struct is zeroed with its length set, as the API requires.
    unsafe {
        let mut m: MEMORYSTATUSEX = std::mem::zeroed();
        m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut m) == 0 { 0 } else { m.ullTotalPhys / 1_048_576 }
    }
}

/// The system Vulkan loader the Vulkan build links against.
#[cfg(not(windows))]
fn vulkan_loader_present() -> bool {
    if let Ok(out) = std::process::Command::new("ldconfig").arg("-p").stdin(std::process::Stdio::null()).output()
        && String::from_utf8_lossy(&out.stdout).contains("libvulkan.so.1")
    {
        return true;
    }
    ["/usr/lib/x86_64-linux-gnu/libvulkan.so.1", "/usr/lib64/libvulkan.so.1", "/usr/lib/libvulkan.so.1"]
        .iter()
        .any(|p| Path::new(p).exists())
}

#[cfg(windows)]
fn vulkan_loader_present() -> bool {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    Path::new(&root).join("System32").join("vulkan-1.dll").exists()
}

/// What the machine has. The engine's own device list (any vendor) wins over
/// nvidia-smi once the engine is installed.
pub(crate) async fn probe(engine: Option<PathBuf>) -> Hw {
    let mut hw = Hw { ram_mib: tokio::task::spawn_blocking(ram_mib).await.unwrap_or(0), ..Hw::default() };
    hw.vulkan_ok = tokio::task::spawn_blocking(vulkan_loader_present).await.unwrap_or(false);
    if let Some(c) = best_card(&nvidia_cards().await) {
        hw.gpu_name = Some(c.name.clone());
        hw.vram_mib = c.vram_mib;
        hw.nvidia = Some((c.cap10.unwrap_or(0), c.driver));
    }
    if let Some(exe) = engine.filter(|e| e.exists()) {
        let devs = list_devices(&exe).await.unwrap_or_default();
        if let Some((name, total)) = devs.into_iter().max_by_key(|d| d.1) {
            hw.gpu_name = Some(name);
            hw.vram_mib = total;
        } else if hw.nvidia.is_none() {
            hw.gpu_name = None;
            hw.vram_mib = 0;
        }
    }
    hw
}

/// Err = the engine wouldn't run at all, which is not the same as "no devices".
async fn list_devices(exe: &Path) -> Result<Vec<(String, u64)>, String> {
    run_cmd_full_async(exe.display().to_string(), vec!["--list-devices".into()]).await.map(|o| parse_devices(&o))
}

/// CUDA 13 builds need compute capability 7.5+ (Turing) and driver 580+; older
/// NVIDIA cards (GTX 10xx) and every other GPU get Vulkan; no Vulkan loader → CPU.
pub(crate) fn pick_build(hw: &Hw) -> Build {
    match hw.nvidia {
        Some((cc, driver)) if cc >= 75 && driver >= 580 => Build::Cuda,
        _ if hw.vulkan_ok => Build::Vulkan,
        _ => Build::Cpu,
    }
}

// ── Fit and recommendation ─────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fit {
    Fits,   // all on the card (or, on CPU, in RAM)
    Spills, // part on the card, part in RAM: runs, slower
    TooBig, // not even RAM
}

pub(crate) fn need_mib(bytes: u64, kv8k_mib: u64, ctx: u32) -> u64 {
    bytes / 1_048_576 + kv8k_mib * ctx as u64 / 8192 + 600
}

pub(crate) fn fit(need: u64, hw: &Hw, gpu: bool) -> Fit {
    let ram = hw.ram_mib.saturating_sub(RAM_RESERVE_MIB);
    let vram = if gpu { hw.vram_mib.saturating_sub(VRAM_RESERVE_MIB) } else { 0 };
    if gpu && vram > 0 {
        if need <= vram {
            Fit::Fits
        } else if need <= vram + ram {
            Fit::Spills
        } else {
            Fit::TooBig
        }
    } else if need <= ram {
        Fit::Fits
    } else {
        Fit::TooBig
    }
}

/// Where a listed model lives: the launcher's download, else a file of yours with
/// the same name (LM Studio's copy, say), else where a download would go.
pub(crate) fn model_path(cfg: &LlmConfig, m: &Model) -> PathBuf {
    let ours = gguf_dir(cfg).join(m.file);
    if ours.exists() {
        return ours;
    }
    cfg.files.iter().chain(&cfg.model).find(|p| p.file_name().is_some_and(|f| f == m.file) && p.exists()).cloned().unwrap_or(ours)
}

/// Auto context: 64K if the model still fits whole on the card (in RAM on the
/// processor), else 32K, else 16K, which is the floor even when it spills.
pub(crate) fn auto_ctx(bytes: u64, kv8k_mib: u64, hw: &Hw, gpu: bool) -> u32 {
    // On the processor a big context is memory taken from everything else, for
    // prompts that would take minutes to read anyway.
    if !(gpu && hw.vram_mib > 0) {
        return 16384;
    }
    AUTO_CTX.iter().copied().find(|c| fit(need_mib(bytes, kv8k_mib, *c), hw, gpu) == Fit::Fits).unwrap_or(16384)
}

/// A model file's size and context cost: the list's numbers, or its size on disk
/// and a cautious guess for one of yours.
pub(crate) fn model_size(path: &Path) -> (u64, u64) {
    match path.file_name().and_then(|f| curated(&f.to_string_lossy())) {
        Some(m) => (m.bytes, m.kv8k_mib),
        None => (std::fs::metadata(path).map(|m| m.len()).unwrap_or(0), 1000),
    }
}

/// Model in the list → its fit at this context (0 = Auto) and device.
pub(crate) fn model_fit(m: &Model, hw: &Hw, ctx: u32, gpu: bool) -> Fit {
    let ctx = if ctx == 0 { auto_ctx(m.bytes, m.kv8k_mib, hw, gpu) } else { ctx };
    fit(need_mib(m.bytes, m.kv8k_mib, ctx), hw, gpu)
}

/// The strongest pickable model that fits, and why, in words.
pub(crate) fn recommend(hw: &Hw, ctx: u32, gpu: bool) -> (usize, String) {
    let on_card = gpu && hw.vram_mib > 0;
    // On the processor, big models fit in RAM but answer painfully slowly.
    // With little RAM, only the smallest.
    let cpu_cap = if hw.ram_mib < 24 * 1024 { 3_000_000_000 } else { 6_000_000_000 };
    let i = MODELS
        .iter()
        .enumerate()
        .rev()
        .find(|(_, m)| m.pick && model_fit(m, hw, if ctx == 0 { 32768 } else { ctx }, gpu) == Fit::Fits && (on_card || m.bytes <= cpu_cap))
        .map_or(0, |(i, _)| i);
    // Auto: the strongest that fits at 32K, then however much more it gets.
    let ctx = if ctx == 0 { auto_ctx(MODELS[i].bytes, MODELS[i].kv8k_mib, hw, gpu) } else { ctx };
    let gb = |mib: u64| format!("{:.0} GB", mib as f64 / 1024.0);
    // When nothing fits, the pick is just the smallest: say what it will be like.
    let pick_fit = fit(need_mib(MODELS[i].bytes, MODELS[i].kv8k_mib, ctx), hw, gpu);
    let card = hw.gpu_name.as_deref().unwrap_or("graphics card");
    let why = if pick_fit == Fit::TooBig {
        format!("This computer ({} RAM) doesn't have the memory for these models. The smallest is marked, but it may not load.", gb(hw.ram_mib))
    } else if on_card && pick_fit == Fit::Fits {
        format!(
            "Recommended for your {} ({}): fits fully on the card at {} context, with ~3.5 GB kept for Sapphire's voice and your desktop.",
            card, gb(hw.vram_mib), ctx_words(ctx)
        )
    } else if on_card {
        format!(
            "The smallest model here, for your {} ({}). With ~3.5 GB kept for Sapphire's voice it doesn't fit whole on the card, so part of it runs in RAM and it answers slower.",
            card, gb(hw.vram_mib)
        )
    } else if hw.vram_mib > 0 {
        format!("Running on the processor ({} RAM). Small models answer fastest there.", gb(hw.ram_mib))
    } else {
        format!("No graphics card found, so this runs on the processor ({} RAM). Small models answer fastest there.", gb(hw.ram_mib))
    };
    (i, why)
}

// ── Test seams (debug builds only) ─────────────────────────────────────────
//
// `tests/llm_flows.rs` runs the real binary through install, download, start, stop
// and the watcher, offline and with no GPU. For that a debug build can be pointed at
// a folder of fixtures in place of GitHub and Hugging Face (`SAPPHIRE_LLM_FIXTURES`),
// and with that set it also answers as a stand-in llama-server when it is run the
// way the engine is. None of this exists in a release build.

/// The fixtures' address: a folder (served as file://) or an http:// base.
fn fixtures() -> Option<String> {
    if !cfg!(debug_assertions) {
        return None;
    }
    let f = std::env::var("SAPPHIRE_LLM_FIXTURES").ok().filter(|s| !s.is_empty())?;
    Some(if f.starts_with("http") { f } else { format!("file://{}", f) })
}

/// A test's own number in place of a constant (seconds between watcher rounds).
fn test_secs(name: &str, normal: u64) -> u64 {
    if cfg!(debug_assertions) { std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(normal) } else { normal }
}

/// The stand-in engine: `--list-devices` lists one card; otherwise it listens on
/// `--port` and says it is healthy until it is stopped. Some(code) = this run was it.
#[cfg(debug_assertions)]
pub(crate) fn fake_engine(args: &[String]) -> Option<i32> {
    use std::io::{Read, Write};
    fixtures()?;
    if args.iter().any(|a| a == "--list-devices") {
        println!("Available devices:\n  Fake0: Stand-in Card (8192 MiB, 8000 MiB free)");
        return Some(0);
    }
    // Only the engine is ever run with --jinja.
    if !args.iter().any(|a| a == "--jinja") {
        return None;
    }
    let port = args.iter().position(|a| a == "--port").and_then(|i| args.get(i + 1)).and_then(|p| p.parse::<u16>().ok())?;
    let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", port)) else { return Some(1) };
    // A test that failed half-way must not leave one of these behind for good.
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(300));
        std::process::exit(0);
    });
    for mut stream in listener.incoming().flatten() {
        let _ = stream.read(&mut [0u8; 1024]);
        let body = r#"{"status":"ok"}"#;
        let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
    }
    Some(0)
}

// ── llama.cpp releases ─────────────────────────────────────────────────────

pub(crate) fn tag_number(tag: &str) -> Option<u32> {
    tag.strip_prefix('b')?.parse().ok()
}

/// The release files this machine needs: the server archive, plus the CUDA runtime
/// for a CUDA build. Matched by pattern, so a CUDA bump (13.4 → 13.5) needs no code.
pub(crate) fn pick_assets(names: &[(String, u64)], build: Build, windows: bool) -> Option<Vec<(String, u64)>> {
    let os = if windows { "-bin-win-" } else { "-bin-ubuntu-" };
    let ext = if windows { ".zip" } else { ".tar.gz" };
    let ok = |n: &str, want: &str| n.contains(os) && n.ends_with(ext) && n.contains(want);
    let main = |want: &str| names.iter().filter(|(n, _)| n.starts_with("llama-") && ok(n, want)).max_by_key(|(n, _)| n.clone());
    match build {
        Build::Cuda => {
            // Newest CUDA 13.x x64 build, and the runtime for the same CUDA version.
            let x64 = format!("-x64{}", ext);
            let server = names
                .iter()
                .filter(|(n, _)| n.starts_with("llama-") && ok(n, "-cuda-13.") && n.ends_with(&x64))
                .max_by_key(|(n, _)| n.clone())?
                .clone();
            let ver = server.0.split("-cuda-").nth(1)?.split('-').next()?.to_string();
            let rt = names.iter().find(|(n, _)| n.starts_with("cudart-") && ok(n, &format!("-cuda-{}-x64", ver)))?.clone();
            Some(vec![server, rt])
        }
        Build::Vulkan => Some(vec![main("-vulkan-x64")?.clone()]),
        Build::Cpu => {
            let want = if windows { "-cpu-x64" } else { "-ubuntu-x64" };
            names.iter().find(|(n, _)| n.starts_with("llama-") && n.contains(os) && n.ends_with(&format!("{}{}", want, ext))).map(|a| vec![a.clone()])
        }
    }
}

fn release_assets(json: &serde_json::Value) -> Vec<(String, u64)> {
    json.get("assets")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| Some((x.get("name")?.as_str()?.to_string(), x.get("size")?.as_u64()?)))
                .collect()
        })
        .unwrap_or_default()
}

async fn curl_json(url: &str) -> Result<serde_json::Value, String> {
    let out = run_cmd_full_async("curl".into(), vec!["-sSfL".into(), "--max-time".into(), "20".into(), url.into()])
        .await
        .map_err(|e| {
            // curl names its exit in the message: "(22) The requested URL returned error: 403".
            if e.contains("(22)") {
                "The site refused the request (it may be limiting how often this network can ask). Try again in a while.".to_string()
            } else if e.is_empty() || e.contains("curl:") {
                // Anything else curl itself reports: no route, no name, a timeout.
                "Couldn't reach the internet.".to_string()
            } else {
                no_curl(&e)
            }
        })?;
    serde_json::from_str(&out).map_err(|_| "Got an unreadable answer.".to_string())
}

async fn assets_for_tag(tag: &str) -> Result<Vec<(String, u64)>, String> {
    let url = match fixtures() {
        Some(f) => format!("{}/gh/{}/release.json", f, tag),
        None => format!("https://api.github.com/repos/{}/releases/tags/{}", LLAMA_REPO, tag),
    };
    let v = curl_json(&url).await?;
    Ok(release_assets(&v))
}

/// The newest llama.cpp release that has files for this build, if newer than `current`.
pub(crate) async fn newer_engine(current: String, build: Build) -> Result<Option<String>, String> {
    let v = curl_json(&format!("https://api.github.com/repos/{}/releases?per_page=10", LLAMA_REPO)).await?;
    let newest = v.as_array().into_iter().flatten().find_map(|r| {
        let tag = r.get("tag_name")?.as_str()?;
        pick_assets(&release_assets(r), build, cfg!(windows))?;
        Some(tag.to_string())
    });
    Ok(newest.filter(|t| tag_number(t) > tag_number(&current)))
}

// ── Hugging Face ───────────────────────────────────────────────────────────

/// "user/repo", "user/repo:Q5_K_M" or a huggingface.co link → (repo, quant).
pub(crate) fn parse_hf(input: &str) -> Option<(String, Option<String>)> {
    let s = input.trim().trim_start_matches("https://").trim_start_matches("huggingface.co/").trim_end_matches('/');
    let (repo, quant) = match s.split_once(':') {
        Some((r, q)) => (r, Some(q.trim().to_string()).filter(|q| !q.is_empty())),
        None => (s, None),
    };
    let mut parts = repo.split('/');
    let (user, name) = (parts.next()?, parts.next()?);
    let good = |p: &str| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    (good(user) && good(name)).then(|| (format!("{}/{}", user, name), quant))
}

/// Pick the file the way `-hf` does: the quant asked for (case-insensitive), else
/// Q4_K_M, else the first model file. Vision projectors are never the model.
/// Err = only split (multi-part) files match, which this version doesn't download.
pub(crate) fn pick_gguf(files: &[(String, u64)], quant: Option<&str>) -> Result<(String, u64), String> {
    let models: Vec<&(String, u64)> = files
        .iter()
        .filter(|(n, _)| n.to_lowercase().ends_with(".gguf") && !n.to_lowercase().contains("mmproj"))
        .collect();
    if models.is_empty() {
        return Err("That page has no GGUF model files.".into());
    }
    let want = quant.unwrap_or("Q4_K_M").to_lowercase();
    let hits: Vec<&&(String, u64)> = models.iter().filter(|(n, _)| n.to_lowercase().contains(&want)).collect();
    let pool: Vec<&(String, u64)> = if hits.is_empty() {
        if quant.is_some() {
            return Err(format!("No {} file there.", quant.unwrap_or_default()));
        }
        models
    } else {
        hits.into_iter().copied().collect()
    };
    let single = pool.iter().find(|(n, _)| !n.contains('/') && !n.contains("-of-0"));
    match single {
        Some(f) => Ok((*f).clone()),
        None => Err("That model is split into several parts. Pick a smaller version (quant) of it.".into()),
    }
}

/// A repo's files with sizes.
pub(crate) async fn hf_files(repo: String) -> Result<Vec<(String, u64)>, String> {
    hf_repo(repo).await.map(|l| l.files)
}

/// One look at a repo: its files with sizes, the revision that look is of ("main"
/// when the site doesn't say), and the SHA-256 the site gives for its big files
/// (not every file has one, and a mirror may give none).
struct Listing {
    files: Vec<(String, u64)>,
    rev: String,
    sha256: Vec<(String, String)>,
}

/// The checksums in a repo listing: (file, 64 lowercase hex digits). Anything that
/// isn't exactly that is left out, so a listing without them checks nothing.
fn listing_sha256(v: &serde_json::Value) -> Vec<(String, String)> {
    let Some(files) = v.get("siblings").and_then(|s| s.as_array()) else { return Vec::new() };
    files
        .iter()
        .filter_map(|x| {
            let name = x.get("rfilename")?.as_str()?;
            let sum = x.get("lfs")?.get("sha256")?.as_str()?.to_lowercase();
            (sum.len() == 64 && sum.chars().all(|c| c.is_ascii_hexdigit())).then(|| (name.to_string(), sum))
        })
        .collect()
}

async fn hf_repo(repo: String) -> Result<Listing, String> {
    let url = match fixtures() {
        Some(f) => format!("{}/hf/{}/api.json", f, repo),
        None => format!("https://huggingface.co/api/models/{}?blobs=true", repo),
    };
    let v = curl_json(&url)
        .await
        .map_err(|e| if e.contains("refused") { format!("Couldn't find {} on Hugging Face (or it refused the request). Try again in a while.", repo) } else { e })?;
    let files = v
        .get("siblings")
        .and_then(|s| s.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| Some((x.get("rfilename")?.as_str()?.to_string(), x.get("size").and_then(|s| s.as_u64()).unwrap_or(0))))
                .collect()
        })
        .unwrap_or_default();
    let rev = v.get("sha").and_then(|s| s.as_str()).filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric())).unwrap_or("main").to_string();
    Ok(Listing { files, rev, sha256: listing_sha256(&v) })
}

/// Where a file is fetched from, at one fixed revision of its repo. "main" moves:
/// a file re-uploaded under the same name while a download was paused would be
/// continued from the old one's bytes, a mix of two models of exactly the right
/// size. A revision never changes, and a part begun at another one has another
/// address, so it is thrown away instead of continued.
pub(crate) fn hf_url(repo: &str, rev: &str, file: &str) -> String {
    match fixtures() {
        Some(f) => format!("{}/hf/{}/{}", f, repo, file),
        None => format!("https://huggingface.co/{}/resolve/{}/{}", repo, rev, file),
    }
}

// ── Jobs: engine install and model download, run by a headless worker ──────
//
// Launchers are controllers, never parents. A download is done by `--llm-job`, a
// headless run of this same exe that reports to `llm-job.json`. Any window, the CLI
// or Sapphire starts it, watches it and cancels it through files, and it keeps
// going when the window that started it closes. See plans/llm-easy-mode.md.

/// One model to fetch from Hugging Face.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct ModelSpec {
    pub repo: String,
    /// Empty = pick the file the way `-hf` does (`quant`, else Q4_K_M).
    pub file: String,
    pub quant: Option<String>,
    /// What to call it while it downloads (empty = the file name).
    pub name: String,
}

impl ModelSpec {
    pub(crate) fn curated(m: &Model) -> ModelSpec {
        ModelSpec { repo: m.repo.into(), file: m.file.into(), quant: None, name: m.name.into() }
    }
}

/// What a worker is asked to do, in this order.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct JobSpec {
    /// Install this llama.cpp release first.
    pub engine: Option<String>,
    pub model: Option<ModelSpec>,
    /// Then start the server (restart it if it runs).
    pub start: bool,
}

impl JobSpec {
    /// A model download that also brings the engine when there is none yet.
    pub(crate) fn for_model(cfg: &LlmConfig, model: ModelSpec, start: bool) -> JobSpec {
        JobSpec { engine: (!cfg.engine_ok()).then(|| LLAMA_PIN.to_string()), model: Some(model), start }
    }

    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "engine": self.engine,
            "model": self.model.as_ref().map(|m| serde_json::json!({"repo": m.repo, "file": m.file, "quant": m.quant, "name": m.name})),
            "start": self.start,
        })
    }

    pub(crate) fn from_json(v: &serde_json::Value) -> Option<JobSpec> {
        let s = |v: &serde_json::Value, k: &str| v.get(k).and_then(|x| x.as_str()).filter(|x| !x.is_empty()).map(str::to_string);
        let model = match v.get("model").filter(|m| m.is_object()) {
            Some(m) => Some(ModelSpec { repo: s(m, "repo")?, file: s(m, "file").unwrap_or_default(), quant: s(m, "quant"), name: s(m, "name").unwrap_or_default() }),
            None => None,
        };
        let spec = JobSpec { engine: s(v, "engine"), model, start: v.get("start").and_then(|x| x.as_bool()).unwrap_or(false) };
        (spec.engine.is_some() || spec.model.is_some() || spec.start).then_some(spec)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JobState {
    Running,
    Done,
    Failed,
    Cancelled,
}

impl JobState {
    fn slug(self) -> &'static str {
        match self {
            JobState::Running => "running",
            JobState::Done => "done",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
        }
    }
}

/// `llm-job.json`: what the worker is doing, rewritten twice a second.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct JobInfo {
    pub pid: u32,
    pub exe: PathBuf,
    pub spec: JobSpec,
    /// The step under way is llama.cpp (else a model).
    pub engine: bool,
    pub label: String,
    /// The model file being downloaded, once known.
    pub dest: Option<PathBuf>,
    pub done: u64,
    pub total: u64,
    pub state: JobState,
    pub error: String,
    pub note: String,
    pub started: u64,
    /// The curl at work right now (0 = none): what `end_stray_curl` looks for.
    pub curl: u32,
}

impl JobInfo {
    /// A job this controller just asked for, before its worker has reported.
    pub(crate) fn waiting(pid: u32, spec: JobSpec) -> JobInfo {
        JobInfo { pid, exe: PathBuf::new(), engine: spec.engine.is_some(), spec, label: String::new(), dest: None, done: 0, total: 0, state: JobState::Running, error: String::new(), note: String::new(), started: now_secs(), curl: 0 }
    }

    pub(crate) fn to_json(&self) -> String {
        let v = serde_json::json!({
            "pid": self.pid, "exe": self.exe.display().to_string(), "spec": self.spec.to_json(),
            "engine": self.engine, "label": self.label,
            "dest": self.dest.as_ref().map(|d| d.display().to_string()),
            "done": self.done, "total": self.total, "state": self.state.slug(),
            "error": self.error, "note": self.note, "started": self.started, "curl": self.curl,
        });
        serde_json::to_string_pretty(&v).unwrap_or_default()
    }

    pub(crate) fn parse(text: &str) -> Option<JobInfo> {
        let v: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let n = |k: &str| v.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
        let state = [JobState::Running, JobState::Done, JobState::Failed, JobState::Cancelled].into_iter().find(|st| st.slug() == s("state"))?;
        Some(JobInfo {
            pid: u32::try_from(v.get("pid")?.as_u64()?).ok()?,
            exe: PathBuf::from(s("exe")),
            spec: JobSpec::from_json(v.get("spec")?)?,
            engine: v.get("engine").and_then(|x| x.as_bool()).unwrap_or(false),
            label: s("label"),
            dest: Some(s("dest")).filter(|d| !d.is_empty()).map(PathBuf::from),
            done: n("done"),
            total: n("total"),
            state,
            error: s("error"),
            note: s("note"),
            started: n("started"),
            curl: n("curl") as u32,
        })
    }

    /// Still being worked on: the file says so AND its pid runs the binary it names.
    /// A recycled pid, or a worker that died mid-download, is not alive.
    pub(crate) fn alive(&self) -> bool {
        self.state == JobState::Running && self.pid != 0 && pid_exe(self.pid).is_some_and(|e| same_exe(&e, &self.exe))
    }

    pub(crate) fn pct(&self) -> Option<u64> {
        (self.total > 0).then(|| (self.done.min(self.total)) * 100 / self.total)
    }
}

/// The job file, whatever state it is in (None = there never was one, or it is unreadable).
pub(crate) fn read_job() -> Option<JobInfo> {
    JobInfo::parse(&std::fs::read_to_string(job_path()).ok()?)
}

/// Ask the running worker to stop: it sees this within half a second, ends curl,
/// deletes the unfinished file and exits. No signals.
pub(crate) fn request_cancel() {
    let _ = std::fs::write(cancel_path(), b"");
}

/// A worker that died the hard way (a crash, a power cut on a laptop lid) can leave
/// its curl running, still writing the part file. End it before anything else
/// touches that file: two curls on one file is a corrupt model.
fn end_stray_curl(job: &JobInfo) {
    if job.curl != 0 && kill_if_named(job.curl, "curl") {
        for _ in 0..20 {
            if pid_exe(job.curl).is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
}

/// The unfinished files a job left: its model's part (and the note of where that
/// came from) and the engine's download scratch.
fn discard_parts(job: &JobInfo) {
    if let Some(dest) = &job.dest {
        let _ = std::fs::remove_file(part_of(dest));
        let _ = std::fs::remove_file(part_source(dest));
    }
    let _ = std::fs::remove_dir_all(llm_root().join("dl"));
}

/// Throw away what an interrupted job left behind.
pub(crate) fn discard(job: &JobInfo) {
    end_stray_curl(job);
    discard_parts(job);
    let _ = std::fs::remove_file(job_path());
}

/// Detach a child from this process: its own session / process group, no window.
fn detach(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
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
}

/// Start a worker for `spec`, detached, unless one is already at work. Returns its pid.
pub(crate) fn spawn_worker(spec: &JobSpec) -> Result<u32, String> {
    // One controller at a time looks and starts: two asking in the same instant
    // would both see "nothing running" and one would be told OK for a worker that
    // then finds the lock taken and leaves without a word.
    let _turn = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(state_file("llm-spawn.lock")).ok().filter(|f| cfg!(test) || f.lock().is_ok());
    let last = read_job();
    if last.as_ref().is_some_and(|j| j.alive()) {
        return Err("A download is already running. Wait for it, or cancel it first.".into());
    }
    // Tests never start a real worker (the "launcher" there is the test binary).
    if cfg!(test) {
        return Ok(0);
    }
    // The lock is the truth about a worker being alive: the report lags it (one
    // that has not written yet, one that has written "done" and not yet gone).
    if lock_taken("llm-job.lock") {
        return Err("A download is already running. Wait for it, or cancel it first.".into());
    }
    let exe = launcher_path().ok_or("Couldn't find the launcher's own file.")?;
    // The last job's report goes, so the next thing in the file is this worker's;
    // and with it any curl its dead worker left running.
    if let Some(last) = &last {
        end_stray_curl(last);
        // An unfinished model file is worth keeping only for the same model (this
        // job then carries on with it, whatever else differs between the two
        // requests). For any other, nothing would ever come back for it.
        if matches!(last.state, JobState::Running | JobState::Failed)
            && last.spec.model.is_some()
            && last.spec.model != spec.model
            && let Some(dest) = &last.dest
        {
            let _ = std::fs::remove_file(part_of(dest));
            let _ = std::fs::remove_file(part_source(dest));
        }
    }
    let _ = std::fs::remove_file(job_path());
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--llm-job")
        .arg(spec.to_json().to_string())
        // Not the folder the window was opened from: a headless child would keep it
        // (a USB stick, a folder being deleted) busy for as long as it runs.
        .current_dir(app_config_dir())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    detach(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| format!("Couldn't start the download: {}", e))?;
    let pid = child.id();
    // "Started" means the worker holds its lock (or has already done a job that had
    // nothing left to fetch). Until then the next caller could still slip past.
    let mut in_place = Ok(());
    for _ in 0..60 {
        if lock_taken("llm-job.lock") {
            break;
        }
        match child.try_wait() {
            // Exit 2 (a spec it couldn't read) and 3 (the lock was taken) write no
            // report: nothing started. Any other end is a job that ran, however
            // briefly, and its report says how it went ("Couldn't reach the internet").
            Ok(Some(status)) if matches!(status.code(), Some(2) | Some(3)) => {
                in_place = Err("The download didn't start. Try again.".to_string());
                break;
            }
            Ok(Some(_)) => break,
            _ => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    in_place.map(|_| pid)
}

/// The worker's voice: progress into the job file, and an ear for the cancel file.
pub(crate) struct Reporter {
    path: PathBuf,
    cancel: PathBuf,
    pub info: JobInfo,
    pub cancelled: bool,
}

impl Reporter {
    pub(crate) fn new(path: PathBuf, cancel: PathBuf, spec: JobSpec) -> Reporter {
        let pid = std::process::id();
        let exe = pid_exe(pid).or_else(|| std::env::current_exe().ok()).unwrap_or_default();
        let info = JobInfo { exe, ..JobInfo::waiting(pid, spec) };
        Reporter { path, cancel, info, cancelled: false }
    }

    fn write(&self) {
        let _ = write_atomic(&self.path, &self.info.to_json(), false);
    }

    fn progress(&mut self, label: &str, done: u64, total: u64) {
        self.info.label = label.to_string();
        self.info.done = done;
        self.info.total = total;
        self.write();
    }

    /// A controller asked to stop.
    fn stop_asked(&mut self) -> bool {
        self.cancelled = self.cancelled || self.cancel.exists();
        self.cancelled
    }

    /// The last word: how it ended. Returns the worker's exit code.
    fn finish(&mut self, result: Result<(), String>) -> i32 {
        let (state, code) = match &result {
            _ if self.cancelled => (JobState::Cancelled, 4),
            Ok(()) => (JobState::Done, 0),
            Err(_) => (JobState::Failed, 1),
        };
        self.info.state = state;
        if state == JobState::Failed {
            self.info.error = result.err().unwrap_or_default();
        }
        self.write();
        let _ = std::fs::remove_file(&self.cancel);
        code
    }
}

/// One worker at a time: an exclusive hold on a lock file for the worker's life.
/// The OS lets go of it when the worker dies, so it is never stale.
fn job_lock() -> Option<std::fs::File> {
    take_lock("llm-job.lock")
}

/// `--llm-job <spec json>`: do the job, report to the job file, exit.
pub(crate) fn worker_main(json: &str) -> i32 {
    let Some(spec) = serde_json::from_str::<serde_json::Value>(json).ok().and_then(|v| JobSpec::from_json(&v)) else {
        return 2;
    };
    let Some(_lock) = job_lock() else { return 3 };
    let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return 1 };
    let _ = std::fs::remove_file(cancel_path()); // a cancel meant for an earlier job
    // We hold the lock, so no worker lives: a curl the last report names is a stray.
    if let Some(last) = read_job() {
        end_stray_curl(&last);
    }
    let mut rep = Reporter::new(job_path(), cancel_path(), spec.clone());
    rep.write();
    let result = rt.block_on(run_job(&spec, &mut rep));
    if rep.cancelled {
        let _ = std::fs::remove_dir_all(llm_root().join("dl"));
    }
    rep.finish(result)
}

/// curl couldn't be started. On Linux that is nearly always "not installed yet",
/// and the Install tab's first step is what installs it.
fn no_curl(e: &str) -> String {
    if cfg!(windows) {
        format!("Couldn't run curl, which does the downloading ({}).", e.lines().last().unwrap_or("").trim())
    } else {
        "Downloading needs curl, which isn't on this computer yet. Press Go on the Install tab first (it installs it), then pick the model again.".to_string()
    }
}

/// Said by anything that needed the settings to act and couldn't read them.
const UNREADABLE: &str = "The LLM's settings couldn't be read just now. Try again.";

/// The worker's whole run: engine, then model, then (maybe) start. Each step saves
/// what it changed to llm.json, so a controller only has to re-read it.
async fn run_job(spec: &JobSpec, rep: &mut Reporter) -> Result<(), String> {
    // Already here (a Resume after the model's download failed, say): the engine
    // is not fetched, or deleted, a second time.
    let cfg = LlmConfig::read_patiently().ok_or(UNREADABLE)?;
    if let Some(tag) = spec.engine.as_ref().filter(|t| !(cfg.engine_ok() && cfg.tag == **t)) {
        // llama-server.exe needs the Visual C++ runtime, which a clean Windows lacks
        // and the Install tab only reaches at its sixth step.
        #[cfg(windows)]
        if !matches!(crate::install::check_vcredist().await.1, crate::install::StepStatus::Found(_)) {
            rep.progress("the Visual C++ runtime", 0, 0);
            if let (_, crate::install::StepStatus::Failed(why), _) = crate::install::install_vcredist().await {
                return Err(format!("llama.cpp needs the Visual C++ runtime, and installing it didn't work: {}", why));
            }
        }
        let build = match cfg.build {
            Some(b) if cfg.engine_ok() => b, // an update keeps its kind
            _ => pick_build(&probe(None).await),
        };
        rep.info.engine = true;
        let done = install_engine(&llm_root(), tag, build, rep).await?;
        let saved = LlmConfig::update(|c| {
            c.engine = Some(done.exe.clone());
            c.tag = done.tag.clone();
            c.build = Some(done.build);
        });
        if !saved {
            return Err("llama.cpp is installed, but its settings couldn't be saved. Try again.".into());
        }
        rep.info.note = done.note.unwrap_or_default();
    }
    if let Some(m) = &spec.model {
        rep.info.engine = false;
        // Read again: the models folder may have been changed while the engine came down.
        let dir = gguf_dir(&LlmConfig::read_patiently().ok_or(UNREADABLE)?);
        let path = download_model(m, &dir, rep).await?;
        // A running server keeps its model ("Restart to apply" offers the switch),
        // unless this job restarts it anyway.
        if (spec.start || find_running().is_none()) && !LlmConfig::update(|c| c.model = Some(path.clone())) {
            return Err("The model is downloaded, but the settings couldn't be saved. Pick it again.".into());
        }
    }
    if spec.start {
        if rep.stop_asked() {
            return Err("Cancelled.".into());
        }
        rep.progress("Starting", 0, 0);
        if let Some(run) = find_running() {
            stop(run).await;
        }
        start_saved().await?;
    }
    Ok(())
}

pub(crate) fn part_of(dest: &Path) -> PathBuf {
    PathBuf::from(format!("{}.part", dest.display()))
}

/// Fetch the model into `dir`, returning its path. A file already there is kept.
async fn download_model(m: &ModelSpec, dir: &Path, rep: &mut Reporter) -> Result<PathBuf, String> {
    let Listing { files, rev, sha256 } = hf_repo(m.repo.clone()).await?;
    let (file, size) = if m.file.is_empty() {
        pick_gguf(&files, m.quant.as_deref())?
    } else {
        let size = files.iter().find(|(n, _)| *n == m.file).map(|f| f.1).ok_or_else(|| format!("{} is no longer on Hugging Face.", m.file))?;
        (m.file.clone(), size)
    };
    let dest = dir.join(&file);
    let name = if m.name.is_empty() { file.clone() } else { m.name.clone() };
    rep.info.dest = Some(dest.clone());
    if !dest.exists() {
        let sum = sha256.iter().find(|(n, _)| *n == file).map(|(_, sum)| sum.as_str());
        download(&hf_url(&m.repo, &rev, &file), &dest, size, sum, &name, rep).await.map_err(|e| {
            if e.starts_with("Not enough room") { format!("{} Models can live on another drive: LLM tab, Advanced, Models folder.", e) } else { e }
        })?;
        // The site gave no checksum (or this is a mirror): at least it is a model.
        if !is_gguf(&dest) {
            let _ = std::fs::remove_file(&dest);
            return Err(format!("What came down for {} isn't a model file, so it was thrown away. Try again.", name));
        }
    }
    Ok(dest)
}

/// A model file starts with the four letters GGUF. One that doesn't is damaged, cut
/// short at nothing, or some other kind of file under a model's name.
pub(crate) fn is_gguf(path: &Path) -> bool {
    use std::io::Read;
    let mut head = [0u8; 4];
    std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut head)).is_ok() && &head == b"GGUF"
}

/// SHA-256 of a file, as lowercase hex. `tick` is told how far it is after every
/// piece and may end it early (None) by answering false.
fn sha256_file(path: &Path, mut tick: impl FnMut(u64) -> bool) -> std::io::Result<Option<String>> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 4 << 20];
    let mut done = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
        done += n as u64;
        if !tick(done) {
            return Ok(None);
        }
    }
    Ok(Some(hash.finalize().iter().map(|b| format!("{:02x}", b)).collect()))
}

/// Room kept free beyond the download itself.
const SPACE_MARGIN: u64 = 500_000_000;
/// curl exits worth waiting out: can't resolve/connect, cut off mid-transfer, stalled.
const TRANSIENT: &[i32] = &[5, 6, 7, 18, 28, 35, 52, 55, 56, 92];
/// How long a download waits for a network that went away, counted from the last
/// byte that arrived.
const NETWORK_PATIENCE_SECS: u64 = 600;

fn gb_words(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

/// Where a part file notes the address it is being fetched from.
fn part_source(dest: &Path) -> PathBuf {
    PathBuf::from(format!("{}.part.url", dest.display()))
}

/// `curl -C -` into `<dest>.part`, renamed into place when complete. Progress is the
/// part file's size, polled, so curl's own output never needs parsing. A network
/// that goes away is waited out (ten minutes without a byte) and the download
/// resumed where it stopped; Cancel deletes the part.
///
/// `sha256`: the checksum its source gives, when it gives one. The right size is not
/// yet the right bytes: a power cut can leave a blank stretch in a part that is then
/// resumed to exactly the promised length. With a checksum the file only gets its
/// name once it matches; without one, size is all there is to go by.
async fn download(url: &str, dest: &Path, size: u64, sha256: Option<&str>, label: &str, rep: &mut Reporter) -> Result<(), String> {
    let part = part_of(dest);
    let source = part_source(dest);
    let dir = dest.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| format!("Couldn't create {}: {}", dir.display(), e))?;
    let have = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    // A part left by a different download of the same file name (another repo's
    // Q4_K_M) must not be continued: curl would glue two models into one file.
    if std::fs::read_to_string(&source).ok().as_deref() != Some(url) {
        let _ = std::fs::remove_file(&part);
    }
    let _ = std::fs::write(&source, url);
    let drop_part = || {
        let _ = std::fs::remove_file(&part);
        let _ = std::fs::remove_file(&source);
    };
    let need = size.saturating_sub(have(&part));
    if let Some(free) = free_bytes(dir)
        && free < need + SPACE_MARGIN
    {
        return Err(format!("Not enough room for {}: it needs {} more and {} has {} free.", label, gb_words(need), dir.display(), gb_words(free)));
    }
    let (mut quiet_since, mut refusals, mut wait) = (std::time::Instant::now(), 0, 2u64);
    loop {
        let before = have(&part);
        let (ok, code, err) = curl_once(url, &part, size, label, rep).await?;
        if rep.cancelled {
            drop_part();
            return Err("Cancelled.".into());
        }
        let got = have(&part);
        if (size > 0 && got == size) || (ok && size == 0) {
            break;
        }
        // curl is content but the size is not the one promised: not the file asked for.
        if ok || (size > 0 && got > size) {
            drop_part();
            return Err(format!("Download of {} came out the wrong size, so it was thrown away. Try again.", label));
        }
        if got > before {
            (quiet_since, refusals, wait) = (std::time::Instant::now(), 0, 2);
        }
        // A refusal (curl 22: the site answered 4xx/5xx) gets three tries; a network
        // that went away gets NETWORK_PATIENCE_SECS to come back.
        let give_up = if code == 22 {
            refusals += 1;
            refusals >= 3
        } else {
            !TRANSIENT.contains(&code) || quiet_since.elapsed().as_secs() >= NETWORK_PATIENCE_SECS
        };
        // The server won't continue this part (no ranges, a bad offset): the next
        // try has to start it over, or it would fail the same way for ever.
        if code == 33 || code == 36 {
            drop_part();
        }
        if give_up {
            let err = err.trim();
            let last = err.lines().last().unwrap_or("").trim();
            let why = if err.contains("No space") {
                "The disk is full.".to_string()
            } else if code == 23 {
                format!("Couldn't write to {} ({}).", dir.display(), last)
            } else if code == 22 {
                format!("The site refused it ({}).", last)
            } else if code == 28 {
                "The connection stalled and didn't come back.".to_string()
            } else if TRANSIENT.contains(&code) {
                "Couldn't reach the internet.".to_string()
            } else if err.is_empty() {
                format!("curl exit {}.", code)
            } else {
                last.to_string()
            };
            return Err(format!("Download of {} stopped: {} Try again to resume.", label, why));
        }
        // Wait a little longer each time, still listening for Cancel.
        for _ in 0..wait * 2 {
            if rep.stop_asked() {
                drop_part();
                return Err("Cancelled.".into());
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        wait = (wait * 2).min(30);
    }
    if let Some(want) = sha256 {
        let checking = format!("Checking {}", label);
        rep.progress(&checking, 0, size);
        // Read here, on the worker's one thread: it has nothing else to do meanwhile.
        let (mut said, mut stopped) = (0u64, false);
        let got = sha256_file(&part, |done| {
            if done - said >= 64 << 20 {
                said = done;
                rep.progress(&checking, done, size);
                stopped = rep.stop_asked();
            }
            !stopped
        });
        match got {
            Ok(Some(got)) if got == want => {}
            Ok(None) => {
                drop_part();
                return Err("Cancelled.".into());
            }
            Ok(Some(_)) => {
                drop_part();
                return Err(format!("Download of {} doesn't match its checksum (it was damaged on the way or on the disk), so it was thrown away. Try again.", label));
            }
            Err(e) => return Err(format!("Couldn't read {} back to check it: {}", part.display(), e)),
        }
    }
    rename_retry(&part, dest).map_err(|e| format!("Couldn't save {}: {}", dest.display(), e))?;
    let _ = std::fs::remove_file(&source);
    Ok(())
}

/// One curl run. Returns (succeeded, exit code, stderr). Ends early when a
/// controller asks to stop; curl is killed and reaped before this returns, so
/// its file is free to delete (Windows).
async fn curl_once(url: &str, part: &Path, size: u64, label: &str, rep: &mut Reporter) -> Result<(bool, i32, String), String> {
    let mut cmd = tokio::process::Command::new("curl");
    // Under 1 KB/s for a minute is a stall: curl gives up (exit 28) and we resume.
    cmd.args(["-fL", "-sS", "--connect-timeout", "20", "--speed-limit", "1024", "--speed-time", "60", "-C", "-", "-o"])
        .arg(part)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    // Linux: curl dies with the worker even when the worker is killed outright
    // (kill_on_drop never runs then). Elsewhere `end_stray_curl` catches it later.
    #[cfg(target_os = "linux")]
    // SAFETY: prctl is async-signal-safe and acts on the child only.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| format!("Couldn't start the download (is curl installed?): {}", e))?;
    rep.info.curl = child.id().unwrap_or(0);
    let have = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let status = loop {
        if rep.stop_asked() {
            let _ = child.kill().await;
            rep.info.curl = 0;
            return Ok((false, -1, String::new()));
        }
        rep.progress(label, have(part), size);
        match tokio::time::timeout(std::time::Duration::from_millis(500), child.wait()).await {
            Ok(r) => break r.map_err(|e| e.to_string())?,
            Err(_) => continue,
        }
    };
    rep.info.curl = 0;
    let mut err = String::new();
    if !status.success()
        && let Some(mut e) = child.stderr.take()
    {
        use tokio::io::AsyncReadExt;
        let _ = e.read_to_string(&mut err).await;
    }
    Ok((status.success(), status.code().unwrap_or(-1), err))
}

/// Move every file under `from` (archives have one top folder, or none) into `to`.
fn flatten(from: &Path, to: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let p = entry.path();
        if entry.file_type()?.is_dir() {
            flatten(&p, to)?;
        } else {
            std::fs::rename(&p, to.join(entry.file_name()))?;
        }
    }
    Ok(())
}

/// Windows' own bsdtar unpacks zips; Git's GNU tar on PATH would not.
fn tar_program() -> String {
    #[cfg(windows)]
    let tar = {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        Path::new(&root).join("System32").join("tar.exe").display().to_string()
    };
    #[cfg(not(windows))]
    let tar = "tar".to_string();
    tar
}

struct EngineDone {
    exe: PathBuf,
    tag: String,
    build: Build,
    note: Option<String>,
}

async fn install_engine(root: &Path, tag: &str, build: Build, rep: &mut Reporter) -> Result<EngineDone, String> {
    let names = assets_for_tag(tag).await.map_err(|e| format!("Couldn't list llama.cpp {}: {}", tag, e))?;
    let mut build = build;
    let mut note = None;
    loop {
        let assets = pick_assets(&names, build, cfg!(windows))
            .ok_or_else(|| format!("llama.cpp {} has no {} build for this computer.", tag, build.words()))?;
        let bin = root.join("bin");
        let dir = bin.join(format!("{}-{}", tag, build.slug()));
        let staging = root.join("dl").join("unpack");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&dir).map_err(|e| format!("Couldn't create {}: {}", dir.display(), e))?;
        std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
        for (i, (name, size)) in assets.iter().enumerate() {
            let archive = root.join("dl").join(name);
            let url = match fixtures() {
                Some(f) => format!("{}/gh/{}/{}", f, tag, name),
                None => format!("https://github.com/{}/releases/download/{}/{}", LLAMA_REPO, tag, name),
            };
            // CUDA comes as two files; say which, so the bar restarting isn't a surprise.
            let part = if assets.len() > 1 { format!(", part {} of {}", i + 1, assets.len()) } else { String::new() };
            let label = format!("llama.cpp ({}{})", build.words(), part);
            download(&url, &archive, *size, None, &label, rep).await?;
            rep.progress(&format!("Unpacking {}", label), 0, 0);
            run_cmd_full_async(tar_program(), vec!["-xf".into(), archive.display().to_string(), "-C".into(), staging.display().to_string()])
                .await
                .map_err(|e| format!("Couldn't unpack {}: {}", name, e))?;
            let _ = std::fs::remove_file(&archive);
            if rep.stop_asked() {
                let _ = std::fs::remove_dir_all(&dir);
                return Err("Cancelled.".into());
            }
        }
        flatten(&staging, &dir).map_err(|e| format!("Couldn't put llama.cpp in place: {}", e))?;
        let _ = std::fs::remove_dir_all(&staging);
        let exe = dir.join(SERVER_EXE);
        if !exe.exists() {
            return Err(format!("{} is missing from the {} download. If an antivirus removed it, allow it there and try again.", SERVER_EXE, build.words()));
        }
        // The CUDA build quietly falls back to the processor when the card or driver
        // won't talk to it; Vulkan usually still can.
        if build == Build::Cuda {
            match list_devices(&exe).await {
                // It ran and listed nothing: the card or driver won't talk to CUDA.
                Ok(devices) if devices.is_empty() => {
                    note = Some("CUDA couldn't see your graphics card, so the launcher installed the Vulkan build instead.".to_string());
                    let _ = std::fs::remove_dir_all(&dir);
                    build = Build::Vulkan;
                    continue;
                }
                Ok(_) => {}
                // It wouldn't run at all: that says nothing about the card, and a
                // different build would not run either.
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&dir);
                    let hint = if cfg!(windows) { " It needs the Visual C++ runtime: press Go on the Install tab, which installs it, then try again." } else { "" };
                    return Err(format!("llama.cpp was downloaded but wouldn't run on this computer.{} ({})", hint, e.lines().last().unwrap_or("no words").trim()));
                }
            }
        }
        // Older engines go; the running one (if any) keeps its files open just fine
        // on Linux, and on Windows a locked folder simply stays until next time.
        // Never the one a running server lives in: with its file gone the launcher
        // could no longer tell that server is its own, or stop it. It goes at the
        // next install, after a restart has moved the server on.
        let in_use = find_running().and_then(|r| r.exe.parent().map(Path::to_path_buf));
        if let Ok(rd) = std::fs::read_dir(&bin) {
            for e in rd.flatten() {
                let p = e.path();
                if p != dir && !in_use.as_ref().is_some_and(|u| same_exe(u, &p)) {
                    let _ = std::fs::remove_dir_all(p);
                }
            }
        }
        return Ok(EngineDone { exe, tag: tag.to_string(), build, note });
    }
}

/// Downloaded `.gguf` files in the models folder (not part files).
pub(crate) fn downloaded(cfg: &LlmConfig) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(gguf_dir(cfg))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "gguf"))
        .collect();
    v.sort();
    v
}

/// Bytes under a folder (the Uninstall tab says how much the LLM takes).
pub(crate) fn dir_bytes(p: &Path) -> u64 {
    std::fs::read_dir(p)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_bytes(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

/// Delete the engine and the default models folder. A models folder of your own
/// choosing is never touched: the launcher can't tell its files from yours there.
pub(crate) async fn remove_all() -> Result<(), String> {
    tokio::task::spawn_blocking(|| {
        let root = llm_root();
        if !root.exists() {
            return Ok(());
        }
        crate::install::delete_by_rename(&root)
    })
    .await
    .map_err(|e| e.to_string())?
}

// ── Running it ─────────────────────────────────────────────────────────────

pub(crate) fn server_args(cfg: &LlmConfig, model: &Path) -> Vec<String> {
    let mut a = vec![
        "-m".into(), model.display().to_string(),
        "-c".into(), cfg.ctx.to_string(),
        "--host".into(), if cfg.lan { "0.0.0.0" } else { "127.0.0.1" }.into(),
        "--port".into(), cfg.port.to_string(),
        "--jinja".into(),
        "-a".into(), model_alias(model),
        // Sapphire calls from her server, so no browser origin needs it: a web page
        // you visit can't read replies from your LLM (llama.cpp's default is any origin).
        "--cors-origins".into(), "localhost".into(),
        // Nothing here reads /slots; don't hand out what each slot is doing.
        "--no-slots".into(),
        // Nor its built-in chat page: Sapphire is the front end.
        "--no-webui".into(),
    ];
    if !cfg.gpu {
        a.extend(["--device".into(), "none".into()]);
    }
    if cfg.private {
        a.push("--log-disable".into());
    }
    a
}

/// Start the server detached: its own session / process group, output to `llm.log`,
/// nothing tied to the launcher's life. Writes the handshake. Blocking (fast).
pub(crate) fn spawn_server(cfg: &LlmConfig) -> Result<LlmRun, String> {
    let exe = cfg.engine.clone().filter(|e| e.exists()).ok_or("llama.cpp isn't installed yet.")?;
    let model = cfg.model.clone().ok_or("Pick a model first.")?;
    if !model.exists() {
        return Err(format!("{} isn't downloaded.", model_label(&model)));
    }
    if !is_gguf(&model) {
        return Err(format!("{} isn't a model file llama.cpp can read: it is damaged or incomplete. Delete it on the LLM tab and get it again.", model_label(&model)));
    }
    // Windows' canonical form (`\\?\C:\...`) is not a safe thing to hand a program as
    // its own path and folder; the plain spelling names the same file.
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let exe = match exe.to_str().and_then(|s| s.strip_prefix(r"\\?\")).filter(|s| !s.starts_with("UNC")).map(PathBuf::from) {
        Some(plain) => plain,
        None => exe,
    };
    let log = log_path();
    if let Some(d) = log.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    // Private mode keeps no log and drops the last one: llama.cpp's own error lines
    // can quote what was said (a broken request, a tool call it couldn't parse).
    let (out, err): (std::process::Stdio, std::process::Stdio) = if cfg.private {
        let _ = std::fs::remove_file(&log);
        (std::process::Stdio::null(), std::process::Stdio::null())
    } else {
        let out = std::fs::File::create(&log).map_err(|e| format!("Couldn't write {}: {}", log.display(), e))?;
        let err = out.try_clone().map_err(|e| e.to_string())?;
        (out.into(), err.into())
    };
    let mut cmd = std::process::Command::new(&exe);
    cmd.args(server_args(cfg, &model))
        .current_dir(exe.parent().unwrap_or(Path::new(".")))
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(err);
    detach(&mut cmd);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setrlimit is async-signal-safe and acts on the child only.
        unsafe {
            cmd.pre_exec(|| {
                // No core dump if it crashes: one holds the model and whatever was in the chat.
                let none = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
                libc::setrlimit(libc::RLIMIT_CORE, &none);
                Ok(())
            });
        }
    }
    let mut child = cmd.spawn().map_err(|e| format!("Couldn't start llama.cpp: {}", e))?;
    let run = LlmRun { pid: child.id(), port: cfg.port, exe, alias: model_alias(&model), model, ctx: cfg.ctx, gpu: cfg.gpu, lan: cfg.lan, private: cfg.private };
    // A server with no handshake is one nobody can see or stop: if the file can't be
    // written, it doesn't get to run.
    if let Err(e) = write_atomic(&run_path(), &run.to_json(), false) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("Couldn't write {}: {}", run_path().display(), e));
    }
    // Reap it when it ends, so a dead server is never a zombie that looks alive.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(run)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Health {
    Ready,
    Loading,
    Down,
}

pub(crate) fn health_from(body: &str) -> Health {
    // The JSON object, whatever framing came round it (a chunked reply's sizes).
    let body = match (body.find('{'), body.rfind('}')) {
        (Some(a), Some(b)) if a < b => &body[a..=b],
        _ => body,
    };
    let v: Option<serde_json::Value> = serde_json::from_str(body).ok();
    match v {
        Some(v) if v.get("status").and_then(|s| s.as_str()) == Some("ok") => Health::Ready,
        Some(v) if v.get("error").is_some() => Health::Loading,
        _ => Health::Down,
    }
}

pub(crate) async fn health(port: u16) -> Health {
    // In-process: this is asked every few seconds for as long as a window is open,
    // and a `curl` per ask is a process (and a virus scan) per ask on Windows.
    let body = tokio::task::spawn_blocking(move || local_get(port, "/health", 2)).await.ok().flatten();
    body.map_or(Health::Down, |b| health_from(&b))
}

/// What the poll saw.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Seen {
    Off,
    Foreign,             // something that isn't ours answers on the port
    Up(LlmRun, Health),  // ours, alive
}

pub(crate) async fn look(port: u16) -> Seen {
    match tokio::task::spawn_blocking(find_running).await.ok().flatten() {
        Some(run) => {
            let h = health(run.port).await;
            Seen::Up(run, h)
        }
        None if health(port).await != Health::Down => Seen::Foreign,
        None => Seen::Off,
    }
}

/// Remove the handshake, but only if it still names this server. Between a stop
/// seeing its server gone and getting here, someone else may have started a new
/// one: deleting that one's handshake would leave a server nobody can see or stop.
fn forget_run(run: &LlmRun) {
    let still_this_one = std::fs::read_to_string(run_path()).ok().and_then(|t| LlmRun::parse(&t)).is_none_or(|now| now.pid == run.pid);
    if still_this_one {
        let _ = std::fs::remove_file(run_path());
    }
}

/// Stop by pid: the graceful signal, then the hard kill after 10 s. Only ever a pid
/// the handshake names AND that runs our binary — never 0, never a stranger.
pub(crate) async fn stop(run: LlmRun) -> bool {
    let alive = |r: &LlmRun| pid_exe(r.pid).is_some_and(|e| same_exe(&e, &r.exe));
    if run.pid == 0 || !alive(&run) {
        forget_run(&run);
        return true;
    }
    let pid = run.pid;
    // No way to ask it nicely (the signal couldn't be sent): don't sit out the ten
    // seconds a graceful stop is given.
    if !tokio::task::spawn_blocking(move || signal_stop(pid)).await.unwrap_or(false) {
        let _ = tokio::task::spawn_blocking(move || kill_process_tree(pid)).await;
    }
    for i in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        if !alive(&run) {
            forget_run(&run);
            return true;
        }
        if i == 39 && alive(&run) {
            let _ = tokio::task::spawn_blocking(move || kill_process_tree(pid)).await;
        }
    }
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let gone = !alive(&run);
    if gone {
        forget_run(&run);
    }
    gone
}

/// The last lines of the server's log, for "it stopped on its own" messages.
pub(crate) fn log_tail(n: usize) -> Vec<String> {
    let text = std::fs::read_to_string(log_path()).unwrap_or_default();
    let lines: Vec<String> = text.lines().filter(|l| !l.trim().is_empty()).map(str::to_string).collect();
    lines[lines.len().saturating_sub(n)..].to_vec()
}

/// Start the server with the saved settings unless it is already up. One start
/// path: the Start button, `--start-llm` and the worker all come here.
pub(crate) async fn start_saved() -> Result<LlmRun, String> {
    // One start at a time, whoever asks (a window, the watcher, the CLI): the
    // second waits here, then finds the first one's server instead of racing it
    // for the port.
    let _turn = tokio::task::spawn_blocking(|| {
        let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(state_file("llm-start.lock")).ok()?;
        f.lock().ok()?;
        Some(f)
    })
    .await
    .ok()
    .flatten();
    set_hold(false); // asked to run: any earlier "stay stopped" is over
    if let Some(run) = tokio::task::spawn_blocking(find_running).await.ok().flatten() {
        return Ok(run);
    }
    let cfg = LlmConfig::read_patiently().ok_or(UNREADABLE)?;
    if health(cfg.port).await != Health::Down {
        return Err(format!("Something else is already using port {}. Pick another port under Advanced.", cfg.port));
    }
    // Auto context needs the hardware: the same look the tab takes (~0.3 s).
    let hw = if cfg.ctx == 0 { Some(probe(cfg.engine.clone()).await) } else { None };
    let cfg = cfg.resolved(hw.as_ref());
    tokio::task::spawn_blocking(move || spawn_server(&cfg)).await.map_err(|e| e.to_string())?
}

/// The note that a server was started for Sapphire: its pid; the file's age is how
/// long ago. Launch brings the LLM up before she answers, and she can die before the
/// watcher ever sees the two up together; this is how it knows the server is hers.
fn for_her_path() -> PathBuf {
    state_file("llm-for-her")
}
pub(crate) fn mark_for_her(pid: u32) {
    let _ = std::fs::write(for_her_path(), pid.to_string());
}
/// (the pid it names, seconds since it was written).
fn for_her() -> Option<(u32, u64)> {
    let pid = std::fs::read_to_string(for_her_path()).ok()?.trim().parse().ok()?;
    let age = std::fs::metadata(for_her_path()).ok()?.modified().ok()?.elapsed().map(|d| d.as_secs()).unwrap_or(0);
    Some((pid, age))
}

/// `start_saved`, for a Sapphire that is starting or up.
pub(crate) async fn start_for_her() -> Result<LlmRun, String> {
    let run = start_saved().await?;
    mark_for_her(run.pid);
    Ok(run)
}

// ── The CLI: what Sapphire (or a script) calls ────────────────────────────
//
// Every switch is headless, prints one JSON object on stdout and exits: 0 = ok,
// 1 = `{"ok": false, "error": ...}`. Sign-in autostart uses `--start-llm`.

/// Every model file this computer has, the default first.
fn models_on_disk(cfg: &LlmConfig) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = cfg.model.iter().cloned().collect();
    v.extend(MODELS.iter().map(|m| model_path(cfg, m)));
    v.extend(downloaded(cfg));
    v.extend(cfg.files.iter().cloned());
    let mut seen = Vec::new();
    v.retain(|p| p.exists() && !seen.contains(p) && { seen.push(p.clone()); true });
    v
}

/// A model on disk by path, file name, list name ("Qwen3.5 4B") or alias ("qwen3.5-4b").
pub(crate) fn find_model(cfg: &LlmConfig, want: &str) -> Option<PathBuf> {
    let w = want.trim().to_lowercase();
    models_on_disk(cfg).into_iter().find(|p| {
        let file = p.file_name().map(|f| f.to_string_lossy().to_lowercase()).unwrap_or_default();
        p.display().to_string().to_lowercase() == w || file == w || file.trim_end_matches(".gguf") == w || model_label(p).to_lowercase() == w || model_alias(p).to_lowercase() == w
    })
}

/// What to download for `want`: a model from the list (by name, alias or file), or
/// `user/repo[:QUANT]` on Hugging Face.
pub(crate) fn download_spec(cfg: &LlmConfig, want: &str) -> Result<JobSpec, String> {
    let w = want.trim().to_lowercase();
    let listed = MODELS.iter().find(|m| m.name.to_lowercase() == w || m.file.to_lowercase() == w || m.name.to_lowercase().replace(' ', "-") == w);
    let model = match listed {
        Some(m) if model_path(cfg, m).exists() => return Err(format!("{} is already downloaded.", m.name)),
        Some(m) => ModelSpec::curated(m),
        None => {
            let (repo, quant) = parse_hf(want).ok_or_else(|| format!("'{}' isn't a model on the list or a Hugging Face name like user/model-GGUF.", want.trim()))?;
            ModelSpec { repo, quant, ..Default::default() }
        }
    };
    Ok(JobSpec::for_model(cfg, model, false))
}

/// The one word for where the LLM is, so a caller needs no rules of its own:
/// `running` (answering), `loading` (started, the model not in yet), `downloading`
/// (a worker is fetching the engine or a model), `stopped` (set up, not running),
/// `none` (nothing set up). A server that is up wins over a download beside it.
pub(crate) fn phase(state: &str, job: bool, set_up: bool) -> &'static str {
    match state {
        "ready" => "running",
        "loading" | "starting" => "loading",
        _ if job => "downloading",
        _ if set_up => "stopped",
        _ => "none",
    }
}

/// Stop the server if it runs. Ok when it is down afterwards.
async fn stop_running() -> Result<(), String> {
    match find_running() {
        Some(run) => stop(run).await.then_some(()).ok_or_else(|| "The LLM didn't stop.".to_string()),
        None => Ok(()),
    }
}

async fn status(her_up: bool) -> serde_json::Value {
    // Showing only, so defaults are the last resort and not an error.
    let cfg = LlmConfig::read_patiently().unwrap_or_default();
    let run = find_running();
    let state = match &run {
        Some(r) => match health(r.port).await {
            Health::Ready => "ready",
            Health::Loading => "loading",
            Health::Down => "starting",
        },
        None => "off",
    };
    let path = |p: &Path| p.display().to_string();
    let on_disk = models_on_disk(&cfg);
    let models: Vec<_> = on_disk
        .iter()
        .map(|p| serde_json::json!({"name": model_label(p), "alias": model_alias(p), "path": path(p), "bytes": std::fs::metadata(p).map(|m| m.len()).unwrap_or(0), "default": cfg.model.as_ref() == Some(p)}))
        .collect();
    let available: Vec<_> = MODELS
        .iter()
        .filter(|m| !model_path(&cfg, m).exists())
        .map(|m| serde_json::json!({"name": m.name, "file": m.file, "bytes": m.bytes, "about": m.blurb}))
        .collect();
    let last = read_job();
    let job = last.as_ref().filter(|j| j.alive()).map(job_json);
    // How the last one ended, so a caller can tell "failed" from "not started".
    let last_job = last.as_ref().filter(|j| !j.alive()).map(last_job_json);
    let set_up = cfg.engine_ok() && cfg.model.as_ref().is_some_and(|m| m.exists());
    let phase = phase(state, job.is_some(), set_up);
    serde_json::json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "phase": phase,
        "set_up": set_up,
        "state": state,
        "sapphire_running": her_up,
        "sign_in": match sign_in() { SignIn::Off => "off", SignIn::Watch => "watch", SignIn::Start => "start" },
        "running": run.as_ref().map(running_json),
        "settings": {"model": cfg.model.as_deref().map(path), "ctx": cfg.ctx, "gpu": cfg.gpu, "port": cfg.port, "on": cfg.is_on(), "with_sapphire": cfg.with_sapphire, "models_dir": path(&gguf_dir(&cfg))},
        "models": models,
        "available": available,
        "job": job,
        "last_job": last_job,
    })
}

// The nested objects of `--llm-status`, each built in one place: Sapphire reads
// these keys, and `the_contract_with_core_keeps_its_keys` pins every one.

/// `job`: a download under way.
fn job_json(j: &JobInfo) -> serde_json::Value {
    serde_json::json!({"label": j.label, "engine": j.engine, "done": j.done, "total": j.total, "percent": j.pct()})
}

/// `last_job`: how the last download ended. One whose worker died without saying
/// so reads "interrupted".
fn last_job_json(j: &JobInfo) -> serde_json::Value {
    serde_json::json!({"state": if j.state == JobState::Running { "interrupted" } else { j.state.slug() }, "label": j.label, "error": j.error})
}

/// `running`: the server that is up.
fn running_json(r: &LlmRun) -> serde_json::Value {
    serde_json::json!({"pid": r.pid, "base_url": format!("http://127.0.0.1:{}/v1", r.port), "alias": r.alias, "model": r.model.display().to_string(), "ctx": r.ctx, "gpu": r.gpu})
}

/// The launcher's headless switches. Some(exit code) = one was given: no window.
pub(crate) fn cli(args: &[String], her_up: impl Fn() -> bool) -> Option<i32> {
    let value = |flag: &str| args.iter().position(|a| a == flag).map(|i| args.get(i + 1).cloned().unwrap_or_default());
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let flags = ["--start-llm", "--stop-llm", "--restart-llm", "--llm-status", "--llm-model", "--llm-download", "--llm-cancel"];
    let given: Vec<&str> = flags.iter().copied().filter(|f| has(f)).collect();
    // A switch nobody here knows is an error said in the caller's own terms, never a
    // window: a caller (Sapphire, a script) built for a newer launcher, or a typo,
    // must get a "no" it can read, not a GUI opening on a headless machine.
    let internal = ["--llm-job", "--llm-watch", "--keep-hold", "--as"];
    if let Some(unknown) = args.iter().skip(1).find(|a| a.starts_with("--") && !flags.contains(&a.as_str()) && !internal.contains(&a.as_str())) {
        println!("{}", serde_json::json!({"ok": false, "error": format!("This launcher ({}) doesn't know the switch {}.", env!("CARGO_PKG_VERSION"), unknown)}));
        return Some(1);
    }
    if given.is_empty() && !has("--llm-job") && !has("--llm-watch") {
        return None;
    }
    // Whatever this run starts (the server, a worker) must not hold on to the
    // caller's output pipe, or the caller waits for it to end.
    no_inherit_std();
    if let Some(json) = value("--llm-job") {
        return Some(worker_main(&json));
    }
    if has("--llm-watch") {
        return Some(watch_main(her_up, has("--keep-hold"), value("--as").filter(|p| !p.is_empty()).map(PathBuf::from)));
    }
    let fail = |e: String| {
        println!("{}", serde_json::json!({"ok": false, "error": e}));
        Some(1)
    };
    // One thing per call, said plainly: no guessing which of two was meant.
    if given.len() > 1 {
        return fail(format!("One switch at a time: got {}.", given.join(" and ")));
    }
    for flag in ["--llm-model", "--llm-download"] {
        if let Some(v) = value(flag)
            && (v.trim().is_empty() || v.starts_with("--"))
        {
            return fail(format!("{} needs a model name after it.", flag));
        }
    }
    let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return Some(1) };
    let started = |r: LlmRun| serde_json::json!({"ok": true, "base_url": format!("http://127.0.0.1:{}/v1", r.port), "alias": r.alias, "pid": r.pid});
    let done = || Ok(serde_json::json!({"ok": true}));
    let out: Result<serde_json::Value, String> = if has("--start-llm") {
        rt.block_on(start_saved()).map(started)
    } else if has("--stop-llm") {
        // Stay stopped: the watcher won't bring it back until asked, or until
        // Sapphire's next run.
        set_hold(true);
        rt.block_on(stop_running()).and_then(|_| done())
    } else if has("--restart-llm") {
        rt.block_on(async {
            stop_running().await?;
            start_saved().await
        })
        .map(started)
    } else if has("--llm-status") {
        Ok(rt.block_on(status(her_up())))
    } else if let Some(want) = value("--llm-model") {
        // The default for the next start; `--restart-llm` applies it to a running one.
        match LlmConfig::read_patiently().map(|cfg| find_model(&cfg, &want)) {
            None => Err(UNREADABLE.to_string()),
            Some(Some(p)) if LlmConfig::update(|c| c.model = Some(p.clone())) => Ok(serde_json::json!({"ok": true, "model": p.display().to_string(), "alias": model_alias(&p)})),
            Some(Some(_)) => Err("The settings couldn't be saved just now. Try again.".to_string()),
            Some(None) => Err(format!("No model called '{}' is downloaded. `--llm-status` lists them.", want.trim())),
        }
    } else if let Some(want) = value("--llm-download") {
        LlmConfig::read_patiently().ok_or(UNREADABLE.to_string()).and_then(|cfg| download_spec(&cfg, &want)).and_then(|spec| spawn_worker(&spec)).and_then(|_| done())
    } else {
        // --llm-cancel
        match read_job().filter(|j| j.alive()) {
            Some(_) => {
                request_cancel();
                done()
            }
            None => Err("No download is running.".to_string()),
        }
    };
    Some(match out {
        Ok(v) => {
            println!("{}", v);
            0
        }
        Err(e) => {
            println!("{}", serde_json::json!({"ok": false, "error": e}));
            1
        }
    })
}

// ── Autostart: the launcher itself, headless, at sign-in ──────────────────

#[cfg(not(windows))]
fn autostart_file() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".config"));
    base.join("autostart").join("sapphire-llm.desktop")
}

#[cfg(windows)]
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

/// The launcher's own lasting path (under an AppImage, the .AppImage, not its mount).
pub(crate) fn launcher_path() -> Option<String> {
    #[cfg(not(windows))]
    if let Ok(p) = std::env::var("APPIMAGE") {
        return Some(p);
    }
    std::env::current_exe().ok().map(|p| p.display().to_string())
}

impl LlmConfig {
    /// What should run at sign-in, from the settings alone: nothing while the LLM is
    /// off; the watcher when it only runs with Sapphire; the LLM itself when it runs
    /// on its own and was asked to.
    pub(crate) fn sign_in_wanted(&self) -> SignIn {
        if !self.is_on() || self.engine.is_none() {
            SignIn::Off
        } else if self.with_sapphire {
            SignIn::Watch
        } else if self.at_sign_in {
            SignIn::Start
        } else {
            SignIn::Off
        }
    }
}

/// What runs at sign-in for the LLM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum SignIn {
    #[default]
    Off,
    /// `--llm-watch`: the watcher, for "Only run with Sapphire".
    Watch,
    /// `--start-llm`: the LLM itself, for people who run it on its own.
    Start,
}

impl SignIn {
    fn switch(self) -> &'static str {
        match self {
            SignIn::Watch => "--llm-watch",
            _ => "--start-llm",
        }
    }
    fn of(command: &str) -> SignIn {
        if command.contains("--llm-watch") {
            SignIn::Watch
        } else if command.contains("--start-llm") {
            SignIn::Start
        } else {
            SignIn::Off
        }
    }
}

/// What the sign-in entry runs now.
pub(crate) fn sign_in() -> SignIn {
    // Tests never read the real entry.
    if cfg!(test) {
        return SignIn::Off;
    }
    #[cfg(not(windows))]
    return SignIn::of(&std::fs::read_to_string(autostart_file()).unwrap_or_default());
    #[cfg(windows)]
    run_value().map_or(SignIn::Off, |command| SignIn::of(&command))
}

/// Windows: what the sign-in entry holds, read straight from the registry. No
/// `reg query` process: `--llm-status` asks this on every call, and Sapphire calls
/// that often.
#[cfg(windows)]
fn run_value() -> Option<String> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    // The key under HKEY_CURRENT_USER: RUN_KEY is written the way `reg` takes it.
    let key = wide(RUN_KEY.strip_prefix("HKCU").unwrap_or(RUN_KEY).trim_start_matches('\\'));
    let name = wide("SapphireLLM");
    let mut buf = vec![0u16; 2048];
    let mut bytes = (buf.len() * 2) as u32;
    // SAFETY: both names are NUL-terminated, and the buffer's size is passed in bytes
    // and comes back as the bytes written, as the API requires.
    let status = unsafe { RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut bytes) };
    if status != 0 {
        return None; // no such value, or one too long to be ours
    }
    let len = (bytes as usize / 2).min(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]).trim_end_matches('\0').to_string())
}

/// Does the sign-in entry name this launcher, where it is now? A launcher that was
/// moved, renamed or replaced by a newer download leaves an entry that starts nothing.
pub(crate) fn sign_in_points_here() -> bool {
    if cfg!(test) {
        return true;
    }
    let Some(exe) = launcher_path() else { return true };
    #[cfg(not(windows))]
    return std::fs::read_to_string(autostart_file()).unwrap_or_default().contains(&crate::desktop_exec(&exe));
    #[cfg(windows)]
    run_value().is_some_and(|command| command.to_lowercase().contains(&exe.to_lowercase()))
}

pub(crate) fn set_sign_in(mode: SignIn) -> Result<(), String> {
    // Nor write it.
    if cfg!(test) {
        return Ok(());
    }
    #[cfg(not(windows))]
    {
        let f = autostart_file();
        if mode == SignIn::Off {
            return match std::fs::remove_file(&f) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
                _ => Ok(()),
            };
        }
        let exe = launcher_path().ok_or("Couldn't tell where the launcher is.")?;
        let entry = format!(
            "[Desktop Entry]\nType=Application\nName=Sapphire LLM\nComment=Runs the local LLM for Sapphire\nExec={} {}\nTerminal=false\nNoDisplay=true\nX-GNOME-Autostart-enabled=true\n",
            crate::desktop_exec(&exe),
            mode.switch()
        );
        if let Some(d) = f.parent() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        std::fs::write(&f, entry).map_err(|e| e.to_string())
    }
    #[cfg(windows)]
    {
        let out = if mode != SignIn::Off {
            let exe = launcher_path().ok_or("Couldn't tell where the launcher is.")?;
            hidden_cmd("reg").args(["add", RUN_KEY, "/v", "SapphireLLM", "/t", "REG_SZ", "/d", &format!("\"{}\" {}", exe, mode.switch()), "/f"]).stdin(std::process::Stdio::null()).output()
        } else {
            hidden_cmd("reg").args(["delete", RUN_KEY, "/v", "SapphireLLM", "/f"]).stdin(std::process::Stdio::null()).output()
        };
        match out {
            Ok(o) if o.status.success() || mode == SignIn::Off => Ok(()),
            Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
            Err(e) => Err(e.to_string()),
        }
    }
}

// ── The watcher: "Only run with Sapphire" with no window open ─────────────
//
// `--llm-watch`, started at sign-in (and by a window, so it works before the next
// sign-in). One small process: every WATCH_SECS it reads a few small files to see
// whether Sapphire is running, starts the LLM when she comes up and stops it when
// she goes down. It spawns nothing and asks the network nothing while it waits, and
// it ends by itself once the LLM is switched off or the box is unticked.

const WATCH_SECS: u64 = 15;

/// "Stop" was asked for while Sapphire runs: the watcher leaves the LLM down until
/// someone starts it, or until she goes down (her next run starts it again).
fn hold_path() -> PathBuf {
    state_file("llm-hold")
}
pub(crate) fn set_hold(on: bool) {
    let _ = if on { std::fs::write(hold_path(), b"") } else { std::fs::remove_file(hold_path()) };
}
fn held() -> bool {
    hold_path().exists()
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Watch {
    Nothing,
    Start,
    Stop,
    Quit,
}

/// One round of the watcher's judgement. `llm` is the running server's pid.
/// `with_her` is the pid of the server that was up while she was: only that one
/// goes down with her. An LLM someone started on its own stays theirs, and so does
/// one that was stopped and started again between two rounds (a different pid).
pub(crate) fn watch_step(cfg: &LlmConfig, her_up: bool, llm: Option<u32>, held: bool, with_her: &mut Option<u32>) -> Watch {
    if !cfg.with_sapphire || !cfg.is_on() || cfg.engine.is_none() {
        return Watch::Quit;
    }
    // "Hers" ends with the server it was said of.
    if *with_her != llm {
        *with_her = None;
    }
    if her_up {
        if llm.is_some() {
            *with_her = llm;
            return Watch::Nothing;
        }
        let ready = cfg.engine_ok() && cfg.model.as_ref().is_some_and(|m| m.exists());
        return if held || !ready { Watch::Nothing } else { Watch::Start };
    }
    if llm.is_some() && with_her.take().is_some() { Watch::Stop } else { Watch::Nothing }
}

/// How long a server started for her may wait for her to come up. A first start on
/// a slow disk takes a while; a Sapphire that crashed on the way up never arrives,
/// and then nothing may keep holding the card.
const FOR_HER_GRACE_SECS: u64 = 300;

/// The running server, if the note says it was started for her and she has had her
/// time: from then on it goes down whenever she is not up.
fn hers_by_note(llm: Option<u32>, note: Option<(u32, u64)>, grace: u64) -> Option<u32> {
    let (pid, age) = note?;
    (llm == Some(pid) && age >= grace).then_some(pid)
}

/// A start that keeps failing (a broken model, a busy port) is retried less and
/// less often: 15 s, 30 s, 1 min ... up to 16 min.
fn watch_backoff(fails: u32) -> std::time::Duration {
    std::time::Duration::from_secs(test_secs("SAPPHIRE_LLM_WATCH_SECS", WATCH_SECS) << fails.min(6))
}

// A watcher lives for a whole session, so it can outlive the launcher it came from:
// a newer launcher is downloaded and opened from another folder, or (Linux) the
// file is swapped for a newer one in place. Left alone it would keep running old
// code, and on Windows it keeps the old file locked: it can't be deleted or
// replaced while its watcher runs. So each round the watcher asks whether it is
// still the right one, and if not hands its job to the right launcher and ends.

/// What a launcher file looks like now: a different size or time is a different
/// program under the same name.
type ExeStamp = (u64, std::time::SystemTime);
fn exe_stamp(path: &Path) -> Option<ExeStamp> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.len(), m.modified().ok()?))
}

/// Two spellings of one file (case, links, short names) are not two launchers. An
/// own file that is gone is not the wanted one; anything else unreadable is left
/// as "the same": a doubt hands nothing over.
fn other_launcher(me: &Path, wanted: &Path) -> bool {
    if me == wanted {
        return false;
    }
    match (std::fs::canonicalize(me), std::fs::canonicalize(wanted)) {
        (Ok(a), Ok(b)) => a != b,
        (Err(_), Ok(_)) => true,
        _ => false,
    }
}

/// The launcher this watcher should hand over to, if it is no longer the right one:
/// the launcher whose window opened last (`wanted`, llm.json's `launcher`) when that
/// is another file and is there; else its own path when the file there was replaced.
fn watch_successor(me: &Path, wanted: Option<&Path>, replaced: bool) -> Option<PathBuf> {
    if let Some(w) = wanted.filter(|w| w.is_file() && other_launcher(me, w)) {
        return Some(w.to_path_buf());
    }
    replaced.then(|| me.to_path_buf())
}

/// `--llm-watch`. `her_up` says whether Sapphire is running; it must be cheap.
/// `keep_hold`: a window started this one mid-session (the last one died), so a
/// Stop the user asked for stands; only a sign-in's watcher starts with a clean slate.
/// `named`: the path a handover started this one by (`--as`). It is used as given:
/// the successor is then the wanted launcher by the very same spelling, so two
/// watchers can never keep handing the job back and forth over how a path is written.
pub(crate) fn watch_main(her_up: impl Fn() -> bool, keep_hold: bool, named: Option<PathBuf>) -> i32 {
    // One watcher: the OS frees the lock when it ends, so it is never stale.
    let Some(mut lock) = take_lock("llm-watch.lock") else { return 0 };
    let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return 1 };
    // Who this is, settled now: later the file may be gone or be another program.
    let me = named.or_else(|| launcher_path().map(PathBuf::from));
    let born = me.as_deref().and_then(exe_stamp);
    // A changed file is believed once it looks the same two rounds running (a copy
    // still being written is not yet a launcher), and a handover is tried once.
    let (mut changed, mut handed) = (None, false);
    // A "stay stopped" from an earlier session is over: it outlives a sign-out or a
    // shutdown with her still running, and would keep her LLM off ever after.
    if !keep_hold {
        set_hold(false);
    }
    let (mut with_her, mut fails, mut next_try) = (None, 0u32, std::time::Instant::now());
    loop {
        // Unreadable for a moment (mid-write, a scanner) is not "switched off".
        let Some(cfg) = LlmConfig::read() else {
            std::thread::sleep(std::time::Duration::from_secs(test_secs("SAPPHIRE_LLM_WATCH_SECS", WATCH_SECS)));
            continue;
        };
        if let Some(me) = me.as_deref().filter(|_| !handed) {
            let now = exe_stamp(me).filter(|now| Some(*now) != born);
            let replaced = now.is_some() && changed == now;
            changed = now;
            if let Some(next) = watch_successor(me, cfg.launcher.as_deref(), replaced) {
                handed = true;
                // Let go first: the successor leaves at once if the lock is taken.
                let _ = lock.unlock();
                drop(lock);
                let took_over = spawn_watcher_from(&next) && (0..30).any(|_| {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    watcher_running()
                });
                if took_over {
                    return 0;
                }
                // It didn't start (a broken file): better this watcher than none.
                match take_lock("llm-watch.lock") {
                    Some(l) => lock = l,
                    None => return 0,
                }
            }
        }
        let up = her_up();
        let run = find_running();
        if !up {
            set_hold(false);
        }
        if run.is_some() {
            fails = 0;
        }
        let llm = run.as_ref().map(|r| r.pid);
        let note = for_her();
        if let Some(pid) = hers_by_note(llm, note, test_secs("SAPPHIRE_LLM_GRACE_SECS", FOR_HER_GRACE_SECS)) {
            with_her = Some(pid);
        } else if note.is_some_and(|(pid, age)| llm != Some(pid) && age >= FOR_HER_GRACE_SECS) {
            // About a server long gone. (A fresh one is never touched here: its
            // server may be a moment younger than this round's look.)
            let _ = std::fs::remove_file(for_her_path());
        }
        match watch_step(&cfg, up, llm, held(), &mut with_her) {
            Watch::Quit => return 0,
            Watch::Start if std::time::Instant::now() >= next_try => {
                // Started for her, so it is hers to take down: no need to wait for a
                // later round to see the two up together.
                with_her = rt.block_on(start_for_her()).ok().map(|r| r.pid);
                // Counted as a failure until the LLM is seen up on a later round.
                fails += 1;
                next_try = std::time::Instant::now() + watch_backoff(fails);
            }
            Watch::Stop => {
                // One that wouldn't stop is still hers: tried again next round.
                if let Some(run) = run {
                    let pid = run.pid;
                    if !rt.block_on(stop(run)) {
                        with_her = Some(pid);
                    }
                }
            }
            _ => {}
        }
        std::thread::sleep(std::time::Duration::from_secs(test_secs("SAPPHIRE_LLM_WATCH_SECS", WATCH_SECS)));
    }
}

/// Make sure a watcher is running. A second one finds the lock taken and exits.
pub(crate) fn spawn_watcher() {
    if cfg!(test) {
        return;
    }
    if let Some(exe) = launcher_path() {
        spawn_watcher_from(Path::new(&exe));
    }
}

/// Start `exe` as the watcher, told the path it was started by. False = it didn't start.
fn spawn_watcher_from(exe: &Path) -> bool {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["--llm-watch", "--keep-hold", "--as"])
        .arg(exe)
        .current_dir(app_config_dir())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        // An AppImage says where it is through these; one handed on from another
        // launcher's watcher would describe that launcher, not this one.
        .env_remove("APPIMAGE")
        .env_remove("APPDIR");
    detach(&mut cmd);
    match cmd.spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hw(vram: u64, ram: u64) -> Hw {
        Hw { gpu_name: Some("Card".into()), vram_mib: vram, ram_mib: ram, nvidia: None, vulkan_ok: true }
    }

    #[test]
    fn device_list_parses_every_backend() {
        let out = "ggml_vulkan: found 1 device\nAvailable devices:\n  Vulkan0: NVIDIA GeForce RTX 5090 (32853 MiB, 26998 MiB free)\n";
        assert_eq!(parse_devices(out), vec![("NVIDIA GeForce RTX 5090".to_string(), 32853)]);
        assert_eq!(parse_devices("  CUDA0: A (B) (8192 MiB, 100 MiB free)"), vec![("A (B)".to_string(), 8192)]);
        assert!(parse_devices("Available devices:\n  (none)\n").is_empty());
    }

    #[test]
    fn build_rule() {
        let mut h = hw(8192, 16384);
        h.nvidia = Some((120, 580));
        assert_eq!(pick_build(&h), Build::Cuda);
        h.nvidia = Some((61, 580)); // Pascal: CUDA 13 dropped it
        assert_eq!(pick_build(&h), Build::Vulkan);
        h.nvidia = Some((86, 560)); // driver too old for CUDA 13
        assert_eq!(pick_build(&h), Build::Vulkan);
        h.nvidia = None;
        assert_eq!(pick_build(&h), Build::Vulkan);
        h.vulkan_ok = false;
        assert_eq!(pick_build(&h), Build::Cpu);
    }

    fn names(v: &[&str]) -> Vec<(String, u64)> {
        v.iter().map(|n| (n.to_string(), 1)).collect()
    }

    #[test]
    fn assets_pick_by_pattern() {
        let n = names(&[
            "cudart-llama-b1-bin-ubuntu-cuda-12.8-x64.tar.gz",
            "cudart-llama-b1-bin-ubuntu-cuda-13.5-x64.tar.gz",
            "cudart-llama-bin-win-cuda-13.5-x64.zip",
            "cudart-llama-bin-win-cuda-13.5-arm64.zip",
            "llama-b1-bin-ubuntu-cuda-12.8-x64.tar.gz",
            "llama-b1-bin-ubuntu-cuda-13.5-x64.tar.gz",
            "llama-b1-bin-ubuntu-cuda-13.5-arm64.tar.gz",
            "llama-b1-bin-ubuntu-vulkan-x64.tar.gz",
            "llama-b1-bin-ubuntu-vulkan-arm64.tar.gz",
            "llama-b1-bin-ubuntu-x64.tar.gz",
            "llama-b1-bin-ubuntu-arm64.tar.gz",
            "llama-b1-bin-win-cuda-13.5-x64.zip",
            "llama-b1-bin-win-cpu-x64.zip",
            "llama-b1-bin-win-vulkan-x64.zip",
        ]);
        let pick = |b, w| pick_assets(&n, b, w).map(|v| v.into_iter().map(|a| a.0).collect::<Vec<_>>());
        assert_eq!(pick(Build::Cuda, false).unwrap(), vec!["llama-b1-bin-ubuntu-cuda-13.5-x64.tar.gz", "cudart-llama-b1-bin-ubuntu-cuda-13.5-x64.tar.gz"]);
        assert_eq!(pick(Build::Cuda, true).unwrap(), vec!["llama-b1-bin-win-cuda-13.5-x64.zip", "cudart-llama-bin-win-cuda-13.5-x64.zip"]);
        assert_eq!(pick(Build::Vulkan, false).unwrap(), vec!["llama-b1-bin-ubuntu-vulkan-x64.tar.gz"]);
        assert_eq!(pick(Build::Vulkan, true).unwrap(), vec!["llama-b1-bin-win-vulkan-x64.zip"]);
        assert_eq!(pick(Build::Cpu, false).unwrap(), vec!["llama-b1-bin-ubuntu-x64.tar.gz"]);
        assert_eq!(pick(Build::Cpu, true).unwrap(), vec!["llama-b1-bin-win-cpu-x64.zip"]);
        // A CUDA build without its runtime is no build.
        assert!(pick_assets(&names(&["llama-b1-bin-ubuntu-cuda-13.5-x64.tar.gz"]), Build::Cuda, false).is_none());
    }

    #[test]
    fn a_model_is_fetched_at_one_revision_so_a_part_is_never_continued_from_another() {
        let at = |rev| hf_url("user/m-GGUF", rev, "m-Q4_K_M.gguf");
        assert_eq!(at("abc123"), "https://huggingface.co/user/m-GGUF/resolve/abc123/m-Q4_K_M.gguf");
        // The part's note is this address: a re-upload moves the revision, the
        // address differs, and `download` starts over instead of gluing.
        assert_ne!(at("abc123"), at("def456"));
    }

    #[test]
    fn hf_input_and_file_pick() {
        assert_eq!(parse_hf("unsloth/Qwen3.5-4B-GGUF"), Some(("unsloth/Qwen3.5-4B-GGUF".into(), None)));
        assert_eq!(parse_hf(" https://huggingface.co/a/b-GGUF/ "), Some(("a/b-GGUF".into(), None)));
        assert_eq!(parse_hf("a/b:q8_0"), Some(("a/b".into(), Some("q8_0".into()))));
        assert_eq!(parse_hf("just-a-name"), None);
        assert_eq!(parse_hf("a/b c"), None);
        let files = names(&["README.md", "m-Q8_0.gguf", "m-Q4_K_M.gguf", "mmproj-F16.gguf", "BF16/m-BF16-00001-of-00002.gguf"]);
        assert_eq!(pick_gguf(&files, None).unwrap().0, "m-Q4_K_M.gguf");
        assert_eq!(pick_gguf(&files, Some("q8_0")).unwrap().0, "m-Q8_0.gguf");
        assert!(pick_gguf(&files, Some("BF16")).unwrap_err().contains("split"));
        assert!(pick_gguf(&files, Some("Q2_K")).is_err());
        assert_eq!(pick_gguf(&names(&["x-Q6_K.gguf"]), None).unwrap().0, "x-Q6_K.gguf");
        assert!(pick_gguf(&names(&["mmproj-F16.gguf"]), None).is_err());
    }

    #[test]
    fn fit_and_recommendation() {
        // 5090: the strongest pickable model that fits (never the untested ones).
        let big = hw(32607, 65536);
        assert_eq!(MODELS[recommend(&big, 16384, true).0].name, "Qwen3.8 27B");
        // 8 GB card: 4.5 GB usable → a 4B.
        let small = hw(8192, 16384);
        let (i, why) = recommend(&small, 16384, true);
        assert_eq!(MODELS[i].name, "Qwen3.5 4B");
        assert!(why.contains("Card"));
        // 12 GB card: 9B fits, 27B spills, nothing is too big for 64 GB RAM.
        let mid = hw(12288, 65536);
        assert_eq!(MODELS[recommend(&mid, 16384, true).0].name, "Qwen3.5 9B");
        let by = |name: &str| MODELS.iter().find(|m| m.name == name).unwrap();
        assert_eq!(model_fit(by("Qwen3.8 27B"), &mid, 16384, true), Fit::Spills);
        // Listed, never the recommendation until it has been tried with her tools.
        assert!(!by("Gemma 4 12B").pick && model_fit(by("Gemma 4 12B"), &mid, 16384, true) == Fit::Fits);
        // A 6 GB card: nothing fits beside her voice. The smallest is offered, and
        // the words say it will be split with RAM, not that it fits.
        let six = hw(6144, 16384);
        let (i, why) = recommend(&six, 0, true);
        assert_eq!(MODELS[i].name, "Qwen3.5 4B");
        assert!(why.contains("part of it runs in RAM") && !why.contains("fits fully"), "{}", why);
        // No card and 16 GB of RAM: the 4B, at the processor's 16K, not a 9B at 64K.
        let laptop = Hw { ram_mib: 16384, ..Hw::default() };
        assert_eq!(MODELS[recommend(&laptop, 0, false).0].name, "Qwen3.5 4B");
        assert_eq!(auto_ctx(MODELS[0].bytes, MODELS[0].kv8k_mib, &laptop, false), 16384);
        // 8 GB and no card: nothing fits, and the words say so.
        assert!(recommend(&Hw { ram_mib: 7600, ..Hw::default() }, 0, false).1.contains("doesn't have the memory"));
        // No card: small models only, even with lots of RAM; huge ones are too big for 16 GB.
        let cpu = Hw { ram_mib: 65536, ..Hw::default() };
        assert_eq!(MODELS[recommend(&cpu, 8192, false).0].name, "Qwen3.5 9B");
        let tiny = Hw { ram_mib: 16384, ..Hw::default() };
        assert_eq!(model_fit(by("Qwen3.6 35B-A3B"), &tiny, 8192, false), Fit::TooBig);
    }

    #[test]
    fn your_file_takes_its_listed_row() {
        let dir = std::env::temp_dir().join(format!("sl-model-path-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let m = MODELS.iter().find(|m| m.name == "Qwen3.6 27B").unwrap();
        let mine = dir.join(m.file);
        std::fs::write(&mine, b"x").unwrap();
        // An empty models folder of its own, so a real download on this box can't answer.
        let none = LlmConfig { models_dir: Some(dir.join("models")), ..LlmConfig::default() };
        assert_eq!(gguf_dir(&none), dir.join("models"));
        assert_eq!(gguf_dir(&LlmConfig::default()), llm_root().join("gguf"));
        assert_eq!(model_path(&none, m), gguf_dir(&none).join(m.file));
        let cfg = LlmConfig { files: vec![mine.clone()], ..none.clone() };
        assert_eq!(model_path(&cfg, m), mine);
        // Remembered across a save.
        let back = LlmConfig::parse(&serde_json::json!({"files": [mine.display().to_string()]}).to_string());
        assert_eq!(back.files, vec![mine.clone()]);
        // Another list model isn't fooled by it.
        let other = MODELS.iter().find(|m| m.name == "Qwen3.5 9B").unwrap();
        assert_eq!(model_path(&cfg, other), gguf_dir(&cfg).join(other.file));
        // A download in the models folder wins over a file of yours with the same name.
        std::fs::create_dir_all(dir.join("models")).unwrap();
        std::fs::write(dir.join("models").join(m.file), b"xy").unwrap();
        assert_eq!(model_path(&cfg, m), dir.join("models").join(m.file));
        assert_eq!(downloaded(&cfg), vec![dir.join("models").join(m.file)]);
        assert_eq!(dir_bytes(&dir), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auto_context() {
        let by = |name: &str| MODELS.iter().find(|m| m.name == name).unwrap();
        let auto = |m: &Model, hw: &Hw| auto_ctx(m.bytes, m.kv8k_mib, hw, true);
        // 5090: the smart 27B gets 64K, and the recommendation says so.
        let big = hw(32607, 65536);
        assert_eq!(auto(by("Qwen3.8 27B"), &big), 65536);
        let (i, why) = recommend(&big, 0, true);
        assert_eq!(MODELS[i].name, "Qwen3.8 27B");
        assert!(why.contains("64K"), "{}", why);
        // 24 GB card: the same model at 32K.
        let mid = hw(24564, 65536);
        assert_eq!(MODELS[recommend(&mid, 0, true).0].name, "Qwen3.8 27B");
        assert_eq!(auto(by("Qwen3.8 27B"), &mid), 32768);
        // 8 GB card: the 4B at 32K. 6 GB: 16K is the floor, even spilling.
        assert_eq!(auto(by("Qwen3.5 4B"), &hw(8192, 16384)), 32768);
        assert_eq!(auto(by("Qwen3.5 4B"), &hw(6144, 8192)), 16384);
        // A manual size stays; Auto with nothing to size is 32K.
        let c = LlmConfig { model: Some(PathBuf::from("/m/Qwen3.5-4B-Q4_K_M.gguf")), ..LlmConfig::default() };
        assert_eq!(c.ctx, 0);
        assert_eq!(c.resolved(Some(&big)).ctx, 65536);
        assert_eq!(LlmConfig { ctx: 8192, ..c.clone() }.resolved(Some(&big)).ctx, 8192);
        assert_eq!(LlmConfig::default().resolved(Some(&big)).ctx, 32768);
        assert_eq!(LlmConfig::parse(r#"{"ctx": 0}"#).ctx, 0);
    }

    #[test]
    fn ctx_and_tags() {
        assert_eq!(parse_ctx("16k"), Some(16384));
        assert_eq!(parse_ctx(" 24K "), Some(24576));
        assert_eq!(parse_ctx("10000"), Some(10000));
        assert_eq!(parse_ctx("12"), None);
        assert_eq!(parse_ctx("lots"), None);
        assert_eq!(ctx_words(16384), "16K");
        assert_eq!(ctx_words(10000), "10000");
        assert!(tag_number("b12198") > tag_number("b9999"));
        assert_eq!(tag_number("latest"), None);
    }

    #[test]
    fn config_and_handshake_round_trip() {
        let c = LlmConfig { engine: Some("/x/llama-server".into()), tag: "b1".into(), build: Some(Build::Vulkan), model: Some("/m/a.gguf".into()), ctx: 8192, gpu: false, port: 9000, lan: true, private: false, files: vec![], models_dir: Some("/big/models".into()), off: true, with_sapphire: false, at_sign_in: true, launcher: Some("/apps/launcher".into()) };
        let v = serde_json::json!({"engine": "/x/llama-server", "tag": "b1", "build": "vulkan", "model": "/m/a.gguf", "ctx": 8192, "gpu": false, "port": 9000, "lan": true, "models_dir": "/big/models", "off": true, "with_sapphire": false, "at_sign_in": true, "launcher": "/apps/launcher"});
        assert!(LlmConfig::parse("{}").with_sapphire, "on unless turned off");
        // The sign-in choice outlives a spell of "Only run with Sapphire".
        let on = |with_sapphire, at_sign_in| LlmConfig { engine: Some("/x/llama-server".into()), model: Some("/m/a.gguf".into()), with_sapphire, at_sign_in, ..LlmConfig::default() };
        assert_eq!(on(true, true).sign_in_wanted(), SignIn::Watch);
        assert_eq!(on(false, true).sign_in_wanted(), SignIn::Start);
        assert_eq!(on(false, false).sign_in_wanted(), SignIn::Off);
        assert_eq!(LlmConfig { off: true, ..on(false, true) }.sign_in_wanted(), SignIn::Off);
        assert!(LlmConfig::parse(&on(true, true).to_json().to_string()).at_sign_in, "and it is saved");
        assert_eq!(LlmConfig::parse(&v.to_string()), c);
        assert_eq!(LlmConfig::parse("garbage"), LlmConfig::default());
        assert_eq!(LlmConfig::parse("\u{feff}{}"), LlmConfig::default());
        // "Not whole right now" is told apart from "empty": only the second may be saved over.
        assert_eq!(LlmConfig::parse_checked(""), None);
        assert_eq!(LlmConfig::parse_checked("{\"engine\": \"/x/llama-ser"), None);
        assert_eq!(LlmConfig::parse_checked("[]"), None);
        assert_eq!(LlmConfig::parse_checked("{}"), Some(LlmConfig::default()));
        // A running server whose file was replaced under it is still that server.
        assert!(same_exe(Path::new("/a/bin/b1-cuda/llama-server (deleted)"), Path::new("/a/bin/b1-cuda/llama-server")));
        assert!(!same_exe(Path::new("/a/bin/b2-cuda/llama-server"), Path::new("/a/bin/b1-cuda/llama-server")));
        let r = LlmRun { pid: 42, port: 8642, exe: "/x/llama-server".into(), model: "/m/Qwen3.5-4B-Q4_K_M.gguf".into(), alias: "qwen3.5-4b".into(), ctx: 8192, gpu: true, lan: false, private: false };
        let back = LlmRun::parse(&r.to_json()).unwrap();
        assert_eq!(back, r);
        assert!(r.to_json().contains("\"base_url\": \"http://127.0.0.1:8642/v1\""));
        assert_eq!(model_alias(Path::new("/m/Qwen3.5-4B-Q4_K_M.gguf")), "qwen3.5-4b");
        assert_eq!(model_alias(Path::new("/m/my-model.Q5.gguf")), "my-model.Q5");
    }

    #[test]
    fn server_args_cpu_and_gpu() {
        let c = LlmConfig { gpu: true, ..LlmConfig::default() };
        let a = server_args(&c, Path::new("/m/x.gguf"));
        assert!(a.contains(&"--jinja".to_string()) && !a.contains(&"--device".to_string()));
        assert!(a.windows(2).any(|w| w[0] == "--host" && w[1] == "127.0.0.1"));
        let a = server_args(&LlmConfig { gpu: false, ..c.clone() }, Path::new("/m/x.gguf"));
        assert!(a.windows(2).any(|w| w[0] == "--device" && w[1] == "none"));
        // Shared with the network: every interface. Changing it needs a restart.
        let shared = LlmConfig { lan: true, ..c.clone() };
        assert!(server_args(&shared, Path::new("/m/x.gguf")).windows(2).any(|w| w[0] == "--host" && w[1] == "0.0.0.0"));
        let run = LlmRun { pid: 1, port: 8642, exe: "/x".into(), model: "/m/x.gguf".into(), alias: String::new(), ctx: c.ctx, gpu: true, lan: false, private: false };
        let c = LlmConfig { model: Some("/m/x.gguf".into()), ..c };
        assert!(run.matches(&c) && !run.matches(&LlmConfig { lan: true, ..c.clone() }));
        // Thinking stays the app's call per request (chat_template_kwargs /
        // reasoning_effort): a server-wide --reasoning off would override it.
        assert!(!server_args(&c, Path::new("/m/x.gguf")).iter().any(|a| a.starts_with("--reasoning")));
        // Always: browsers on other sites can't read it, no /slots. Private: no log.
        let a = server_args(&c, Path::new("/m/x.gguf"));
        assert!(a.windows(2).any(|w| w[0] == "--cors-origins" && w[1] == "localhost") && a.contains(&"--no-slots".to_string()) && a.contains(&"--no-webui".to_string()));
        assert!(!a.contains(&"--log-disable".to_string()));
        assert!(server_args(&LlmConfig { private: true, ..c.clone() }, Path::new("/m/x.gguf")).contains(&"--log-disable".to_string()));
        assert!(!run.matches(&LlmConfig { private: true, ..c.clone() }));
        assert_eq!(run.url(), "http://127.0.0.1:8642/v1");
    }

    /// The whole path against the real network and GPU: install the pinned engine,
    /// start a model, see it ready, find it by handshake, stop it by pid. Opt-in:
    /// LLM_E2E=<scratch dir> LLM_E2E_MODEL=<a .gguf> XDG_CONFIG_HOME=<scratch>/cfg
    /// cargo test e2e -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn e2e_install_run_stop() {
        let base = PathBuf::from(std::env::var("LLM_E2E").expect("LLM_E2E"));
        let model_src = PathBuf::from(std::env::var("LLM_E2E_MODEL").expect("LLM_E2E_MODEL"));
        assert!(std::env::var("XDG_CONFIG_HOME").is_ok_and(|x| x.starts_with(base.to_str().unwrap())), "keep the handshake out of the real config dir");
        let root = base.join("llm");
        let models = base.join("gguf");
        let build = std::env::var("LLM_E2E_BUILD").ok().and_then(|b| Build::from_slug(&b)).unwrap_or(Build::Cuda);
        let mut rep = Reporter::new(base.join("job.json"), base.join("job.cancel"), JobSpec { engine: Some(LLAMA_PIN.into()), ..Default::default() });
        let done = install_engine(&root, LLAMA_PIN, build, &mut rep).await.expect("engine install");
        let EngineDone { exe, build: got, note, .. } = done;
        println!("engine {} ({:?}) note={:?} last report={:?}", exe.display(), got, note, JobInfo::parse(&std::fs::read_to_string(base.join("job.json")).unwrap()).map(|j| j.label));
        let hw = probe(Some(exe.clone())).await;
        println!("hw {:?}", hw);
        let model = models.join(model_src.file_name().unwrap());
        std::fs::create_dir_all(model.parent().unwrap()).unwrap();
        if !model.exists() {
            std::fs::hard_link(&model_src, &model).or_else(|_| std::fs::copy(&model_src, &model).map(|_| ())).unwrap();
        }
        let cfg = LlmConfig { engine: Some(exe), tag: LLAMA_PIN.into(), build: Some(got), model: Some(model.clone()), ctx: 4096, gpu: true, port: 8644, lan: false, private: false, files: vec![], models_dir: Some(models), ..LlmConfig::default() };
        cfg.save();
        let run = spawn_server(&cfg).expect("spawn");
        let mut ready = false;
        for _ in 0..120 {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            match look(8644).await {
                Seen::Up(_, Health::Ready) => { ready = true; break; }
                Seen::Off => panic!("died: {:?}", log_tail(5)),
                _ => {}
            }
        }
        assert!(ready, "never ready: {:?}", log_tail(5));
        assert_eq!(find_running().map(|r| r.pid), Some(run.pid));
        #[cfg(target_os = "linux")]
        {
            let limits = std::fs::read_to_string(format!("/proc/{}/limits", run.pid)).unwrap();
            let core = limits.lines().find(|l| l.starts_with("Max core file size")).unwrap();
            assert!(core.split_whitespace().skip(4).take(2).all(|v| v == "0"), "{}", core);
        }
        assert!(downloaded(&cfg).contains(&model));
        assert!(stop(run).await, "stop");
        assert!(find_running().is_none());
        assert_eq!(look(8644).await, Seen::Off);
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sl-llm-test-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn model_spec() -> ModelSpec {
        ModelSpec { repo: "u/m-GGUF".into(), file: "m-Q4_K_M.gguf".into(), quant: None, name: "M".into() }
    }

    #[test]
    fn job_spec_and_report_round_trip() {
        let spec = JobSpec { engine: Some("b1".into()), model: Some(model_spec()), start: true };
        assert_eq!(JobSpec::from_json(&spec.to_json()), Some(spec.clone()));
        // A Hugging Face name with no file yet: the worker picks it.
        let byo = JobSpec { engine: None, model: Some(ModelSpec { repo: "u/m".into(), quant: Some("Q5_K_M".into()), ..Default::default() }), start: false };
        assert_eq!(JobSpec::from_json(&byo.to_json()), Some(byo));
        // Nothing to do, or a model with no repo, is no job.
        assert_eq!(JobSpec::from_json(&serde_json::json!({})), None);
        assert_eq!(JobSpec::from_json(&serde_json::json!({"model": {"file": "x.gguf"}})), None);

        let d = scratch("report");
        let mut rep = Reporter::new(d.join("job.json"), d.join("job.cancel"), spec);
        rep.info.dest = Some(d.join("m.gguf"));
        rep.progress("M", 25, 100);
        let seen = JobInfo::parse(&std::fs::read_to_string(d.join("job.json")).unwrap()).unwrap();
        assert_eq!(seen, rep.info);
        assert_eq!(seen.pct(), Some(25));
        // This process is the worker here: its pid runs the exe the report names.
        assert!(seen.alive());
        assert!(!JobInfo { pid: 0, ..seen.clone() }.alive());
        assert!(!JobInfo { exe: "/some/other/program".into(), ..seen.clone() }.alive(), "a recycled pid is not the worker");
        // How it ended is the last word, and a finished job is not alive.
        assert_eq!(rep.finish(Err("no internet".into())), 1);
        let end = JobInfo::parse(&std::fs::read_to_string(d.join("job.json")).unwrap()).unwrap();
        assert!(end.state == JobState::Failed && end.error == "no internet" && !end.alive());
        assert_eq!(JobInfo::parse("garbage"), None);
    }

    #[tokio::test]
    async fn remove_all_takes_the_engine_and_the_default_models_only() {
        // Under test `llm_root()` is a scratch folder of this process's own.
        let root = llm_root();
        let mine = scratch("my-models");
        std::fs::create_dir_all(root.join("bin").join("b1-cpu")).unwrap();
        std::fs::create_dir_all(root.join("gguf")).unwrap();
        std::fs::write(root.join("bin").join("b1-cpu").join(SERVER_EXE), b"engine").unwrap();
        std::fs::write(root.join("gguf").join("m.gguf"), b"model").unwrap();
        std::fs::write(mine.join("mine.gguf"), b"keep").unwrap();
        assert_eq!(dir_bytes(&root), 11);
        remove_all().await.expect("remove");
        assert!(!root.exists());
        assert!(mine.join("mine.gguf").exists(), "a models folder of your own is never touched");
        remove_all().await.expect("nothing there is not an error");
    }

    fn watch_cfg(name: &str) -> LlmConfig {
        let d = scratch(name);
        std::fs::write(d.join("llama-server"), b"").unwrap();
        std::fs::write(d.join("m.gguf"), b"").unwrap();
        LlmConfig { engine: Some(d.join("llama-server")), model: Some(d.join("m.gguf")), ..LlmConfig::default() }
    }

    /// Every sequence of seven things that can happen (she starts, she stops, someone
    /// starts the LLM, someone stops it, the watcher takes a round) keeps the rules.
    /// The world here is `watch_step` as `watch_main` calls it on a good day: a start
    /// that fails, the backoff and a stop that doesn't take are not modelled.
    #[test]
    fn a_server_started_for_her_is_hers_once_she_has_had_her_time() {
        // Launch started it and she died before any round saw the two together.
        assert_eq!(hers_by_note(Some(7), Some((7, 10)), 300), None, "she may still be on her way up");
        assert_eq!(hers_by_note(Some(7), Some((7, 300)), 300), Some(7));
        // The note is about one server only: not the next, not none, and no note is no claim.
        assert_eq!(hers_by_note(Some(8), Some((7, 900)), 300), None);
        assert_eq!(hers_by_note(None, Some((7, 900)), 300), None);
        assert_eq!(hers_by_note(Some(7), None, 300), None);
        // And then the watcher's own rule ends it: she is down, it is hers.
        let cfg = LlmConfig { engine: Some(PathBuf::from("/e")), model: Some(PathBuf::from("/m")), with_sapphire: true, ..LlmConfig::default() };
        let mut with_her = hers_by_note(Some(7), Some((7, 300)), 300);
        assert_eq!(watch_step(&cfg, false, Some(7), false, &mut with_her), Watch::Stop);
    }

    #[test]
    fn the_watcher_keeps_its_rules_over_every_sequence() {
        #[derive(Clone, Copy, Debug)]
        enum Ev {
            HerUp,
            HerDown,
            UserStart,
            UserStop,
            Round,
        }
        const EVS: [Ev; 5] = [Ev::HerUp, Ev::HerDown, Ev::UserStart, Ev::UserStop, Ev::Round];
        let cfg = watch_cfg("watch-rules");
        const LEN: u32 = 7;
        for code in 0..5u32.pow(LEN) {
            let seq: Vec<Ev> = (0..LEN).map(|i| EVS[(code / 5u32.pow(i) % 5) as usize]).collect();
            // The world: is she up, the server's pid, the "stay stopped" flag.
            let (mut her, mut llm, mut hold) = (false, None::<u32>, false);
            let mut next_pid = 100;
            // The truth the watcher must track: this very server was up with her at
            // a round, or the watcher started it for her.
            let mut hers = false;
            let mut with_her = None;
            for ev in &seq {
                match ev {
                    Ev::HerUp => her = true,
                    Ev::HerDown => her = false,
                    Ev::UserStart => {
                        hold = false; // any start ends a hold
                        if llm.is_none() {
                            next_pid += 1;
                            llm = Some(next_pid);
                            hers = false;
                        }
                    }
                    Ev::UserStop => {
                        hold = true;
                        llm = None;
                        hers = false;
                    }
                    Ev::Round => {
                        if !her {
                            hold = false;
                        }
                        match watch_step(&cfg, her, llm, hold, &mut with_her) {
                            Watch::Start => {
                                assert!(her && llm.is_none() && !hold, "{:?}: started with her down, or over a hold", seq);
                                next_pid += 1;
                                llm = Some(next_pid);
                                with_her = llm; // as the loop does after a start
                                hers = true;
                            }
                            Watch::Stop => {
                                assert!(!her && llm.is_some() && hers, "{:?}: stopped an LLM that was not hers", seq);
                                llm = None;
                                hers = false;
                            }
                            Watch::Nothing => {
                                assert!(!(her && llm.is_none() && !hold), "{:?}: she is up and nothing holds it, yet no start", seq);
                                assert!(!(!her && llm.is_some() && hers), "{:?}: hers, she is down, yet no stop", seq);
                            }
                            Watch::Quit => panic!("{:?}: quit with the LLM switched on", seq),
                        }
                        if her && llm.is_some() {
                            hers = true;
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_watcher_waits_or_quits_when_it_should() {
        let cfg = watch_cfg("watch-quit");
        let mut w = None;
        // A model that isn't on disk can't be started; the watcher just waits.
        let gone = LlmConfig { model: Some(PathBuf::from("/nowhere/missing.gguf")), ..cfg.clone() };
        assert_eq!(watch_step(&gone, true, None, false, &mut w), Watch::Nothing);
        // Switched off, unticked, or removed: the watcher ends by itself.
        for off in [LlmConfig { off: true, ..cfg.clone() }, LlmConfig { model: None, ..cfg.clone() }, LlmConfig { with_sapphire: false, ..cfg.clone() }, LlmConfig { engine: None, ..cfg.clone() }] {
            assert_eq!(watch_step(&off, true, Some(7), false, &mut w), Watch::Quit);
        }
        // A start that keeps failing is tried less and less often, up to 16 minutes.
        assert_eq!(watch_backoff(1).as_secs(), 30);
        assert_eq!(watch_backoff(40).as_secs(), 960);
        // The sign-in entry says which of the two it runs.
        assert_eq!(SignIn::of("Exec=\"/a/b.AppImage\" --llm-watch"), SignIn::Watch);
        assert_eq!(SignIn::of("    SapphireLLM    REG_SZ    \"C:\\x\\launcher.exe\" --start-llm"), SignIn::Start);
        assert_eq!(SignIn::of(""), SignIn::Off);
    }

    /// What Sapphire's core reads from us. A key renamed or dropped here breaks her
    /// side without a compile error anywhere, so the names are pinned.
    #[tokio::test]
    async fn the_contract_with_core_keeps_its_keys() {
        let keys = |v: &serde_json::Value| -> Vec<String> {
            let mut k: Vec<String> = v.as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
            k.sort();
            k
        };
        let st = status(true).await;
        assert_eq!(
            keys(&st),
            ["available", "job", "last_job", "models", "ok", "phase", "running", "sapphire_running", "set_up", "settings", "sign_in", "state", "version"]
        );
        assert_eq!(keys(&st["settings"]), ["ctx", "gpu", "model", "models_dir", "on", "port", "with_sapphire"]);
        assert_eq!(keys(&st["available"][0]), ["about", "bytes", "file", "name"]);
        assert!(st["sapphire_running"] == true && st["ok"] == true);
        assert!(["running", "loading", "downloading", "stopped", "none"].contains(&st["phase"].as_str().unwrap()));
        // llm-run.json: core reads base_url (and may read the rest).
        let run = LlmRun { pid: 42, port: 8642, exe: "/x/llama-server".into(), model: "/m/a.gguf".into(), alias: "a".into(), ctx: 8192, gpu: true, lan: false, private: false };
        let v: serde_json::Value = serde_json::from_str(&run.to_json()).unwrap();
        assert_eq!(keys(&v), ["alias", "base_url", "ctx", "exe", "gpu", "lan", "lan_url", "model", "pid", "port", "private", "started"]);
        assert_eq!(v["base_url"], "http://127.0.0.1:8642/v1");
        // llm.json: what core needs to find and call the launcher is among these.
        let cfg = LlmConfig { launcher: Some("/apps/launcher".into()), ..LlmConfig::default() }.to_json();
        assert_eq!(keys(&cfg), ["at_sign_in", "build", "ctx", "engine", "files", "gpu", "lan", "launcher", "model", "models_dir", "off", "port", "private", "tag", "with_sapphire"]);
        assert!(cfg["launcher"] == "/apps/launcher" && cfg["with_sapphire"] == true);
        // The nested objects of the status reply: core reads phase, job.{label,percent},
        // running.{alias,ctx} and last_job.{state,error}. All of each are pinned.
        let mut at_work = JobInfo::waiting(1, JobSpec { start: true, ..Default::default() });
        at_work.label = "Qwen3.5 4B".into();
        at_work.done = 50;
        at_work.total = 200;
        let j = job_json(&at_work);
        assert_eq!(keys(&j), ["done", "engine", "label", "percent", "total"]);
        assert!(j["label"] == "Qwen3.5 4B" && j["percent"] == 25, "{}", j);
        assert_eq!(keys(&running_json(&run)), ["alias", "base_url", "ctx", "gpu", "model", "pid"]);
        assert!(running_json(&run)["alias"] == "a" && running_json(&run)["ctx"] == 8192 && running_json(&run)["base_url"] == "http://127.0.0.1:8642/v1");
        // How a download ended: one word, plus why when it failed.
        let ended = |state: JobState, error: &str| last_job_json(&JobInfo { state, error: error.into(), ..at_work.clone() });
        assert_eq!(keys(&ended(JobState::Done, "")), ["error", "label", "state"]);
        assert_eq!(ended(JobState::Done, "")["state"], "done");
        assert_eq!(ended(JobState::Cancelled, "")["state"], "cancelled");
        assert_eq!(ended(JobState::Running, "")["state"], "interrupted", "its worker died without a word");
        let failed = ended(JobState::Failed, "Couldn't reach the internet.");
        assert!(failed["state"] == "failed" && failed["error"] == "Couldn't reach the internet.");
        // Every word `phase` can be.
        let phases = [phase("ready", false, true), phase("loading", true, true), phase("starting", false, true), phase("off", true, true), phase("off", false, true), phase("off", false, false)];
        assert_eq!(phases, ["running", "loading", "loading", "downloading", "stopped", "none"]);
        // llm-job.json: what a progress bar reads.
        let job: serde_json::Value = serde_json::from_str(&JobInfo::waiting(1, JobSpec { start: true, ..Default::default() }).to_json()).unwrap();
        assert_eq!(keys(&job), ["curl", "dest", "done", "engine", "error", "exe", "label", "note", "pid", "spec", "started", "state", "total"]);
    }

    #[test]
    fn a_switch_nobody_knows_is_refused_and_never_opens_the_window() {
        let run = |args: &[&str]| cli(&std::iter::once("launcher").chain(args.iter().copied()).map(String::from).collect::<Vec<_>>(), || false);
        // A caller built for a newer launcher, and a typo: an error it can read.
        assert_eq!(run(&["--llm-health"]), Some(1));
        assert_eq!(run(&["--llm-statsu"]), Some(1));
        assert_eq!(run(&["--llm-status", "--verbose"]), Some(1), "next to a real one too");
        // Two real ones at once, and one with no value: the errors from before.
        assert_eq!(run(&["--start-llm", "--stop-llm"]), Some(1));
        assert_eq!(run(&["--llm-model"]), Some(1));
        // No switch at all, or a plain word (a file dropped on the icon): the window.
        assert_eq!(run(&[]), None);
        assert_eq!(run(&["somefile.txt"]), None);
    }

    #[test]
    fn one_word_for_where_the_llm_is() {
        assert_eq!(phase("off", false, false), "none");
        assert_eq!(phase("off", true, false), "downloading");
        assert_eq!(phase("off", false, true), "stopped");
        assert_eq!(phase("off", true, true), "downloading", "a second model coming in while it is stopped");
        assert_eq!(phase("starting", false, true), "loading");
        assert_eq!(phase("loading", false, true), "loading");
        assert_eq!(phase("ready", false, true), "running");
        assert_eq!(phase("ready", true, true), "running", "a server that answers wins over a download beside it");
    }

    #[test]
    fn one_worker_at_a_time() {
        let first = job_lock().expect("the first worker gets the lock");
        assert!(job_lock().is_none(), "a second worker is turned away while the first lives");
        drop(first);
        // "Gone" can lag by a moment here only: another test forking a child at that
        // instant gives the child a copy of the handle until it execs.
        let freed = (0..100).any(|_| {
            job_lock().is_some() || {
                std::thread::sleep(std::time::Duration::from_millis(10));
                false
            }
        });
        assert!(freed, "and the lock is free again once it is gone");
    }

    /// The download path against a local file (curl speaks file://): resume, finish,
    /// cancel. No network.
    #[cfg(unix)]
    #[tokio::test]
    async fn download_resumes_finishes_and_cancel_deletes_the_part() {
        if std::process::Command::new("curl").arg("--version").output().is_err() {
            return; // no curl on this box
        }
        let d = scratch("download");
        let src = d.join("source.bin");
        let body: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&src, &body).unwrap();
        let url = format!("file://{}", src.display());
        let dest = d.join("models").join("m.gguf");
        let mut rep = Reporter::new(d.join("job.json"), d.join("job.cancel"), JobSpec { model: Some(model_spec()), ..Default::default() });

        // A part left by an earlier try at this same address is continued, not
        // restarted: its first bytes are kept as they are (zeros here, to prove it).
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(part_of(&dest), vec![0u8; 50_000]).unwrap();
        std::fs::write(part_source(&dest), &url).unwrap();
        download(&url, &dest, body.len() as u64, None, "M", &mut rep).await.expect("download");
        let got = std::fs::read(&dest).unwrap();
        assert_eq!(got.len(), body.len());
        assert!(got[..50_000].iter().all(|b| *b == 0) && got[50_000..] == body[50_000..], "resumed, not restarted");
        assert!(!part_of(&dest).exists() && !part_source(&dest).exists());
        assert_eq!(rep.info.label, "M");

        // A part from somewhere else (another repo's file of the same name), or one
        // with no note of where it came from, is never glued onto: it starts over.
        let dest_b = d.join("models").join("b.gguf");
        std::fs::write(part_of(&dest_b), vec![0u8; 50_000]).unwrap();
        std::fs::write(part_source(&dest_b), "https://example.invalid/other-repo/b.gguf").unwrap();
        download(&url, &dest_b, body.len() as u64, None, "B", &mut rep).await.expect("download");
        assert_eq!(std::fs::read(&dest_b).unwrap(), body);

        // curl content, size not the one promised: thrown away, not kept as a model.
        let dest_w = d.join("models").join("w.gguf");
        let err = download(&url, &dest_w, body.len() as u64 + 7, None, "W", &mut rep).await.unwrap_err();
        assert!(err.contains("wrong size"), "{}", err);
        assert!(!dest_w.exists() && !part_of(&dest_w).exists());

        // The site's checksum: the right bytes get their name...
        let sum = sha256_file(&src, |_| true).unwrap().unwrap();
        let dest_s = d.join("models").join("s.gguf");
        download(&url, &dest_s, body.len() as u64, Some(&sum), "S", &mut rep).await.expect("download");
        assert_eq!(std::fs::read(&dest_s).unwrap(), body);
        assert_eq!(rep.info.label, "Checking S");
        // ...and a part with a blank stretch in it, resumed to exactly the promised
        // size (the first case above: what a power cut leaves), does not.
        let dest_c = d.join("models").join("c.gguf");
        std::fs::write(part_of(&dest_c), vec![0u8; 50_000]).unwrap();
        std::fs::write(part_source(&dest_c), &url).unwrap();
        let err = download(&url, &dest_c, body.len() as u64, Some(&sum), "C", &mut rep).await.unwrap_err();
        assert!(err.contains("checksum"), "{}", err);
        assert!(!dest_c.exists() && !part_of(&dest_c).exists() && !part_source(&dest_c).exists());

        // Cancel: curl is ended and the unfinished file is deleted.
        let dest2 = d.join("models").join("n.gguf");
        std::fs::write(d.join("job.cancel"), b"").unwrap();
        let err = download(&url, &dest2, body.len() as u64, None, "N", &mut rep).await.unwrap_err();
        assert!(err.contains("Cancelled") && rep.cancelled);
        assert!(!dest2.exists() && !part_of(&dest2).exists() && !part_source(&dest2).exists());
        assert_eq!(rep.finish(Err(err)), 4);
        assert_eq!(JobInfo::parse(&std::fs::read_to_string(d.join("job.json")).unwrap()).unwrap().state, JobState::Cancelled);
        assert!(!d.join("job.cancel").exists(), "the cancel flag is used up");

        // A source that isn't there fails in plain words and keeps nothing half-made.
        let mut rep = Reporter::new(d.join("job.json"), d.join("job.cancel"), JobSpec { model: Some(model_spec()), ..Default::default() });
        let err = download(&format!("file://{}", d.join("missing.bin").display()), &d.join("models").join("o.gguf"), 10, None, "O", &mut rep).await.unwrap_err();
        assert!(err.contains("Download of O stopped"), "{}", err);
        // No room: refused before a byte moves.
        let err = download(&url, &d.join("models").join("p.gguf"), u64::MAX / 4, None, "P", &mut rep).await.unwrap_err();
        assert!(err.contains("Not enough room"), "{}", err);
        assert!(free_bytes(&d).is_some_and(|f| f > 0));
    }

    #[test]
    fn a_watcher_knows_when_it_is_no_longer_the_right_one() {
        let d = scratch("successor");
        let (a, b) = (d.join("a").join("launcher"), d.join("b").join("launcher"));
        for f in [&a, &b] {
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, b"exe").unwrap();
        }
        // Nobody named another launcher, or the one named is this one, however spelled.
        assert_eq!(watch_successor(&a, None, false), None);
        assert_eq!(watch_successor(&a, Some(&a), false), None);
        assert_eq!(watch_successor(&a, Some(&d.join("a").join("..").join("a").join("launcher")), false), None);
        // Another launcher's window opened last: the job goes to it.
        assert_eq!(watch_successor(&a, Some(&b), false), Some(b.clone()));
        // One that is named but gone (a download since deleted) gets nothing.
        assert_eq!(watch_successor(&a, Some(&d.join("gone").join("launcher")), false), None);
        // This one's own file replaced by a newer one: the same path, started afresh.
        assert_eq!(watch_successor(&a, Some(&a), true), Some(a.clone()));
        assert_eq!(watch_successor(&a, None, true), Some(a.clone()));
        // Its own file deleted and the other still there: hand over.
        std::fs::remove_file(&a).unwrap();
        assert_eq!(watch_successor(&a, Some(&b), false), Some(b.clone()));
        // A file's stamp moves when the file does.
        let before = exe_stamp(&b).unwrap();
        std::fs::write(&b, b"a longer exe").unwrap();
        assert_ne!(exe_stamp(&b).unwrap(), before);
        assert_eq!(exe_stamp(&a), None);
    }

    #[test]
    fn a_model_is_checked_by_checksum_when_the_site_gives_one_and_by_its_first_bytes() {
        let d = scratch("sums");
        std::fs::write(d.join("abc"), b"abc").unwrap();
        assert_eq!(sha256_file(&d.join("abc"), |_| true).unwrap().unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(sha256_file(&d.join("abc"), |_| false).unwrap(), None, "it stops when told to");
        assert!(sha256_file(&d.join("missing"), |_| true).is_err());
        // What Hugging Face lists: only a real checksum is taken, in lowercase.
        let v = serde_json::json!({"siblings": [
            {"rfilename": "m.gguf", "size": 3, "lfs": {"sha256": "AB".repeat(32), "size": 3}},
            {"rfilename": "README.md", "size": 3},
            {"rfilename": "odd.gguf", "lfs": {"sha256": "not-a-checksum"}},
            {"rfilename": "short.gguf", "lfs": {"sha256": "abc123"}},
            {"rfilename": "none.gguf", "lfs": {}},
        ]});
        assert_eq!(listing_sha256(&v), vec![("m.gguf".to_string(), "ab".repeat(32))]);
        assert!(listing_sha256(&serde_json::json!({})).is_empty(), "no listing: nothing to check against, and no error");
        // The first four bytes.
        std::fs::write(d.join("ok.gguf"), b"GGUF\x03\0\0\0 and the rest").unwrap();
        std::fs::write(d.join("zip.gguf"), b"PK\x03\x04 an archive under a model's name").unwrap();
        std::fs::write(d.join("cut.gguf"), b"GG").unwrap();
        assert!(is_gguf(&d.join("ok.gguf")));
        assert!(!is_gguf(&d.join("zip.gguf")) && !is_gguf(&d.join("cut.gguf")) && !is_gguf(&d.join("missing.gguf")));
        // A start says so itself, before llama.cpp gets a file it can only choke on.
        std::fs::write(d.join("llama-server"), b"").unwrap();
        let cfg = LlmConfig { engine: Some(d.join("llama-server")), model: Some(d.join("zip.gguf")), ..LlmConfig::default() };
        assert!(spawn_server(&cfg).unwrap_err().contains("damaged"));
    }

    #[test]
    fn cli_names_find_models_and_downloads() {
        let d = scratch("cli");
        let cfg = LlmConfig { models_dir: Some(d.clone()), ..LlmConfig::default() };
        let four = MODELS.iter().find(|m| m.name == "Qwen3.5 4B").unwrap();
        // Nothing on disk: nothing to switch to, everything to download.
        assert_eq!(find_model(&cfg, "Qwen3.5 4B"), None);
        let spec = download_spec(&cfg, "qwen3.5-4b").unwrap();
        assert_eq!(spec.model, Some(ModelSpec::curated(four)));
        assert_eq!(spec.engine.as_deref(), Some(LLAMA_PIN), "a first download brings the engine");
        assert!(!spec.start, "a download starts nothing");
        assert_eq!(download_spec(&cfg, four.file).unwrap().model, Some(ModelSpec::curated(four)));
        // A Hugging Face name: the worker picks the file.
        let hf = download_spec(&cfg, "someone/Some-Model-GGUF:Q5_K_M").unwrap().model.unwrap();
        assert_eq!((hf.repo.as_str(), hf.file.as_str(), hf.quant.as_deref()), ("someone/Some-Model-GGUF", "", Some("Q5_K_M")));
        assert!(download_spec(&cfg, "not a model").is_err());
        // On disk: found by list name, alias, file name or path; not downloaded twice.
        let path = d.join(four.file);
        std::fs::write(&path, b"x").unwrap();
        for want in ["Qwen3.5 4B", "qwen3.5-4b", "QWEN3.5-4B-Q4_K_M.GGUF", "Qwen3.5-4B-Q4_K_M", &path.display().to_string()] {
            assert_eq!(find_model(&cfg, want), Some(path.clone()), "{}", want);
        }
        assert!(download_spec(&cfg, "Qwen3.5 4B").unwrap_err().contains("already downloaded"));
        // One of yours, by its file name.
        let mine = d.join("my-model.Q5.gguf");
        std::fs::write(&mine, b"x").unwrap();
        assert_eq!(find_model(&cfg, "my-model.Q5"), Some(mine));
    }

    #[test]
    fn health_bodies() {
        assert_eq!(health_from(r#"{"status":"ok"}"#), Health::Ready);
        assert_eq!(health_from(r#"{"error":{"message":"Loading model","code":503}}"#), Health::Loading);
        assert_eq!(health_from(""), Health::Down);
        assert_eq!(health_from("<html>"), Health::Down);
    }
}
