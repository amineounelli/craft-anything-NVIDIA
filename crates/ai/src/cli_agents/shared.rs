//! Agent chats with a project shared by the desktop and MCP process. All preparation, polling
//! and project refresh work stays on this worker, so drawing never waits for filesystem I/O.

use super::*;
use crate::BackendEntry;
use craft_anything_engine::Engine;
use std::sync::mpsc::RecvTimeoutError;

#[allow(dead_code)]
enum Command {
    Send(String),
    Answer(String, bool),
    Stop,
    Quit,
}

#[allow(dead_code)]
pub(crate) struct SharedConversation {
    commands: Sender<Command>,
    events: Receiver<AgentEvent>,
    report: Sender<AgentEvent>,
    busy: Arc<AtomicBool>,
    session: Arc<Mutex<Option<String>>>,
}

impl SharedConversation {
    #[allow(dead_code)]
    pub(crate) fn new(engine: Arc<Engine>, entry: BackendEntry, beginner: bool) -> Result<Self, ToolError> {
        let (commands, requests) = std::sync::mpsc::channel();
        let (output, events) = std::sync::mpsc::channel();
        let report = output.clone();
        let busy = Arc::new(AtomicBool::new(false));
        let session = Arc::new(Mutex::new(None));
        let (working, saved_session) = (busy.clone(), session.clone());
        std::thread::Builder::new()
            .name("ai-shared-project".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut conversation: Option<Box<dyn Conversation>> = None;
                    let mut context: Option<Value> = None;
                    loop {
                        match requests.recv_timeout(Duration::from_millis(25)) {
                            Ok(Command::Quit) | Err(RecvTimeoutError::Disconnected) => break,
                            Ok(Command::Send(text)) => {
                                let _ = output.send(AgentEvent::Status { text: "Preparing the shared investigation...".into() });
                                match prepare(&engine, &entry, beginner) {
                                    Ok(mut spec) => {
                                        let next = json!({"project": spec.project, "targets": spec.targets, "deep": spec.deep, "options": spec.options});
                                        if context.as_ref() != Some(&next) {
                                            if context.as_ref().and_then(|context| context.get("project")) == next.get("project") {
                                                spec.resume = conversation.as_ref().and_then(|conversation| conversation.session_id());
                                            }
                                            conversation = Some(super::conversation(spec));
                                            context = Some(next);
                                        }
                                    }
                                    Err(error) => {
                                        let _ = output.send(AgentEvent::Error { message: error.message, remediation: error.remediation, log: None });
                                        let _ = output.send(AgentEvent::TurnDone {
                                            backend: entry.config.name.clone(),
                                            input_tokens: 0,
                                            output_tokens: 0,
                                            cost_usd: None,
                                            cost_is_estimate: false,
                                        });
                                        working.store(false, Ordering::SeqCst);
                                        continue;
                                    }
                                }
                                if let Some(conversation) = conversation.as_mut() {
                                    conversation.send(&text);
                                }
                            }
                            Ok(Command::Answer(id, allow)) => {
                                if let Some(conversation) = conversation.as_mut() {
                                    conversation.answer_consent(&id, allow);
                                }
                            }
                            Ok(Command::Stop) => {
                                if let Some(conversation) = conversation.as_mut() {
                                    conversation.stop();
                                }
                            }
                            Err(RecvTimeoutError::Timeout) => {}
                        }
                        let Some(conversation) = conversation.as_mut() else { continue };
                        let events = conversation.poll();
                        if events.iter().any(|event| matches!(event, AgentEvent::ToolFinished { .. } | AgentEvent::TurnDone { .. })) {
                            let refresh = {
                                let mut session = engine.session();
                                let project = session.project.clone();
                                project.map(|project| session.open_project(project)).transpose()
                            };
                            if let Err(error) = refresh {
                                let _ = output.send(AgentEvent::Error {
                                    message: format!("Couldn't refresh the shared investigation: {}", error.message),
                                    remediation: error.remediation,
                                    log: None,
                                });
                            }
                        }
                        *saved_session.lock().unwrap_or_else(PoisonError::into_inner) = conversation.session_id();
                        for event in events {
                            let _ = output.send(event);
                        }
                        working.store(conversation.is_busy(), Ordering::SeqCst);
                    }
                }));
                if result.is_err() {
                    let _ = output.send(AgentEvent::Error {
                        message: "The shared investigation worker stopped safely after an internal error.".into(),
                        remediation: None,
                        log: None,
                    });
                    let _ = output.send(AgentEvent::TurnDone {
                        backend: entry.config.name.clone(),
                        input_tokens: 0,
                        output_tokens: 0,
                        cost_usd: None,
                        cost_is_estimate: false,
                    });
                }
                working.store(false, Ordering::SeqCst);
            })
            .map_err(|error| ToolError::internal(format!("Couldn't start the investigation worker: {error}")))?;
        Ok(Self { commands, events, report, busy, session })
    }
}

#[allow(dead_code)]
fn prepare(engine: &Engine, entry: &BackendEntry, beginner: bool) -> Result<CliSpec, ToolError> {
    let mut session = engine.session();
    let config = crate::AiConfig::from_settings(&session.settings);
    let entry = config.get(&entry.config.name).unwrap_or(entry);
    let project = session.ensure_shared_project()?;
    let targets = session.shared_targets();
    serde_json::to_value(&targets).map_err(|error| ToolError::invalid(format!("Couldn't share the target metadata: {error}")))?;
    let mut spec = CliSpec::new(entry.config.clone(), entry.options.clone(), beginner, Some(project));
    spec.targets = targets;
    spec.deep = !session.clean_room();
    spec.logs = Some(crate::failure_log::dir_in(&session.config_dir()));
    Ok(spec)
}

impl Conversation for SharedConversation {
    fn send(&mut self, text: &str) {
        if self.busy.swap(true, Ordering::SeqCst) {
            let _ = self.report.send(AgentEvent::Error {
                message: "Still working on the last message.".into(),
                remediation: Some("Wait for it to finish or press stop.".into()),
                log: None,
            });
            return;
        }
        if self.commands.send(Command::Send(text.into())).is_err() {
            self.busy.store(false, Ordering::SeqCst);
            let _ = self.report.send(AgentEvent::Error {
                message: "The investigation worker is no longer running.".into(),
                remediation: Some("Start a new conversation and try again.".into()),
                log: None,
            });
        }
    }

    fn session_id(&self) -> Option<String> {
        self.session.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
    fn poll(&mut self) -> Vec<AgentEvent> {
        self.events.try_iter().collect()
    }
    fn answer_consent(&mut self, id: &str, allow: bool) {
        let _ = self.commands.send(Command::Answer(id.into(), allow));
    }
    fn stop(&mut self) {
        let _ = self.commands.send(Command::Stop);
    }
    fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }
}

impl Drop for SharedConversation {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Quit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BackendConfig, BackendKind};
    use craft_anything_engine::target::Target;

    #[test]
    fn preparation_shares_the_project_and_keeps_the_active_target_last() {
        let directory = tempfile::tempdir().unwrap();
        let engine = Engine::new(craft_anything_engine::Session::isolated(directory.path()));
        let first = directory.path().join("first.bin");
        let second = directory.path().join("second.bin");
        std::fs::write(&first, b"first").unwrap();
        std::fs::write(&second, b"second").unwrap();
        let active = engine.session().add_target(Target::load(&first).unwrap());
        engine.session().add_target(Target::load(&second).unwrap());
        engine.session().set_active(&active).unwrap();
        let entry = BackendEntry {
            config: BackendConfig { name: "Agent".into(), kind: BackendKind::Nvidia, model: "copilot".into(), base_url: None, ssh_host: None, enabled: true },
            options: BackendOptions::default(),
        };
        let spec = prepare(&engine, &entry, false).unwrap();
        assert_eq!(spec.targets.len(), 2);
        assert_eq!(spec.targets.last().unwrap().path, first);
        assert_eq!(spec.project, engine.session().project.as_ref().map(|project| project.root.clone()));
        let command = super::super::mcp_for(&spec, false);
        assert_eq!(command.args.iter().filter(|argument| argument.as_str() == "--target").count(), 2);
        let mut updated = entry.clone();
        updated.options.cli_context = Some(crate::config::CliContext::Long);
        updated.options.cli_reasoning_effort = Some(crate::config::CliReasoningEffort::Max);
        let mut config = crate::AiConfig::default();
        config.upsert(updated);
        engine.session().settings.ai = config.to_table().unwrap();
        let refreshed = prepare(&engine, &entry, false).unwrap();
        assert_eq!(refreshed.options.cli_context, Some(crate::config::CliContext::Long));
        assert_eq!(refreshed.options.cli_reasoning_effort, Some(crate::config::CliReasoningEffort::Max));
    }
}
