//! Exercise the real exit-save path in a child whose global config directory is isolated.

use craft_anything_ai::{AiConfig, AiHub, BackendConfig, BackendEntry, BackendKind, BackendOptions, Effort, KeyStore};
use craft_anything_engine::{Engine, Mode, Session, Settings};
use craft_anything_platform::process::Process;
use craft_anything_ui_egui::CraftApp;
use eframe::App;
use std::{sync::Arc, time::Duration};

/// Closing with pending app edits must preserve an AI selection committed since the app snapshot.
#[test]
fn exit_save_preserves_latest_ai_and_pending_app_settings() {
    const CHILD: &str = "CRAFT_ANYTHING_EXIT_SAVE_TEST";
    let Some(dir) = std::env::var_os(CHILD) else {
        for busy in ["false", "true"] {
            let dir = tempfile::tempdir().unwrap();
            Process::new(std::env::current_exe().unwrap())
                .args(["--exact", "exit_save_preserves_latest_ai_and_pending_app_settings", "--nocapture"])
                .env(CHILD, dir.path())
                .env("CRAFT_ANYTHING_CONFIG_DIR", dir.path())
                .env("CRAFT_ANYTHING_EXIT_SAVE_BUSY", busy)
                .timeout(Duration::from_secs(30))
                .run_checked()
                .unwrap();
        }
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    assert_eq!(Settings::path(), dir.join("config.toml"));
    let engine = Arc::new(Engine::new(Session::isolated(&dir)));
    let hub = Arc::new(AiHub::with_stores(engine.clone(), Settings::path(), KeyStore::file_only(dir.join("keys.toml"))));
    let mut entry = BackendEntry {
        config: BackendConfig { name: "OpenCode".into(), kind: BackendKind::Nvidia, model: "opencode".into(), base_url: None, ssh_host: None, enabled: true },
        options: BackendOptions { cli_model: Some("test/old".into()), cli_thinking: Some(Effort::Low), ..Default::default() },
    };
    hub.add_backend_with(entry.clone(), None).unwrap();
    let mut app = CraftApp::new(engine.clone(), hub.clone());
    app.settings.mode = Mode::Experienced;
    app.settings.language = "de".into();
    app.settings_changed();

    // Commit the newer selection through the hub, leaving the app's AI snapshot stale.
    entry.options.cli_model = Some("test/new".into());
    entry.options.cli_thinking = Some(Effort::Max);
    hub.add_backend_with(entry.clone(), None).unwrap();
    let mut latest_ai = engine.session().settings.ai.clone();
    assert_ne!(app.settings.ai, latest_ai);
    if std::env::var("CRAFT_ANYTHING_EXIT_SAVE_BUSY").as_deref() == Ok("true") {
        // A long tool holds the session until after shutdown returns, then commits newer AI data.
        entry.options.cli_model = Some("test/final".into());
        let mut config = AiConfig::from_table(&latest_ai).unwrap();
        config.upsert(entry.clone());
        latest_ai = config.to_table().unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker_engine = engine.clone();
        let committed = latest_ai.clone();
        let worker = std::thread::spawn(move || {
            let mut session = worker_engine.session();
            session.label("Test running tool");
            ready_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(5));
            session.settings.ai = committed;
            session.settings.save().unwrap();
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let start = std::time::Instant::now();
        app.on_exit();
        assert!(start.elapsed() < Duration::from_secs(1), "shutdown must not wait for the tool");
        assert!(Settings::path().with_extension("pending.toml").exists());
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        let saved = Settings::load();
        assert_eq!(saved.ai, latest_ai, "startup keeps even an AI commit made after shutdown returned");
        assert_eq!(saved.mode, Mode::Experienced);
        assert_eq!(saved.language, "de");
        assert!(!Settings::path().with_extension("pending.toml").exists());
        assert_eq!(Settings::load(), saved, "recovery is durable and runs only once");
        return;
    }
    // A sidecar left by an earlier failed recovery must not undo this successful full save.
    let stale = Settings { language: "fr".into(), mode: Mode::Beginner, ..app.settings.clone() };
    stale.defer_save_to(&Settings::path()).unwrap();
    app.on_exit();
    assert!(!Settings::path().with_extension("pending.toml").exists());
    Settings::discard_pending(&Settings::path()).unwrap(); // Already absent is also success.

    let saved = Settings::load();
    assert_eq!(AiConfig::from_table(&saved.ai).unwrap().get("OpenCode"), Some(&entry));
    assert_eq!(saved.mode, Mode::Experienced);
    assert_eq!(saved.language, "de");
    assert_eq!(app.settings.ai, latest_ai);
    let session = engine.session();
    assert_eq!(session.settings.ai, latest_ai);
    assert_eq!(session.settings.mode, saved.mode);
    assert_eq!(session.settings.language, saved.language);
}
