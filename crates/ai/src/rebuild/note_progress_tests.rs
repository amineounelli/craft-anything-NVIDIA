use super::*;
use crate::BackendKind;
use craft_anything_engine::workspace::progress::BUILD_PROGRESS_FILE;

fn consumed_notes(notes: &Mutex<Vec<Note>>, dir: &Path) {
    for (id, text) in [(71, "Keep the header blue"), (72, "Use larger text")] {
        notes.lock().unwrap().push(Note { id, text: text.into(), shown: text.into(), pictures: Vec::new() });
        notes::hand_over(dir, id, text).unwrap();
    }
    assert_eq!(notes::claim(dir).len(), 2);
}

struct NotesDuringPoll {
    notes: Arc<Mutex<Vec<Note>>>,
    notes_dir: PathBuf,
    progress: PathBuf,
    directory: bool,
    polls: usize,
    sends: usize,
}

impl Conversation for NotesDuringPoll {
    fn send(&mut self, _: &str) {
        self.sends += 1;
    }

    fn poll(&mut self) -> Vec<AgentEvent> {
        self.polls += 1;
        if self.polls == 1 {
            consumed_notes(&self.notes, &self.notes_dir);
            if self.directory {
                std::fs::create_dir(&self.progress).unwrap();
            } else {
                std::fs::write(&self.progress, "{ broken progress").unwrap();
            }
            return vec![AgentEvent::TextDelta { text: "Finished the edit".into() }];
        }
        vec![AgentEvent::TurnDone { backend: "scripted".into(), input_tokens: 0, output_tokens: 0, cost_usd: None, cost_is_estimate: false }]
    }

    fn answer_consent(&mut self, _: &str, _: bool) {}
    fn stop(&mut self) {}
    fn is_busy(&self) -> bool {
        self.polls < 2
    }
}

#[test]
fn consumed_notes_wait_for_turn_done_before_accessing_progress() {
    for directory in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut ws = Workspace::create(dir.path(), "Note polling", "Build an app", TargetRef::None).unwrap();
        let mut worker = check_worker(&ws, false);
        let (tx, rx) = std::sync::mpsc::channel();
        worker.tx = tx;
        let mut agent = NotesDuringPoll {
            notes: worker.notes.clone(),
            notes_dir: notes::notes_dir(&ws.inbox_dir()),
            progress: ws.root.join(BUILD_PROGRESS_FILE),
            directory,
            polls: 0,
            sends: 0,
        };
        let result = worker.turn_with_wait(&mut ws, &mut agent, "Apply the notes", None, |_, _| panic!("a local record cannot retry the builder"));
        assert_eq!(result.unwrap(), "Finished the edit");
        assert_eq!((agent.sends, agent.polls), (1, 2), "keep polling the existing request until TurnDone");
        assert!(!agent.is_busy());
        let events: Vec<_> = rx.try_iter().collect();
        assert_eq!(events.iter().filter(|event| matches!(event, RunEvent::Note { .. })).count(), 2);
        assert_eq!(events.iter().filter(|event| matches!(event, RunEvent::Agent(AgentEvent::TurnDone { .. }))).count(), 1);
        assert!(worker.notes.lock().unwrap().is_empty());
        assert!(BuildProgress::load(&ws.root).is_err(), "polling must not overwrite the broken progress record");
    }
}

#[test]
fn consumed_note_progress_errors_preserve_the_completed_turn() {
    use crate::agent::tests::{Scripted, turn};
    use crate::agent::{ApiConversation, EngineHost, LoopConfig, ModelSlot};
    use crate::model::{Block, ChatModel, Delta, Stop, StopReason, TurnRequest, TurnResult};

    struct NotesInTurn {
        script: Scripted,
        notes: Arc<Mutex<Vec<Note>>>,
        notes_dir: PathBuf,
        progress: PathBuf,
        directory: bool,
    }

    impl ChatModel for NotesInTurn {
        fn turn(&mut self, request: &TurnRequest<'_>, delta: &mut dyn FnMut(Delta), stop: &Stop) -> Result<TurnResult, ToolError> {
            consumed_notes(&self.notes, &self.notes_dir);
            if self.directory {
                std::fs::remove_file(&self.progress).unwrap();
                std::fs::create_dir(&self.progress).unwrap();
            } else {
                std::fs::write(&self.progress, "{ broken progress").unwrap();
            }
            self.script.turn(request, delta, stop)
        }

        fn model(&self) -> String {
            self.script.model()
        }
    }

    for directory in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut ws = Workspace::create(dir.path(), "Note persistence", "Build an app", TargetRef::None).unwrap();
        std::fs::create_dir_all(ws.capture_dir()).unwrap();
        ws.manifest.stack = "Rust (console)".into();
        ws.save().unwrap();
        let mut worker = check_worker(&ws, false);
        let seen = Arc::default();
        let model = NotesInTurn {
            script: Scripted {
                turns: [turn(vec![Block::Text { text: "Finished the requested edit".into() }], StopReason::EndTurn)].into(),
                seen: Arc::clone(&seen),
            },
            notes: worker.notes.clone(),
            notes_dir: notes::notes_dir(&ws.inbox_dir()),
            progress: ws.root.join(BUILD_PROGRESS_FILE),
            directory,
        };
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
        let opts = RunOptions { message: Some("Apply the requested edits".into()), ..RunOptions::default() };
        let error = worker.run(&mut ws, entry, Some(api), None, &opts).unwrap_err();
        let saved = Workspace::open(&ws.root).unwrap();
        assert!(saved.manifest.awaiting_check, "the finished turn must be recorded before a progress error: {error:?}");
        assert_eq!(saved.manifest.last_reply, "Finished the requested edit");
        assert_eq!(seen.lock().unwrap().len(), 1, "progress persistence must not send another model request");
        assert!(error.message.contains("build-progress.json"), "retain the actual record error: {error:?}");
        assert!(BuildProgress::load(&ws.root).is_err(), "do not discard or repair an unreadable record implicitly");
    }
}

#[test]
fn consumed_note_resets_survive_failed_and_cancelled_turns() {
    use crate::agent::{ApiConversation, EngineHost, LoopConfig, ModelSlot};
    use crate::model::{ChatModel, Delta, Stop, TurnRequest, TurnResult};
    use std::sync::atomic::AtomicUsize;

    struct InterruptedNote {
        notes: Arc<Mutex<Vec<Note>>>,
        notes_dir: PathBuf,
        progress: PathBuf,
        text: &'static str,
        record: &'static str,
        cancelled: bool,
        stop: CancelToken,
        calls: Arc<AtomicUsize>,
    }

    impl ChatModel for InterruptedNote {
        fn turn(&mut self, _: &TurnRequest<'_>, _: &mut dyn FnMut(Delta), _: &Stop) -> Result<TurnResult, ToolError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.notes.lock().unwrap().push(Note { id: 73, text: self.text.into(), shown: self.text.into(), pictures: Vec::new() });
            notes::hand_over(&self.notes_dir, 73, self.text).unwrap();
            assert_eq!(notes::claim(&self.notes_dir), [self.text]);
            if !self.record.is_empty() {
                std::fs::write(&self.progress, self.record).unwrap();
            }
            if self.cancelled {
                self.stop.cancel();
                Err(ToolError::cancelled())
            } else {
                Err(ToolError::new(ErrorCode::NoPermission, "fixture sign-in required").with_remediation("Sign in to the fixture provider."))
            }
        }

        fn model(&self) -> String {
            "scripted".into()
        }
    }

    for cancelled in [false, true] {
        for text in ["Keep the header blue", "   "] {
            for record in ["", "{ broken progress", r#"{"version":2,"fingerprint":null,"unchanged_turns":1}"#] {
                let dir = tempfile::tempdir().unwrap();
                let mut ws = Workspace::create(dir.path(), "Interrupted note", "Build an app", TargetRef::None).unwrap();
                std::fs::create_dir_all(ws.capture_dir()).unwrap();
                std::fs::create_dir_all(ws.app_dir()).unwrap();
                std::fs::write(ws.app_dir().join("app.txt"), "previous app files").unwrap();
                ws.manifest.stack = "Rust (console)".into();
                ws.manifest.last_reply = "Previous completed turn".into();
                ws.save().unwrap();
                let mut progress = BuildProgress::default();
                progress.before_turn(ws.app_fingerprint(&CancelToken::default()).unwrap());
                progress.unchanged_turns = 1;
                progress.save(&ws.root).unwrap();
                let mut worker = check_worker(&ws, false);
                let (tx, rx) = std::sync::mpsc::channel();
                worker.tx = tx;
                let calls = Arc::new(AtomicUsize::new(0));
                let model = InterruptedNote {
                    notes: worker.notes.clone(),
                    notes_dir: notes::notes_dir(&ws.inbox_dir()),
                    progress: ws.root.join(BUILD_PROGRESS_FILE),
                    text,
                    record,
                    cancelled,
                    stop: worker.stop.clone(),
                    calls: calls.clone(),
                };
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
                let opts = RunOptions { continuing: true, ..RunOptions::default() };
                let error = worker.run(&mut ws, entry, Some(api), None, &opts).unwrap_err();
                assert_eq!(calls.load(Ordering::SeqCst), 1, "an interrupted request must not be sent again");
                if cancelled {
                    assert_eq!(error.code, ErrorCode::Cancelled, "retain cancellation even when the progress record is unreadable");
                } else {
                    assert_eq!(error.code, ErrorCode::ProgramFailed);
                    assert_eq!(error.message, "Scripted: fixture sign-in required");
                    assert_eq!(error.remediation.as_deref(), Some("Sign in to the fixture provider."));
                }
                let saved = Workspace::open(&ws.root).unwrap();
                assert!(!saved.manifest.awaiting_check, "an interrupted turn is not a completed build");
                assert_eq!(saved.manifest.last_reply, "Previous completed turn");
                let events: Vec<_> = rx.try_iter().collect();
                assert_eq!(events.iter().filter(|event| matches!(event, RunEvent::Note { .. })).count(), 1);
                assert!(worker.notes.lock().unwrap().is_empty());
                assert!(!notes::was_read(&notes::notes_dir(&ws.inbox_dir()), 73));
                if record.is_empty() {
                    let recovered = BuildProgress::load(&saved.root).unwrap();
                    assert_eq!(recovered.fingerprint, progress.fingerprint, "the interrupted turn does not change the comparison baseline");
                    assert_eq!(recovered.unchanged_turns, u32::from(text.trim().is_empty()), "a consumed new request survives reopening");
                } else {
                    assert_eq!(std::fs::read_to_string(ws.root.join(BUILD_PROGRESS_FILE)).unwrap(), record, "never replace a broken or newer record");
                    let reported = events
                        .iter()
                        .any(|event| matches!(event, RunEvent::Agent(AgentEvent::Status { text }) if text.contains("couldn't reset the unchanged-turn count")));
                    assert_eq!(reported, !text.trim().is_empty(), "report a failed reset separately from the original failure");
                }
            }
        }
    }
}
