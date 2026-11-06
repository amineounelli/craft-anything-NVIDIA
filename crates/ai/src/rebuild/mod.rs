//! The rebuild workflow: study a target, have a coding agent build it in the workspace's `app/`
//! folder, check the build against the target, and refine — asking the person when needed.
//!
//! ```text
//! Study   website: web_site_open + web_site_capture → capture/   (screenshots, blueprint, HTML)
//!         local:   triage (+ app graph for JS apps)  → capture/README.md
//! Build   the coding agent (an agent CLI such as Claude Code) works in the workspace with our
//!         MCP server; edits outside app/ and unusual commands ask the person (core_action_approve),
//!         questions arrive as cards (core_person_ask). Both go through the workspace inbox.
//! Check   website: serve app/dist on loopback, capture it the same way, compare screenshots
//!         (layout similarity per width). Other stacks: the build must succeed.
//! Refine  measured differences go back to the agent, up to `max_rounds`.
//! ```
//!
//! Everything runs on one worker thread; the UI drains [`RunEvent`]s with [`RebuildRun::poll`].
//!
//! Modules: `kinds` (what is made: [`TargetKind`]s and [`Check`]s), `study`, `brief` (the builder's prompts), `check`,
//! `locate` (what to study in a local target) and `output` (what a build produced).

use crate::{AgentEvent, AiHub, Conversation, Role};
use craft_anything_engine::inbox::{Answer, Inbox, InboxDir, Request};
use craft_anything_engine::project::{Project, Purpose};
use craft_anything_engine::workspace::progress::{AppFingerprintCache, BuildProgress};
use craft_anything_engine::workspace::{CheckResult, ProjectUsage, Status, TargetRef, Workspace};
use craft_anything_engine::{CallOptions, Caller, CancelToken, Engine, ToolOutput};
use craft_anything_engines::game;
use craft_anything_evidence::{ErrorCode, ToolError};
pub use craft_anything_platform::serve::StaticServer;
use craft_anything_platform::serve::serve_dir;
use craft_anything_platform::sync::Mutex;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

pub mod activity;
mod brief;
mod check;
pub mod feed;
pub mod kinds;
mod locate;
pub mod notes;
mod output;
pub mod project;
pub mod question;
pub mod runs;
pub mod self_check;
pub mod stacks;
pub mod stand_in;
mod study;
#[cfg(test)]
mod tests;
pub mod tools;

use brief::*;
pub use kinds::{
    AppKind, BriefCx, BuildCheck, Check, CheckCx, DetectCx, GameKind, KindRegistry, KindWords, MechanicsCheck, TargetKind, WebVisualCheck, WebsiteKind,
};
pub use locate::game_folder;
use locate::*;
use output::*;
pub use output::{built_program, launch, site_dir};
pub use runs::{RunSet, Started, Stopped, keep_queue_moving};
pub use stand_in::StandIns;

pub use craft_anything_engine::workspace::Step;

/// What the UI shows, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum RunEvent {
    /// A stage started (with a plain-words line on what's happening).
    Step { step: Step, text: String },
    /// Something from the coding agent (text, tool calls, errors).
    Agent(AgentEvent),
    /// A note the person added while it worked, now read by the agent (as the feed shows it),
    /// with the pictures attached to it (their copies in the project).
    Note { shown: String, pictures: Vec<PathBuf> },
    /// A question or approval waiting for the person.
    Ask(Request),
    /// A check finished.
    Checked(CheckResult),
    /// The build can be previewed here.
    Preview(String),
    Finished {
        ok: bool,
        message: String,
        /// The person paused it: not a failure.
        paused: bool,
    },
}

/// Called after each event a run sends, so a window draws it at once instead of checking on a
/// timer.
#[derive(Clone, Default)]
pub struct Wake(Option<Arc<dyn Fn() + Send + Sync>>);

impl Wake {
    pub fn new(f: impl Fn() + Send + Sync + 'static) -> Wake {
        Wake(Some(Arc::new(f)))
    }

    fn call(&self) {
        if let Some(f) = &self.0 {
            f();
        }
    }
}

impl std::fmt::Debug for Wake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() { "Wake(set)" } else { "Wake(none)" })
    }
}

/// Options for one run.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// The agent backend by name; `None` picks the Builder role, else the first agent app.
    pub backend: Option<String>,
    /// A follow-up request on an existing build (skips Study).
    pub message: Option<String>,
    /// How the person's message is shown in the project's feed (it can differ from `message`,
    /// which carries attachment paths). `None` adds nothing to the feed.
    pub shown: Option<String>,
    /// The pictures attached to `shown` (their copies in the project), so the feed keeps them
    /// with the message for Up to bring back.
    pub shown_pictures: Vec<PathBuf>,
    /// Pick up a build whose last run stopped part-way (paused, the app closed, the computer
    /// stopped): Study isn't repeated when it was done, and an agent with a saved session is
    /// told to carry on rather than given the brief again. A run with no message on a project
    /// whose builder has already worked does the same, so "Try again" never starts over.
    pub continuing: bool,
    /// Refinement rounds after the first build (each fixes what the last check found).
    pub max_rounds: u32,
    /// Stop refining once the build looks at least this alike (percent).
    pub good_enough: f64,
    pub beginner: bool,
    /// The kinds of thing this run can make (website, app, game, and any a fork registers).
    pub kinds: KindRegistry,
    /// Told after each event (the app repaints).
    pub wake: Wake,
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions {
            backend: None,
            message: None,
            shown: None,
            shown_pictures: Vec::new(),
            continuing: false,
            max_rounds: 6,
            good_enough: 90.0,
            beginner: true,
            kinds: KindRegistry::built_in(),
            wake: Wake::default(),
        }
    }
}

/// Something the person added while a run works, until the agent reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    /// Unique in this process, so the app can take a note back.
    pub id: u64,
    /// What the agent reads.
    pub text: String,
    /// How it appears in the project's feed.
    pub shown: String,
    /// The pictures attached to it (their copies in the project), for the feed's history.
    pub pictures: Vec<PathBuf>,
}

/// A running (or finished) rebuild.
pub struct RebuildRun {
    rx: Receiver<RunEvent>,
    stop: CancelToken,
    inbox: Inbox,
    running: Arc<AtomicBool>,
    preview: Arc<Mutex<Option<StaticServer>>>,
    /// What the person added while it works; the agent reads them at its next step.
    notes: Arc<Mutex<Vec<Note>>>,
    /// The project's conversation, kept in its folder.
    feed: Arc<Mutex<feed::FeedLog>>,
    pub root: PathBuf,
    started: Instant,
    activity: Arc<activity::Activity>,
}

impl RebuildRun {
    /// Start a run on a worker thread. Fails fast (before any work) when no coding agent is set up.
    pub fn start(hub: &AiHub, ws: Workspace, opts: RunOptions) -> Result<RebuildRun, ToolError> {
        let cfg = hub.config();
        let entry = builder_backend(&cfg, opts.backend.as_deref().or_else(|| project_builder(&cfg, &ws)))?;
        // An API backend builds with our own agent loop and the Build tools; its conversation is
        // kept in the project and continued by follow-ups (a fresh build starts a new one).
        // Only NVIDIA (an OpenAI-compatible API backend) exists now, so every backend builds
        // through our own agent loop and the Build tools.
        let api = {
            if opts.message.is_none() || !ws.capture_dir().exists() {
                let _ = std::fs::remove_file(ws.transcript_path());
            }
            Some(hub.start_builder(&entry, opts.beginner, Some(ws.transcript_path()))?)
        };
        // A chosen extractor (an API backend, e.g. Kimi K3) takes a game's files out after a fresh
        // study; the run only uses it in Deep mode on a game (see Worker::extract).
        let fresh = opts.message.is_none() || !ws.capture_dir().exists();
        let extractor = match hub.chosen(Role::Extractor).and_then(|n| hub.config().get(&n).cloned()) {
            Some(e) if fresh => {
                let _ = std::fs::remove_file(ws.root.join(EXTRACT_TRANSCRIPT));
                Some((e.config.name.clone(), hub.start_extractor(&e, opts.beginner, Some(ws.root.join(EXTRACT_TRANSCRIPT)))?))
            }
            _ => None,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = CancelToken::default();
        let running = Arc::new(AtomicBool::new(true));
        let preview: Arc<Mutex<Option<StaticServer>>> = Arc::default();
        let notes: Arc<Mutex<Vec<Note>>> = Arc::default();
        let inbox = Inbox::new(ws.inbox_dir());
        // Notes left by an earlier run would be handed over again (ids restart with the process);
        // cleared before the app can add any to this one.
        notes::clear(&notes::notes_dir(&ws.inbox_dir()));
        let root = ws.root.clone();
        // The project's conversation: the run keeps it in the folder as it goes.
        let feed = Arc::new(Mutex::new(feed::FeedLog::open(&ws)));
        if let Some(shown) = opts.shown.clone() {
            feed.lock().unwrap_or_else(PoisonError::into_inner).push(feed::FeedItem::User(feed::UserItem::new(shown, opts.shown_pictures.clone())));
        }
        let ctx = Worker {
            wake: opts.wake.clone(),
            engine: hub.engine.clone(),
            tx,
            stop: stop.clone(),
            preview: preview.clone(),
            seen: HashSet::new(),
            inbox: inbox.clone(),
            editor: None,
            notes: notes.clone(),
            feed: feed.clone(),
            assets: hub.has_asset_keys(),
            decompiled: false,
            usage: Some(UsageClock { root: root.clone(), since: Instant::now() }),
            self_check: None,
            reset_progress_after_notes: false,
        };
        let r2 = running.clone();
        std::thread::Builder::new()
            .name("rebuild".into())
            .spawn(move || {
                let mut ctx = ctx;
                let mut ws = ws;
                // A clone can run for hours: keep the computer from sleeping until it ends.
                let awake = craft_anything_platform::keep_awake();
                let _here = self_check::BuildingHere::new(&ws.root);
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ctx.run(&mut ws, entry, api, extractor, &opts)));
                let (ok, message, paused) = match result {
                    Ok(Ok(m)) => (true, m, false),
                    Ok(Err(e)) if e.code == ErrorCode::Cancelled => {
                        let _ = ws.set_status(Status::Stopped, "Paused. Send a message to continue.");
                        (false, "Paused.".to_string(), true)
                    }
                    Ok(Err(e)) => {
                        let msg = e.for_people();
                        let _ = ws.set_status(Status::NeedsYou, msg.clone());
                        (false, msg, false)
                    }
                    Err(_) => (false, "The workflow hit a bug and stopped safely. Your files are untouched.".to_string(), false),
                };
                ctx.record_usage(None);
                // The CLI can exit as soon as it sees completion: release the helper while its
                // supervisor threads are still available to stop it.
                drop(awake);
                ctx.emit(RunEvent::Finished { ok, message, paused });
                r2.store(false, Ordering::SeqCst);
            })
            .map_err(|e| ToolError::internal(e.to_string()))?;
        let activity = feed.lock().unwrap_or_else(PoisonError::into_inner).activity();
        Ok(RebuildRun { rx, stop, inbox, running, preview, notes, feed, root, started: Instant::now(), activity })
    }

    /// Add something while it works; the agent reads it at its next step, and only then does it
    /// join the feed (as `shown`, with `pictures`). Returns its id, to take it back with `unsend`
    /// until then.
    pub fn add_note(&self, text: String, shown: String, pictures: Vec<PathBuf>) -> u64 {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        // Also left for an agent that takes notes at its next step; without it, after the turn.
        if let Err(e) = notes::hand_over(&notes::notes_dir(self.inbox.dir()), id, &text) {
            log::warn!("A note waits for the builder's turn to end: {}", e.message);
        }
        self.notes.lock().unwrap_or_else(PoisonError::into_inner).push(Note { id, text, shown, pictures });
        id
    }

    /// The notes still waiting for the agent, oldest first.
    pub fn waiting(&self) -> Vec<Note> {
        self.notes.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Take back a note the agent hasn't read yet; `None` once it has.
    pub fn unsend(&self, id: u64) -> Option<Note> {
        let mut notes = self.notes.lock().unwrap_or_else(PoisonError::into_inner);
        let i = notes.iter().position(|n| n.id == id)?;
        let dir = notes::notes_dir(self.inbox.dir());
        // Taking its file back first means the agent can't claim it after this.
        if !notes::take_back(&dir, id) && notes::was_read(&dir, id) {
            return None;
        }
        Some(notes.remove(i))
    }

    /// How many notes are still waiting for the agent.
    pub fn queued(&self) -> usize {
        self.notes.lock().unwrap_or_else(PoisonError::into_inner).len()
    }

    /// Notes the agent never got to (the run ended first), to send as the next message.
    pub fn take_notes(&self) -> Vec<Note> {
        std::mem::take(&mut *self.notes.lock().unwrap_or_else(PoisonError::into_inner))
    }

    pub fn poll(&mut self) -> Vec<RunEvent> {
        self.rx.try_iter().collect()
    }

    /// The project's conversation as the run has it now, for showing a project that is already
    /// building. Events not yet polled are in it, so they're dropped from the queue.
    pub fn feed_now(&mut self) -> Vec<feed::FeedItem> {
        let feed = self.feed.lock().unwrap_or_else(PoisonError::into_inner);
        while self.rx.try_recv().is_ok() {}
        feed.items().to_vec()
    }

    /// Answer a question or approval card.
    pub fn answer(&self, id: &str, answer: &Answer) -> Result<(), ToolError> {
        self.answer_as(id, answer, &answer.as_text())
    }

    /// Answer a card; `shown` is how the answer appears in the project's feed.
    pub fn answer_as(&self, id: &str, answer: &Answer, shown: &str) -> Result<(), ToolError> {
        self.inbox.answer(id, answer)?;
        self.feed.lock().unwrap_or_else(PoisonError::into_inner).answered(id, shown);
        Ok(())
    }

    pub fn stop(&self) {
        self.stop.cancel();
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// When it began working (a queued run begins when its turn comes).
    pub fn started(&self) -> Instant {
        self.started
    }

    /// How long since the run last had anything to say. Cheap: safe to read while drawing.
    pub fn quiet_for(&self) -> Duration {
        self.activity.quiet_for()
    }

    /// What it's on now. Cheap: safe to read while drawing.
    pub fn activity_now(&self) -> activity::ActivityNow {
        self.activity.now()
    }

    /// How many lines of its activity are at hand for [`RebuildRun::activity_lines`].
    pub fn activity_len(&self) -> usize {
        self.activity.len()
    }

    /// Some of its latest activity. Cheap: safe to read while drawing.
    pub fn activity_lines(&self, range: std::ops::Range<usize>) -> Vec<activity::ActivityLine> {
        self.activity.lines_in(range)
    }

    /// The file with all of it.
    pub fn activity_log(&self) -> PathBuf {
        self.root.join(craft_anything_engine::workspace::ACTIVITY_LOG)
    }

    pub fn preview_url(&self) -> Option<String> {
        self.preview.lock().unwrap_or_else(PoisonError::into_inner).as_ref().map(StaticServer::url)
    }
}

impl Drop for RebuildRun {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

/// Serve a workspace's built site for previewing (without running the workflow).
pub fn preview(ws: &Workspace) -> Result<StaticServer, ToolError> {
    let site = site_dir(&ws.app_dir()).ok_or_else(|| {
        ToolError::new(ErrorCode::NotFound, "There's no built website to show yet.").with_remediation("Ask for a rebuild, or open the project folder.")
    })?;
    serve_dir(&site).map_err(|e| ToolError::io(site.display(), e))
}

struct Worker {
    /// Told after each event.
    wake: Wake,
    engine: Arc<Engine>,
    tx: Sender<RunEvent>,
    stop: CancelToken,
    preview: Arc<Mutex<Option<StaticServer>>>,
    seen: HashSet<String>,
    inbox: Inbox,
    /// The game engine's editor for this run (Unity, Unreal or Godot stacks).
    editor: Option<PathBuf>,
    notes: Arc<Mutex<Vec<Note>>>,
    feed: Arc<Mutex<feed::FeedLog>>,
    /// An asset provider's key is set up, so the builder can generate pictures and 3D models if the
    /// project allows it.
    assets: bool,
    /// The builder has decompiled something this run (Deep mode requires it).
    decompiled: bool,
    /// Where this run adds its working time, tokens and cost (`None` in tests that don't build).
    usage: Option<UsageClock>,
    /// The kind and bar the builder's own checks (`rebuild_check`) are judged by, once known.
    self_check: Option<(Arc<dyn kinds::TargetKind>, f64)>,
    /// A hook consumed a new request; persist its reset only after the builder finishes.
    reset_progress_after_notes: bool,
}

/// The project a run records its usage in, and when it last did.
struct UsageClock {
    root: PathBuf,
    since: Instant,
}

/// How often a long agent turn saves the time worked so far, so a crash loses little of it.
const USAGE_FLUSH: Duration = Duration::from_secs(60);

impl Worker {
    /// Add the time worked since the last record (and an agent turn's tokens and reported cost)
    /// to the project's usage.json. The run is its only writer; a failure is logged and the build
    /// carries on.
    fn record_usage(&mut self, turn: Option<(u64, u64, Option<f64>, bool)>) {
        let Some(clock) = self.usage.as_mut() else { return };
        let ms = u64::try_from(clock.since.elapsed().as_millis()).unwrap_or(u64::MAX);
        clock.since = Instant::now();
        let saved = ProjectUsage::update(&clock.root, |u| {
            u.work_ms = u.work_ms.saturating_add(ms);
            if let Some((input, output, cost, estimate)) = turn {
                u.add_turn(input, output, cost, estimate);
            }
        });
        if let Err(e) = saved {
            log::warn!("Couldn't save the project's usage in {}: {}", clock.root.display(), e.message);
        }
    }

    /// Recorded and sent under one lock, so [`RebuildRun::feed_now`] sees each event once.
    fn emit(&self, e: RunEvent) {
        let mut feed = self.feed.lock().unwrap_or_else(PoisonError::into_inner);
        feed.record(&e);
        let _ = self.tx.send(e);
        drop(feed);
        self.wake.call();
    }

    fn step(&self, ws: &mut Workspace, step: Step, status: Status, text: &str) -> Result<(), ToolError> {
        self.stop.check()?;
        ws.set_status(status, text)?;
        self.emit(RunEvent::Step { step, text: text.to_string() });
        Ok(())
    }

    fn call(&self, tool: &str, params: Value) -> Result<ToolOutput, ToolError> {
        self.stop.check()?;
        // The person started this run for this target, which is the consent for opening it.
        let opts = CallOptions::new(Caller::Ui).with_consent(craft_anything_engine::Consent::Granted).with_cancel(self.stop.clone());
        // Said as it happens, so a long step shows what it's doing and the feed counts it.
        let call_id = format!("run-{tool}");
        self.emit(RunEvent::Agent(AgentEvent::ToolStarted { call_id: call_id.clone(), tool: tool.to_string(), input: Value::Null }));
        let result = self.engine.call(tool, params, opts);
        self.emit(RunEvent::Agent(AgentEvent::ToolFinished { call_id, tool: tool.to_string(), ok: result.is_ok(), summary: String::new(), evidence_id: None }));
        result
    }

    /// Point the engine at this workspace: its investigation project and inbox.
    fn attach(&self, ws: &Workspace) -> Result<(), ToolError> {
        let dir = ws.investigation_dir();
        let project = Project::open(&dir).or_else(|_| Project::create(&dir, &ws.manifest.name, Purpose::default()))?;
        let mut s = self.engine.session();
        s.open_project(project)?;
        s.ext.insert(Arc::new(InboxDir(ws.inbox_dir())));
        Ok(())
    }

    /// The prompt for notes the person added while the agent worked, if there are any.
    fn added(&self, ws: &mut Workspace) -> Option<String> {
        let mut notes = std::mem::take(&mut *self.notes.lock().unwrap_or_else(PoisonError::into_inner));
        if notes.is_empty() {
            return None;
        }
        // They go in this message, so no hook hands them over again.
        let dir = notes::notes_dir(&ws.inbox_dir());
        for n in &notes {
            notes::forget(&dir, n.id);
        }
        notes.retain(|n| !n.text.trim().is_empty());
        if notes.is_empty() {
            return None;
        }
        for n in &notes {
            self.emit(RunEvent::Note { shown: n.shown.clone(), pictures: n.pictures.clone() });
        }
        // A cancelled step can't be reported, but the notes go in either way.
        let _ = self.step(ws, Step::Build, Status::Building, "Reading what you added…");
        let texts: Vec<String> = notes.into_iter().map(|n| n.text).collect();
        Some(added_prompt(&texts) + &pictures_reminder(ws))
    }

    /// The editor for a game-engine stack. An engine that isn't ready stops the run before any
    /// work, with the way to get it.
    fn find_editor(&self, stack: &str) -> Result<Option<PathBuf>, ToolError> {
        let Some(b) = game::Builder::from_stack(stack).filter(|b| *b != game::Builder::Rust) else { return Ok(None) };
        let store = self.engine.session().settings.engine_store_dir();
        let inv = game::inventory(&game::Places::here(store));
        match inv.best(b) {
            Some(i) if inv.ready(b) => Ok(Some(i.program.clone())),
            _ => Err(ToolError::new(ErrorCode::BadSettings, format!("{} isn't ready on this computer yet.", b.name())).with_remediation(
                "Get it (Settings → Engines in the app), or pick another engine (\"Build with\" in the app, --build-with in the CLI) and start again.",
            )),
        }
    }

    /// The whole run: checks the builder can start, studies a new project, then builds and checks
    /// in rounds until it's good enough or the rounds run out. Every backend is NVIDIA (an
    /// OpenAI-compatible API backend), so every run builds through our own agent loop and the Build
    /// tools. Returns the summary shown when it's ready.
    fn run(
        &mut self,
        ws: &mut Workspace,
        _entry: crate::BackendEntry,
        api: Option<crate::agent::ApiConversation>,
        extractor: Option<(String, crate::agent::ApiConversation)>,
        opts: &RunOptions,
    ) -> Result<String, ToolError> {
        let mut progress = BuildProgress::load(&ws.root)?;
        let mut progress_available = true;
        let mut fingerprints = AppFingerprintCache::default();
        // Try again has no message. Only an explicit new request starts another attempt on
        // unchanged files; restarting the app or resuming a paused run keeps the saved count.
        if opts.message.as_deref().is_some_and(|m| !m.trim().is_empty()) {
            progress.reset();
            progress.save(&ws.root)?;
        }
        self.attach(ws)?;
        std::fs::create_dir_all(ws.app_dir()).map_err(|e| ToolError::io(ws.app_dir().display(), e))?;
        let no_kind = || {
            ToolError::new(ErrorCode::BadSettings, "No kind of project matches this request.")
                .with_remediation("Describe what to make: a website, an app or a game.")
        };

        if ws.manifest.stack.is_empty() {
            let deep = !self.engine.session().clean_room();
            ws.manifest.stack = project::first_stack(&opts.kinds, ws, deep).ok_or_else(no_kind)?.to_string();
            ws.save()?;
        }
        let kind = opts.kinds.detect(&ws.manifest.request, &ws.manifest.target, &ws.manifest.stack).ok_or_else(no_kind)?;
        self.self_check = Some((kind.clone(), opts.good_enough));
        let kind = kind.as_ref();
        self.editor = self.find_editor(&ws.manifest.stack)?;
        let carrying_on = opts.continuing || (opts.message.is_none() && ws.builder_started());
        let first = (opts.message.is_none() && !carrying_on) || !ws.capture_dir().exists();
        if first {
            self.study(ws, kind)?;
            if let Some((name, ex)) = extractor {
                self.extract(ws, kind, &name, ex)?;
            }
        }
        // Every backend is NVIDIA (an API backend), so the run always builds through our own
        // agent loop and the Build tools; the API conversation is always started by the caller.
        let api_loop = true;
        let resumable = api.as_ref().is_some_and(crate::agent::ApiConversation::resumed);
        // A coding agent that never saved a session starts over, so it needs the brief.
        let has_history = resumable;
        self.step(ws, Step::Build, Status::Building, "Getting the builder ready…")?;
        let mut agent: Box<dyn Conversation> = match api {
            Some(api) => Box::new(api),
            None => {
                return Err(ToolError::new(ErrorCode::BadSettings, "No AI backend was started for this build.")
                    .with_remediation("Set up a NVIDIA AI backend in Settings → AI, then try again."));
            }
        };
        // The builder's tool server is started in this run's mode (`--deep` or `--clean-room`).
        let s = self.engine.session();
        let (deep, ghidra) = (!s.clean_room(), s.settings.ghidra_launcher().is_some());
        let image_model = s.settings.assets.image_model.clone();
        drop(s);
        let cx = BriefCx { ws, deep, ghidra, editor: self.editor.as_deref(), assets: self.assets, image_model: image_model.as_deref() };
        // The project's settings as they are now, for an agent that carries on its session.
        let now = || kind.follow_up_sections(&cx).iter().map(|s| format!("\n\n{s}")).collect::<String>() + &pictures_reminder(ws);
        let mut prompt = match &opts.message {
            // An agent with a saved session gets the follow-up; one without (an API loop) starts
            // from the brief with the request added.
            Some(m) if !first && has_history => follow_up(m, kind) + &now(),
            // Carrying on after a stop: the same, told to finish what it was doing.
            None if carrying_on && !first && has_history => continue_prompt(kind) + &now(),
            _ => {
                let b = brief(kind, &cx, opts.message.as_deref());
                if api_loop { format!("{API_BUILDER}\n\n{b}") } else { b }
            }
        };
        // The extraction step left the original's files in app/Original/: the builder starts from them.
        if first && ws.app_dir().join(ORIGINALS_MANIFEST).is_file() {
            prompt.push_str(ORIGINALS_READY);
        }
        let mut round = ws.manifest.checks.iter().map(|c| c.round).max().unwrap_or(0);
        // A new request starts the count of checks that found no working build again. "Try again" on
        // a build that already reached the limit stops before another builder turn is paid for (#736).
        if opts.message.is_some() {
            if ws.manifest.not_working_checks != 0 {
                ws.manifest.not_working_checks = 0;
                ws.save()?;
            }
        } else if ws.manifest.not_working_checks >= NOT_WORKING_CHECK_LIMIT {
            return Err(not_working(ws));
        }
        let mut refinements = 0;
        // Deep mode on a program: decompiling is required, so a build turn without any gets one reminder.
        let must_decompile = !self.engine.session().clean_room() && matches!(ws.manifest.target, TargetRef::Local { .. });
        let mut reminded = false;
        // The builder finished and only the check didn't: the build is there to check.
        let mut only_check = carrying_on && !first && opts.message.is_none() && ws.manifest.awaiting_check;
        loop {
            if !std::mem::take(&mut only_check) {
                progress = BuildProgress::load(&ws.root)?;
                if let Some(fingerprint) = self.progress_fingerprint(ws, &mut progress, &mut progress_available, &mut fingerprints)? {
                    progress.before_turn(fingerprint);
                    if progress.stalled() {
                        return Err(no_build_progress(ws));
                    }
                    progress.save(&ws.root)?;
                }
                let doing = match (round, carrying_on) {
                    (0, false) => "Building the first version…",
                    (0, true) => "Carrying on with the build…",
                    _ => "Improving the build…",
                };
                self.step(ws, Step::Build, Status::Building, doing)?;
                ws.manifest.awaiting_check = false;
                let reply = self.turn(ws, agent.as_mut(), &prompt, None);
                let reply = match reply {
                    Ok(reply) => reply,
                    Err(error) => {
                        // Consumed instructions start a fresh attempt even if this turn fails
                        // or is cancelled. The provider retry loop has ended; stop any remaining
                        // work before touching the record, and retain the original failure.
                        agent.stop();
                        drop(agent);
                        if self.reset_progress_after_notes
                            && let Err(reset_error) = self.progress_after_notes(ws)
                        {
                            self.emit(RunEvent::Agent(AgentEvent::Status {
                                text: format!("The new request couldn't reset the unchanged-turn count ({}).", reset_error.message),
                            }));
                        }
                        return Err(error);
                    }
                };
                if !reply.trim().is_empty() {
                    ws.manifest.last_reply = reply.trim().to_string();
                }
                // The stack is recorded as the one the build uses, once it shows: left to the builder,
                // or another than the default because the request asked for it. A game engine stays
                // (it says the project is a game, and its editor is found for it).
                if game::Builder::from_stack(&ws.manifest.stack).is_none()
                    && let Some(s) = stacks::used_in(&ws.app_dir())
                {
                    ws.manifest.stack = s.name.clone();
                }
                ws.manifest.awaiting_check = true;
                ws.save()?;
                // Keep the finished turn before scanning: a cancellation during the scan
                // must not make Try again repeat work that only needs its checks.
                // A consumed note starts another attempt, but its record I/O must not
                // interrupt a live builder or enter the provider retry path.
                progress = self.progress_after_notes(ws)?;
                if let Some(fingerprint) = self.progress_fingerprint(ws, &mut progress, &mut progress_available, &mut fingerprints)? {
                    progress.after_turn(fingerprint);
                    progress.save(&ws.root)?;
                }
                // What the person added while the agent worked goes in before anything else.
                if let Some(p) = self.added(ws) {
                    progress.reset();
                    progress.save(&ws.root)?;
                    ws.manifest.not_working_checks = 0;
                    prompt = p;
                    continue;
                }
                if must_decompile && !self.decompiled && !reminded {
                    reminded = true;
                    prompt = DECOMPILE_FIRST.to_string();
                    continue;
                }
            }
            round += 1;
            let checking = if ws.manifest.target == TargetRef::None { "Checking the build…" } else { "Checking the build against the original…" };
            self.step(ws, Step::Check, Status::Checking, checking)?;
            let checks = self.checks_with_a_retry(ws, kind, round)?;
            ws.manifest.checks.extend(checks.iter().cloned());
            ws.manifest.awaiting_check = false;
            // A round in which every check works (built and served) ends the streak; any other
            // round adds to it, across runs, until the limit stops the build.
            ws.manifest.not_working_checks = if checks.iter().all(|c| c.passed) { 0 } else { ws.manifest.not_working_checks.saturating_add(1) };
            ws.save()?;
            for c in &checks {
                self.emit(RunEvent::Checked(c.clone()));
            }
            if let Some(p) = self.added(ws) {
                progress.reset();
                progress.save(&ws.root)?;
                ws.manifest.not_working_checks = 0;
                prompt = p;
                continue;
            }
            let not_good = checks.iter().find(|c| !c.good(opts.good_enough));
            if not_good.is_some() && progress_available && progress.stalled() {
                // A check-only resume skipped the builder's before-turn scan. A real edit
                // while the project was closed (or during this check) still starts an attempt.
                if let Some(fingerprint) = self.progress_fingerprint(ws, &mut progress, &mut progress_available, &mut fingerprints)? {
                    progress.before_turn(fingerprint);
                    progress.save(&ws.root)?;
                    if progress.stalled() {
                        return Err(no_build_progress(ws));
                    }
                }
            }
            let Some(check) = not_good else { break };
            if ws.manifest.not_working_checks >= NOT_WORKING_CHECK_LIMIT {
                return Err(not_working(ws));
            }
            if refinements >= opts.max_rounds {
                break;
            }
            refinements += 1;
            // Nothing written yet: a fix for a build that doesn't exist sends the builder looking for one.
            // It is asked to write the project and to show its build output (#736).
            let refined = if app_is_empty(ws) { NO_PROJECT_PROMPT.to_string() } else { kind.refine(check) };
            prompt = refined + &pictures_reminder(ws);
            if !api_loop {
                prompt.push_str(CHECK_BEFORE_REPLYING);
            }
        }
        if let Some(url) = self.preview.lock().unwrap_or_else(PoisonError::into_inner).as_ref().map(StaticServer::url) {
            self.emit(RunEvent::Preview(url));
        }
        let last = ws.manifest.checks.last().cloned();
        // A site that builds but isn't close enough is still usable; no working build is not.
        let usable = last.as_ref().is_some_and(|c| c.passed);
        if !usable {
            let why = last.map(|c| c.note).unwrap_or_default();
            return Err(ToolError::new(ErrorCode::ProgramFailed, format!("The build isn't working yet. {why}").trim().to_string())
                .with_remediation("Ask for a fix in the box below, or press Try again."));
        }
        let last = ws.manifest.checks.last().cloned();
        // The last round's checks, all of them: the last one alone (say a mechanics sheet) isn't the verdict.
        let last_round: Vec<CheckResult> = last.map(|l| ws.manifest.checks.iter().filter(|c| c.round == l.round).cloned().collect()).unwrap_or_default();
        let mut summary = if last_round.is_empty() { "Ready.".to_string() } else { format!("Ready: {}.", self_check::summary(&last_round, opts.good_enough)) };
        // What the builder couldn't find out is said, not hidden.
        match self_check::open_questions(ws).len() {
            0 => {}
            1 => summary.push_str(" 1 question about the original is still open."),
            n => summary.push_str(&format!(" {n} questions about the original are still open.")),
        }
        ws.set_status(Status::Ready, summary.clone())?;
        self.emit(RunEvent::Step { step: Step::Done, text: summary.clone() });
        Ok(summary)
    }

    /// Read progress after the turn has ended. A consumed request resets the allowance on
    /// success, failure or cancellation; a failed save never acknowledges the pending reset.
    fn progress_after_notes(&mut self, ws: &Workspace) -> Result<BuildProgress, ToolError> {
        let mut progress = BuildProgress::load(&ws.root)?;
        if self.reset_progress_after_notes {
            progress.reset();
            progress.save(&ws.root)?;
            self.reset_progress_after_notes = false;
        }
        Ok(progress)
    }

    /// File scanning is an optional guard, not a prerequisite for building or checking. Once
    /// it is unavailable, skip it for this run and start a new comparison on the next run.
    fn progress_fingerprint(
        &self,
        ws: &Workspace,
        progress: &mut BuildProgress,
        available: &mut bool,
        fingerprints: &mut AppFingerprintCache,
    ) -> Result<Option<String>, ToolError> {
        self.stop.check()?;
        if !*available {
            return Ok(None);
        }
        match fingerprints.scan(ws, &self.stop) {
            Ok(fingerprint) => Ok(Some(fingerprint)),
            Err(e) if e.code == ErrorCode::Cancelled => Err(e),
            Err(e) => {
                *available = false;
                progress.invalidate();
                // Record errors remain errors: never overwrite a broken or newer record.
                progress.save(&ws.root)?;
                self.emit(RunEvent::Agent(AgentEvent::Status {
                    text: format!("Build progress couldn't be checked ({}). Continuing without the unchanged-turn limit for this run.", e.message),
                }));
                Ok(None)
            }
        }
    }

    /// One builder turn. Its session is kept even when the turn fails, so nothing is lost; a turn
    /// cut short (a time limit, a crash, a dropped connection) is picked up where it stopped, up to
    /// three times. Problems only the person can fix (sign-in, credit, a missing app) end the run.
    /// Deep mode on a game: the chosen extractor takes the game's own files out into app/Original/
    /// before the build (the person owns the game and chose this; nothing is decrypted).
    fn extract(&mut self, ws: &mut Workspace, kind: &dyn kinds::TargetKind, name: &str, mut ex: crate::agent::ApiConversation) -> Result<(), ToolError> {
        let TargetRef::Local { path } = ws.manifest.target.clone() else { return Ok(()) };
        if kind.id() != kinds::GameKind::ID || self.engine.session().clean_room() {
            return Ok(());
        }
        let folder = locate::game_folder(&path).unwrap_or(path);
        self.step(ws, Step::Study, Status::Inspecting, &format!("{name} is taking the game's own files out…"))?;
        std::fs::create_dir_all(ws.app_dir().join("Original")).map_err(|e| ToolError::io(ws.app_dir().display(), e))?;
        // A failed extraction doesn't stop the build: it says why, and the builder uses what's there.
        match self.converse(ws, &mut ex, &extract_brief(&folder)) {
            Err(e) if e.code == ErrorCode::Cancelled => Err(e),
            Err(e) => {
                let why = e.message.chars().take(200).collect::<String>();
                self.step(ws, Step::Study, Status::Inspecting, &format!("{name} couldn't finish taking the files out ({why}). Building with what's there."))
            }
            Ok(_) => Ok(()),
        }
    }

    /// The round's checks. A check that fails says what's wrong with the build; one that returns
    /// an error couldn't look at all (a browser that went away), which says nothing about the
    /// build, so it gets a second go before the run stops, and the run says what is really wrong.
    fn checks_with_a_retry(&self, ws: &Workspace, kind: &dyn kinds::TargetKind, round: u32) -> Result<Vec<CheckResult>, ToolError> {
        let e = match self.run_checks(ws, kind, round) {
            Err(e) if e.code != ErrorCode::Cancelled => e,
            done => return done,
        };
        self.emit(RunEvent::Agent(AgentEvent::Status { text: format!("The check couldn't finish ({}). Trying it once more…", e.message) }));
        self.run_checks(ws, kind, round).map_err(|e| {
            if e.code == ErrorCode::Cancelled {
                return e;
            }
            ToolError::new(e.code, format!("The build is finished, but it couldn't be checked: {}", e.message))
                .with_remediation("Press Try again to check it again. Nothing needs building again.")
        })
    }

    /// `fresh`: what to send instead when the agent's saved session turns out not to be on this
    /// computer.
    fn turn(&mut self, ws: &mut Workspace, agent: &mut dyn Conversation, prompt: &str, fresh: Option<String>) -> Result<String, ToolError> {
        self.turn_with_wait(ws, agent, prompt, fresh, |stop, seconds| {
            for _ in 0..seconds {
                stop.check()?;
                std::thread::sleep(Duration::from_secs(1));
            }
            stop.check()
        })
    }

    fn turn_with_wait(
        &mut self,
        ws: &mut Workspace,
        agent: &mut dyn Conversation,
        prompt: &str,
        mut fresh: Option<String>,
        mut wait_for: impl FnMut(&CancelToken, u64) -> Result<(), ToolError>,
    ) -> Result<String, ToolError> {
        let mut prompt = prompt.to_string();
        let mut tries = 0u64;
        let mut retry = false;
        loop {
            let result = self.converse_turn(ws, agent, &prompt, retry);
            let lost = result.is_err() && agent.session_lost();
            if lost {
                // The id is for a session this computer doesn't have: never ask for it again.
                ws.manifest.agent_session = None;
                ws.save()?;
            } else if let Some(id) = agent.session_id() {
                ws.manifest.agent_session = Some(id);
                ws.save()?;
            }
            match result {
                Err(_) if lost && let Some(f) = fresh.take() => {
                    self.step(
                        ws,
                        Step::Build,
                        Status::Building,
                        "The builder's earlier conversation isn't on this computer. Starting a new one from what the project already has…",
                    )?;
                    prompt = f;
                    retry = false;
                }
                // A missing session can only start fresh above, never resume. Only backend
                // Error events become ProgramFailed here; a simultaneous backend failure must
                // not make an inbox or workspace write failure retryable.
                Err(e) if !lost && e.code == ErrorCode::ProgramFailed && tries < 3 && agent.last_failure().is_some_and(crate::TurnFailure::retryable) => {
                    tries += 1;
                    let why = e.message.chars().take(160).collect::<String>();
                    // Typed status decides whether to retry. A display-only wait remains a
                    // compatibility hint, and never overrides a structured provider delay.
                    let delay = agent.last_failure().and_then(|f| f.retry_after_secs).or_else(|| crate::failure::retry_after(&e.message));
                    let seconds = match delay {
                        Some(secs) => {
                            let note = format!("The AI service is busy or its rate limit was reached, and asked to wait {secs} s. Trying again then…");
                            self.step(ws, Step::Build, Status::Building, &note)?;
                            secs.saturating_add(1).min(crate::failure::MAX_RETRY_WAIT)
                        }
                        None => {
                            self.step(ws, Step::Build, Status::Building, &format!("The builder stopped ({why}). Picking up where it left off…"))?;
                            30 * tries
                        }
                    };
                    wait_for(&self.stop, seconds)?;
                    self.stop.check()?;
                    retry = true;
                }
                r => return r,
            }
        }
    }

    /// Send one message and pump the agent until its turn ends, relaying events and inbox
    /// requests. Returns the agent's reply text.
    fn converse(&mut self, ws: &mut Workspace, agent: &mut dyn Conversation, text: &str) -> Result<String, ToolError> {
        self.converse_turn(ws, agent, text, false)
    }

    fn converse_turn(&mut self, ws: &mut Workspace, agent: &mut dyn Conversation, text: &str, retry: bool) -> Result<String, ToolError> {
        if retry {
            agent.retry(text);
        } else {
            agent.send(text);
        }
        let mut reply = String::new();
        let mut failed = None;
        loop {
            if self.stop.is_cancelled() {
                agent.stop();
            }
            for e in agent.poll() {
                if matches!(e, AgentEvent::ToolFinished { .. } | AgentEvent::TurnDone { .. }) {
                    let mut session = self.engine.session();
                    let project = session.project.clone().filter(|project| project.root == ws.investigation_dir());
                    if let Some(project) = project
                        && let Err(error) = session.open_project(project)
                    {
                        log::warn!("Couldn't refresh the investigation: {}", error.message);
                    }
                }
                match &e {
                    AgentEvent::TextDelta { text } => reply.push_str(text),
                    // Our own loop asks before tools that need consent. The Build tools are the
                    // builder's job (they apply the approval policy themselves); nothing else runs.
                    AgentEvent::ConsentNeeded { call_id, tool, .. } => {
                        let build = self.engine.registry().get(tool).is_some_and(|s| s.toolset == craft_anything_engine::Toolset::Build);
                        agent.answer_consent(call_id, build);
                    }
                    // Decided on the tool's own id (its CLI and `mcp__…__` names included).
                    AgentEvent::ToolStarted { tool, .. }
                        if self.engine.registry().canonical(tool) == Some(craft_anything_engine::tools::native_function::PSEUDOCODE_TOOL) =>
                    {
                        self.decompiled = true
                    }
                    AgentEvent::Error { message, remediation, .. } => failed = Some((message.clone(), remediation.clone())),
                    _ => {}
                }
                if let AgentEvent::TurnDone { input_tokens, output_tokens, cost_usd, cost_is_estimate, .. } = &e {
                    self.record_usage(Some((*input_tokens, *output_tokens, *cost_usd, *cost_is_estimate)));
                }
                let done = matches!(e, AgentEvent::TurnDone { .. });
                // A saved session that isn't on this computer isn't the build failing: the turn
                // starts a new one (or, with nothing to start it from, the run's error says why).
                if !(matches!(e, AgentEvent::Error { .. }) && agent.session_lost()) {
                    self.emit(RunEvent::Agent(e));
                }
                if done {
                    self.relay_read_notes(ws);
                    self.stop.check()?;
                    // An agent-level error (sign-in, crash, limits) ends the run: checking a
                    // build that never happened would only mislead.
                    if let Some((m, r)) = failed {
                        return Err(ToolError::new(ErrorCode::ProgramFailed, m).with_remediation(r.unwrap_or_default()));
                    }
                    return Ok(reply);
                }
            }
            self.relay_inbox();
            self.relay_read_notes(ws);
            self.answer_self_checks(ws)?;
            if self.usage.as_ref().is_some_and(|c| c.since.elapsed() >= USAGE_FLUSH) {
                self.record_usage(None);
            }
            std::thread::sleep(Duration::from_millis(120));
        }
    }

    /// Notes the builder took at a step of this turn (its hook claimed them): they join the feed
    /// now and aren't sent again after the turn.
    fn relay_read_notes(&mut self, ws: &Workspace) {
        let dir = notes::notes_dir(&ws.inbox_dir());
        let read: Vec<Note> = {
            let mut notes = self.notes.lock().unwrap_or_else(PoisonError::into_inner);
            let (read, waiting) = std::mem::take(&mut *notes).into_iter().partition(|n| notes::was_read(&dir, n.id));
            *notes = waiting;
            read
        };
        let new_request = read.iter().any(|n| !n.text.trim().is_empty());
        for n in read {
            notes::forget(&dir, n.id);
            self.emit(RunEvent::Note { shown: n.shown, pictures: n.pictures });
        }
        self.reset_progress_after_notes |= new_request;
    }

    fn relay_inbox(&mut self) {
        for r in self.inbox.pending() {
            if self.seen.insert(r.id.clone()) {
                self.emit(RunEvent::Ask(r));
            }
        }
    }
}

/// Checks in a row that may find the build not working before the run stops and asks for a new
/// request. Each "Try again" used to start another `max_rounds` of checks, with nothing counting
/// across runs, so a build that never came up was checked 40 and 49 times (#736).
pub const NOT_WORKING_CHECK_LIMIT: u32 = 10;

fn not_working(ws: &Workspace) -> ToolError {
    ToolError::new(ErrorCode::OverLimit, format!("The build still isn't working after {} checks in a row, so it stopped.", ws.manifest.not_working_checks))
        .with_remediation("Send a new request with a concrete correction to start again. The last check's note in the feed says what it found.")
}

/// The builder's next request when app/ holds no files: write the project first, then say what the
/// build printed. The reporter's agent claimed `npm run build` had passed while app/ was empty (#736).
const NO_PROJECT_PROMPT: &str = "The check found no project: app/ has no files, so nothing has been written yet and nothing can be built. Create the project's files in app/ now (for a website, a package.json and an index.html or the source files), then build it. Reply with the exact command you ran and the last lines of its output. Say something is built only when that output says so.";

fn no_build_progress(ws: &Workspace) -> ToolError {
    let stop = format!("The build stopped after two builder turns changed no files in {}.", ws.app_place());
    // #736: an empty app/ means the builder never wrote the project, whatever its replies said.
    if app_is_empty(ws) {
        ToolError::new(ErrorCode::OverLimit, format!("{stop} {}", no_files_yet(ws)))
            .with_remediation("Send a new request that asks for the project's files in app/ (such as package.json and index.html), or try another builder.")
    } else {
        ToolError::new(ErrorCode::OverLimit, stop)
            .with_remediation("Send a new request with a concrete correction, or edit the app's files before trying again.")
    }
}

/// Whether the builder has written nothing into app/ (#736). A project folder always holds the
/// workspace's own files, so only app/ can be empty.
fn app_is_empty(ws: &Workspace) -> bool {
    !ws.manifest.app_in_root && folder_is_empty(&ws.app_dir())
}

/// Whether a folder holds nothing: no files or folders, or no folder at all.
fn folder_is_empty(dir: &Path) -> bool {
    std::fs::read_dir(dir).map(|mut entries| entries.next().is_none()).unwrap_or(true)
}

/// Said when app/ is empty, so a builder that claims it built the project can't hide that nothing
/// was written (#736).
fn no_files_yet(ws: &Workspace) -> String {
    format!("{} has no files yet, so the builder hasn't written the project.", ws.app_place())
}

/// What an API-backend builder is told first: its tools differ from an agent app's.
const API_BUILDER: &str = "Your tools: build_files_list, build_file_read, build_file_write and build_file_edit for the project's files (paths relative to the project folder, e.g. app/src/main.rs), and build_command_run for commands, which run in app/ (never start dev servers or watchers). Where the steps below mention Read, Write, Edit or Bash, use these. For the checklist step use build_checklist (the same shape as TodoWrite: todos with content, activeForm and status) and call it again as you go. You can't look at pictures: use capture/README.md, capture/capture.json and the evidence instead. Ask the person with core_person_ask only when a choice really changes the result. Other Craft Anything tools are available too; enable a toolset with enable_toolset when you need it. rebuild_check isn't available in this loop: Craft Anything checks your build after each turn instead.";

/// Added to a refinement for a builder that can check its own work.
const CHECK_BEFORE_REPLYING: &str = " Before you reply, check your build again with rebuild_check and fix what it still lists.";

/// The backend the person chose for this project, while it's still set up and on (else the
/// default builds it).
pub fn project_builder<'a>(cfg: &crate::AiConfig, ws: &'a Workspace) -> Option<&'a str> {
    ws.manifest.builder.as_deref().filter(|n| cfg.get(n).is_some_and(|b| b.config.enabled))
}

/// The backend that builds: the one named, else the Builder role's, else an agent app, else any
/// enabled AI backend (which builds with our own agent loop and the Build tools).
pub fn builder_backend(cfg: &crate::AiConfig, named: Option<&str>) -> Result<crate::BackendEntry, ToolError> {
    let found = match named {
        Some(n) => cfg.get(n).cloned(),
        None => cfg.backend_for(Role::Builder).or_else(|| cfg.enabled().next()).cloned(),
    };
    found.ok_or_else(|| {
        ToolError::new(ErrorCode::BadSettings, "Building needs a coding agent (such as Claude Code) or an AI backend with an API key.")
            .with_remediation("Install a coding agent or add an API key in Settings → AI, then try again.")
    })
}
