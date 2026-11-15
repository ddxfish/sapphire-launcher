// The local LLM's face: the LLM tab, the LLM bar under the header and the Install
// tab's row. State, messages and views for the llama.cpp server (llm.rs).
use crate::llm::*;
use crate::*;
use iced::widget::{button, checkbox, column, container, horizontal_space, pick_list, progress_bar, radio, row, text, text_input, tooltip, Column};
use iced::{color, Color, Element, Font, Length};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub(crate) enum LlmMsg {
    Probed(Hw),
    Poll,
    Polled(Seen),
    JobPoll,
    JobSeen(Option<JobInfo>),
    Select(PathBuf),
    Pick(Pick),
    Ctx(CtxChoice),
    CtxText(String),
    CtxApply,
    Gpu(bool),
    SetUp,
    Download,
    Cancel,
    Resume,
    Start,
    Started(Result<LlmRun, String>),
    Stop,
    Restart,
    Stopped(bool),
    Autostart(bool),
    WithSapphire(bool),
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
    ModelsBrowse,
    ModelsDir(Option<PathBuf>),
    Remove,
    Removed(Result<(), String>),
}

/// A context size in the dropdown ("16K").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CtxChoice(pub u32);

impl std::fmt::Display for CtxChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0 == 0 { write!(f, "Auto") } else { write!(f, "{}", ctx_words(self.0)) }
    }
}

/// An entry in the Install tab's dropdown: None, or a model on the list (its index).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Pick(pub Option<usize>, pub String);

impl std::fmt::Display for Pick {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.1)
    }
}

#[derive(Debug, Default)]
pub(crate) struct LlmUi {
    pub cfg: LlmConfig,
    /// llm.json as this window last read or wrote it (see `llm_sync_cfg`).
    pub cfg_stamp: Option<(std::time::SystemTime, u64)>,
    pub hw: Option<Hw>,
    pub seen: Option<Seen>,
    pub starting: Option<Instant>,
    pub stopping: bool,
    pub restart_after_stop: bool,
    /// The download under way: the worker's report, whoever started it.
    pub job: Option<JobInfo>,
    /// A worker this window started and has not heard from yet.
    pub launched: Option<Instant>,
    /// Cancel was clicked before that worker reported.
    pub cancel_wanted: bool,
    /// A download whose worker died (crash, power loss): Resume or Discard.
    pub stalled: Option<JobInfo>,
    /// Download speed: (when, bytes then, bytes per second).
    pub rate: Option<(Instant, u64, f64)>,
    /// The LLM and Sapphire were seen up together, so it goes down with her.
    pub for_her: bool,
    /// A model picked in the Install dropdown whose download (or engine) is still
    /// coming: when it lands it becomes the default and is switched on.
    pub picked: Option<PathBuf>,
    pub selected: Option<PathBuf>,
    pub ctx_text: String,
    pub port_text: String,
    pub byo_text: String,
    pub byo: Option<(String, String, u64)>,
    pub note: Option<(String, bool)>, // (words, is a problem)
    pub advanced: bool,
    pub newer: Option<String>,
    /// "Start the LLM when I sign in" (only offered when it runs on its own).
    pub autostart: bool,
    /// What the sign-in entry runs now, and whether this window has checked it.
    pub sign_in: SignIn,
    pub sign_in_checked: bool,
}

impl LlmUi {
    pub(crate) fn new() -> Self {
        let mut cfg = LlmConfig::load();
        // Sapphire calls the launcher by this path; it moves when the file does.
        let me = launcher_path().map(PathBuf::from);
        if !cfg!(test) && me.is_some() && cfg.launcher != me {
            cfg.launcher = me;
            cfg.save();
        }
        let sign_in = llm::sign_in();
        LlmUi { port_text: cfg.port.to_string(), selected: cfg.model.clone(), autostart: sign_in == SignIn::Start, sign_in, cfg_stamp: config_stamp(), cfg, ..Default::default() }
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

    /// Engine and a default model are both on disk.
    pub(crate) fn ready(&self) -> bool {
        self.cfg.engine_ok() && self.cfg.model.as_ref().is_some_and(|m| m.exists())
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

fn speed_words(bps: f64) -> String {
    if bps >= 1e6 { format!("{:.1} MB/s", bps / 1e6) } else { format!("{:.0} KB/s", bps / 1e3) }
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

/// A "?" that says what a switch does on hover.
fn help<'a>(words: &'static str) -> Element<'a, Message> {
    tooltip(
        container(text("?").size(12).color(MUTED)).padding([0, 6]).style(|_t| container::Style {
            border: iced::Border { color: MUTED, width: 1.0, radius: 8.0.into() },
            ..Default::default()
        }),
        container(text(words).size(12)).padding(10).max_width(340).style(container::rounded_box),
        tooltip::Position::Bottom,
    )
    .into()
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
            Some(model_path(&self.llm.cfg, &MODELS[i]))
        })
    }

    /// Take in llm.json if someone else wrote it: another window, the worker, or
    /// Sapphire through the CLI. Every controller reads before it writes.
    fn llm_sync_cfg(&mut self) {
        let stamp = config_stamp();
        if stamp != self.llm.cfg_stamp {
            self.llm.cfg = LlmConfig::load();
            self.llm.cfg_stamp = stamp;
            if self.llm.port_text.trim().parse::<u16>().ok() != Some(self.llm.cfg.port) {
                self.llm.port_text = self.llm.cfg.port.to_string();
            }
        }
    }

    fn llm_set_cfg(&mut self, f: impl FnOnce(&mut LlmConfig)) {
        self.llm_sync_cfg();
        f(&mut self.llm.cfg);
        self.llm.cfg.save();
        self.llm.cfg_stamp = config_stamp();
    }

    /// What should run at sign-in: nothing while the LLM is off; the watcher when it
    /// only runs with Sapphire; the LLM itself when it runs on its own and was asked to.
    pub(crate) fn llm_sign_in_wanted(&self) -> SignIn {
        let c = &self.llm.cfg;
        if c.model.is_none() || c.engine.is_none() {
            SignIn::Off
        } else if c.with_sapphire {
            SignIn::Watch
        } else if self.llm.autostart {
            SignIn::Start
        } else {
            SignIn::Off
        }
    }

    /// Bring the sign-in entry in line with the settings, and make sure a watcher
    /// is running now if one is wanted (it works before the next sign-in too).
    fn llm_sync_sign_in(&mut self) {
        let want = self.llm_sign_in_wanted();
        if want != self.llm.sign_in {
            match set_sign_in(want) {
                Ok(()) => {
                    self.llm.sign_in = want;
                    self.llm_log(match want {
                        SignIn::Watch => "A small watcher now starts the LLM when Sapphire runs and stops it when she doesn't.",
                        SignIn::Start => "The LLM will start when you sign in.",
                        SignIn::Off => "Nothing of the LLM's starts at sign-in.",
                    });
                }
                Err(e) => self.llm_note(format!("Couldn't change what starts at sign-in: {}", e), true),
            }
        }
        if want == SignIn::Watch {
            spawn_watcher();
        }
    }

    /// Hand a job to a worker. The worker reports to the job file; `JobSeen` follows it.
    fn llm_launch(&mut self, spec: JobSpec) -> Task<Message> {
        if self.llm.job.is_some() {
            return Task::none();
        }
        match spawn_worker(&spec) {
            Ok(pid) => {
                self.llm.note = None;
                self.llm.stalled = None;
                self.llm.rate = None;
                self.llm.cancel_wanted = false;
                self.llm.launched = Some(Instant::now());
                self.llm.job = Some(JobInfo::waiting(pid, spec));
            }
            Err(e) => self.llm_note(e, true),
        }
        Task::none()
    }

    /// The selected model as a download, if it is one from the list.
    fn llm_selected_spec(&mut self, path: &std::path::Path) -> Option<ModelSpec> {
        let file = path.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
        let spec = curated(&file).map(ModelSpec::curated);
        if spec.is_none() {
            self.llm_note("That model isn't on the list; use Your own model under Advanced.", true);
        }
        spec
    }

    fn llm_rate(&mut self, j: &JobInfo) {
        let now = Instant::now();
        match self.llm.rate {
            Some((at, done, bps)) if j.done >= done => {
                let dt = now.duration_since(at).as_secs_f64();
                if dt >= 1.0 {
                    let inst = (j.done - done) as f64 / dt;
                    self.llm.rate = Some((now, j.done, if bps > 0.0 { bps * 0.6 + inst * 0.4 } else { inst }));
                }
            }
            _ => self.llm.rate = Some((now, j.done, 0.0)), // a new file
        }
    }

    /// A job this window was following has ended.
    fn llm_job_finished(&mut self, j: JobInfo) -> Task<Message> {
        self.llm.rate = None;
        self.llm_sync_cfg(); // the worker saved the engine and the default model
        match j.state {
            JobState::Done => {
                let mut tasks = Vec::new();
                if !j.note.is_empty() {
                    self.llm_note(j.note.clone(), false);
                }
                if j.spec.engine.is_some() {
                    self.llm_log(format!("llama.cpp {} installed.", self.llm.cfg.tag));
                    self.llm.newer = None;
                    if self.llm.running().is_some() && !j.spec.start {
                        self.llm_note("Updated. Restart the LLM to use the new llama.cpp.", false);
                    }
                    // The engine's own device list sees every vendor's card.
                    self.llm.hw = None;
                    tasks.push(self.llm_probe());
                }
                if let Some(dest) = &j.dest {
                    self.llm_log(format!("{} downloaded.", model_label(dest)));
                    self.llm.selected = Some(dest.clone());
                    self.llm.byo = None;
                }
                // A pick in the Install dropdown asked for this model to be the one
                // in use: now that it (and the engine) are here, switch it on.
                if let Some(p) = self.llm.picked.take().filter(|p| p.exists() && self.llm.cfg.engine_ok()) {
                    tasks.push(self.llm_use(p));
                }
                if j.spec.start {
                    self.llm.starting = Some(Instant::now());
                    tasks.push(self.llm_poll());
                }
                Task::batch(tasks)
            }
            JobState::Failed => {
                self.llm.picked = None;
                self.llm_note(j.error, true);
                Task::none()
            }
            JobState::Cancelled => {
                self.llm.picked = None;
                self.llm_note("Download cancelled.", false);
                Task::none()
            }
            JobState::Running => Task::none(),
        }
    }

    /// Make `path` (on disk) the model in use: the LLM is on, with this model. A
    /// running one moves over to it; a stopped one starts, unless it only runs with
    /// Sapphire and she isn't running (then it starts when she does).
    fn llm_use(&mut self, path: PathBuf) -> Task<Message> {
        self.llm.selected = Some(path.clone());
        self.llm_set_cfg(|c| c.model = Some(path.clone()));
        self.llm_sync_sign_in();
        match self.llm.running() {
            Some(run) if run.model == path => Task::none(),
            Some(_) => self.llm_update(LlmMsg::Restart),
            None if self.llm.cfg.with_sapphire && !self.sapphire_running => {
                self.llm_log(format!("{} is on. It starts when Sapphire does.", model_label(&path)));
                Task::none()
            }
            None => self.llm_update(LlmMsg::Start),
        }
    }

    fn llm_begin_stop(&mut self, restart: bool) -> Task<Message> {
        let Some(run) = self.llm.running().cloned() else { return Task::none() };
        if self.llm.stopping {
            return Task::none();
        }
        self.llm.restart_after_stop = restart;
        self.llm.stopping = true;
        self.llm.starting = None;
        self.llm.for_her = false;
        self.llm_log("Stopping the LLM.");
        Task::perform(llm::stop(run), |ok| Message::Llm(LlmMsg::Stopped(ok)))
    }

    /// "Only run with Sapphire": Launch brings her LLM up first.
    pub(crate) fn llm_start_with_her(&mut self) -> Task<Message> {
        let l = &self.llm;
        if !l.cfg.with_sapphire || !l.ready() || l.running().is_some() || l.busy() || matches!(l.seen, Some(Seen::Foreign)) {
            return Task::none();
        }
        self.llm_log("Starting the LLM with Sapphire.");
        self.llm_update(LlmMsg::Start)
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
                Task::none()
            }
            LlmMsg::Poll => {
                self.llm_sync_cfg();
                // Once per window: an entry from an older launcher, or none at all.
                if !std::mem::replace(&mut self.llm.sign_in_checked, true) {
                    self.llm_sync_sign_in();
                }
                let job = self.llm_update(LlmMsg::JobSeen(read_job()));
                Task::batch([job, self.llm_poll()])
            }
            LlmMsg::JobPoll => self.llm_update(LlmMsg::JobSeen(read_job())),
            LlmMsg::JobSeen(seen) => {
                let alive = seen.as_ref().is_some_and(|j| j.alive());
                if let Some(at) = self.llm.launched {
                    // The last report was removed when our worker was started: no
                    // file yet means it hasn't written its first one.
                    if seen.is_none() {
                        if at.elapsed().as_secs() >= 10 {
                            self.llm.launched = None;
                            self.llm.job = None;
                            self.llm_note("The download didn't start. Try again.", true);
                        }
                        return Task::none();
                    }
                    self.llm.launched = None;
                }
                match seen {
                    Some(j) if alive => {
                        self.llm_rate(&j);
                        self.llm.stalled = None;
                        self.llm.job = Some(j);
                        if std::mem::take(&mut self.llm.cancel_wanted) {
                            request_cancel();
                        }
                        Task::none()
                    }
                    // Its worker is gone mid-download: a crash, a power cut.
                    Some(j) if j.state == JobState::Running => {
                        self.llm.job = None;
                        self.llm.stalled = Some(j);
                        Task::none()
                    }
                    Some(j) => {
                        self.llm.stalled = None;
                        match self.llm.job.take() {
                            Some(_) => self.llm_job_finished(j),
                            None => Task::none(), // news from before this window opened
                        }
                    }
                    None => {
                        self.llm.job = None;
                        Task::none()
                    }
                }
            }
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
                // "Only run with Sapphire": once the two were up together, the LLM
                // goes down when she does, however she went (Stop, a crash, systemd).
                if self.llm.cfg.with_sapphire && self.llm.running().is_some() && !self.sapphire_stopping {
                    if self.sapphire_running {
                        self.llm.for_her = true;
                    } else if std::mem::take(&mut self.llm.for_her) {
                        self.llm_log("Sapphire is down, so her LLM stops too.");
                        return self.llm_begin_stop(false);
                    }
                }
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
            LlmMsg::Pick(Pick(i, _)) => {
                // The Install tab's dropdown is the LLM's switch. A model = on, with
                // that model, now (downloaded first if it isn't here). None = off.
                if self.llm.job.is_some() {
                    return Task::none(); // locked while one downloads
                }
                let Some(m) = i.and_then(|i| MODELS.get(i)) else {
                    // Off: nothing runs, nothing starts by itself. Its files stay.
                    let was_on = self.llm.cfg.model.is_some() || self.llm.running().is_some() || self.llm.autostart;
                    self.llm.selected = None;
                    self.llm.picked = None;
                    self.llm.restart_after_stop = false;
                    if self.llm.cfg.model.is_some() {
                        self.llm_set_cfg(|c| c.model = None);
                    }
                    self.llm.autostart = false;
                    self.llm_sync_sign_in(); // nothing at sign-in; the watcher ends by itself
                    if was_on {
                        self.llm_log("Local LLM off: it won't start by itself. Its files stay; pick a model to turn it back on.");
                    }
                    return self.llm_begin_stop(false);
                };
                let path = model_path(&self.llm.cfg, m);
                self.llm.selected = Some(path.clone());
                if path.exists() && self.llm.cfg.engine_ok() {
                    return self.llm_use(path);
                }
                self.llm.picked = Some(path.clone());
                self.llm_log(format!("Downloading {} ({}).", m.name, gb(m.bytes)));
                let mut spec = JobSpec::for_model(&self.llm.cfg, ModelSpec::curated(m), false);
                if path.exists() {
                    spec.model = None; // only the engine is missing
                }
                self.llm_launch(spec)
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
                // Whatever is missing (engine, the model announced under the button),
                // then start: one worker does all three.
                if self.llm.job.is_some() {
                    return Task::none();
                }
                let Some(sel) = self.llm_selected() else { return Task::none() };
                self.llm.selected = Some(sel.clone());
                let model = if sel.exists() {
                    None
                } else {
                    match self.llm_selected_spec(&sel) {
                        Some(m) => Some(m),
                        None => return Task::none(),
                    }
                };
                let engine = (!self.llm.cfg.engine_ok()).then(|| LLAMA_PIN.to_string());
                if engine.is_none() && model.is_none() {
                    return self.llm_update(if self.llm.running().is_some() { LlmMsg::Restart } else { LlmMsg::Start });
                }
                if sel.exists() {
                    self.llm_set_cfg(|c| c.model = Some(sel));
                }
                self.llm_log("Setting up the LLM.");
                self.llm_launch(JobSpec { engine, model, start: true })
            }
            LlmMsg::Download => {
                let Some(p) = self.llm_selected() else { return Task::none() };
                self.llm.selected = Some(p.clone());
                let Some(m) = self.llm_selected_spec(&p) else { return Task::none() };
                self.llm_log(format!("Downloading {}.", m.name));
                self.llm_launch(JobSpec::for_model(&self.llm.cfg, m, false))
            }
            LlmMsg::Cancel => {
                if let Some(j) = self.llm.stalled.take() {
                    discard(&j);
                    self.llm_note("Deleted the unfinished download.", false);
                } else if self.llm.launched.is_some() {
                    self.llm.cancel_wanted = true; // its worker hasn't reported yet
                } else if self.llm.job.is_some() {
                    self.llm_log("Cancelling the download...");
                    request_cancel();
                }
                Task::none()
            }
            LlmMsg::Resume => match self.llm.stalled.take() {
                Some(j) => {
                    self.llm_log("Resuming the download.");
                    self.llm_launch(j.spec)
                }
                None => Task::none(),
            },
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
                Task::perform(llm::start_saved(), |r| Message::Llm(LlmMsg::Started(r)))
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
            LlmMsg::Stop => {
                // The Stop button: it stays stopped. The watcher won't bring it back
                // until Start, or until Sapphire's next run.
                if self.llm.running().is_some() && !self.llm.stopping {
                    set_hold(true);
                }
                self.llm_begin_stop(false)
            }
            LlmMsg::Restart => self.llm_begin_stop(true),
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
                self.llm.autostart = on;
                self.llm_sync_sign_in();
                Task::none()
            }
            LlmMsg::WithSapphire(on) => {
                self.llm_set_cfg(|c| c.with_sapphire = on);
                if !on {
                    self.llm.for_her = false;
                }
                self.llm_sync_sign_in();
                self.llm_log(if on { "The LLM starts with Sapphire and stops when she does." } else { "The LLM runs on its own: Start and Stop are yours." });
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
                self.llm_log(format!("Downloading {} from {}.", file, repo));
                self.llm_launch(JobSpec::for_model(&self.llm.cfg, ModelSpec { repo, name: file.clone(), file, quant: None }, false))
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
                self.llm_log(format!("Installing llama.cpp {}.", tag));
                self.llm_launch(JobSpec { engine: Some(tag), model: None, start: false })
            }
            LlmMsg::Delete => {
                let Some(sel) = self.llm.selected.clone().filter(|p| p.exists()) else { return Task::none() };
                if self.llm.running().is_some_and(|r| r.model == sel) {
                    self.llm_note("Stop the LLM before deleting the model it runs.", true);
                    return Task::none();
                }
                // Only files the launcher downloaded; a file of yours elsewhere stays.
                if !sel.starts_with(gguf_dir(&self.llm.cfg)) {
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
            LlmMsg::ModelsBrowse => Task::perform(
                async { rfd::AsyncFileDialog::new().set_title("Choose a folder for LLM models").pick_folder().await.map(|f| f.path().to_path_buf()) },
                |picked| match picked {
                    Some(p) => Message::Llm(LlmMsg::ModelsDir(Some(p))),
                    None => Message::Log("Browse cancelled.".into()),
                },
            ),
            LlmMsg::ModelsDir(dir) => {
                if self.llm.job.is_some() {
                    return Task::none();
                }
                // Nothing moves: new downloads go there, and the list looks there.
                let dir = dir.filter(|d| *d != llm_root().join("gguf"));
                self.llm_set_cfg(|c| c.models_dir = dir);
                // A pick that was never downloaded pointed into the old folder.
                if self.llm.selected.as_ref().is_some_and(|p| !p.exists()) {
                    self.llm.selected = None;
                }
                self.llm_log(format!("Models folder: {}.", gguf_dir(&self.llm.cfg).display()));
                Task::none()
            }
            LlmMsg::Remove => {
                let why = if self.llm.running().is_some() {
                    Some("Stop the LLM before removing it.")
                } else if self.llm.job.is_some() {
                    Some("Wait for the LLM download to finish, or cancel it on the LLM tab.")
                } else {
                    None
                };
                if let Some(why) = why {
                    self.disarm(Arm::LlmRemove);
                    self.llm_log(why);
                    return Task::none();
                }
                if !self.confirm(Arm::LlmRemove) {
                    return after(ARM_SECS, Message::ArmExpired);
                }
                self.llm_log(format!("Removing {}...", llm_root().display()));
                Task::perform(llm::remove_all(), |r| Message::Llm(LlmMsg::Removed(r)))
            }
            LlmMsg::Removed(r) => match r {
                Ok(()) => {
                    self.llm.autostart = false;
                    self.llm.selected = None;
                    self.llm.newer = None;
                    self.llm_set_cfg(|c| {
                        c.engine = None;
                        c.tag.clear();
                        c.build = None;
                        if c.model.as_ref().is_some_and(|m| !m.exists()) {
                            c.model = None;
                        }
                    });
                    self.llm_sync_sign_in(); // nothing left to start at sign-in
                    self.llm_log("Removed the local LLM.");
                    // Without the engine, only NVIDIA cards can be seen again.
                    self.llm.hw = None;
                    self.llm_probe()
                }
                Err(e) => {
                    self.llm_note(e, true);
                    Task::none()
                }
            },
            LlmMsg::CopyUrl => {
                let Some(url) = self.llm.running().map(|r| r.url()) else { return Task::none() };
                self.llm_log(format!("Copied {}", url));
                iced::clipboard::write(url)
            }
        }
    }

    // ── Status words (the bar, and the Install tab's row) ─────────────────

    pub(crate) fn llm_status(&self) -> (String, Color) {
        let l = &self.llm;
        if l.stopping {
            return ("Stopping...".into(), YELLOW);
        }
        if l.running().is_none() {
            if let Some(j) = &l.job {
                return (self.llm_job_words(j), BLUE);
            }
            if let Some(j) = &l.stalled {
                let what = j.dest.as_deref().map(model_label).unwrap_or_else(|| "llama.cpp".into());
                return (format!("Download interrupted: {}", what), YELLOW);
            }
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
            _ if !l.ready() => ("Off".into(), MUTED),
            _ if l.cfg.with_sapphire && !self.sapphire_running => ("Waiting for Sapphire".into(), MUTED),
            _ => ("Stopped".into(), MUTED),
        }
    }

    /// "Downloading Qwen3.5 9B 37% · 12.0 MB/s", "Installing llama.cpp (CUDA) 12%", "Unpacking ...".
    fn llm_job_words(&self, j: &JobInfo) -> String {
        let verb = if j.engine { "Installing" } else { "Downloading" };
        let speed = self.llm.rate.filter(|r| r.2 > 0.0).map(|r| format!(" · {}", speed_words(r.2))).unwrap_or_default();
        if j.label.is_empty() {
            "Starting the download...".into()
        } else if j.label.starts_with("Unpacking") || j.label == "Starting" {
            format!("{}...", j.label)
        } else {
            match j.pct() {
                Some(pct) => format!("{} {} {}%{}", verb, j.label, pct, speed),
                None => format!("{} {}...", verb, j.label),
            }
        }
    }

    /// Percent of the model being downloaded right now, if `path` is it.
    fn llm_row_pct(&self, path: &std::path::Path) -> Option<u64> {
        let j = self.llm.job.as_ref()?;
        (j.dest.as_deref() == Some(path)).then(|| j.pct().unwrap_or(0))
    }

    // ── The LLM bar: a second header row that travels with every tab ──────

    /// Hidden until the LLM is something: set up, downloading, running, or being looked at.
    pub(crate) fn llm_bar_shown(&self) -> bool {
        let l = &self.llm;
        self.active_tab == Tab::Llm || l.cfg.engine_ok() || l.job.is_some() || l.stalled.is_some() || l.running().is_some()
    }

    pub(crate) fn view_llm_bar(&self) -> Element<'_, Message> {
        let l = &self.llm;
        let (words, color) = self.llm_status();
        let running = l.running();
        let btn = |label: &str, m: LlmMsg, style: fn(&iced::Theme, button::Status) -> button::Style, on: bool| {
            button(text(label.to_string()).size(12).font(bold())).on_press_maybe(on.then_some(Message::Llm(m))).style(style).padding([2, 10])
        };
        let mut line = row![dot(color), text("LLM").size(13).font(bold()), text(words).size(12).color(color)].spacing(8).align_y(iced::Alignment::Center);
        // A download while it runs (an engine update, a second model).
        if let (Some(_), Some(j)) = (running, &l.job) {
            line = line.push(text(self.llm_job_words(j)).size(12).color(BLUE));
        }
        // A failure has to be seen from any tab.
        line = match &l.note {
            Some((note, true)) => line.push(text(note.clone()).size(11).color(RED).width(Length::Fill)),
            _ => line.push(horizontal_space()),
        };
        if l.job.is_some() {
            line = line.push(btn("Cancel", LlmMsg::Cancel, button::secondary, !l.cancel_wanted));
        } else if l.stalled.is_some() {
            line = line.push(btn("Resume", LlmMsg::Resume, button::primary, true)).push(btn("Discard", LlmMsg::Cancel, button::secondary, true));
        }
        if let Some(run) = running {
            if !run.matches(&l.cfg.resolved(l.hw.as_ref())) {
                line = line.push(btn("Restart to apply", LlmMsg::Restart, button::primary, !l.busy()));
            }
            line = line.push(btn("Stop", LlmMsg::Stop, button::danger, !l.stopping));
        } else if l.job.is_none() {
            let sel = self.llm_selected();
            if l.cfg.engine_ok() && sel.as_ref().is_some_and(|p| p.exists()) {
                line = line.push(btn("Start", LlmMsg::Start, button::success, !l.busy() && !matches!(l.seen, Some(Seen::Foreign))));
            } else if l.stalled.is_none() && self.active_tab == Tab::Llm {
                // Only on the LLM tab, where the list shows which model it means and
                // how big it is. (An interrupted download has Resume and Discard.)
                line = line.push(btn(if l.cfg.engine_ok() { "Download and start" } else { "Set up LLM" }, LlmMsg::SetUp, button::success, sel.is_some()));
            }
        }
        line = line.push(checkbox("Only run with Sapphire", l.cfg.with_sapphire).on_toggle(|b| Message::Llm(LlmMsg::WithSapphire(b))).size(14).text_size(12));

        // The row's bottom edge is the download's progress.
        let edge: Element<'_, Message> = match l.job.as_ref().and_then(|j| j.pct()) {
            Some(pct) => progress_bar(0.0..=100.0, pct as f32)
                .height(3)
                .style(|_t| progress_bar::Style { background: color!(0x313244).into(), bar: BLUE.into(), border: iced::Border::default() })
                .into(),
            None => container(text("")).height(3).width(Length::Fill).into(),
        };
        container(column![container(line).padding([4, 8]), edge])
            .width(Length::Fill)
            .style(|_theme| container::Style { background: Some(iced::Background::Color(color!(0x181825))), ..Default::default() })
            .into()
    }

    // ── The Install tab's row ─────────────────────────────────────────────

    /// The dropdown's entries: None, then every listed model this computer can hold.
    fn llm_picks(&self) -> Vec<Pick> {
        let l = &self.llm;
        let rec = l.hw.as_ref().map(|hw| recommend(hw, l.cfg.ctx, l.cfg.gpu).0);
        let mut picks = vec![Pick(None, "None".into())];
        for (i, m) in MODELS.iter().enumerate() {
            if l.hw.as_ref().is_some_and(|hw| model_fit(m, hw, l.cfg.ctx, l.cfg.gpu) == Fit::TooBig) {
                continue;
            }
            let rec = if rec == Some(i) { " · recommended" } else { "" };
            let have = if model_path(&l.cfg, m).exists() { " · downloaded" } else { "" };
            picks.push(Pick(Some(i), format!("{} · {}{}{}", m.name, gb(m.bytes), rec, have)));
        }
        picks
    }

    /// The optional LLM step under the install steps: the LLM's switch. A model in
    /// the dropdown = on with that model (downloaded first if need be); None = off.
    /// While one downloads the dropdown gives way to Cancel.
    pub(crate) fn view_llm_row(&self) -> Element<'_, Message> {
        const SPINNER: &[&str] = &["/", "-", "\\", "|"];
        let l = &self.llm;
        let (status, c) = self.llm_status();
        let mark = if l.job.is_some() {
            SPINNER[self.spinner_tick % SPINNER.len()]
        } else if l.ready() {
            "+"
        } else {
            "-"
        };
        let small = |label: &str, m: Message| button(text(label.to_string()).size(12)).on_press(m).style(button::secondary).padding([2, 10]);
        let mut r = row![text(mark).size(16).color(c).width(20), text("Local LLM (optional)").size(15)].spacing(10).align_y(iced::Alignment::Center);
        let picking = l.job.is_none() && l.stalled.is_none();
        if l.job.is_some() {
            // Locked while it downloads: Cancel is the only way out.
            r = r.push(text(format!("— {}", status)).size(12).color(c)).push(small("Cancel", Message::Llm(LlmMsg::Cancel)));
        } else if l.stalled.is_some() {
            r = r
                .push(text(format!("— {}", status)).size(12).color(c))
                .push(small("Resume", Message::Llm(LlmMsg::Resume)))
                .push(small("Discard", Message::Llm(LlmMsg::Cancel)));
        } else {
            let picks = self.llm_picks();
            // What is chosen: the default model's entry, None when there is none, or
            // (a model of your own, not on the list) its name as the placeholder.
            let default = l.cfg.model.as_ref().filter(|m| m.exists());
            let chosen = match default {
                None => picks.first().cloned(),
                Some(m) => picks.iter().find(|p| p.0.is_some_and(|i| &model_path(&l.cfg, &MODELS[i]) == m)).cloned(),
            };
            let mut list = pick_list(picks, chosen, |p| Message::Llm(LlmMsg::Pick(p))).text_size(13).padding([2, 8]).width(290);
            if let Some(m) = default {
                list = list.placeholder(model_label(m));
            }
            r = r.push(list);
            // Fill, so the button after it keeps its width and the words wrap instead.
            r = r.push(if l.cfg.engine_ok() {
                text(format!("— {}", status)).size(12).color(c).width(Length::Fill)
            } else {
                let hint = match &l.hw {
                    None => "Looking at this computer...",
                    Some(hw) if hw.vram_mib == 0 => "Picking one downloads it and switches it on. No NVIDIA card seen: replies may be slow.",
                    Some(_) => "Picking one downloads it and switches it on. Runs on this computer, no key needed.",
                };
                text(hint).size(11).color(MUTED).width(Length::Fill)
            });
        }
        if !picking {
            r = r.push(horizontal_space());
        }
        // Clear of the scrollbar, which overlays the content's right edge.
        container(r.push(small("LLM tab", Message::TabSelected(Tab::Llm)))).padding(iced::Padding { right: 14.0, ..Default::default() }).into()
    }

    // ── The LLM tab ───────────────────────────────────────────────────────

    pub(crate) fn view_llm_tab(&self) -> Element<'_, Message> {
        let l = &self.llm;
        let small = |label: &str, m: LlmMsg| button(text(label.to_string()).size(12)).on_press(Message::Llm(m)).style(button::secondary).padding([2, 10]);

        // Status and the main button live in the LLM bar above. Here: the address
        // Sapphire uses while it runs, and the switches.
        let mut top = row![].spacing(8).align_y(iced::Alignment::Center);
        if let Some(run) = l.running() {
            top = top.push(text(run.url()).size(12).color(MUTED)).push(small("Copy", LlmMsg::CopyUrl));
        }
        let adv_label = if l.advanced { "[-] Advanced" } else { "[+] Advanced" };
        top = top
            .push(horizontal_space())
            .push(help(
                "llama.cpp writes no log at all, and the last one is deleted. Its normal log keeps timings and \
                 errors, never chats, but an error can quote one. If it won't start in private mode, untick this \
                 and start again to see why.",
            ))
            .push(checkbox("Private", l.cfg.private).on_toggle(|b| Message::Llm(LlmMsg::Private(b))).size(14).text_size(13))
            .push(button(text(adv_label).size(12)).on_press(Message::Llm(LlmMsg::ToggleAdvanced)).style(button::text).padding([2, 4]));
        let mut col = Column::new().spacing(10).push(top);
        let sel = self.llm_selected();

        // What a fresh setup is about to do: no surprises on a metered line.
        if !l.cfg.engine_ok() && l.job.is_none() {
            if let (Some(hw), Some(sel)) = (&l.hw, &sel) {
                let build = pick_build(hw);
                let engine_mb = match build {
                    Build::Cuda => "~580 MB",
                    Build::Vulkan => "~35 MB",
                    Build::Cpu => "~22 MB",
                };
                let model = sel.file_name().and_then(|f| curated(&f.to_string_lossy())).filter(|_| !sel.exists()).map(|m| format!(" and {} ({})", m.name, gb(m.bytes))).unwrap_or_default();
                col = col.push(text(format!("Set up LLM downloads llama.cpp for {} ({}){}, then starts it. Everything stays on this computer.", build.words(), engine_mb, model)).size(11).color(MUTED));
            } else {
                col = col.push(text("Looking at this computer...").size(11).color(MUTED));
            }
        }
        // The same for a model that isn't here yet.
        if l.cfg.engine_ok()
            && l.job.is_none()
            && let Some(m) = sel.as_ref().filter(|p| !p.exists()).and_then(|p| p.file_name()).and_then(|f| curated(&f.to_string_lossy()))
        {
            col = col.push(text(format!("Download and start gets {} ({}), then starts it.", m.name, gb(m.bytes))).size(11).color(MUTED));
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
        let dir = gguf_dir(&l.cfg);
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
            list = list.push(row_for(model_path(&l.cfg, m), m.name.to_string(), Some(m.bytes), m.blurb.to_string(), fit, is_rec));
        }
        // Models of yours: downloaded from Hugging Face, or a file elsewhere.
        let mut extra: Vec<PathBuf> = downloaded(&l.cfg).into_iter().filter(|p| curated(&p.file_name().unwrap_or_default().to_string_lossy()).is_none()).collect();
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

        // What runs at sign-in: the watcher when it only runs with Sapphire, else
        // (if asked) the LLM itself.
        let sign_in: Element<'_, Message> = if l.cfg.with_sapphire {
            text(match l.sign_in {
                SignIn::Watch => "A small watcher starts the LLM when Sapphire runs and stops it when she doesn't, window open or not.",
                _ => "Pick a model and a small watcher will start the LLM with Sapphire and stop it with her.",
            })
            .size(12)
            .color(MUTED)
            .width(Length::Fill)
            .into()
        } else {
            checkbox("Start the LLM when I sign in", l.autostart).on_toggle(|b| Message::Llm(LlmMsg::Autostart(b))).size(14).text_size(13).into()
        };
        col = col.push(row![text("At sign-in").size(13).width(110), sign_in].spacing(8).align_y(iced::Alignment::Center));

        // Where models are kept: the default, or a bigger drive.
        let mut mrow = row![
            text("Models folder").size(13).width(110),
            text(gguf_dir(&l.cfg).display().to_string()).size(12).color(MUTED).width(Length::Fill),
            button(text("Change").size(12)).on_press_maybe(l.job.is_none().then_some(Message::Llm(LlmMsg::ModelsBrowse))).style(button::secondary).padding([3, 12]),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);
        if l.cfg.models_dir.is_some() {
            mrow = mrow.push(button(text("Use default").size(12)).on_press_maybe(l.job.is_none().then_some(Message::Llm(LlmMsg::ModelsDir(None)))).style(button::secondary).padding([3, 12]));
        }
        col = col.push(mrow);

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
