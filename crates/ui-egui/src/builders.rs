//! Who builds: the AIs that can build, the one that builds by default, the choice on Make
//! ("Built by"), and the dialog when nothing can build yet. One rule decides "can it build?"
//! ([`craft_anything_ai::default_builder`]), so the Make screen's warning and Start agree.

use crate::i18n::{tr, trf};
use crate::state::Modal;
use crate::theme::{self, Tokens};
use crate::{CraftApp, icons, widgets};
use craft_anything_ai::BackendConfig;
use egui::{RichText, Ui, Vec2};

/// A live agent catalogue and a draft selection, tied to the settings that were queried.
pub struct AgentModelPicker {
    pub request: u64,
    pub context: craft_anything_ai::ModelContext,
    pub result: Option<Result<Vec<craft_anything_ai::AgentModel>, String>>,
    pub model: Option<String>,
    pub thinking: Option<craft_anything_ai::Effort>,
}

/// The UI keeps the draft; discovery, validation and persistence live in the AI layer.
pub fn agent_model_picker(ui: &mut Ui, picker: &mut AgentModelPicker) -> bool {
    let t = Tokens::get(ui.ctx());
    let Some(result) = &picker.result else {
        ui.add(crate::widgets::Spinner::new().size(theme::CAPTION));
        ui.label(tr("Asking for its models…"));
        return false;
    };
    let models = match result {
        Ok(models) => models,
        Err(message) => {
            ui.label(RichText::new(message).color(t.warning));
            return false;
        }
    };
    let agent = craft_anything_ai::cli_agents::display_for(&picker.context.backend.config.model);
    if models.is_empty() {
        ui.label(trf("No chat models are visible in {agent}. Check its setup, then reload.", &[("agent", &agent)]));
    }
    let default = if picker.context.backend.config.model == "opencode" { tr("OpenCode default") } else { tr("Model default") };
    let selected = picker.model.as_deref().unwrap_or(&default);
    let response = egui::ComboBox::from_id_salt(("agent-model", &picker.context.backend.config.name))
        .icon(widgets::combo_icon)
        .selected_text(selected)
        .show_ui(ui, |ui| {
            if ui.selectable_label(picker.model.is_none(), &default).clicked() {
                picker.model = None;
                picker.thinking = None;
            }
            for model in models {
                let label = if model.name == model.id { model.name.clone() } else { format!("{} · {}", model.name, model.id) };
                if ui.selectable_label(picker.model.as_ref() == Some(&model.id), label).clicked() && picker.model.as_ref() != Some(&model.id) {
                    picker.model = Some(model.id.clone());
                    picker.thinking = None;
                }
            }
        });
    widgets::named(response.response, if picker.context.backend.config.model == "opencode" { "OpenCode model" } else { "Model" });
    let model = models.iter().find(|m| picker.model.as_ref() == Some(&m.id));
    let missing = picker.model.is_some() && model.is_none();
    if missing {
        ui.label(
            RichText::new(trf("The saved model is no longer visible. Choose another model or the {agent} default.", &[("agent", &agent)])).color(t.warning),
        );
    }
    let depths = model.map(|m| m.thinking.as_slice()).unwrap_or_default();
    ui.label(tr("Thinking depth"));
    let default_depth = tr("Model default");
    let response = egui::ComboBox::from_id_salt(("agent-thinking", &picker.context.backend.config.name))
        .icon(widgets::combo_icon)
        .selected_text(picker.thinking.map(|d| d.title()).unwrap_or(&default_depth))
        .show_ui(ui, |ui| {
            if ui.selectable_label(picker.thinking.is_none(), &default_depth).clicked() {
                picker.thinking = None;
            }
            for depth in depths {
                if ui.selectable_label(picker.thinking == Some(*depth), depth.title()).clicked() {
                    picker.thinking = Some(*depth);
                }
            }
        });
    widgets::named(response.response, "Thinking depth");
    let unsupported = picker.thinking.is_some_and(|d| !depths.contains(&d));
    if unsupported {
        ui.label(RichText::new(tr("The saved thinking depth is unavailable for this model. Choose its default or an offered depth.")).color(t.warning));
    }
    ui.add_enabled_ui(!missing && !unsupported, |ui| widgets::link(ui, "Apply").clicked()).inner
}

/// One AI that could build.
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    pub name: String,
    /// What it is, in a few words ("Agent app", "Anthropic (Claude API) · claude-…").
    pub detail: String,
    /// Found on this computer and not set up yet: it's set up when a build starts with it.
    pub found: bool,
}

fn detail(b: &BackendConfig) -> String {
    if b.model.is_empty() { tr(b.kind.title()) } else { format!("{} · {}", tr(b.kind.title()), b.model) }
}

impl CraftApp {
    /// Every AI that can build: the ones set up and on.
    pub fn builder_choices(&self) -> Vec<Choice> {
        self.backends.iter().filter(|b| b.enabled).map(|b| Choice { name: b.name.clone(), detail: detail(b), found: false }).collect()
    }

    /// What builds when the person doesn't choose: the default from Settings → AI.
    pub fn default_builder(&self) -> Option<String> {
        craft_anything_ai::default_builder(&self.backends, self.builder.as_deref()).map(|b| b.name.clone())
    }

    /// Something can build.
    pub fn can_build(&self) -> bool {
        self.default_builder().is_some()
    }

    /// The AI to build with: `wanted` while it can build, else the default.
    pub fn builder_backend(&mut self, wanted: Option<&str>) -> Option<String> {
        let choices = self.builder_choices();
        wanted.filter(|w| choices.iter().any(|c| c.name == *w)).map(str::to_string).or_else(|| self.default_builder())
    }

    /// Turn an AI on or off in Settings → AI (it keeps its key).
    pub fn set_backend_enabled(&mut self, name: &str, on: bool) {
        let (hub, name) = (self.hub.clone(), name.to_string());
        self.jobs.spawn(if on { tr("Turn on an AI") } else { tr("Turn off an AI") }, false, move |_| {
            hub.set_enabled(&name, on).map_err(|e| e.message)?;
            Ok(crate::backends(&hub))
        });
    }

    /// Ask an AI which models it can use, for Settings → AI's model picker.
    pub fn pick_model(&mut self, name: &str) {
        let (hub, name) = (self.hub.clone(), name.to_string());
        self.model_picker = Some((name.clone(), None));
        self.jobs.spawn(tr("List an AI's models"), false, move |_| {
            let result = hub.model_choices(&name);
            Ok(crate::Done::ModelChoices { name, result })
        });
    }

    /// Capture the backend context and load its catalogue in a worker tagged with a fresh picker request id.
    /// If the session cannot be read, report the error and keep the existing draft.
    pub fn pick_agent_model(&mut self, name: &str) {
        let context = match self.hub.model_context(name) {
            Ok(context) => context,
            Err(e) => {
                self.toast(e.for_people(), true);
                return;
            }
        };
        let model = context.backend.options.cli_model.clone();
        let thinking = context
            .backend
            .options
            .cli_thinking
            .or_else(|| context.backend.options.cli_reasoning_effort.and_then(|value| craft_anything_ai::Effort::parse(value.as_str())));
        self.load_agent_model_catalog(context, model, thinking);
    }

    pub fn reload_agent_model(&mut self, name: &str) {
        if let Some(picker) = self.agent_model_picker.as_ref().filter(|picker| picker.context.backend.config.name == name) {
            self.load_agent_model_catalog(picker.context.clone(), picker.model.clone(), picker.thinking);
        } else {
            self.pick_agent_model(name);
        }
    }

    fn load_agent_model_catalog(&mut self, context: craft_anything_ai::ModelContext, model: Option<String>, thinking: Option<craft_anything_ai::Effort>) {
        self.model_request = self.model_request.wrapping_add(1);
        let request = self.model_request;
        self.agent_model_picker = Some(AgentModelPicker { request, model: model.clone(), thinking, context: context.clone(), result: None });
        let hub = self.hub.clone();
        self.jobs.spawn(tr("List an AI's models"), false, move |_| {
            let mut query = context.clone();
            query.backend.options.cli_model = model;
            let result = hub.agent_models(&query);
            Ok(crate::Done::AgentModelChoices { request, context, result })
        });
    }

    /// Submit the matching picker's selection for background validation and persistence against its captured context.
    pub fn set_agent_model(&mut self, name: &str, model: Option<String>, thinking: Option<craft_anything_ai::Effort>) {
        let Some(picker) = self.agent_model_picker.take().filter(|p| p.context.backend.config.name == name) else { return };
        let hub = self.hub.clone();
        self.jobs.spawn(tr("Change an AI's model"), false, move |_| {
            hub.set_agent_selection(&picker.context, model.as_deref(), thinking).map_err(|e| e.for_people())?;
            Ok(crate::backends(&hub))
        });
    }

    /// Use `model` for an AI from now on.
    pub fn set_model(&mut self, name: &str, model: &str) {
        self.model_picker = None;
        if let Some(state) = self.model_tuning.get_mut(name) {
            state.saving = true;
        }
        let (hub, name, model) = (self.hub.clone(), name.to_string(), model.to_string());
        self.jobs.spawn(tr("Change an AI's model"), false, move |_| {
            let result = hub.set_model(&name, &model).and_then(|_| {
                hub.options().get(&name).cloned().ok_or_else(|| craft_anything_engine::evidence::ToolError::invalid("The AI backend was removed."))
            });
            Ok(crate::Done::ModelChanged { name, model, result })
        });
    }

    /// Populate the persistent model controls on a worker, once for each backend/model pair.
    pub fn pick_model_options(&mut self, name: &str) {
        let options = self.backend_options.get(name).cloned().unwrap_or_default();
        self.model_tuning.insert(
            name.into(),
            crate::ModelTuningDraft {
                backend: name.into(),
                cli_model: options.cli_model.clone(),
                choices: None,
                context: options.cli_context,
                saving: false,
                error: None,
            },
        );
        let (hub, name, cli_model) = (self.hub.clone(), name.to_string(), options.cli_model);
        self.jobs.spawn(tr("Load model options"), false, move |_| {
            let result = hub.model_tuning(&name);
            Ok(crate::Done::ModelTuning { name, cli_model, result })
        });
    }

    pub fn set_model_options(&mut self, params: craft_anything_ai::model_options::SetModelOptions) {
        if let Some(state) = self.model_tuning.get_mut(&params.backend) {
            state.saving = true;
        }
        let hub = self.hub.clone();
        self.jobs.spawn(tr("Set model options"), false, move |_| {
            let result = hub.set_model_options(&params);
            Ok(crate::Done::ModelOptionsSaved { params, result })
        });
    }

    /// How hard a Claude AI thinks from now on.
    pub fn set_effort(&mut self, name: &str, effort: craft_anything_ai::Effort) {
        let (hub, name) = (self.hub.clone(), name.to_string());
        self.jobs.spawn(tr("Change how hard an AI thinks"), false, move |_| {
            hub.set_effort(&name, Some(effort)).map_err(|e| e.message)?;
            Ok(crate::backends(&hub))
        });
    }

    /// Connect the server in Settings → AI's address form. Its key goes to the keychain; the form
    /// keeps what was typed until it's added, so a mistake can be fixed.
    pub fn add_server(&mut self) {
        let hub = self.hub.clone();
        let f = &self.server_form;
        let (name, address, model, key) = (f.name.trim().to_string(), f.address.clone(), f.model.trim().to_string(), Some(f.key.clone()));
        let choices = self.backend_choices;
        self.jobs.spawn(trf("Connect {name}", &[("name", &name)]), true, move |_| {
            let result = hub.add_server(&name, &address, Some(model.as_str()).filter(|m| !m.is_empty()), key).map(|_| ());
            Ok(crate::Done::BackendAdded { name, result, choices })
        });
    }

    /// Run an agent app from this program (`None`: find it by its usual name again).
    pub fn set_agent_program(&mut self, name: &str, program: Option<String>) {
        self.agent_model_picker = None;
        let (hub, name) = (self.hub.clone(), name.to_string());
        self.jobs.spawn(tr("Set an AI's program"), false, move |_| {
            hub.set_command(&name, program.as_deref()).map_err(|e| e.message)?;
            Ok(crate::backends(&hub))
        });
    }

    /// Keep an agent app's sign-in, settings and sessions in this folder, so the AI is another
    /// account (`None`: its usual folder again).
    pub fn set_agent_folder(&mut self, name: &str, folder: Option<String>) {
        let (hub, name) = (self.hub.clone(), name.to_string());
        self.jobs.spawn(tr("Set an AI's folder"), false, move |_| {
            hub.set_config_dir(&name, folder.as_deref()).map_err(|e| e.message)?;
            Ok(crate::backends(&hub))
        });
    }

    /// Remove an AI and its stored key.
    pub fn remove_backend(&mut self, name: &str) {
        let (hub, name) = (self.hub.clone(), name.to_string());
        self.jobs.spawn(tr("Remove an AI"), false, move |_| {
            hub.remove_backend(&name).map_err(|e| e.message)?;
            Ok(crate::backends(&hub))
        });
    }

    /// Nothing can build: say how to set something up (`start`: Start was pressed on Make, and
    /// picking an AI here starts the project).
    pub fn need_builder(&mut self, start: bool) {
        self.detect_backends();
        self.state.modal = Some(Modal::NeedBuilder { start });
    }
}

/// "Built by" on Make: the AI that builds this project. Automatic follows Settings → AI; picking
/// one is remembered by the project (its later changes are built by it too). With nothing that
/// can build, a warning and the way to set one up instead.
pub fn built_by(app: &mut CraftApp, ui: &mut Ui) {
    let t = Tokens::get(ui.ctx());
    let Some(default) = app.default_builder() else {
        icons::show(ui, "triangle-alert", 16.0, t.warning);
        ui.label(RichText::new(tr("Building needs an AI with an API key.")).size(theme::BODY).color(t.text_dim));
        if widgets::link(ui, "Set one up").clicked() {
            app.need_builder(false);
        }
        return;
    };
    let choices = app.builder_choices();
    // A choice that was removed or turned off meanwhile goes back to automatic.
    if app.state.make.ai.as_ref().is_some_and(|a| !choices.iter().any(|c| &c.name == a)) {
        app.state.make.ai = None;
    }
    let automatic = trf("Automatic ({name})", &[("name", &default)]);
    let shown = app.state.make.ai.clone().unwrap_or_else(|| automatic.clone());
    ui.label(RichText::new(tr("Built by")).size(theme::BODY).color(t.text_dim));
    let mut pick = None;
    let r = egui::ComboBox::from_id_salt("built-by").icon(widgets::combo_icon).selected_text(shown).width(220.0).show_ui(ui, |ui| {
        if ui.selectable_label(app.state.make.ai.is_none(), automatic).on_hover_text(tr("Whatever Settings → AI builds with.")).clicked() {
            pick = Some(None);
        }
        for c in &choices {
            let note = if c.found { format!("{} · {}", c.detail, tr("found on this computer; set up when you start")) } else { c.detail.clone() };
            if ui.selectable_label(app.state.make.ai.as_ref() == Some(&c.name), &c.name).on_hover_text(note).clicked() {
                pick = Some(Some(c.name.clone()));
            }
        }
    });
    widgets::named(r.response, "Built by");
    if let Some(p) = pick {
        app.state.make.ai = p;
    }
}

/// Nothing can build yet: what was found on this computer (one click to use it), a way to look
/// again, and Settings for API keys. Never a dead end.
pub fn need_builder_modal(app: &mut CraftApp, ui: &mut Ui, start: bool, close: &mut bool) {
    let t = Tokens::get(ui.ctx());
    ui.set_width(520.0);
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(48.0), egui::Sense::hover());
        ui.painter().rect_filled(rect, t.radius, t.accent_soft);
        icons::paint(ui, rect, "bot", 24.0, t.accent_ink);
        ui.vertical(|ui| {
            ui.label(theme::heading(tr("Set up a builder"), theme::CARD));
            ui.add(
                egui::Label::new(RichText::new(tr("Building needs an AI with an API key. Craft Anything never sees your account.")).color(t.text_dim)).wrap(),
            );
        });
    });
    ui.add_space(14.0);
    let found: Vec<BackendConfig> = Vec::new();
    if app.detecting {
        ui.horizontal(|ui| {
            ui.add(crate::widgets::Spinner::new().color(t.accent));
            ui.label(RichText::new(tr("Looking on this computer…")).color(t.text_dim));
        });
    } else if found.is_empty() {
        ui.label(RichText::new(tr("No AI was found on this computer yet.")).color(t.text_dim));
    }
    for b in found {
        widgets::panel(&t).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                icons::show(ui, "bot", 18.0, t.accent);
                ui.vertical(|ui| {
                    ui.label(RichText::new(&b.name).font(theme::semibold(theme::SMALL)));
                    ui.label(RichText::new(tr("Found on this computer")).color(t.text_faint).size(theme::CAPTION));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let label = if start { trf("Build with {name}", &[("name", &b.name)]) } else { trf("Use {name}", &[("name", &b.name)]) };
                    if widgets::primary(ui, &label).clicked() {
                        *close = true;
                        if start {
                            app.state.make.ai = Some(b.name.clone());
                            app.start_make();
                        } else {
                            app.builder_backend(Some(&b.name));
                        }
                    }
                });
            });
        });
        ui.add_space(6.0);
    }
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        if !app.detecting && widgets::secondary(ui, Some("radar"), "Look again").on_hover_text(tr("After installing one, look for it again.")).clicked() {
            app.detect_backends();
        }
        if widgets::secondary(ui, Some("key-round"), "Use an API key…").clicked() {
            *close = true;
            app.state.settings_open = true;
            app.state.settings_tab = crate::state::SettingsTab::Ai;
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if widgets::secondary(ui, None, "Cancel").clicked() {
                *close = true;
            }
        });
    });
}
