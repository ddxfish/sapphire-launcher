// The LLM tab: state, messages and view for the local llama.cpp server (llm.rs).
use crate::llm::*;
use crate::*;
use iced::widget::{button, checkbox, column, container, horizontal_space, pick_list, radio, row, text, text_input, tooltip, Column};
use iced::{color, Color, Element, Font, Length};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub(crate) enum LlmMsg {
    Probed(Hw),
    Poll,
    Polled(Seen),
    Select(PathBuf),
    Ctx(CtxChoice),
    CtxText(String),
    CtxApply,
    Gpu(bool),
    SetUp,
    Download,
    Cancel,
    Progress(String, u64, u64),
    JobDone(Result<JobDone, String>),
    Start,
    Started(Result<LlmRun, String>),
    Stop,
    Restart,
    Stopped(bool),
    Autostart(bool),
    ToggleAdvanced,
    ByoText(String),
    ByoFind,
    ByoFound(Result<(String, String, u64), String>),
    ByoDownload,
    CheckEngine,
    EngineChecked(Result<Option<String>, String>),
    UpdateEngine,
    Delete,
    OpenLog,
    PortText(String),
    CopyUrl,
    Lan(bool),
    Private(bool),
}

/// A context size in the dropdown ("16K").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CtxChoice(pub u32);

impl std::fmt::Display for CtxChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0 == 0 { write!(f, "Auto") } else { write!(f, "{}", ctx_words(self.0)) }
    }
}

#[derive(Debug, Default)]
pub(crate) struct LlmUi {
    pub cfg: LlmConfig,
    pub hw: Option<Hw>,
    pub seen: Option<Seen>,
    pub starting: Option<Instant>,
    pub stopping: bool,
    pub restart_after_stop: bool,
    pub job: Option<Job>,
    pub job_epoch: usize,
    pub progress: Option<(String, u64, u64)>,
    /// "Set up" in progress: engine → model → start, each step chaining the next.
    pub chain: bool,
    pub selected: Option<PathBuf>,
    pub ctx_text: String,
    pub port_text: String,
    pub byo_text: String,
    pub byo: Option<(String, String, u64)>,
    pub note: Option<(String, bool)>, // (words, is a problem)
    pub advanced: bool,
    pub newer: Option<String>,
    pub autostart: bool,
}

impl LlmUi {
    pub(crate) fn new() -> Self {
        let cfg = LlmConfig::load();
        LlmUi { port_text: cfg.port.to_string(), selected: cfg.model.clone(), autostart: llm::autostart_on(), cfg, ..Default::default() }
    }

    pub(crate) fn running(&self) -> Option<&LlmRun> {
        match &self.seen {
            Some(Seen::Up(run, _)) => Some(run),
            _ => None,
        }
    }

    /// Anything animating: a job, a start waiting for the model to load, a stop.
    pub(crate) fn busy(&self) -> bool {
        self.job.is_some() || self.starting.is_some() || self.stopping
    }
}

const GREEN: Color = color!(0x4caf50);
const YELLOW: Color = color!(0xf9e154);
const RED: Color = color!(0xe74c3c);
const MUTED: Color = color!(0x7f849c);
const BLUE: Color = color!(0x3d85c6);

fn fit_color(f: Fit) -> Color {
    match f {
        Fit::Fits => GREEN,
        Fit::Spills => YELLOW,
        Fit::TooBig => RED,
    }
}

fn gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

/// A small coloured square: ASCII-safe (no glyph the bundled font might lack).
fn dot<'a>(c: Color) -> Element<'a, Message> {
    container(text("")).width(9).height(9).style(move |_t| container::Style {
        background: Some(iced::Background::Color(c)),
        border: iced::Border { radius: 2.0.into(), ..Default::default() },
        ..Default::default()
    })
    .into()
}

fn bold() -> Font {
    Font { weight: iced::font::Weight::Bold, ..Font::DEFAULT }
}

impl App {
    fn llm_log(&mut self, s: impl Into<String>) {
        self.log_lines.push(format!("[llm] {}", s.into()));
    }

    fn llm_note(&mut self, s: impl Into<String>, problem: bool) {
        let s = s.into();
        self.llm_log(s.clone());
        self.llm.note = Some((s, problem));
    }

    fn llm_probe(&self) -> Task<Message> {
        let engine = self.llm.cfg.engine.clone();
        Task::perform(llm::probe(engine), |h| Message::Llm(LlmMsg::Probed(h)))
    }

    fn llm_poll(&self) -> Task<Message> {
        Task::perform(llm::look(self.llm.cfg.port), |s| Message::Llm(LlmMsg::Polled(s)))
    }

    /// The selected model's file on disk (where it is, or where it will be).
    fn llm_selected(&self) -> Option<PathBuf> {
        self.llm.selected.clone().or_else(|| {
            let hw = self.llm.hw.as_ref()?;
            let (i, _) = recommend(hw, self.llm.cfg.ctx, self.llm.cfg.gpu);
            Some(model_path(&self.install_path, &self.llm.cfg, &MODELS[i]))
        })
    }

    fn llm_start_job(&mut self, job: Job) -> Task<Message> {
        self.llm.job = Some(job);
        self.llm.job_epoch += 1;
        self.llm.progress = None;
        self.llm.note = None;
        Task::none()
    }

    fn llm_engine_job(&mut self, tag: String) -> Task<Message> {
        let Some(hw) = self.llm.hw.clone() else {
            return self.llm_probe();
        };
        let build = match self.llm.cfg.build {
            Some(b) if self.llm.cfg.engine_ok() => b, // an update keeps its kind
            _ => pick_build(&hw),
        };
        self.llm_log(format!("Installing llama.cpp {} ({}).", tag, build.words()));
        self.llm_start_job(Job::Engine { root: llm_root(&self.install_path), tag, build })
    }

    fn llm_model_job(&mut self, path: &std::path::Path) -> Task<Message> {
        let file = path.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
        let Some(m) = curated(&file) else {
            self.llm_note("That model isn't on the list; use Your own model under Advanced.", true);
            return Task::none();
        };
        self.llm_log(format!("Downloading {} ({}).", m.name, gb(m.bytes)));
        self.llm_start_job(Job::Model { repo: m.repo.into(), file: m.file.into(), dest: path.to_path_buf(), name: m.name.into() })
    }

    /// One step of "Set up": whatever is missing next, else start.
    fn llm_setup_next(&mut self) -> Task<Message> {
        if !self.llm.cfg.engine_ok() {
            return self.llm_engine_job(LLAMA_PIN.to_string());
        }
        let Some(sel) = self.llm_selected() else {
            self.llm.chain = false;
            return Task::none();
        };
        if !sel.exists() {
            self.llm.selected = Some(sel.clone());
            return self.llm_model_job(&sel);
        }
        self.llm.chain = false;
        if self.llm.running().is_some() {
            return self.llm_update(LlmMsg::Restart);
        }
        self.llm_update(LlmMsg::Start)
    }

    fn llm_set_cfg(&mut self, f: impl FnOnce(&mut LlmConfig)) {
        f(&mut self.llm.cfg);
        self.llm.cfg.save();
    }

    pub(crate) fn llm_update(&mut self, msg: LlmMsg) -> Task<Message> {
        match msg {
            LlmMsg::Probed(hw) => {
                if self.llm.hw.is_none() {
                    let gpu = hw.gpu_name.as_deref().map_or("no graphics card".to_string(), |n| format!("{} ({} MiB)", n, hw.vram_mib));
                    self.llm_log(format!("This computer: {}, {} MiB RAM.", gpu, hw.ram_mib));
                }
                // No card seen (yet: AMD/Intel cards show up once the engine can list
                // them) leaves the GPU choice alone; fit and recommend read "no card" as CPU.
                self.llm.hw = Some(hw);
                if self.llm.chain && self.llm.job.is_none() {
                    return self.llm_setup_next();
                }
                Task::none()
            }
            LlmMsg::Poll => self.llm_poll(),
            LlmMsg::Polled(seen) => {
                if self.llm.stopping {
                    return Task::none();
                }
                if let Some(t) = self.llm.starting {
                    match &seen {
                        Seen::Up(run, Health::Ready) => {
                            self.llm.starting = None;
                            self.llm.note = None; // the status line says it
                            self.llm_log(format!("{} is ready ({} context, {}).", model_label(&run.model), ctx_words(run.ctx), if run.gpu { "graphics card" } else { "processor" }));
                        }
                        Seen::Off if t.elapsed().as_secs() >= 3 => {
                            self.llm.starting = None;
                            let tail = log_tail(4);
                            for l in &tail {
                                self.llm_log(format!("  {}", l));
                            }
                            match tail.last() {
                                Some(last) => self.llm_note(format!("The LLM stopped while starting. Last words: {}", last), true),
                                None if self.llm.cfg.private => self.llm_note("The LLM stopped while starting. Private mode keeps no log: untick it and start again to see why.", true),
                                None => self.llm_note("The LLM stopped while starting and left no words.", true),
                            }
                        }
                        _ => {}
                    }
                }
                self.llm.seen = Some(seen);
                Task::none()
            }
            LlmMsg::Select(p) => {
                // A downloaded pick becomes the default (what Start and sign-in load);
                // while one runs, "Restart to apply" offers the switch.
                self.llm.selected = Some(p.clone());
                if p.exists() {
                    self.llm_set_cfg(|c| c.model = Some(p));
                }
                Task::none()
            }
            LlmMsg::Ctx(c) => {
                self.llm.ctx_text.clear();
                self.llm_set_cfg(|cfg| cfg.ctx = c.0);
                Task::none()
            }
            LlmMsg::CtxText(s) => {
                self.llm.ctx_text = s;
                Task::none()
            }
            LlmMsg::CtxApply => {
                match parse_ctx(&self.llm.ctx_text) {
                    Some(n) => {
                        self.llm_set_cfg(|c| c.ctx = n);
                        self.llm.ctx_text.clear();
                    }
                    None => self.llm_note("Context is a number of tokens, like 24576 or 24k.", true),
                }
                Task::none()
            }
            LlmMsg::Gpu(on) => {
                self.llm_set_cfg(|c| c.gpu = on);
                Task::none()
            }
            LlmMsg::PortText(s) => {
                if let Ok(p) = s.trim().parse::<u16>()
                    && p >= 1024
                {
                    self.llm_set_cfg(|c| c.port = p);
                }
                self.llm.port_text = s;
                Task::none()
            }
            LlmMsg::SetUp => {
                if self.llm.job.is_some() {
                    return Task::none();
                }
                // The model announced under the button is the one downloaded, even if
                // the engine's device list changes the recommendation halfway.
                self.llm.selected = self.llm_selected();
                self.llm.chain = true;
                self.llm_setup_next()
            }
            LlmMsg::Download => {
                if self.llm.job.is_some() {
                    return Task::none();
                }
                match self.llm_selected() {
                    Some(p) => {
                        self.llm.selected = Some(p.clone());
                        self.llm_model_job(&p)
                    }
                    None => Task::none(),
                }
            }
            LlmMsg::Cancel => {
                self.llm.job = None;
                self.llm.chain = false;
                self.llm.progress = None;
                self.llm_note("Stopped the download. Download again to pick up where it left off.", false);
                Task::none()
            }
            LlmMsg::Progress(label, done, total) => {
                if self.llm.job.is_some() {
                    self.llm.progress = Some((label, done, total));
                }
                Task::none()
            }
            LlmMsg::JobDone(result) => {
                self.llm.job = None;
                self.llm.progress = None;
                match result {
                    Ok(JobDone::Engine { exe, tag, build, note }) => {
                        self.llm_log(format!("llama.cpp {} ({}) installed.", tag, build.words()));
                        if let Some(n) = note {
                            self.llm_note(n, false);
                        }
                        self.llm.newer = None;
                        self.llm_set_cfg(|c| {
                            c.engine = Some(exe);
                            c.tag = tag;
                            c.build = Some(build);
                        });
                        if self.llm.running().is_some() && !self.llm.chain {
                            self.llm_note("Updated. Restart the LLM to use the new llama.cpp.", false);
                        }
                        // The engine's own device list sees every vendor's card.
                        self.llm.hw = None;
                        self.llm_probe()
                    }
                    Ok(JobDone::Model(path)) => {
                        self.llm_log(format!("{} downloaded.", model_label(&path)));
                        self.llm.selected = Some(path.clone());
                        self.llm.byo = None;
                        if self.llm.running().is_none() {
                            self.llm_set_cfg(|c| c.model = Some(path));
                        }
                        if self.llm.chain {
                            return self.llm_setup_next();
                        }
                        Task::none()
                    }
                    Err(e) => {
                        self.llm.chain = false;
                        self.llm_note(e, true);
                        Task::none()
                    }
                }
            }
            LlmMsg::Start => {
                if self.llm.running().is_some() || self.llm.starting.is_some() || self.llm.stopping {
                    return Task::none();
                }
                if let Some(sel) = self.llm_selected().filter(|p| p.exists()) {
                    self.llm_set_cfg(|c| c.model = Some(sel));
                }
                let cfg = self.llm.cfg.resolved(self.llm.hw.as_ref());
                self.llm.note = None;
                self.llm_log(format!(
                    "Starting {} ({} context, {}).",
                    cfg.model.as_deref().map(model_label).unwrap_or_default(),
                    ctx_words(cfg.ctx),
                    if cfg.gpu { "graphics card" } else { "processor" }
                ));
                Task::perform(
                    async move {
                        if llm::health(cfg.port).await != Health::Down {
                            return Err(format!("Something else is already using port {}. Pick another port under Advanced.", cfg.port));
                        }
                        tokio::task::spawn_blocking(move || spawn_server(&cfg)).await.map_err(|e| e.to_string())?
                    },
                    |r| Message::Llm(LlmMsg::Started(r)),
                )
            }
            LlmMsg::Started(r) => match r {
                Ok(run) => {
                    self.llm.starting = Some(Instant::now());
                    self.llm.seen = Some(Seen::Up(run, Health::Loading));
                    Task::none()
                }
                Err(e) => {
                    self.llm_note(e, true);
                    Task::none()
                }
            },
            LlmMsg::Stop | LlmMsg::Restart => {
                let Some(run) = self.llm.running().cloned() else { return Task::none() };
                if self.llm.stopping {
                    return Task::none();
                }
                self.llm.restart_after_stop = matches!(msg, LlmMsg::Restart);
                self.llm.stopping = true;
                self.llm.starting = None;
                self.llm_log("Stopping the LLM.");
                Task::perform(llm::stop(run), |ok| Message::Llm(LlmMsg::Stopped(ok)))
            }
            LlmMsg::Stopped(ok) => {
                self.llm.stopping = false;
                if !ok {
                    self.llm_note("The LLM didn't stop. Try again, or end llama-server in your task manager.", true);
                    return self.llm_poll();
                }
                self.llm.seen = Some(Seen::Off);
                self.llm_log("LLM stopped.");
                if std::mem::take(&mut self.llm.restart_after_stop) {
                    return self.llm_update(LlmMsg::Start);
                }
                Task::none()
            }
            LlmMsg::Autostart(on) => {
                match llm::set_autostart(on) {
                    Ok(()) => {
                        self.llm.autostart = on;
                        self.llm_log(if on { "The LLM will start when you sign in." } else { "The LLM won't start at sign-in." });
                    }
                    Err(e) => self.llm_note(format!("Couldn't change autostart: {}", e), true),
                }
                Task::none()
            }
            LlmMsg::ToggleAdvanced => {
                self.llm.advanced = !self.llm.advanced;
                Task::none()
            }
            LlmMsg::ByoText(s) => {
                self.llm.byo_text = s;
                self.llm.byo = None;
                Task::none()
            }
            LlmMsg::ByoFind => {
                let input = self.llm.byo_text.trim().to_string();
                // A file on this computer: used where it is, not copied.
                let local = PathBuf::from(&input);
                if local.is_file() && input.to_lowercase().ends_with(".gguf") {
                    if input.to_lowercase().contains("mmproj") {
                        self.llm_note("That's a vision add-on, not a model. Pick the main .gguf file next to it.", true);
                        return Task::none();
                    }
                    // Remembered; a name from the list (LM Studio's copy, say) takes that row.
                    if !self.llm.cfg.files.contains(&local) {
                        self.llm_set_cfg(|c| c.files.push(local.clone()));
                    }
                    self.llm.selected = Some(local.clone());
                    self.llm_log(format!("Using your file {}.", local.display()));
                    if self.llm.running().is_none() {
                        self.llm_set_cfg(|c| c.model = Some(local));
                    }
                    return Task::none();
                }
                let Some((repo, quant)) = parse_hf(&input) else {
                    self.llm_note("Paste a Hugging Face name like user/model-GGUF (add :Q5_K_M for a version), or the path to a .gguf file.", true);
                    return Task::none();
                };
                Task::perform(
                    async move {
                        let files = hf_files(repo.clone()).await?;
                        let (file, size) = pick_gguf(&files, quant.as_deref())?;
                        Ok((repo, file, size))
                    },
                    |r| Message::Llm(LlmMsg::ByoFound(r)),
                )
            }
            LlmMsg::ByoFound(r) => {
                match r {
                    Ok(found) => {
                        self.llm.note = None;
                        self.llm.byo = Some(found);
                    }
                    Err(e) => self.llm_note(e, true),
                }
                Task::none()
            }
            LlmMsg::ByoDownload => {
                let Some((repo, file, _)) = self.llm.byo.clone() else { return Task::none() };
                if self.llm.job.is_some() {
                    return Task::none();
                }
                let dest = gguf_dir(&self.install_path).join(&file);
                self.llm_log(format!("Downloading {} from {}.", file, repo));
                self.llm_start_job(Job::Model { repo, name: file.clone(), file, dest })
            }
            LlmMsg::CheckEngine => {
                let Some(build) = self.llm.cfg.build else { return Task::none() };
                Task::perform(newer_engine(self.llm.cfg.tag.clone(), build), |r| Message::Llm(LlmMsg::EngineChecked(r)))
            }
            LlmMsg::EngineChecked(r) => {
                match r {
                    Ok(Some(tag)) => self.llm.newer = Some(tag),
                    Ok(None) => self.llm_note(format!("llama.cpp {} is the newest.", self.llm.cfg.tag), false),
                    Err(e) => self.llm_note(format!("Couldn't check for a newer llama.cpp: {}", e), true),
                }
                Task::none()
            }
            LlmMsg::UpdateEngine => {
                let Some(tag) = self.llm.newer.clone() else { return Task::none() };
                if self.llm.job.is_some() {
                    return Task::none();
                }
                self.llm_engine_job(tag)
            }
            LlmMsg::Delete => {
                let Some(sel) = self.llm.selected.clone().filter(|p| p.exists()) else { return Task::none() };
                if self.llm.running().is_some_and(|r| r.model == sel) {
                    self.llm_note("Stop the LLM before deleting the model it runs.", true);
                    return Task::none();
                }
                // Only files the launcher downloaded; a file of yours elsewhere stays.
                if !sel.starts_with(gguf_dir(&self.install_path)) {
                    self.llm.selected = None;
                    self.llm_set_cfg(|c| {
                        c.files.retain(|f| f != &sel);
                        if c.model.as_ref() == Some(&sel) {
                            c.model = None;
                        }
                    });
                    self.llm_log(format!("Forgot {} (your file is untouched).", sel.display()));
                    return Task::none();
                }
                if !self.confirm(Arm::LlmDelete) {
                    return after(ARM_SECS, Message::ArmExpired);
                }
                match std::fs::remove_file(&sel) {
                    Ok(()) => {
                        self.llm_log(format!("Deleted {}.", model_label(&sel)));
                        if self.llm.cfg.model.as_ref() == Some(&sel) {
                            self.llm_set_cfg(|c| c.model = None);
                        }
                    }
                    Err(e) => self.llm_note(format!("Couldn't delete it: {}", e), true),
                }
                Task::none()
            }
            LlmMsg::OpenLog => {
                open_file(&log_path());
                Task::none()
            }
            LlmMsg::Lan(on) => {
                self.llm_set_cfg(|c| c.lan = on);
                Task::none()
            }
            LlmMsg::Private(on) => {
                self.llm_set_cfg(|c| c.private = on);
                if on {
                    // The last log goes now, not at the next start.
                    let _ = std::fs::remove_file(log_path());
                }
                Task::none()
            }
            LlmMsg::CopyUrl => {
                let Some(url) = self.llm.running().map(|r| r.url()) else { return Task::none() };
                self.llm_log(format!("Copied {}", url));
                iced::clipboard::write(url)
            }
        }
    }

    // ── Status words (also the Install tab's row) ─────────────────────────

    pub(crate) fn llm_status(&self) -> (String, Color) {
        let l = &self.llm;
        if l.stopping {
            return ("Stopping...".into(), YELLOW);
        }
        if l.job.is_some() && l.running().is_none() {
            return (self.llm_job_words(), BLUE);
        }
        match &l.seen {
            Some(Seen::Up(run, h)) => {
                let what = format!("{} · {} · {}", model_label(&run.model), ctx_words(run.ctx), if run.gpu { "GPU" } else { "CPU" });
                match h {
                    Health::Ready => (format!("Running · {}", what), GREEN),
                    _ => (format!("Loading {}...", what), BLUE),
                }
            }
            Some(Seen::Foreign) => (format!("Port {} is used by another program", l.cfg.port), YELLOW),
            _ if !l.cfg.engine_ok() => ("Not set up".into(), MUTED),
            _ => ("Stopped".into(), MUTED),
        }
    }

    /// The tab's headline ("LLM active") and the line under it (model, or what's happening).
    fn llm_headline(&self) -> (&'static str, String, Color) {
        let l = &self.llm;
        let (words, color) = self.llm_status();
        let head = match &l.seen {
            _ if l.stopping => "LLM stopping",
            _ if l.chain => "LLM setting up",
            Some(Seen::Up(_, Health::Ready)) => "LLM active",
            Some(Seen::Up(..)) => "LLM loading",
            _ if !l.cfg.engine_ok() && l.job.is_none() => "LLM not set up",
            _ => "LLM inactive",
        };
        let under = match (&l.seen, l.running()) {
            (_, Some(run)) => format!("{} · {} · {}", model_label(&run.model), ctx_words(run.ctx), if run.gpu { "GPU" } else { "CPU" }),
            (Some(Seen::Foreign), _) => words,
            _ if l.job.is_some() => words,
            _ => String::new(),
        };
        (head, under, color)
    }

    // ── View ──────────────────────────────────────────────────────────────

    /// "Downloading Qwen3.5 9B 37%", "Installing llama.cpp (CUDA) 12%", "Unpacking ...".
    fn llm_job_words(&self) -> String {
        let l = &self.llm;
        let verb = match &l.job {
            Some(Job::Engine { .. }) => "Installing",
            _ => "Downloading",
        };
        match &l.progress {
            Some((label, _, _)) if label.starts_with("Unpacking") => format!("{}...", label),
            Some((label, done, total)) if *total > 0 => format!("{} {} {}%", verb, label, done * 100 / total),
            Some((label, _, _)) => format!("{} {}...", verb, label),
            None => "Starting the download...".into(),
        }
    }

    /// Percent of the model being downloaded right now, if `path` is it.
    fn llm_row_pct(&self, path: &std::path::Path) -> Option<u64> {
        match (&self.llm.job, &self.llm.progress) {
            (Some(Job::Model { dest, .. }), Some((_, done, total))) if dest == path => Some(if *total > 0 { done * 100 / total } else { 0 }),
            (Some(Job::Model { dest, .. }), None) if dest == path => Some(0),
            _ => None,
        }
    }

    pub(crate) fn view_llm_tab(&self) -> Element<'_, Message> {
        let l = &self.llm;
        if !looks_like_install(&self.install_path) {
            return column![
                text("LLM not set up").size(17).font(bold()).color(MUTED),
                text("Install Sapphire first: the LLM and its models live in her folder.").size(13).color(MUTED),
            ]
            .spacing(10)
            .into();
        }
        let (head, under, scolor) = self.llm_headline();
        let running = l.running();
        let wait = l.busy();
        let small = |label: &str, m: LlmMsg| button(text(label.to_string()).size(12)).on_press(Message::Llm(m)).style(button::secondary).padding([2, 10]);

        // Top: the status (headline, then model and address) on the left; the switches
        // (each says what it does on hover) and the main button on the right.
        let help = |words: &'static str| {
            tooltip(
                container(text("?").size(12).color(MUTED)).padding([0, 6]).style(|_t| container::Style {
                    border: iced::Border { color: MUTED, width: 1.0, radius: 8.0.into() },
                    ..Default::default()
                }),
                container(text(words).size(12)).padding(10).max_width(340).style(container::rounded_box),
                tooltip::Position::Bottom,
            )
        };
        let adv_label = if l.advanced { "[-] Advanced" } else { "[+] Advanced" };
        let toggles = row![
            help(
                "llama.cpp writes no log at all, and the last one is deleted. Its normal log keeps timings and \
                 errors, never chats, but an error can quote one. If it won't start in private mode, untick this \
                 and start again to see why.",
            ),
            checkbox("Private", l.cfg.private).on_toggle(|b| Message::Llm(LlmMsg::Private(b))).size(14).text_size(13),
            help(
                "When you sign in to this computer, the launcher starts the LLM in the background with no window, \
                 using the default model and settings below, then gets out of the way. Sapphire finds it at the \
                 address on the status line. Untick to stop that; it doesn't stop a running LLM.",
            ),
            checkbox("Autostart", l.autostart).on_toggle(|b| Message::Llm(LlmMsg::Autostart(b))).size(14).text_size(13),
            button(text(adv_label).size(12)).on_press(Message::Llm(LlmMsg::ToggleAdvanced)).style(button::text).padding([2, 4]),
        ]
        .spacing(10)
        .align_y(iced::Alignment::Center);

        let mut sub = row![text(under).size(12).color(MUTED)].spacing(8).align_y(iced::Alignment::Center);
        if let Some(run) = running {
            sub = sub.push(text(run.url()).size(12).color(MUTED)).push(small("Copy", LlmMsg::CopyUrl));
        }
        let status = column![row![dot(scolor), text(head).size(17).font(bold()).color(scolor)].spacing(8).align_y(iced::Alignment::Center), sub].spacing(2);
        let mut top = row![].spacing(8).align_y(iced::Alignment::Center);
        let sel = self.llm_selected();
        let sel_on_disk = sel.as_ref().is_some_and(|p| p.exists());
        let btn = |label: &str, m: LlmMsg, style: fn(&iced::Theme, button::Status) -> button::Style, on: bool| {
            button(text(label.to_string()).size(13).font(bold())).on_press_maybe(on.then_some(Message::Llm(m))).style(style).padding([4, 14])
        };
        if l.job.is_some() {
            top = top.push(small("Cancel", LlmMsg::Cancel));
        }
        if let Some(run) = running {
            if !run.matches(&l.cfg.resolved(l.hw.as_ref())) {
                top = top.push(btn("Restart to apply", LlmMsg::Restart, button::primary, !wait));
            }
            top = top.push(btn("Stop", LlmMsg::Stop, button::danger, !l.stopping));
        } else if l.job.is_some() {
            // Cancel is the button.
        } else if !l.cfg.engine_ok() || !sel_on_disk {
            top = top.push(btn(if l.cfg.engine_ok() { "Download and start" } else { "Set up LLM" }, LlmMsg::SetUp, button::success, l.hw.is_some()));
        } else {
            top = top.push(btn("Start", LlmMsg::Start, button::success, !wait && !matches!(l.seen, Some(Seen::Foreign))));
        }
        let header = row![
            status,
            horizontal_space(),
            column![toggles, top].spacing(6).align_x(iced::Alignment::End),
        ]
        .align_y(iced::Alignment::Center);
        let mut col = Column::new().spacing(10).push(header);

        // What a fresh setup is about to do: no surprises on a metered line.
        if !l.cfg.engine_ok() && l.job.is_none() {
            if let (Some(hw), Some(sel)) = (&l.hw, &sel) {
                let build = pick_build(hw);
                let engine_mb = match build {
                    Build::Cuda => "~580 MB",
                    Build::Vulkan => "~35 MB",
                    Build::Cpu => "~22 MB",
                };
                let model = sel.file_name().and_then(|f| curated(&f.to_string_lossy())).map(|m| format!(" and {} ({})", m.name, gb(m.bytes))).unwrap_or_default();
                col = col.push(text(format!("Downloads llama.cpp for {} ({}){}, then starts it. Everything stays on this computer.", build.words(), engine_mb, model)).size(11).color(MUTED));
            } else {
                col = col.push(text("Looking at this computer...").size(11).color(MUTED));
            }
        }
        // An engine update while it runs has no row to show its % in.
        if matches!(l.job, Some(Job::Engine { .. })) && running.is_some() {
            col = col.push(text(self.llm_job_words()).size(12).color(BLUE));
        }
        if let Some((note, problem)) = &l.note {
            col = col.push(text(note.clone()).size(12).color(if *problem { RED } else { GREEN }));
        }
        if l.advanced {
            col = col.push(self.view_llm_advanced());
        }

        // Context + device.
        let ctx_pick = pick_list(CTX_CHOICES.iter().map(|c| CtxChoice(*c)).collect::<Vec<_>>(), Some(CtxChoice(l.cfg.ctx)), |c| Message::Llm(LlmMsg::Ctx(c)))
            .text_size(13)
            .placeholder(ctx_words(l.cfg.ctx));
        let ctx_custom = text_input("custom", &l.ctx_text)
            .on_input(|s| Message::Llm(LlmMsg::CtxText(s)))
            .on_submit(Message::Llm(LlmMsg::CtxApply))
            .size(13)
            .width(80);
        let mut settings = row![text("Context").size(13), ctx_pick, ctx_custom].spacing(8).align_y(iced::Alignment::Center);
        // Auto says what it settles on for the picked model.
        if l.cfg.ctx == 0 && l.hw.is_some() && sel.is_some() {
            let got = LlmConfig { model: sel.clone(), ..l.cfg.clone() }.resolved(l.hw.as_ref()).ctx;
            settings = settings.push(text(format!("{} for this model", ctx_words(got))).size(12).color(MUTED));
        }
        settings = settings.push(horizontal_space());
        let has_card = l.hw.as_ref().is_some_and(|h| h.vram_mib > 0);
        if has_card {
            settings = settings
                .push(text("Run on").size(13))
                .push(radio("GPU", true, Some(l.cfg.gpu), |b| Message::Llm(LlmMsg::Gpu(b))).size(14).text_size(13))
                .push(radio("CPU", false, Some(l.cfg.gpu), |b| Message::Llm(LlmMsg::Gpu(b))).size(14).text_size(13));
        } else if l.hw.is_some() {
            settings = settings.push(text("Runs on the processor (no graphics card found)").size(12).color(MUTED));
        }
        col = col.push(settings).push(self.view_llm_models(sel.as_ref()));

        // Clear of the scrollbar, which overlays the content's right edge.
        container(col).padding(iced::Padding { right: 14.0, ..Default::default() }).into()
    }

    fn view_llm_models(&self, sel: Option<&PathBuf>) -> Element<'_, Message> {
        let l = &self.llm;
        let dir = gguf_dir(&self.install_path);
        let rec = l.hw.as_ref().map(|hw| recommend(hw, l.cfg.ctx, l.cfg.gpu));
        let mut list = Column::new().spacing(1);
        let row_for = |path: PathBuf, name: String, size: Option<u64>, blurb: String, fit: Option<Fit>, is_rec: bool| -> Element<'_, Message> {
            let on_disk = path.exists();
            let picked = sel == Some(&path);
            let mut r = row![
                dot(fit.map_or(MUTED, fit_color)),
                text(name).size(13).font(if picked { bold() } else { Font::DEFAULT }).width(150),
                text(size.map(gb).unwrap_or_default()).size(12).color(MUTED).width(60),
                text(blurb).size(12).color(MUTED).width(Length::Fill),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center);
            if is_rec {
                r = r.push(text("recommended").size(11).color(BLUE));
            }
            if on_disk && l.cfg.model.as_ref() == Some(&path) {
                r = r.push(text("default").size(11).color(color!(0xcba6f7)));
            }
            if let Some(pct) = self.llm_row_pct(&path) {
                r = r.push(text(format!("downloading {}%", pct)).size(11).color(BLUE));
            } else if on_disk {
                r = r.push(text("downloaded").size(11).color(GREEN));
            }
            let b = button(r).on_press(Message::Llm(LlmMsg::Select(path))).padding([3, 6]).width(Length::Fill);
            if picked {
                b.style(|t, s| button::Style { background: Some(iced::Background::Color(color!(0x2a2b3d))), ..button::text(t, s) }).into()
            } else {
                b.style(button::text).into()
            }
        };
        for (i, m) in MODELS.iter().enumerate() {
            let fit = l.hw.as_ref().map(|hw| model_fit(m, hw, l.cfg.ctx, l.cfg.gpu));
            let is_rec = rec.as_ref().is_some_and(|(r, _)| *r == i);
            list = list.push(row_for(model_path(&self.install_path, &l.cfg, m), m.name.to_string(), Some(m.bytes), m.blurb.to_string(), fit, is_rec));
        }
        // Models of yours: downloaded from Hugging Face, or a file elsewhere.
        let mut extra: Vec<PathBuf> = downloaded(&self.install_path).into_iter().filter(|p| curated(&p.file_name().unwrap_or_default().to_string_lossy()).is_none()).collect();
        for p in l.cfg.files.iter().cloned().chain([l.cfg.model.clone(), l.selected.clone()].into_iter().flatten()) {
            if p.exists() && !extra.contains(&p) && curated(&p.file_name().unwrap_or_default().to_string_lossy()).is_none() {
                extra.push(p);
            }
        }
        for p in extra {
            let size = std::fs::metadata(&p).map(|m| m.len()).ok();
            let fit = l.hw.as_ref().zip(size).map(|(hw, s)| {
                let ctx = if l.cfg.ctx == 0 { auto_ctx(s, 1000, hw, l.cfg.gpu) } else { l.cfg.ctx };
                fit(need_mib(s, 1000, ctx), hw, l.cfg.gpu)
            });
            let name = model_label(&p);
            list = list.push(row_for(p, name, size, "Your model".into(), fit, false));
        }
        let mut col = column![list].spacing(6);
        if let Some((_, why)) = rec {
            col = col.push(text(why).size(11).color(MUTED));
        }
        // Actions on the selected model, and the colour key, on one line.
        let mut foot = row![].spacing(8).align_y(iced::Alignment::Center);
        if let Some(p) = sel {
            if !p.exists() && l.job.is_none() && l.cfg.engine_ok() {
                foot = foot.push(button(text("Download").size(12)).on_press(Message::Llm(LlmMsg::Download)).style(button::primary).padding([3, 12]));
            }
            if p.exists() {
                let label = if self.is_armed(Arm::LlmDelete) { "Click again to delete" } else if p.starts_with(&dir) { "Delete model" } else { "Forget this file" };
                foot = foot.push(button(text(label).size(12)).on_press(Message::Llm(LlmMsg::Delete)).style(button::secondary).padding([3, 12]));
            }
        }
        foot = foot
            .push(horizontal_space())
            .push(row![dot(GREEN), text("fits").size(11).color(MUTED), dot(YELLOW), text("partly in RAM, slower").size(11).color(MUTED), dot(RED), text("too big").size(11).color(MUTED)].spacing(6).align_y(iced::Alignment::Center));
        col.push(foot).into()
    }

    fn view_llm_advanced(&self) -> Element<'_, Message> {
        let l = &self.llm;
        let mut col = Column::new().spacing(6).padding(iced::Padding { left: 12.0, ..Default::default() });

        // Bring your own model.
        col = col.push(
            row![
                text("Your own model").size(13).width(110),
                text_input("user/model-GGUF  or  user/model-GGUF:Q5_K_M  or  a .gguf file path", &l.byo_text)
                    .on_input(|s| Message::Llm(LlmMsg::ByoText(s)))
                    .on_submit(Message::Llm(LlmMsg::ByoFind))
                    .size(13),
                button(text("Find").size(12)).on_press(Message::Llm(LlmMsg::ByoFind)).style(button::secondary).padding([3, 12]),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        );
        if let Some((repo, file, size)) = &l.byo {
            col = col.push(
                row![
                    text(format!("{} from {} ({})", file, repo, gb(*size))).size(12).width(Length::Fill),
                    button(text("Download").size(12)).on_press_maybe((l.job.is_none()).then_some(Message::Llm(LlmMsg::ByoDownload))).style(button::primary).padding([3, 12]),
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center),
            );
        }

        // Engine, log and port on one line.
        let engine = match (&l.cfg.build, l.cfg.engine_ok()) {
            (Some(b), true) => format!("{} ({})", l.cfg.tag, b.words()),
            _ => "not installed".into(),
        };
        let mut erow = row![text("llama.cpp").size(13).width(110), text(engine).size(12).color(MUTED)].spacing(8).align_y(iced::Alignment::Center);
        if l.cfg.engine_ok() {
            erow = erow.push(match &l.newer {
                Some(tag) => button(text(format!("Update to {}", tag)).size(12)).on_press_maybe(l.job.is_none().then_some(Message::Llm(LlmMsg::UpdateEngine))).style(button::primary).padding([3, 12]),
                None => button(text("Check for update").size(12)).on_press(Message::Llm(LlmMsg::CheckEngine)).style(button::secondary).padding([3, 12]),
            });
        }
        if !l.cfg.private {
            erow = erow.push(button(text("Open log").size(12)).on_press(Message::Llm(LlmMsg::OpenLog)).style(button::secondary).padding([3, 12]));
        }
        erow = erow
            .push(horizontal_space())
            .push(text("Port").size(13))
            .push(text_input("8642", &l.port_text).on_input(|s| Message::Llm(LlmMsg::PortText(s))).size(13).width(70));
        col = col.push(erow);

        // Who can reach it: Sapphire on this PC, or on another PC on the network.
        let mut lrow = row![
            text("Reachable from").size(13).width(110),
            radio("This computer", false, Some(l.cfg.lan), |b| Message::Llm(LlmMsg::Lan(b))).size(14).text_size(13),
            radio("My local network", true, Some(l.cfg.lan), |b| Message::Llm(LlmMsg::Lan(b))).size(14).text_size(13),
        ]
        .spacing(12)
        .align_y(iced::Alignment::Center);
        if l.cfg.lan {
            let at = lan_ip().map_or_else(|| "this PC's address".to_string(), |ip| format!("http://{}:{}/v1", ip, l.cfg.port));
            lrow = lrow.push(text(format!("Point the other PC's Sapphire at {}. Anyone on this network can use it.", at)).size(11).color(YELLOW).width(Length::Fill));
        }
        col.push(lrow).into()
    }
}
