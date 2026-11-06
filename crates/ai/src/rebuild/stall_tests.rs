use super::*;
use crate::BackendKind;
use crate::agent::tests::{Scripted, turn};
use crate::agent::{ApiConversation, EngineHost, LoopConfig, ModelSlot};
use crate::model::{Block, ChatModel, Delta, Msg, Stop, StopReason, TurnRequest, TurnResult};

/// A kind whose one check always says the build isn't there yet (as a website with an empty
/// app/ does, #736). The builder is scripted to answer every turn without writing a file.
struct NeverBuilt;

struct NeverBuiltCheck;

impl Check for NeverBuiltCheck {
    fn id(&self) -> &'static str {
        "never-built"
    }
    fn run(&self, cx: &mut CheckCx) -> Result<CheckResult, ToolError> {
        Ok(CheckResult::new(self.id(), cx.round, false, "No built site found in app/ yet."))
    }
}

impl TargetKind for NeverBuilt {
    fn id(&self) -> &'static str {
        "never-built"
    }
    fn words(&self) -> KindWords {
        KindWords { button: "Build it", icon: "code", ready: "It's ready", change_hint: "Ask for a change" }
    }
    fn detect(&self, _cx: &DetectCx) -> Option<u32> {
        Some(1000)
    }
    fn default_stack(&self, _cx: &DetectCx) -> &'static str {
        "Scripted stack"
    }
    fn goal(&self) -> &'static str {
        "a scripted app"
    }
    fn checks(&self, _ws: &Workspace) -> Vec<Box<dyn Check>> {
        vec![Box::new(NeverBuiltCheck)]
    }
}

/// Runs one build whose builder never writes a file, and returns the run's error and how many
/// builder turns the model was asked for.
fn run_idle_builder(create_app_dir: bool) -> (ToolError, Vec<Vec<Msg>>) {
    let dir = tempfile::tempdir().unwrap();
    let mut ws = Workspace::create(dir.path(), "Idle build", "Build an app", TargetRef::None).unwrap();
    if create_app_dir {
        std::fs::create_dir_all(ws.app_dir()).unwrap();
    }
    ws.manifest.stack = "Scripted stack".into();
    ws.save().unwrap();
    let mut worker = check_worker(&ws, false);
    let seen = Arc::default();
    let turns = (0..20).map(|_| turn(vec![Block::Text { text: "Built it and ran npm run build.".into() }], StopReason::EndTurn)).collect();
    let model = Scripted { turns, seen: Arc::clone(&seen) };
    let slot = ModelSlot { name: "Scripted".into(), model: Box::new(model), local: true, effort: None, max_turns: 10, context_tokens: 0 };
    let api = ApiConversation::start(Arc::new(EngineHost(worker.engine.clone())), vec![slot], LoopConfig::default()).unwrap();
    let entry = crate::BackendEntry {
        config: crate::BackendConfig {
            name: "Scripted".into(),
            kind: BackendKind::Nvidia,
            model: "scripted".into(),
            base_url: None,
            ssh_host: None,
            enabled: true,
        },
        options: Default::default(),
    };
    let mut kinds = KindRegistry::built_in();
    kinds.register(Arc::new(NeverBuilt));
    let opts = RunOptions { message: Some("Build the app".into()), kinds, max_rounds: 6, ..RunOptions::default() };
    let error = worker.run(&mut ws, entry, Some(api), None, &opts).unwrap_err();
    let requests = seen.lock().unwrap().clone();
    (error, requests)
}

#[test]
fn an_idle_builder_with_an_empty_app_stops_after_two_turns() {
    for create_app_dir in [true, false] {
        let (error, requests) = run_idle_builder(create_app_dir);
        assert!(error.message.contains("has no files yet"), "app dir {create_app_dir}: {error:?}");
        assert!(
            requests.len() <= 3,
            "app dir {create_app_dir}: the builder was asked {} times; it should stop after two turns that changed nothing",
            requests.len()
        );
    }
}

/// #736: with app/ empty, the stop says app/ has no files yet, and the builder is told to write
/// the project's files (not to fix a build) and to show the build's output.
#[test]
fn an_empty_app_is_said_to_have_no_project() {
    let (error, requests) = run_idle_builder(true);
    assert!(error.message.contains("has no files yet"), "{error:?}");
    assert!(error.remediation.as_deref().unwrap_or_default().contains("asks for the project's files"), "{error:?}");
    let second = requests.get(1).map(|m| m.iter().map(Msg::text).collect::<String>()).unwrap_or_default();
    assert!(second.contains("app/ has no files"), "the next prompt says so: {second}");
    assert!(second.contains("exact command you ran"), "and asks for the build's output: {second}");
    assert!(!second.contains("Fix this in app/"), "not a fix for a build that doesn't exist: {second}");
}

/// A builder that changes one file in app/ every turn, so the unchanged-turn limit never fires.
struct Busy {
    app: PathBuf,
    turn: usize,
    seen: Arc<Mutex<usize>>,
}

impl ChatModel for Busy {
    fn turn(&mut self, _request: &TurnRequest<'_>, _delta: &mut dyn FnMut(Delta), _stop: &Stop) -> Result<TurnResult, ToolError> {
        self.turn += 1;
        // Numbered across runs too (a shared count), so each turn writes a file the last didn't.
        let mut count = self.seen.lock().unwrap();
        *count += 1;
        std::fs::create_dir_all(&self.app).unwrap();
        std::fs::write(self.app.join(format!("attempt-{}.txt", *count)), "try").unwrap();
        turn(vec![Block::Text { text: "Changed the app.".into() }], StopReason::EndTurn)
    }
    fn model(&self) -> String {
        "busy".into()
    }
}

/// One run of the build (a request, or "Try again" when `message` is `None`), with a builder
/// that keeps changing files and a check that never passes.
fn run_busy_builder(ws: &mut Workspace, seen: &Arc<Mutex<usize>>, message: Option<&str>) -> ToolError {
    let mut worker = check_worker(ws, false);
    let model = Busy { app: ws.app_dir(), turn: 0, seen: Arc::clone(seen) };
    let slot = ModelSlot { name: "Busy".into(), model: Box::new(model), local: true, effort: None, max_turns: 10, context_tokens: 0 };
    let api = ApiConversation::start(Arc::new(EngineHost(worker.engine.clone())), vec![slot], LoopConfig::default()).unwrap();
    let entry = crate::BackendEntry {
        config: crate::BackendConfig { name: "Busy".into(), kind: BackendKind::Nvidia, model: "busy".into(), base_url: None, ssh_host: None, enabled: true },
        options: Default::default(),
    };
    let mut kinds = KindRegistry::built_in();
    kinds.register(Arc::new(NeverBuilt));
    let opts = RunOptions { message: message.map(str::to_string), continuing: message.is_none(), kinds, max_rounds: 6, ..RunOptions::default() };
    worker.run(ws, entry, Some(api), None, &opts).unwrap_err()
}

#[test]
fn try_again_does_not_restart_a_build_that_never_works() {
    let dir = tempfile::tempdir().unwrap();
    let mut ws = Workspace::create(dir.path(), "Busy build", "Build an app", TargetRef::None).unwrap();
    ws.manifest.stack = "Scripted stack".into();
    ws.save().unwrap();
    let seen = Arc::new(Mutex::new(0));
    let first = run_busy_builder(&mut ws, &seen, Some("Build the app"));
    assert!(first.message.contains("isn't working yet"), "{first:?}");
    // Try again, many times: each press must not buy another full set of rounds.
    for _ in 0..5 {
        run_busy_builder(&mut ws, &seen, None);
    }
    let total = ws.manifest.checks.len();
    assert!(total <= NOT_WORKING_CHECK_LIMIT as usize, "{total} checks over six runs; the build must stop at its limit");
    assert_eq!(ws.manifest.not_working_checks, NOT_WORKING_CHECK_LIMIT, "the count is kept in the project");

    // Once at the limit, Try again stops before paying for another builder turn.
    let turns = *seen.lock().unwrap();
    let stopped = run_busy_builder(&mut ws, &seen, None);
    assert!(stopped.message.contains("in a row"), "{stopped:?}");
    assert_eq!(*seen.lock().unwrap(), turns, "no builder turn is started at the limit");
    assert_eq!(ws.manifest.checks.len(), total, "and no check runs either");

    // A new request starts the count again: the build gets its checks back.
    let restarted = run_busy_builder(&mut ws, &seen, Some("Use a plain page instead"));
    assert!(restarted.message.contains("isn't working yet"), "{restarted:?}");
    assert!(ws.manifest.checks.len() > total, "a new request runs checks again");
    assert!(ws.manifest.not_working_checks <= NOT_WORKING_CHECK_LIMIT, "{}", ws.manifest.not_working_checks);
}
