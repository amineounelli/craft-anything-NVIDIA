use super::check::build_check;
use super::*;

#[path = "note_progress_tests.rs"]
mod note_progress;

#[path = "stall_tests.rs"]
mod stall;

// Helpers that take a built-in kind's id, so each test reads as one line.
fn kind(id: &str) -> Arc<dyn TargetKind> {
    KindRegistry::built_in().get(id).unwrap()
}
fn choose(request: &str, target: &TargetRef) -> (&'static str, &'static str) {
    let (k, stack) = KindRegistry::built_in().choose(request, target, true).unwrap();
    (k.id(), stack)
}
fn kind_of(request: &str, target: &TargetRef, stack: &str) -> &'static str {
    KindRegistry::built_in().detect(request, target, stack).unwrap().id()
}
fn brief(ws: &Workspace, k: &str, deep: bool, ghidra: bool, editor: Option<&Path>, assets: bool, extra: Option<&str>) -> String {
    super::brief(kind(k).as_ref(), &BriefCx { ws, deep, ghidra, editor, assets, image_model: None }, extra)
}
fn refine(k: &str, check: &CheckResult) -> String {
    kind(k).refine(check)
}
fn follow_up(message: &str, k: &str) -> String {
    super::follow_up(message, kind(k).as_ref())
}
fn locate(path: &Path, k: &str) -> (PathBuf, Option<PathBuf>) {
    super::locate(path, kind(k).studies_folder())
}

#[test]
fn stack_choice() {
    let web = TargetRef::Website { url: "https://a.com".into() };
    assert_eq!(choose("Clone this website", &web), ("website", stacks::BUILDERS_CHOICE), "the builder picks a website's stack, like the original's");
    assert_eq!(choose("Clone this website with React", &web).1, "React + TypeScript (Vite)");
    assert_eq!(choose(SAID_NEXT_SHADCN, &web), ("website", "Next.js + TypeScript"), "the stack the person named, not the default");
    assert_eq!(choose("Recreate this Steam game's gameplay", &TargetRef::None), ("game", "Unity"), "games default to Unity");
    assert_eq!(choose("Rebuild this desktop application in Rust", &TargetRef::None), ("app", "Rust + egui (eframe)"));
    assert_eq!(choose("make a tool in python", &TargetRef::None).1, "Python");
    assert_eq!(choose("Recreate this game's core gameplay as a small game in Rust", &TargetRef::None).1, "Rust + macroquad");
    assert_eq!(choose("A platformer game with Godot", &TargetRef::None).1, "Godot");
    assert_eq!(choose("Remake this game in Unreal Engine 5", &TargetRef::None).1, "Unreal Engine");
    assert_eq!(choose("A snake game using unity", &TargetRef::None).1, "Unity");
    assert_eq!(choose("A pirate game in rustic colours", &TargetRef::None).1, "Unity", "a word that only starts like an engine doesn't count");
    assert_eq!(choose("A racing game, made in Rust.", &TargetRef::None).1, "Rust + macroquad");
    assert_eq!(choose("a landing page for my bakery", &TargetRef::None).0, "website");
    // The request names how to build: that wins over every kind's default.
    assert_eq!(choose("A notes app in Electron", &TargetRef::None), ("app", "Electron"));
    assert_eq!(choose("An Electron app with React and shadcn", &TargetRef::None).1, "Electron", "the app's own stack before its page library");
    assert_eq!(choose("A racing game in Electron", &TargetRef::None), ("game", "Electron"));
    let kinds = KindRegistry::built_in();
    assert_eq!(kinds.named("A racing game in Godot", &TargetRef::None), Some("Godot"));
    assert_eq!(kinds.named("a music player, built with electron", &TargetRef::None), Some("Electron"));
    assert_eq!(kinds.named("A racing game", &TargetRef::None), None, "nothing named: each kind's default applies");
    // Built with a game engine means a game, even when the request never says "game".
    let folder = TargetRef::Local { path: PathBuf::from("D:/Games/Untitled Project") };
    assert_eq!(choose("Clone Untitled Project. Match the original as closely as possible 1:1.", &folder).0, "app");
    assert_eq!(kind_of("Clone Untitled Project. Match the original as closely as possible 1:1.", &folder, "Unreal Engine"), "game");
    assert_eq!(kind_of("Copy this tool", &folder, "Rust + egui (eframe)"), "app", "the app stack stays an app");
}

#[test]
fn german_requests_keep_their_project_kind() {
    for request in ["Ein einfaches Snake-Spiel", "Dasselbe Spiel mit einer Bestenliste"] {
        assert_eq!(choose(request, &TargetRef::None), ("game", "Unity"), "{request}");
    }
    for request in ["Eine Website für meine Bäckerei", "Diese Webseite nachbauen"] {
        assert_eq!(choose(request, &TargetRef::None), ("website", stacks::BUILDERS_CHOICE), "{request}");
    }
}

#[test]
fn chinese_requests_keep_their_project_kind() {
    for request in ["一个简单的贪吃蛇游戏", "簡單的貪食蛇遊戲", "复刻它的玩法"] {
        assert_eq!(choose(request, &TargetRef::None), ("game", "Unity"), "{request}");
    }
    for request in ["给面包店做一个网站", "為我的麵包店製作網站", "做一个网页", "製作網頁"] {
        assert_eq!(choose(request, &TargetRef::None), ("website", stacks::BUILDERS_CHOICE), "{request}");
    }
    assert_eq!(choose("制作一个笔记应用", &TargetRef::None).0, "app");
}

#[test]
fn site_dir_prefers_build_output() {
    let d = tempfile::tempdir().unwrap();
    assert!(site_dir(d.path()).is_none());
    std::fs::write(d.path().join("index.html"), "x").unwrap();
    assert_eq!(site_dir(d.path()).as_deref(), Some(d.path()), "a plain static site");
    std::fs::write(d.path().join("package.json"), "{}").unwrap();
    assert!(site_dir(d.path()).is_none(), "a source index.html of an unbuilt Vite app doesn't count");
    std::fs::create_dir(d.path().join("dist")).unwrap();
    std::fs::write(d.path().join("dist/index.html"), "x").unwrap();
    assert_eq!(site_dir(d.path()), Some(d.path().join("dist")));
}

#[test]
fn built_program_finds_the_cargo_output_only() {
    let d = tempfile::tempdir().unwrap();
    let debug = d.path().join("target/debug");
    std::fs::create_dir_all(debug.join("build")).unwrap();
    let exe = |name: &str| if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    let make = |p: &Path| {
        std::fs::write(p, "x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    };
    make(&debug.join(exe("snake")));
    std::fs::write(debug.join("snake.d"), "deps").unwrap();
    make(&debug.join("build").join(exe("build-script-build")));
    assert!(built_program(d.path()).is_none(), "not a cargo project");
    std::fs::write(d.path().join("Cargo.toml"), "[package]").unwrap();
    assert_eq!(built_program(d.path()), Some(debug.join(exe("snake"))));
}

#[test]
fn play_starts_the_window_app_of_a_workspace_not_its_newest_program() {
    // Play started the CLI a workspace also built (it was newer), which printed help and quit
    // with no console, so nothing appeared.
    let d = tempfile::tempdir().unwrap();
    let app = d.path();
    let debug = app.join("target/debug");
    std::fs::create_dir_all(&debug).unwrap();
    std::fs::create_dir_all(app.join("src")).unwrap();
    std::fs::create_dir_all(app.join("crates/cli/src")).unwrap();
    std::fs::create_dir_all(app.join("crates/doc/src")).unwrap();
    std::fs::write(app.join("src/main.rs"), "fn main() {}").unwrap();
    std::fs::write(app.join("crates/cli/src/main.rs"), "fn main() {}").unwrap();
    std::fs::write(
        app.join("Cargo.toml"),
        "[workspace]\nmembers = [\".\", \"crates/*\"]\n\n[package]\nname = \"anim\"\n\n[dependencies]\neframe = { workspace = true }\n",
    )
    .unwrap();
    std::fs::write(app.join("crates/cli/Cargo.toml"), "[package]\nname = \"anim-cli\"\n\n[[bin]]\nname = \"anim-cli\"\npath = \"src/main.rs\"\n").unwrap();
    std::fs::write(app.join("crates/doc/Cargo.toml"), "[package]\nname = \"anim-doc\"\n").unwrap();
    let exe = |name: &str| if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    let make = |name: &str| {
        let p = debug.join(exe(name));
        std::fs::write(&p, "x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // Each later one is newer.
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    make("anim");
    make("anim.running-144900");
    make("anim-cli");
    assert_eq!(built_program(app), Some(debug.join(exe("anim"))), "the package that opens a window");
    // A default-run says which, whatever it uses.
    std::fs::write(
        app.join("Cargo.toml"),
        "[workspace]\nmembers = [\".\", \"crates/*\"]\n\n[package]\nname = \"anim\"\ndefault-run = \"anim-cli\"\n\n[dependencies]\neframe = \"0.36\"\n",
    )
    .unwrap();
    assert_eq!(built_program(app), Some(debug.join(exe("anim-cli"))));
}

#[test]
fn a_launcher_leads_to_the_game_and_its_files() {
    let d = tempfile::tempdir().unwrap();
    let game = d.path();
    for dir in ["bin_win32", "packages/maps", "packages/textures/walls", "packages/audio"] {
        std::fs::create_dir_all(game.join(dir)).unwrap();
    }
    let exe = |n: &str| if cfg!(windows) { format!("{n}.exe") } else { n.to_string() };
    let make = |p: &Path, bytes: usize| {
        std::fs::write(p, vec![0u8; bytes]).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    };
    make(&game.join("bin_win32").join(exe("client")), 10);
    make(&game.join("bin_win32").join(exe("server")), 99); // bigger, but a server isn't the game
    make(&game.join(exe("Uninstall")), 500);
    std::fs::write(game.join("packages/maps/dust.cgz"), "m").unwrap();
    std::fs::write(game.join("packages/textures/walls/brick.jpg"), "t").unwrap();
    std::fs::write(game.join("packages/audio/shot.ogg"), "s").unwrap();
    std::fs::write(game.join("packages/textures/walls/license.txt"), "CC-BY").unwrap();
    let bat = game.join("play.bat");
    std::fs::write(&bat, format!("start bin_win32\\{} \"--home=x\" --init %1\n", exe("client"))).unwrap();

    let (program, folder) = locate(&bat, "app");
    assert_eq!(program, game.join("bin_win32").join(exe("client")), "the program the script starts");
    assert_eq!(folder.as_deref(), Some(game), "a launcher's folder is studied whatever the kind");
    assert_eq!(main_program(game), Some(game.join("bin_win32").join(exe("client"))), "skips installers and servers");
    let (_, up) = locate(&game.join("bin_win32").join(exe("client")), "game");
    // Picking Unreal's shared Engine/ folder (or Binaries/Win64) finds the game above it.
    let ue = d.path().join("Untitled Project");
    let shipping = ue.join("MyGame/Binaries/Win64").join(exe("MyGame-Win64-Shipping"));
    std::fs::create_dir_all(shipping.parent().unwrap()).unwrap();
    make(&shipping, 4096);
    std::fs::create_dir_all(ue.join("Engine/Binaries/Win64")).unwrap();
    std::fs::write(ue.join("Engine/Binaries/Win64").join(exe("CrashReportClient")), b"MZ").unwrap();
    for picked in [ue.join("Engine"), ue.clone()] {
        assert_eq!(locate(&picked, "game"), (shipping.clone(), Some(ue.clone())), "{}", picked.display());
    }
    // Inside the game's own project folder, that folder (with its Content/) is the game.
    assert_eq!(locate(&ue.join("MyGame").join("Binaries"), "game"), (shipping.clone(), Some(ue.join("MyGame"))));
    assert_eq!(game_root(d.path()), d.path(), "a folder that isn't inside a game stays as picked");
    assert_eq!(up.as_deref(), Some(game), "content lives above bin_win32");

    let inv = inventory(game);
    assert!(inv.contains("Maps and levels: 1 files") && inv.contains("Textures and pictures: 1 files") && inv.contains("Sounds and music: 1 files"), "{inv}");
    assert!(inv.contains("brick.jpg"));
    assert!(inv.contains("Read-me and licence files: 1 files") && !inv.contains("Settings and scripts"), "{inv}");
}

#[test]
fn engine_stacks_get_their_own_steps_checks_and_play() {
    let ed = Path::new("C:/Engines/Unity/Editor/Unity.exe");
    let unity = stack_steps("Unity", Some(ed));
    assert!(unity.contains("CraftBuild.Build") && unity.contains("Unity.exe") && unity.contains("app/Build/Game."));
    assert!(stack_steps("Godot", Some(ed)).contains("--headless --path app"));
    assert!(stack_steps("Unreal Engine", Some(ed)).contains("Game.uproject"));

    let d = tempfile::tempdir().unwrap();
    let stop = CancelToken::default();
    assert_eq!(build_check(d.path(), "app/", "Unity", Some(ed), &stop), (false, "No Unity project found in app/ yet.".into()));
    assert_eq!(build_check(d.path(), "app/", "Godot", Some(ed), &stop), (false, "No Godot project (project.godot) found in app/ yet.".into()));
    assert!(build_check(d.path(), "app/", "Unreal Engine", Some(ed), &stop).1.contains(".uproject"));
    // #174: an app edited in place is said to be in the project folder, not in an app/ that isn't there.
    let none = build_check(d.path(), "the project folder", "Builder's choice", None, &stop);
    assert_eq!(none, (false, "No project to build found in the project folder yet.".into()));

    // Play: a Godot project opens in Godot; a packaged Unity build runs its own program.
    std::fs::write(d.path().join("project.godot"), "config_version=5").unwrap();
    let g = Path::new("C:/godot.exe");
    assert_eq!(launch(d.path(), Some(g)), Some((g.to_path_buf(), vec!["--path".into(), d.path().display().to_string()])));
    assert_eq!(launch(d.path(), None), None, "no Godot, nothing to open it with");
    std::fs::remove_file(d.path().join("project.godot")).unwrap();
    let build = d.path().join("Build");
    std::fs::create_dir_all(&build).unwrap();
    let exe = |n: &str| if cfg!(windows) { format!("{n}.exe") } else { n.to_string() };
    for n in ["Game", "UnityCrashHandler64"] {
        let p = build.join(exe(n));
        std::fs::write(&p, "x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    assert_eq!(launch(d.path(), None).map(|l| l.0), Some(build.join(exe("Game"))), "crash reporters aren't the game");
}

#[test]
fn games_hear_about_generated_assets_only_when_allowed_and_with_a_key() {
    let d = tempfile::tempdir().unwrap();
    let mut ws = Workspace::create(d.path(), "Game", "A snake game", TargetRef::None).unwrap();
    ws.manifest.stack = "Unity".into();
    assert!(!brief(&ws, "game", false, false, None, true, None).contains("assets_model_generate"), "a key alone isn't the person's choice");
    ws.manifest.generate_assets = true;
    let with = brief(&ws, "game", false, false, None, true, None);
    assert!(with.contains("assets_model_generate") && with.contains("$5.00") && with.contains("Assets/Generated/"), "{with}");
    assert!(with.contains("never a recognisable character, logo or brand artwork"));
    assert!(!brief(&ws, "game", false, false, None, false, None).contains("assets_model_generate"), "no key, no asset tools");
    assert!(!brief(&ws, "app", false, false, None, true, None).contains("assets_model_generate"), "games only");
}

#[test]
fn a_new_project_keeps_the_persons_asset_choice() {
    let d = tempfile::tempdir().unwrap();
    let settings = craft_anything_engine::Settings::default();
    for allow in [false, true] {
        let ws = project::new_project(&settings, "-", "A snake game", project::Place::In(d.path()), Some("unity"), allow, None).unwrap();
        assert_eq!(Workspace::open(&ws.root).unwrap().manifest.generate_assets, allow);
    }
}

#[test]
fn the_assets_note_reads_prices_from_the_asset_table() {
    let d = tempfile::tempdir().unwrap();
    let ws = Workspace::create(d.path(), "Game", "A snake game", TargetRef::None).unwrap();
    // The default picture model is Nano Banana Pro ($0.15 a picture).
    let before = "Realistic assets: you can make them with fal.ai. assets_model_generate makes a textured, game-ready low-poly 3D model (GLB) of one object from a description (about $0.65 each); set faces to the fewest that still look right (about 500-1500 for small props, 2000-5000 for weapons and vehicles, 6000-15000 for main characters) so the game stays fast; assets_image_generate makes a picture (about $0.15) for textures, skies, backgrounds and menu art. This project's asset budget is $5.00 ($0.00 used), so spend it on what the player sees most (the main characters, weapons, vehicles and the level's key props) and use simple shapes for the rest. Save each one inside app/ where the engine loads it (e.g. assets/generated/). Every asset is an original design in the original's style: describe what it is, its shape, materials and colours, never a recognisable character, logo or brand artwork. List each generated file in app/CREDITS.md as made with fal.ai.";
    assert_eq!(assets_note(&ws, None), before);
    assert_eq!(assets_note(&ws, Some("no-such-model")), before, "an unknown model quotes the default's prices");
    let schnell = assets_note(&ws, Some("flux-schnell"));
    assert!(schnell.contains("(about $0.50 each)") && schnell.contains("a picture (under $0.01)"), "{schnell}");
}

#[test]
fn notes_added_while_it_works_reach_the_agent_at_its_next_step() {
    let d = tempfile::tempdir().unwrap();
    let mut ws = Workspace::create(d.path(), "x", "Copy this game", TargetRef::None).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let notes: Arc<Mutex<Vec<Note>>> = Arc::default();
    let w = Worker {
        wake: Default::default(),
        engine: Arc::new(Engine::new(craft_anything_engine::testing::session())),
        tx,
        stop: CancelToken::default(),
        preview: Arc::default(),
        seen: HashSet::new(),
        inbox: Inbox::new(ws.inbox_dir()),
        editor: None,
        notes: notes.clone(),
        feed: Arc::new(Mutex::new(feed::FeedLog::open(&ws))),
        assets: false,
        decompiled: false,
        usage: None,
        self_check: None,
        reset_progress_after_notes: false,
    };
    assert!(w.added(&mut ws).is_none(), "nothing added, nothing to say");
    let note = |id, text: &str| Note { id, text: format!("{text} (pictures: a.png)"), shown: text.into(), pictures: Vec::new() };
    notes.lock().unwrap().extend([note(1, "Make it faster"), note(2, "A blue sky")]);
    let p = w.added(&mut ws).unwrap();
    assert!(p.contains("- Make it faster (pictures: a.png)\n- A blue sky"), "{p}");
    assert!(notes.lock().unwrap().is_empty(), "each note goes in once");
    // The notes join the feed as they're read, before the step that reads them.
    let events: Vec<RunEvent> = rx.try_iter().collect();
    let shown: Vec<_> = events.iter().filter_map(|e| if let RunEvent::Note { shown, .. } = e { Some(shown.as_str()) } else { None }).collect();
    assert_eq!(shown, ["Make it faster", "A blue sky"]);
    assert!(matches!(events.last(), Some(RunEvent::Step { text, .. }) if text == "Reading what you added…"));
}

#[test]
fn pictures_of_the_original_reach_the_builder() {
    let d = tempfile::tempdir().unwrap();
    let ws = Workspace::create(d.path(), "Game", "Copy this game", TargetRef::None).unwrap();
    assert!(!brief(&ws, "game", false, false, None, false, None).contains("reference/picture"));
    assert!(pictures_reminder(&ws).is_empty());
    let pic = d.path().join("shot.webp");
    std::fs::write(&pic, "x").unwrap();
    ws.add_references(&[pic]).unwrap();
    let b = brief(&ws, "game", false, false, None, false, None);
    assert!(b.contains("reference/picture-1.webp") && b.contains("--screenshot"), "{b}");
    assert!(!brief(&ws, "website", false, false, None, false, None).contains("--screenshot"), "websites are photographed for it");
    assert!(pictures_reminder(&ws).contains("1 picture(s)"));
}

mod typed_retries {
    use super::*;
    use crate::{FailureKind, TurnFailure};
    use std::collections::VecDeque;

    struct FailingConversation {
        failures: VecDeque<Option<TurnFailure>>,
        failure: Option<TurnFailure>,
        queued: Vec<AgentEvent>,
        sent: Vec<String>,
        session: Option<String>,
        message: &'static str,
    }

    impl FailingConversation {
        fn new(failures: impl IntoIterator<Item = Option<TurnFailure>>, session: bool) -> Self {
            Self {
                failures: failures.into_iter().collect(),
                failure: None,
                queued: Vec::new(),
                sent: Vec::new(),
                session: session.then(|| "saved".into()),
                message: "provider response",
            }
        }
    }

    impl Conversation for FailingConversation {
        fn send(&mut self, text: &str) {
            self.sent.push(text.into());
            self.failure = self.failures.pop_front().flatten();
            self.queued.push(if self.failure.is_some() {
                AgentEvent::Error { message: self.message.into(), remediation: None, log: None }
            } else {
                AgentEvent::TextDelta { text: "Done".into() }
            });
            self.queued.push(AgentEvent::TurnDone { backend: "scripted".into(), input_tokens: 0, output_tokens: 0, cost_usd: None, cost_is_estimate: false });
        }
        fn session_id(&self) -> Option<String> {
            self.session.clone()
        }
        fn last_failure(&self) -> Option<TurnFailure> {
            self.failure
        }
        fn poll(&mut self) -> Vec<AgentEvent> {
            std::mem::take(&mut self.queued)
        }
        fn answer_consent(&mut self, _: &str, _: bool) {}
        fn stop(&mut self) {}
        fn is_busy(&self) -> bool {
            false
        }
    }

    #[test]
    fn transient_failures_retry_without_requiring_an_external_session() {
        for saved in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mut ws = Workspace::create(dir.path(), "x", "Build a thing", TargetRef::None).unwrap();
            let mut worker = check_worker(&ws, false);
            let failure = TurnFailure { kind: FailureKind::RateLimited, retry_after_secs: Some(2) };
            let mut agent = FailingConversation::new([Some(failure), None], saved);
            agent.message = "Authentication and billing service restarting. Please retry in 90s.";
            let mut waits = Vec::new();
            let result = worker.turn_with_wait(&mut ws, &mut agent, "Build the requested app", None, |_, seconds| {
                waits.push(seconds);
                Ok(())
            });
            assert_eq!(result.unwrap(), "Done");
            assert_eq!(waits, [3], "typed provider wait overrides the text hint");
            assert_eq!(agent.sent.len(), 2);
            assert_eq!(agent.sent[0], "Build the requested app");
            assert_eq!(agent.sent[1], if saved { crate::failure::CONTINUE_TURN } else { "Build the requested app" });
        }
    }

    #[test]
    fn setup_errors_do_not_retry_even_with_a_wait_in_the_message() {
        for kind in [
            FailureKind::Authentication,
            FailureKind::Billing,
            FailureKind::MissingProgram,
            FailureKind::ToolsUnavailable,
            FailureKind::Configuration,
            FailureKind::Unknown,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut ws = Workspace::create(dir.path(), "x", "Build a thing", TargetRef::None).unwrap();
            let mut worker = check_worker(&ws, false);
            let mut agent = FailingConversation::new([Some(TurnFailure::new(kind))], true);
            agent.message = "需要修改设置。Please retry in 1s.";
            let result = worker.turn_with_wait(&mut ws, &mut agent, "Build it", None, |_, _| panic!("a setup error must not wait and retry"));
            assert!(result.is_err(), "{kind:?}");
            assert_eq!(agent.sent.len(), 1, "{kind:?}");
        }
    }

    #[test]
    fn retries_are_bounded_and_cancelled_waits_send_nothing_more() {
        assert_eq!(RunOptions::default().max_rounds, 6, "room to keep fixing until it works");
        let dir = tempfile::tempdir().unwrap();
        let mut ws = Workspace::create(dir.path(), "x", "Build a thing", TargetRef::None).unwrap();
        let mut worker = check_worker(&ws, false);
        let failure = TurnFailure::new(FailureKind::Transient);
        let mut agent = FailingConversation::new([Some(failure); 5], false);
        let mut waits = Vec::new();
        assert!(
            worker
                .turn_with_wait(&mut ws, &mut agent, "Build it", None, |_, seconds| {
                    waits.push(seconds);
                    Ok(())
                })
                .is_err()
        );
        assert_eq!(agent.sent.len(), 4, "initial attempt and at most three retries");
        assert_eq!(waits, [30, 60, 90]);
        let mut agent = FailingConversation::new([Some(failure)], false);
        let error = worker
            .turn_with_wait(&mut ws, &mut agent, "Build it", None, |stop, _| {
                stop.cancel();
                stop.check()
            })
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Cancelled);
        assert_eq!(agent.sent.len(), 1);
    }

    #[test]
    fn a_local_self_check_write_error_is_not_retried_as_a_backend_rate_limit() {
        #[derive(Clone)]
        struct BlockReply {
            answer: PathBuf,
        }
        impl Check for BlockReply {
            fn id(&self) -> &'static str {
                "blocked-reply"
            }
            fn run(&self, cx: &mut CheckCx) -> Result<CheckResult, ToolError> {
                // The request was pending when the run read it; its answer now cannot be
                // written. The workspace itself remains writable, including retry status.
                std::fs::create_dir_all(&self.answer).unwrap();
                Ok(CheckResult::new(Check::id(self), cx.round, true, "The check finished."))
            }
        }
        impl TargetKind for BlockReply {
            fn id(&self) -> &'static str {
                "blocked-reply"
            }
            fn words(&self) -> KindWords {
                SlidesKind.words()
            }
            fn detect(&self, _: &DetectCx) -> Option<u32> {
                Some(1)
            }
            fn default_stack(&self, cx: &DetectCx) -> &'static str {
                SlidesKind.default_stack(cx)
            }
            fn goal(&self) -> &'static str {
                SlidesKind.goal()
            }
            fn checks(&self, _: &Workspace) -> Vec<Box<dyn Check>> {
                vec![Box::new(self.clone())]
            }
        }
        struct RateLimited {
            sent: usize,
            first_poll: bool,
        }
        impl Conversation for RateLimited {
            fn send(&mut self, _: &str) {
                self.sent += 1;
                self.first_poll = true;
            }
            fn poll(&mut self) -> Vec<AgentEvent> {
                vec![if std::mem::take(&mut self.first_poll) {
                    AgentEvent::Error { message: "The provider is rate limited.".into(), remediation: None, log: None }
                } else {
                    AgentEvent::TurnDone { backend: "scripted".into(), input_tokens: 0, output_tokens: 0, cost_usd: None, cost_is_estimate: false }
                }]
            }
            fn last_failure(&self) -> Option<TurnFailure> {
                Some(TurnFailure::new(FailureKind::RateLimited))
            }
            fn answer_consent(&mut self, _: &str, _: bool) {}
            fn stop(&mut self) {}
            fn is_busy(&self) -> bool {
                false
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let mut ws = Workspace::create(dir.path(), "x", "Build a thing", TargetRef::None).unwrap();
        let asks = Inbox::new(self_check::asks_dir(&ws.root));
        let id = asks.post(craft_anything_engine::inbox::RequestKind::Question, "Check the build", "", Vec::new(), false, None).unwrap();
        let mut worker = check_worker(&ws, false);
        worker.self_check = Some((Arc::new(BlockReply { answer: asks.dir().join(format!("{id}.answer.json")) }), 90.0));
        let mut agent = RateLimited { sent: 0, first_poll: false };
        let mut waits = Vec::new();
        let error = worker
            .turn_with_wait(&mut ws, &mut agent, "Build the app", None, |_, seconds| {
                waits.push(seconds);
                Ok(())
            })
            .unwrap_err();
        assert_eq!(agent.sent, 1, "a backend's retry hint cannot retry a local persistence failure");
        assert!(waits.is_empty());
        assert_ne!(error.code, ErrorCode::ProgramFailed);
        assert!(error.message.contains(".answer.json"), "the original local failure is retained: {error:?}");
    }

    #[test]
    fn an_api_rate_limit_continues_history_without_repeating_users_or_completed_tools() {
        use crate::agent::tests::{Scripted, turn};
        use crate::agent::{ApiConversation, EngineHost, LoopConfig, ModelSlot};
        use crate::model::{Block, Speaker, StopReason};
        for write_first in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mut ws = Workspace::create(dir.path(), "x", "Build a notes app", TargetRef::None).unwrap();
            let mut worker = check_worker(&ws, false);
            worker.attach(&ws).unwrap();
            let (tx, rx) = std::sync::mpsc::channel();
            worker.tx = tx;
            let seen = Arc::default();
            let mut script = Scripted { turns: VecDeque::new(), seen: Arc::clone(&seen) };
            if write_first {
                script.turns.push_back(turn(
                    vec![Block::ToolUse {
                        id: "write-once".into(),
                        name: "build_file_write".into(),
                        input: json!({"path":"app/README.md","content":"Built once"}),
                    }],
                    StopReason::ToolUse,
                ));
            }
            script.turns.push_back(Err(ToolError::new(ErrorCode::OverLimit, "服务限流").with_details(json!({"status":429,"retry_after_secs":3}))));
            script.turns.push_back(turn(vec![Block::Text { text: "Built the notes app".into() }], StopReason::EndTurn));
            let slot = ModelSlot { name: "Scripted".into(), model: Box::new(script), local: false, effort: None, max_turns: 10, context_tokens: 0 };
            let enabled = std::collections::BTreeSet::from([craft_anything_engine::Toolset::Core, craft_anything_engine::Toolset::Build]);
            let mut api = ApiConversation::start_with(Arc::new(EngineHost(worker.engine.clone())), vec![slot], LoopConfig::default(), enabled).unwrap();
            let mut waits = Vec::new();
            assert_eq!(
                worker
                    .turn_with_wait(&mut ws, &mut api, "Build the notes app", None, |_, seconds| {
                        waits.push(seconds);
                        Ok(())
                    })
                    .unwrap(),
                "Built the notes app"
            );
            assert_eq!(waits, [4]);
            assert!(ws.manifest.agent_session.is_none());
            let requests = seen.lock().unwrap();
            assert_eq!(requests.len(), if write_first { 3 } else { 2 });
            let before_retry = &requests[requests.len() - 2];
            let after_retry = requests.last().unwrap();
            assert_eq!(before_retry, after_retry, "retry must continue the same model request");
            assert_eq!(after_retry.iter().filter(|m| m.role == Speaker::User && m.text() == "Build the notes app").count(), 1);
            let calls = rx.try_iter().filter(|e| matches!(e, RunEvent::Agent(AgentEvent::ToolStarted { tool, .. }) if tool == "build_file_write")).count();
            assert_eq!(calls, usize::from(write_first), "a completed write is not replayed");
            if write_first {
                assert_eq!(std::fs::read_to_string(ws.app_dir().join("README.md")).unwrap(), "Built once");
            }
        }
    }

    #[test]
    fn games_get_the_mechanics_sheet_and_decompile_only_in_deep_mode() {
        let d = tempfile::tempdir().unwrap();
        let ws = Workspace::create(d.path(), "Game", "Copy this game", TargetRef::Local { path: d.path().join("game.exe") }).unwrap();
        let deep = brief(&ws, "game", true, false, None, false, None);
        let clean = brief(&ws, "game", false, false, None, false, None);
        for b in [&deep, &clean] {
            assert!(b.contains("MECHANICS.md"), "{b}");
        }
        // Deep mode: 1:1 with the original's own files, for private use, never decrypting anything.
        assert!(
            deep.contains("original game's own files") && deep.contains("Never decrypt") && deep.contains("must not be shared") && deep.contains("CREDITS.md")
        );
        assert!(deep.contains(PRIVATE_USE_NOTE), "the README line the credits check looks for, as written");
        // It points the builder at the app's own archive tools before any outside converter.
        assert!(deep.contains("enable_toolset with \"formats\"") && deep.contains("game_pak_extract") && deep.contains("game_unreal_texture_export"));
        assert!(!clean.contains("game_pak_extract"), "Clean room copies no files out of the game");
        // Clean room copies none of the original's files: its own art, and it says Deep mode is needed.
        assert!(clean.contains("take nothing from the original game's files") && clean.contains("Make your own simple versions"), "{clean}");
        assert!(!clean.contains("original game's own files") && !clean.contains("must not be shared") && clean.contains("needs Deep mode"));
        assert!(deep.contains("native_function_pseudocode") && deep.contains("never paste decompiled code"));
        assert!(clean.contains("don't open, read, list or decompile the game's files or code") && !clean.contains("decompile the program before"));
        assert!(clean.contains("nothing inside the program, its files or its folder was read") && !clean.contains("core_target_open"), "{clean}");
        // Deep mode requires decompiling before estimating, and says to wait for Ghidra.
        assert!(deep.contains("decompile the program before you estimate anything") && deep.contains("only after you looked for it in the code"));
        // It says which decompiler it really has.
        assert!(deep.contains("Ghidra isn't installed") && deep.contains("built-in decompiler"));
        // Generated assets replace the original's art; its sounds and the rest still come from the game.
        let mut gen_ws = ws.clone();
        gen_ws.manifest.generate_assets = true;
        let generated = brief(&gen_ws, "game", true, false, None, true, None);
        assert!(generated.contains("instead of copying the original's") && generated.contains("Realistic assets"), "{generated}");
        assert!(!generated.contains("Art, sounds, models") && generated.contains("Sounds, music, fonts, animations and maps"), "{generated}");
        // Clean room with generated assets: generated art, the rest its own; still nothing copied.
        let generated = brief(&gen_ws, "game", false, false, None, true, None);
        assert!(
            generated.contains("take nothing from the original game's files") && generated.contains("with assets_model_generate and assets_image_generate"),
            "{generated}"
        );
        assert!(generated.contains("Realistic assets") && !generated.contains("original game's own files"), "{generated}");
        let with_ghidra = brief(&ws, "game", true, true, None, false, None);
        assert!(
            with_ghidra.contains("reads the code with Ghidra") && with_ghidra.contains("never skip decompiling") && !with_ghidra.contains("isn't installed")
        );
        // An app rebuilt from a program gets the same rule in Deep mode, and only then.
        let app = brief(&ws, "app", true, true, None, false, None);
        assert!(app.contains("decompile the program before you estimate anything") && app.contains("features the person asked for"), "{app}");
        assert!(!brief(&ws, "app", false, true, None, false, None).contains("decompile the program before"));
        assert!(!clean.contains("Ghidra"), "Clean room never mentions Ghidra");
        // Without a target there is nothing to decompile or license; the sheet still helps.
        let none = Workspace::create(d.path(), "Fresh", "a snake game", TargetRef::None).unwrap();
        let b = brief(&none, "game", true, true, None, false, None);
        assert!(b.contains("MECHANICS.md") && !b.contains("decompile") && !b.contains("CREDITS.md"));
        // Every game, the snake demo too, plays its core loop for the self-test check.
        for b in [&b, &deep, &clean] {
            assert!(b.contains(kinds::self_test::SELF_TEST_CONTRACT), "{b}");
        }
        assert!(!brief(&ws, "app", false, false, None, false, None).contains("MECHANICS.md"), "apps get the sheet only in Deep mode");
    }
}

/// What a person typed in the app when the clone came out in Vite without shadcn.
const SAID_NEXT_SHADCN: &str = "make a copy of this website, get everything right from padding, mobile responsiveness, colors. use nextjs and shadcn";

#[test]
fn a_named_stack_and_component_library_reach_the_builder() {
    let d = tempfile::tempdir().unwrap();
    let web = TargetRef::Website { url: "https://example.com".into() };
    let mut ws = Workspace::create(d.path(), "ex", SAID_NEXT_SHADCN, web.clone()).unwrap();
    ws.manifest.stack = choose(SAID_NEXT_SHADCN, &web).1.into();
    let b = brief(&ws, "website", false, false, None, false, None);
    assert!(b.contains("Use Next.js") && b.contains("output: 'export'") && b.contains("app/out") && !b.contains("Vite"), "{b}");
    assert!(b.contains("shadcn/ui components") && b.contains("npx shadcn@latest init --yes --defaults"), "{b}");
    // Without the library in the request, no shadcn; with a stack it doesn't fit, none either.
    ws.manifest.request = "Clone this website in Next.js".into();
    assert!(!brief(&ws, "website", false, false, None, false, None).contains("shadcn"));
    ws.manifest.request = "A tool in python with shadcn".into();
    ws.manifest.stack = "Python".into();
    assert!(!brief(&ws, "app", false, false, None, false, None).contains("shadcn@latest"));
}

#[test]
fn prompts_carry_the_request_rules_and_measurements() {
    let d = tempfile::tempdir().unwrap();
    let mut ws = Workspace::create(d.path(), "ex", "Clone this website", TargetRef::Website { url: "https://example.com".into() }).unwrap();
    ws.manifest.stack = "React + TypeScript (Vite)".into();
    let b = brief(&ws, "website", false, false, None, false, Some("make it blue"));
    assert!(b.contains("Clone this website") && b.contains("https://example.com") && b.contains("core_person_ask") && b.contains("npm run build"));
    assert!(b.contains("make it blue") && b.contains("Never start dev servers"));
    // A part of the request that won't fit is asked about, never dropped silently.
    assert!(b.contains("before leaving it out") && b.contains("never drop it silently"));
    let r = refine("website", &CheckResult { similarity: Some(71.4), ..CheckResult::new(CheckResult::WEB_VISUAL, 2, true, "1440 px: 70% alike") });
    assert!(r.contains("71% alike") && r.contains("checks/round-2/"));
    assert!(follow_up("bigger title", "app").contains("build it again"));
}

#[test]
fn needs_an_agent_app() {
    let engine = Arc::new(Engine::new(craft_anything_engine::testing::session()));
    let d = tempfile::tempdir().unwrap();
    let hub = AiHub::with_stores(engine, d.path().join("settings.toml"), crate::KeyStore::file_only(d.path().join("keys.toml")));
    let ws = Workspace::create(d.path(), "x", "y", TargetRef::None).unwrap();
    let err = RebuildRun::start(&hub, ws, RunOptions::default()).err().unwrap();
    assert!(err.message.contains("coding agent"), "{}", err.message);
}

/// A fork's own kind: recognised from its words, with its own stack and check.
struct SlidesKind;
struct OutlineCheck;

impl Check for OutlineCheck {
    fn id(&self) -> &'static str {
        "outline"
    }
    fn run(&self, cx: &mut CheckCx) -> Result<CheckResult, ToolError> {
        let ok = cx.ws.app_dir().join("slides.md").is_file();
        Ok(CheckResult::new(self.id(), cx.round, ok, if ok { "Slides found." } else { "No slides.md yet." }))
    }
}

impl TargetKind for SlidesKind {
    fn id(&self) -> &'static str {
        "slides"
    }
    fn words(&self) -> KindWords {
        KindWords { button: "Open the slides", icon: "presentation", ready: "Your slides are ready", change_hint: "Ask for a change" }
    }
    fn detect(&self, cx: &DetectCx) -> Option<u32> {
        cx.says(&[" slides", " deck"]).then_some(80)
    }
    fn default_stack(&self, _cx: &DetectCx) -> &'static str {
        "Markdown slides"
    }
    fn goal(&self) -> &'static str {
        "a slide deck"
    }
    fn checks(&self, _ws: &Workspace) -> Vec<Box<dyn Check>> {
        vec![Box::new(OutlineCheck)]
    }
}

#[test]
fn a_registered_kind_is_chosen_from_its_words_and_brings_its_checks() {
    let mut kinds = KindRegistry::built_in();
    assert_eq!(kinds.ids(), ["website", "game", "app"]);
    kinds.register(Arc::new(SlidesKind));
    let (k, stack) = kinds.choose("Make slides about our product", &TargetRef::None, true).unwrap();
    assert_eq!((k.id(), stack), ("slides", "Markdown slides"));
    assert_eq!(kinds.choose("Make a website about our product", &TargetRef::None, true).unwrap().0.id(), "website", "built-ins still win their own words");
    assert_eq!(kinds.choose("A notes tool", &TargetRef::None, true).unwrap().0.id(), "app", "app is still the fallback");
    // Registering the same id again replaces it.
    kinds.register(Arc::new(SlidesKind));
    assert_eq!(kinds.ids(), ["website", "game", "app", "slides"]);

    let d = tempfile::tempdir().unwrap();
    let ws = Workspace::create(d.path(), "Deck", "Make slides", TargetRef::None).unwrap();
    let checks = kinds.get("slides").unwrap().checks(&ws);
    assert_eq!(checks.iter().map(|c| c.id()).collect::<Vec<_>>(), ["outline"]);
    let b = super::brief(&SlidesKind, &BriefCx { ws: &ws, deep: false, ghidra: false, editor: None, assets: false, image_model: None }, None);
    assert!(b.contains("What to build: a slide deck."), "{b}");
}

#[test]
fn the_brief_puts_the_request_before_the_default_stack() {
    let d = tempfile::tempdir().unwrap();
    for (k, stack) in [("app", "Rust + egui (eframe)"), ("game", "Unity"), ("website", stacks::BUILDERS_CHOICE)] {
        let mut ws = Workspace::create(d.path(), k, "A music player in Tauri", TargetRef::None).unwrap();
        ws.manifest.stack = stack.into();
        let b = brief(&ws, k, false, false, Some(Path::new("/ed/Unity")), false, None);
        let at = |s: &str| b.find(s).unwrap_or_else(|| panic!("{k}: no {s:?} in {b}"));
        assert!(at("Their request: \"A music player in Tauri\"") < at("How to build it, unless their request says otherwise: "), "{k}");
        assert!(b.contains("Their request comes first. When it says how to build"), "{k}");
        assert!(!b.contains("npm run build` again"), "{k}");
    }
}

#[test]
fn an_electron_app_starts_in_its_own_electron_and_is_no_website() {
    let d = tempfile::tempdir().unwrap();
    let app = d.path();
    std::fs::write(app.join("package.json"), r#"{"main": "main.js", "devDependencies": {"electron": "38", "vite": "7"}}"#).unwrap();
    std::fs::create_dir_all(app.join("dist")).unwrap();
    std::fs::write(app.join("dist/index.html"), "x").unwrap();
    assert!(site_dir(app).is_none(), "its bundled page is its window, not a site");
    assert!(launch(app, None).is_none(), "nothing to start before npm install");
    let dist = app.join("node_modules/electron/dist");
    let exe = if cfg!(windows) {
        dist.join("electron.exe")
    } else if cfg!(target_os = "macos") {
        dist.join("Electron.app/Contents/MacOS/Electron")
    } else {
        dist.join("electron")
    };
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    std::fs::write(&exe, "").unwrap();
    assert_eq!(launch(app, None), Some((exe, vec![app.display().to_string()])));
    assert_eq!(stacks::used_in(app).map(|s| s.name.as_str()), Some("Electron"), "recorded as Electron, not Vite");
    std::fs::write(app.join("package.json"), "not json").unwrap();
    assert!(launch(app, None).is_none() && site_dir(app).is_some(), "a broken package.json is no Electron app");
}

#[test]
fn website_kind_compares_only_when_there_is_an_original() {
    let d = tempfile::tempdir().unwrap();
    let site = Workspace::create(d.path(), "Site", "Clone it", TargetRef::Website { url: "https://example.com".into() }).unwrap();
    let scratch = Workspace::create(d.path(), "Page", "A landing page", TargetRef::None).unwrap();
    let ids = |ws: &Workspace| kinds::WebsiteKind.checks(ws).iter().map(|c| c.id()).collect::<Vec<_>>();
    assert_eq!(ids(&site), [CheckResult::WEB_VISUAL]);
    assert_eq!(ids(&scratch), [CheckResult::BUILD]);
}

/// #494: the website check, run for real with no site to look at, fails with a note that names the
/// places a site is accepted and where the app is: `app/`, or the project folder for a project
/// that edits its own folder.
#[test]
fn the_website_check_with_no_site_says_where_it_looked() {
    let d = tempfile::tempdir().unwrap();
    for in_root in [false, true] {
        let mut ws = Workspace::create(
            &d.path().join(if in_root { "root" } else { "app" }),
            "Site",
            "Clone it",
            TargetRef::Website { url: "https://example.com".into() },
        )
        .unwrap();
        ws.manifest.app_in_root = in_root;
        let place = if in_root { "the project folder" } else { "app/" };
        let r = check_worker(&ws, false).run_checks(&ws, &kinds::WebsiteKind, 1).unwrap();
        assert_eq!(r.len(), 1);
        assert!(!r[0].passed && r[0].check == CheckResult::WEB_VISUAL, "{:?}", r[0]);
        let note = &r[0].note;
        assert!(note.contains(&format!("No built site found in {place} yet")), "{note}");
        assert!(note.contains(&format!("index.html straight in {place}")) && note.contains("dist/, build/ or out/"), "{note}");
        assert!(in_root || !note.contains("project folder"), "{note}");
    }
}

#[test]
fn a_picked_game_is_rebuilt_with_its_own_engine_when_we_can() {
    let d = tempfile::tempdir().unwrap();
    let game = d.path().join("Arena");
    std::fs::create_dir_all(game.join("Content").join("Paks")).unwrap();
    std::fs::write(game.join("Content").join("Paks").join("Arena-Windows.pak"), b"pak").unwrap();
    let target = TargetRef::Local { path: game.clone() };
    assert_eq!(choose("Recreate this game", &target), ("game", "Unreal Engine"), "an Unreal game is rebuilt in Unreal");
    assert_eq!(choose("Recreate this game in Godot", &target).1, "Godot", "an engine the person names still wins");
    let empty = tempfile::tempdir().unwrap();
    assert_eq!(choose("Recreate this game", &TargetRef::Local { path: empty.path().to_path_buf() }).1, "Unity", "no engine found: Unity");
}

/// A scripted API model builds with the Build tools in one turn, and leaves no session to resume.
#[test]
fn an_api_backend_builds_with_the_build_tools() {
    use crate::agent::tests::{Scripted, turn};
    use crate::agent::{ApiConversation, EngineHost, LoopConfig, ModelSlot};
    use crate::model::{Block, StopReason};
    let d = tempfile::tempdir().unwrap();
    let mut ws = Workspace::create(d.path(), "Notes", "A notes app", TargetRef::None).unwrap();
    let engine = Arc::new(Engine::new(craft_anything_engine::testing::session()));
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut w = Worker {
        wake: Default::default(),
        engine: engine.clone(),
        tx,
        stop: CancelToken::default(),
        preview: Arc::default(),
        seen: HashSet::new(),
        inbox: Inbox::new(ws.inbox_dir()),
        editor: None,
        notes: Arc::default(),
        feed: Arc::new(Mutex::new(feed::FeedLog::open(&ws))),
        assets: false,
        decompiled: false,
        usage: None,
        self_check: None,
        reset_progress_after_notes: false,
    };
    w.attach(&ws).unwrap();
    // The model writes a file with the Build tools, then answers.
    let write = Block::ToolUse { id: "w".into(), name: "build_file_write".into(), input: serde_json::json!({"path": "app/README.md", "content": "# Notes\n"}) };
    let script = Scripted {
        turns: std::collections::VecDeque::from([
            turn(vec![write], StopReason::ToolUse),
            turn(vec![Block::Text { text: "Built the notes app.".into() }], StopReason::EndTurn),
        ]),
        seen: Arc::default(),
    };
    let slot = ModelSlot { name: "Scripted".into(), model: Box::new(script), local: false, effort: None, max_turns: 10, context_tokens: 0 };
    let enabled = std::collections::BTreeSet::from([craft_anything_engine::Toolset::Core, craft_anything_engine::Toolset::Build]);
    let mut api = ApiConversation::start_with(Arc::new(EngineHost(engine)), vec![slot], LoopConfig::default(), enabled).unwrap();
    let reply = w.turn(&mut ws, &mut api, "Build it.", None).unwrap();
    assert_eq!(reply.trim(), "Built the notes app.");
    assert_eq!(std::fs::read_to_string(ws.app_dir().join("README.md")).unwrap(), "# Notes\n");
    assert!(ws.manifest.agent_session.is_none(), "an API loop has no session to resume");
}

#[test]
fn the_builder_is_an_agent_app_first_then_any_ai_backend() {
    let none = crate::AiConfig::default();
    assert!(builder_backend(&none, None).unwrap_err().message.contains("API key"));
}

#[test]
fn other_pages_of_the_same_site_are_picked_from_its_links() {
    use kinds::website::{ROUTES_FILE, pick_routes, routes};
    let links = serde_json::json!({"links": [
        {"href": "https://shop.example.com/pricing", "text": "Pricing"},
        {"href": "https://shop.example.com/pricing/#plans", "text": "Plans"},
        {"href": "https://SHOP.example.com/about?ref=nav", "text": "About"},
        {"href": "https://shop.example.com/", "text": "Home"},
        {"href": "https://shop.example.com/brochure.pdf", "text": "PDF"},
        {"href": "https://other.example.com/blog", "text": "Elsewhere"},
        {"href": "mailto:hi@example.com", "text": "Mail"},
        {"href": "https://shop.example.com/docs/getting-started.html", "text": "Docs"},
        {"href": "https://shop.example.com/team", "text": "Team"},
        {"href": "https://shop.example.com/jobs", "text": "Jobs"},
    ]});
    let r = pick_routes(&links, "https://shop.example.com/", 4);
    let paths: Vec<&str> = r.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(paths, ["/pricing", "/about", "/docs/getting-started.html", "/team"], "same site, pages only, each once, at most 4");
    assert_eq!(r[2].slug, "docs-getting-started-html");
    assert_eq!(r[1].url, "https://shop.example.com/about");
    assert!(pick_routes(&links, "not a url", 4).is_empty());

    // The brief mentions the other pages only when some were captured.
    let d = tempfile::tempdir().unwrap();
    let ws = Workspace::create(d.path(), "Shop", "Clone it", TargetRef::Website { url: "https://shop.example.com/".into() }).unwrap();
    let cx = |ws| BriefCx { ws, deep: false, ghidra: false, editor: None, assets: false, image_model: None };
    assert!(kinds::WebsiteKind.brief_sections(&cx(&ws)).is_empty());
    std::fs::create_dir_all(ws.capture_dir()).unwrap();
    std::fs::write(ws.capture_dir().join(ROUTES_FILE), serde_json::to_vec(&r).unwrap()).unwrap();
    assert_eq!(routes(&ws).len(), 4);
    let s = kinds::WebsiteKind.brief_sections(&cx(&ws)).join(" ");
    assert!(s.contains("/pricing (capture/routes/pricing/)"), "{s}");

    // Opened menus, pop-ups and tabs are listed when the study pictured some.
    let states = ws.capture_dir().join(kinds::website::STATES_DIR);
    std::fs::create_dir_all(&states).unwrap();
    let list = json!({"states": [
        {"kind": "menu", "label": "Products", "file": "state-1.png"},
        {"kind": "dialog", "label": "Sign up", "changed": ["address changed"]}
    ]});
    std::fs::write(states.join("states.json"), serde_json::to_vec(&list).unwrap()).unwrap();
    let s = kinds::WebsiteKind.brief_sections(&cx(&ws)).join(" ");
    assert!(s.contains("state-1.png (Products, menu)") && !s.contains("Sign up"), "only pictured states: {s}");
}

/// Only the mechanics check, so a test needn't build anything first.
struct SheetKind;

impl TargetKind for SheetKind {
    fn id(&self) -> &'static str {
        "sheet"
    }
    fn words(&self) -> KindWords {
        KindWords { button: "Play", icon: "play", ready: "Ready", change_hint: "Ask for a change" }
    }
    fn detect(&self, _cx: &DetectCx) -> Option<u32> {
        None
    }
    fn default_stack(&self, _cx: &DetectCx) -> &'static str {
        "Rust"
    }
    fn goal(&self) -> &'static str {
        "a game"
    }
    fn checks(&self, _ws: &Workspace) -> Vec<Box<dyn Check>> {
        vec![Box::new(MechanicsCheck)]
    }
}

#[test]
fn the_mechanics_sheet_is_required_and_held_against_the_code() {
    let d = tempfile::tempdir().unwrap();
    let ws = Workspace::create(d.path(), "Arena", "Recreate this game", TargetRef::None).unwrap();
    let engine = Arc::new(Engine::new(craft_anything_engine::testing::session()));
    let (tx, _rx) = std::sync::mpsc::channel();
    let w = Worker {
        wake: Default::default(),
        engine,
        tx,
        stop: CancelToken::default(),
        preview: Arc::default(),
        seen: HashSet::new(),
        inbox: Inbox::new(ws.inbox_dir()),
        editor: None,
        notes: Arc::default(),
        feed: Arc::new(Mutex::new(feed::FeedLog::open(&ws))),
        assets: false,
        decompiled: false,
        usage: None,
        self_check: None,
        reset_progress_after_notes: false,
    };
    let check = |round| w.run_checks(&ws, &SheetKind, round).unwrap().remove(0);

    let none = check(1);
    assert!(!none.passed && none.note.contains("no app/mechanics.json") && none.note.contains("\"version\": 1"), "{}", none.note);

    std::fs::create_dir_all(ws.app_dir().join("src")).unwrap();
    std::fs::write(ws.mechanics_path(), "{ not json").unwrap();
    assert!(!check(2).passed, "an unreadable sheet fails");

    std::fs::write(ws.app_dir().join("src/main.rs"), "const WALK: f32 = 4.5;\nconst GRAVITY: f32 = -9.8;\n").unwrap();
    std::fs::write(
        ws.mechanics_path(),
        r#"{"version": 1, "values": [
            {"name": "walk speed", "value": 4.5, "unit": "m/s", "source": "function 0x140012a0"},
            {"name": "gravity", "value": -9.8, "estimated": true},
            {"name": "jump height", "value": 1.2, "unit": "m", "estimated": true},
            {"name": "crouch", "value": "hold"}
        ]}"#,
    )
    .unwrap();
    let ok = check(3);
    assert!(ok.passed, "an estimate may stay out of the code: {}", ok.note);
    assert!(ok.note.contains("4 values (1 with a source, 2 estimated)"), "{}", ok.note);
    assert!(ok.note.contains("neither a source nor \"estimated\": crouch."), "{}", ok.note);
    assert!(!ok.note.contains("aren't in the code"), "{}", ok.note);
    let claimed: Vec<(&str, bool)> = ok.claims.iter().map(|c| (c.title.as_str(), c.passed)).collect();
    assert_eq!(claimed, [("walk speed (4.5) is used in the code", true)], "estimates and words make no claim");

    // A number found in the original but not used in the code fails, and says what to do.
    std::fs::write(
        ws.mechanics_path(),
        r#"{"version": 1, "values": [
            {"name": "walk speed", "value": 4.5, "source": "function 0x140012a0"},
            {"name": "grunt speed", "value": 290, "unit": "cm/s", "source": "pak: ENE_Spider_Grunt_Base"},
            {"name": "guard health", "value": 270, "source": "pak: ENE_Spider_Grunt_Guard"}
        ]}"#,
    )
    .unwrap();
    let unused = check(4);
    assert!(!unused.passed, "{}", unused.note);
    assert!(unused.note.contains("2 numbers from the original aren't in the code as written: grunt speed (290), guard health (270)."), "{}", unused.note);
    assert!(unused.note.contains("mark it \"estimated\""), "{}", unused.note);
    let failed: Vec<&str> = unused.failed_claims().map(|c| c.title.as_str()).collect();
    assert_eq!(failed, ["grunt speed (290) is used in the code", "guard health (270) is used in the code"]);
}

#[test]
fn clean_room_studies_nothing_inside_and_deep_mode_looks() {
    let d = tempfile::tempdir().unwrap();
    let game = d.path().join("Some Game");
    std::fs::create_dir_all(game.join("Game_Data")).unwrap();
    std::fs::write(game.join("Game.exe"), b"MZ").unwrap();
    std::fs::write(game.join("Game_Data/globalgamemanagers"), b"\0\0\0\0\x16\0\x002022.3.62f2\0").unwrap();
    for (deep, name) in [(false, "clean"), (true, "deep")] {
        let mut ws = Workspace::create(&d.path().join("projects"), name, "Recreate this game", TargetRef::Local { path: game.clone() }).unwrap();
        let mut w = check_worker(&ws, deep);
        w.study(&mut ws, kind("game").as_ref()).unwrap();
        let notes = std::fs::read_to_string(ws.capture_dir().join("README.md")).unwrap();
        let files: Vec<_> = std::fs::read_dir(ws.capture_dir()).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        if deep {
            assert!(notes.contains("# What we found") && notes.contains("Unity"), "Deep mode looks inside: {notes}");
        } else {
            assert!(notes.starts_with("# Clean room") && !notes.contains("Unity") && !notes.contains("Game_Data"), "{notes}");
            assert_eq!(files, ["README.md"], "nothing else was written from the original");
        }
    }
}

#[cfg(unix)]
mod picking_up_a_build {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A kind whose check can't run while `app/check-fails` still counts down, and a stand-in coding
    /// agent that notes each prompt it gets in `prompts.txt` beside the project.
    struct FlakyKind;
    struct FlakyCheck;

    impl Check for FlakyCheck {
        fn id(&self) -> &'static str {
            "flaky"
        }
        fn run(&self, cx: &mut CheckCx) -> Result<CheckResult, ToolError> {
            let rejection = std::fs::read_to_string(cx.ws.root.join("reject-build")).unwrap_or_default();
            if !rejection.is_empty() && (rejection != "once" || cx.ws.manifest.checks.is_empty()) {
                return Ok(CheckResult::new(self.id(), cx.round, false, "The app is still missing."));
            }
            let file = cx.ws.app_dir().join("check-fails");
            let left: u32 = std::fs::read_to_string(&file).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
            if left > 0 {
                std::fs::write(&file, (left - 1).to_string()).unwrap();
                return Err(ToolError::new(ErrorCode::EngineMissing, "the WebSocket is closed"));
            }
            Ok(CheckResult::new(self.id(), cx.round, true, "Fine."))
        }
    }

    impl TargetKind for FlakyKind {
        fn id(&self) -> &'static str {
            "flaky"
        }
        fn words(&self) -> KindWords {
            KindWords { button: "Open", icon: "box", ready: "Ready", change_hint: "Ask" }
        }
        fn detect(&self, _cx: &DetectCx) -> Option<u32> {
            Some(1000)
        }
        fn default_stack(&self, _cx: &DetectCx) -> &'static str {
            "Anything"
        }
        fn goal(&self) -> &'static str {
            "a thing"
        }
        fn checks(&self, _ws: &Workspace) -> Vec<Box<dyn Check>> {
            vec![Box::new(FlakyCheck)]
        }
    }

    struct Bench {
        _dir: tempfile::TempDir,
        hub: AiHub,
        projects: PathBuf,
        prompts: PathBuf,
    }

    fn bench() -> Bench {
        let dir = tempfile::tempdir().unwrap();
        let prompts = dir.path().join("prompts.txt");
        let agent = dir.path().join("agent.sh");
        std::fs::write(&agent, format!("#!/bin/sh\nfor last; do :; done\nprintf '%s\\n===\\n' \"$last\" >> '{}'\necho Done.\n", prompts.display())).unwrap();
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
        let engine = Arc::new(Engine::new(craft_anything_engine::testing::session()));
        let hub = AiHub::with_stores(engine, dir.path().join("settings.toml"), crate::KeyStore::file_only(dir.path().join("keys.toml")));
        let mut cfg = hub.config();
        cfg.upsert(crate::BackendEntry {
            config: crate::BackendConfig {
                name: "Stand-in".into(),
                kind: BackendKind::Nvidia,
                model: "stand-in".into(),
                base_url: None,
                ssh_host: None,
                enabled: true,
            },
            options: crate::BackendOptions { command: Some(agent.display().to_string()), ..Default::default() },
        });
        hub.engine.session().settings.ai = cfg.to_table().unwrap();
        let projects = dir.path().join("projects");
        std::fs::create_dir_all(&projects).unwrap();
        Bench { _dir: dir, hub, projects, prompts }
    }

    fn try_it(b: &Bench, ws: &Workspace) -> (bool, Vec<feed::FeedItem>) {
        try_with(b, ws, RunOptions::default())
    }

    fn try_with(b: &Bench, ws: &Workspace, mut opts: RunOptions) -> (bool, Vec<feed::FeedItem>) {
        opts.kinds.register(Arc::new(FlakyKind));
        opts.backend = Some("Stand-in".into());
        let run = RebuildRun::start(&b.hub, Workspace::open(&ws.root).unwrap(), opts).unwrap();
        let began = Instant::now();
        let mut ok = None;
        while ok.is_none() {
            assert!(began.elapsed() < Duration::from_secs(30), "the run never finished");
            match run.rx.recv_timeout(Duration::from_secs(30)) {
                Ok(RunEvent::Finished { ok: o, .. }) => ok = Some(o),
                Ok(_) => {}
                Err(e) => panic!("the run went away: {e}"),
            }
        }
        (ok.unwrap(), feed::load(&Workspace::open(&ws.root).unwrap()))
    }

    fn prompts_given(b: &Bench) -> usize {
        std::fs::read_to_string(&b.prompts).unwrap_or_default().matches("===").count()
    }

    fn studied(feed: &[feed::FeedItem]) -> bool {
        feed.iter().any(|i| matches!(i, feed::FeedItem::Step { step: Step::Study, .. }))
    }

    /// A project whose builder has worked: its session and its study are there.
    fn worked_on(b: &Bench) -> Workspace {
        let mut ws = Workspace::create(&b.projects, "x", "A thing", TargetRef::None).unwrap();
        ws.manifest.agent_session = Some("session-1".into());
        ws.manifest.stack = "Anything".into();
        ws.save().unwrap();
        std::fs::create_dir_all(ws.capture_dir()).unwrap();
        std::fs::create_dir_all(ws.app_dir()).unwrap();
        ws
    }

    /// #91: with the helper missing, the build stops before studying or asking the agent, and
    /// says why.

    #[test]
    fn try_again_after_a_check_that_could_not_run_only_checks() {
        let b = bench();
        let mut ws = worked_on(&b);
        ws.manifest.awaiting_check = true;
        ws.save().unwrap();
        let (ok, feed) = try_it(&b, &ws);
        assert!(ok, "{feed:?}");
        assert_eq!(prompts_given(&b), 0, "the builder had finished: it isn't asked again");
        assert!(!studied(&feed), "the original isn't studied again");
        let ws = Workspace::open(&ws.root).unwrap();
        assert!(!ws.manifest.awaiting_check);
        assert_eq!(ws.manifest.checks.len(), 1);
    }

    #[test]
    fn try_again_after_the_builder_stopped_carries_on_instead_of_starting_over() {
        let b = bench();
        let ws = worked_on(&b);
        let (ok, feed) = try_it(&b, &ws);
        assert!(ok, "{feed:?}");
        assert!(!studied(&feed), "the original isn't studied again");
        let told = std::fs::read_to_string(&b.prompts).unwrap();
        assert!(told.contains("Pick up where you left off"), "{told}");
        assert!(!told.contains("Their request"), "not the first-build brief again: {told}");
    }

    #[test]
    fn unchanged_builder_turns_stop_after_two_and_stay_stopped_when_reopened() {
        let b = bench();
        let ws = worked_on(&b);
        std::fs::write(ws.root.join("reject-build"), "not ready").unwrap();
        let (ok, feed) = try_it(&b, &ws);
        assert!(!ok, "{feed:?}");
        assert_eq!(prompts_given(&b), 2, "a third unchanged turn would spend more without progress");
        assert!(
            feed.iter().any(|i| matches!(i, feed::FeedItem::Error { message, .. } if message.contains("two builder turns"))),
            "the stop explains what happened: {feed:?}"
        );
        let reopened = Workspace::open(&ws.root).unwrap();
        let (ok, _) = try_it(&b, &reopened);
        assert!(!ok);
        assert_eq!(prompts_given(&b), 2, "Try again must keep the saved progress limit");
    }

    #[test]
    fn single_turn_runs_share_the_same_unchanged_turn_limit() {
        let b = bench();
        let ws = worked_on(&b);
        std::fs::write(ws.root.join("reject-build"), "not ready").unwrap();
        for expected in [1, 2, 2] {
            let (ok, _) = try_with(&b, &ws, RunOptions { max_rounds: 0, ..RunOptions::default() });
            assert!(!ok);
            assert_eq!(prompts_given(&b), expected);
        }
    }

    #[test]
    fn a_real_edit_or_nonempty_new_request_starts_another_attempt() {
        let b = bench();
        let ws = worked_on(&b);
        std::fs::write(ws.root.join("reject-build"), "not ready").unwrap();
        assert!(!try_it(&b, &ws).0);
        assert_eq!(prompts_given(&b), 2);
        std::fs::write(ws.app_dir().join("index.html"), "a new starting point").unwrap();
        assert!(!try_it(&b, &ws).0);
        assert_eq!(prompts_given(&b), 4, "a real edit reset the previous limit");
        let opts = RunOptions { message: Some("Use the new index.html and add a working button.".into()), ..RunOptions::default() };
        assert!(!try_with(&b, &ws, opts).0);
        assert_eq!(prompts_given(&b), 6, "a new request starts another attempt");
        let empty = RunOptions { message: Some("   ".into()), ..RunOptions::default() };
        assert!(!try_with(&b, &ws, empty).0);
        assert_eq!(prompts_given(&b), 6, "an empty message cannot erase the saved count");
    }

    #[test]
    fn a_successful_check_can_finish_the_second_unchanged_turn() {
        let b = bench();
        let ws = worked_on(&b);
        std::fs::write(ws.root.join("reject-build"), "once").unwrap();
        assert!(try_it(&b, &ws).0);
        assert_eq!(prompts_given(&b), 2);
        assert_eq!(BuildProgress::load(&ws.root).unwrap().unchanged_turns, 2);
    }

    #[test]
    fn a_saved_check_only_run_is_not_blocked_by_the_progress_limit() {
        let b = bench();
        let mut ws = worked_on(&b);
        ws.manifest.awaiting_check = true;
        ws.save().unwrap();
        let mut progress = BuildProgress::load(&ws.root).unwrap();
        progress.before_turn(ws.app_fingerprint(&CancelToken::default()).unwrap());
        progress.unchanged_turns = 2;
        progress.save(&ws.root).unwrap();
        assert!(try_it(&b, &ws).0);
        assert_eq!(prompts_given(&b), 0);
    }

    #[test]
    fn a_failed_check_only_resume_detects_manual_edits_before_refusing_more_build_turns() {
        for edited in [false, true] {
            let b = bench();
            let mut ws = worked_on(&b);
            std::fs::write(ws.root.join("reject-build"), "not ready").unwrap();
            ws.manifest.awaiting_check = true;
            ws.save().unwrap();
            let mut progress = BuildProgress::load(&ws.root).unwrap();
            progress.before_turn(ws.app_fingerprint(&CancelToken::default()).unwrap());
            progress.unchanged_turns = 2;
            progress.save(&ws.root).unwrap();
            if edited {
                std::fs::write(ws.app_dir().join("index.html"), "a manual correction").unwrap();
            }
            assert!(!try_it(&b, &ws).0);
            assert_eq!(prompts_given(&b), if edited { 2 } else { 0 }, "edited={edited}");
            assert_eq!(BuildProgress::load(&ws.root).unwrap().unchanged_turns, 2);
        }
    }

    #[test]
    fn scan_io_errors_skip_only_the_guard_and_cancellation_is_never_skipped() {
        for fail_scan_first in [false, true] {
            let b = bench();
            let ws = worked_on(&b);
            let worker = check_worker(&ws, false);
            let mut progress = BuildProgress::default();
            progress.before_turn(ws.app_fingerprint(&CancelToken::default()).unwrap());
            progress.unchanged_turns = 2;
            progress.save(&ws.root).unwrap();
            let mut available = true;
            let mut fingerprints = AppFingerprintCache::default();
            if fail_scan_first {
                // A deterministic read_dir I/O failure, including when tests run as root.
                std::fs::remove_dir(ws.app_dir()).unwrap();
                std::fs::write(ws.app_dir(), "the app directory is temporarily unavailable").unwrap();
                assert!(worker.progress_fingerprint(&ws, &mut progress, &mut available, &mut fingerprints).unwrap().is_none());
                assert!(!available);
                assert_eq!(progress, BuildProgress::default());
                assert_eq!(BuildProgress::load(&ws.root).unwrap(), progress);
                std::fs::remove_file(ws.app_dir()).unwrap();
                std::fs::create_dir(ws.app_dir()).unwrap();
                assert!(
                    worker.progress_fingerprint(&ws, &mut progress, &mut available, &mut fingerprints).unwrap().is_none(),
                    "a recovered directory does not resume comparisons within the same run"
                );
            }
            let before = progress.clone();
            worker.stop.cancel();
            assert_eq!(worker.progress_fingerprint(&ws, &mut progress, &mut available, &mut fingerprints).unwrap_err().code, ErrorCode::Cancelled);
            assert_eq!(progress, before, "cancellation does not erase a known comparison");
            assert_eq!(BuildProgress::load(&ws.root).unwrap(), before);
        }
    }

    #[test]
    fn a_skipped_scan_does_not_hide_the_builders_own_failure() {
        let b = bench();
        let ws = worked_on(&b);
        let nested = (0..65).fold(ws.app_dir().join("nested"), |p, _| p.join("d"));
        std::fs::create_dir_all(nested).unwrap();
        // Authentication is a typed provider failure. A plain exit 1 is transient, regardless
        // of the English words it prints, and is covered by the retry-policy tests.
        let mut config = b.hub.config();
        let mut entry = config.get("Stand-in").unwrap().clone();
        entry.config.model = "opencode".into();
        config.upsert(entry);
        b.hub.engine.session().settings.ai = config.to_table().unwrap();
        let agent = b._dir.path().join("agent.sh");
        let script = std::fs::read_to_string(&agent).unwrap();
        let script = script.replacen("#!/bin/sh\n", "#!/bin/sh\nif [ \"$2\" = '--help' ]; then printf '%s\\n' '--format'; exit 0; fi\n", 1);
        let error = json!({"type":"error","error":{"name":"APIError","data":{"message":"fixture sign in needed","statusCode":401,"isRetryable":false}}});
        std::fs::write(&agent, script.replace("echo Done.", &format!("printf '%s\\n' '{error}'\nexit 1"))).unwrap();
        let (ok, feed) = try_it(&b, &ws);
        assert!(!ok);
        assert_eq!(prompts_given(&b), 1, "the unavailable scan does not block or repeat the builder");
        assert!(feed.iter().any(|i| matches!(i, feed::FeedItem::Error { message, .. } if message.contains("fixture sign in needed"))), "{feed:?}");
        let saved = Workspace::open(&ws.root).unwrap();
        assert!(!saved.manifest.awaiting_check, "a failed turn has not finished its build");
        assert!(saved.manifest.checks.is_empty(), "the failed builder is not described as a successful check");
    }

    #[test]
    fn scan_limits_do_not_block_builds_or_keep_a_stale_stall_count() {
        for only_check in [false, true] {
            let b = bench();
            let mut ws = worked_on(&b);
            let mut progress = BuildProgress::default();
            progress.before_turn(ws.app_fingerprint(&CancelToken::default()).unwrap());
            progress.unchanged_turns = 2;
            progress.save(&ws.root).unwrap();
            ws.manifest.awaiting_check = only_check;
            ws.save().unwrap();
            let nested = (0..65).fold(ws.app_dir().join("nested"), |p, _| p.join("d"));
            std::fs::create_dir_all(&nested).unwrap();
            assert_eq!(ws.app_fingerprint(&CancelToken::default()).unwrap_err().code, ErrorCode::OverLimit);
            std::fs::write(ws.root.join("reject-build"), "once").unwrap();
            let (ok, feed) = try_it(&b, &ws);
            assert!(ok, "a scan limit must not fail the build (only_check={only_check}): {feed:?}");
            assert_eq!(prompts_given(&b), if only_check { 1 } else { 2 });
            assert_eq!(BuildProgress::load(&ws.root).unwrap(), BuildProgress::default(), "an unknown comparison breaks the saved streak");
            let notices = feed
                .iter()
                .filter_map(|i| match i {
                    feed::FeedItem::Work { notes, .. } => Some(notes.iter().filter(|n| n.contains("without the unchanged-turn limit")).count()),
                    _ => None,
                })
                .sum::<usize>();
            assert_eq!(notices, 1, "one notice explains that this run skipped the unavailable guard");
            assert_eq!(Workspace::open(&ws.root).unwrap().manifest.status, Status::Ready);
            std::fs::remove_dir_all(ws.app_dir().join("nested")).unwrap();
            assert!(try_it(&b, &ws).0);
            assert_eq!(BuildProgress::load(&ws.root).unwrap().unchanged_turns, 1, "the next readable run starts a new comparison");
        }
    }

    #[test]
    fn a_post_turn_scan_limit_keeps_the_finished_turn_for_check_only_recovery() {
        let b = bench();
        let ws = worked_on(&b);
        std::fs::write(ws.app_dir().join("check-fails"), "2").unwrap();
        let nested = (0..65).fold(ws.app_dir().join("nested"), |p, _| p.join("d"));
        let agent = b._dir.path().join("agent.sh");
        let script = std::fs::read_to_string(&agent).unwrap();
        std::fs::write(&agent, format!("{script}\nmkdir -p '{}'\n", nested.display())).unwrap();
        let (ok, feed) = try_it(&b, &ws);
        assert!(!ok, "the original check failure still stops the run");
        let saved = Workspace::open(&ws.root).unwrap();
        assert!(saved.manifest.awaiting_check, "the successful builder turn is saved before an unavailable scan");
        assert_eq!(saved.manifest.last_reply, "Done.");
        assert_eq!(prompts_given(&b), 1);
        assert_eq!(BuildProgress::load(&ws.root).unwrap(), BuildProgress::default());
        assert!(
            feed.iter().any(|i| matches!(i, feed::FeedItem::Error { message, .. } if message.contains("The build is finished, but it couldn't be checked"))),
            "{feed:?}"
        );
        assert!(try_it(&b, &saved).0, "Try again can finish the checks while the scan is still unavailable");
        assert_eq!(prompts_given(&b), 1, "the completed builder turn is not repeated");
        assert!(!Workspace::open(&ws.root).unwrap().manifest.awaiting_check);
    }

    #[test]
    fn an_api_builder_has_the_same_two_unchanged_turn_limit() {
        use crate::agent::tests::{Scripted, turn};
        use crate::agent::{ApiConversation, EngineHost, LoopConfig, ModelSlot};
        use crate::model::{Block, StopReason};
        let b = bench();
        let mut ws = worked_on(&b);
        std::fs::write(ws.root.join("reject-build"), "not ready").unwrap();
        let seen = Arc::default();
        let script = Scripted {
            turns: (0..5).map(|_| turn(vec![Block::Text { text: "No files changed.".into() }], StopReason::EndTurn)).collect(),
            seen: Arc::clone(&seen),
        };
        let mut worker = check_worker(&ws, false);
        let slot = ModelSlot { name: "Scripted".into(), model: Box::new(script), local: true, effort: None, max_turns: 10, context_tokens: 0 };
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
        let mut opts = RunOptions::default();
        opts.kinds.register(Arc::new(FlakyKind));
        let error = worker.run(&mut ws, entry, Some(api), None, &opts).unwrap_err();
        assert!(error.message.contains("two builder turns"), "{error:?}");
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[test]
    fn new_notes_during_a_turn_or_check_get_their_own_attempt_but_empty_notes_do_not() {
        use crate::agent::tests::{Scripted, turn};
        use crate::agent::{ApiConversation, EngineHost, LoopConfig, ModelSlot};
        use crate::model::{Block, ChatModel, Delta, Stop, StopReason, TurnRequest, TurnResult};
        type Notes = Arc<Mutex<Vec<Note>>>;
        fn note(text: &str) -> Note {
            Note { id: 42, text: text.into(), shown: text.into(), pictures: Vec::new() }
        }
        struct NoteModel {
            script: Scripted,
            notes: Notes,
            add: bool,
            claim: bool,
            dir: std::path::PathBuf,
            text: &'static str,
        }
        impl ChatModel for NoteModel {
            fn turn(&mut self, req: &TurnRequest<'_>, delta: &mut dyn FnMut(Delta), stop: &Stop) -> Result<TurnResult, ToolError> {
                if self.add && self.script.seen.lock().unwrap().len() == 1 {
                    self.notes.lock().unwrap().push(note(self.text));
                    if self.claim {
                        notes::hand_over(&self.dir, 42, self.text).unwrap();
                        assert_eq!(notes::claim(&self.dir), [self.text]);
                    }
                }
                self.script.turn(req, delta, stop)
            }
            fn model(&self) -> String {
                self.script.model()
            }
        }
        struct NoteCheck {
            notes: Notes,
            add: bool,
            text: &'static str,
        }
        impl Check for NoteCheck {
            fn id(&self) -> &'static str {
                "note-check"
            }
            fn run(&self, cx: &mut CheckCx) -> Result<CheckResult, ToolError> {
                if self.add && cx.round == 2 {
                    self.notes.lock().unwrap().push(note(self.text));
                }
                Ok(CheckResult::new(self.id(), cx.round, false, "The app is still missing."))
            }
        }
        struct NoteKind {
            notes: Notes,
            add: bool,
            text: &'static str,
        }
        impl TargetKind for NoteKind {
            fn id(&self) -> &'static str {
                "note-kind"
            }
            fn words(&self) -> KindWords {
                FlakyKind.words()
            }
            fn detect(&self, _: &DetectCx) -> Option<u32> {
                Some(1001)
            }
            fn default_stack(&self, _: &DetectCx) -> &'static str {
                "Anything"
            }
            fn goal(&self) -> &'static str {
                "a thing"
            }
            fn checks(&self, _: &Workspace) -> Vec<Box<dyn Check>> {
                vec![Box::new(NoteCheck { notes: self.notes.clone(), add: self.add, text: self.text })]
            }
        }
        for mode in ["queued-turn", "queued-check", "hook"] {
            for text in ["Use the built-in renderer", "   "] {
                let b = bench();
                let mut ws = worked_on(&b);
                let mut worker = check_worker(&ws, false);
                let seen = Arc::default();
                let script = Scripted {
                    turns: (0..5).map(|_| turn(vec![Block::Text { text: "No files changed.".into() }], StopReason::EndTurn)).collect(),
                    seen: Arc::clone(&seen),
                };
                let model = NoteModel {
                    script,
                    notes: worker.notes.clone(),
                    add: mode != "queued-check",
                    claim: mode == "hook",
                    dir: notes::notes_dir(&ws.inbox_dir()),
                    text,
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
                let mut opts = RunOptions::default();
                opts.kinds.register(Arc::new(NoteKind { notes: worker.notes.clone(), add: mode == "queued-check", text }));
                let error = worker.run(&mut ws, entry, Some(api), None, &opts).unwrap_err();
                assert!(error.message.contains("two builder turns"), "{error:?}");
                let requests = seen.lock().unwrap();
                let expected = if text.trim().is_empty() {
                    2
                } else if mode == "hook" {
                    3
                } else {
                    4
                };
                assert_eq!(requests.len(), expected, "mode={mode}, text={text:?}");
                if !text.trim().is_empty() && mode != "hook" {
                    assert!(requests[2].last().unwrap().text().contains(text), "the queued instruction reaches the builder");
                }
                assert!(worker.notes.lock().unwrap().is_empty(), "each instruction is consumed once");
                assert!(!notes::was_read(&notes::notes_dir(&ws.inbox_dir()), 42));
                assert_eq!(BuildProgress::load(&ws.root).unwrap().unchanged_turns, 2);
                assert!(!try_it(&b, &ws).0);
                assert_eq!(prompts_given(&b), 0, "plain Try again still cannot reset the saved count");
            }
        }
    }

    /// #469: after the builder changed, the session the old one saved isn't handed to the new
    /// one, which starts from the brief; the same agent still carries on.
    #[test]
    fn another_agents_session_is_not_continued() {
        let b = bench();
        let mut ws = worked_on(&b);
        ws.manifest.agent_session_by = Some("claude-code".into());
        ws.save().unwrap();
        let (ok, feed) = try_it(&b, &ws);
        assert!(ok, "{feed:?}");
        let told = std::fs::read_to_string(&b.prompts).unwrap();
        assert!(told.contains("Their request") && !told.contains("Pick up where you left off"), "{told}");
        let m = Workspace::open(&ws.root).unwrap().manifest;
        assert_eq!((m.agent_session, m.agent_session_by.as_deref()), (None, Some("stand-in")));

        let b = bench();
        let mut ws = worked_on(&b);
        ws.manifest.agent_session_by = Some("stand-in".into());
        ws.save().unwrap();
        let (ok, feed) = try_it(&b, &ws);
        assert!(ok, "{feed:?}");
        assert!(std::fs::read_to_string(&b.prompts).unwrap().contains("Pick up where you left off"));
        assert_eq!(Workspace::open(&ws.root).unwrap().manifest.agent_session.as_deref(), Some("session-1"));
    }

    #[test]
    fn a_new_project_still_starts_from_the_top() {
        let b = bench();
        let ws = Workspace::create(&b.projects, "x", "A thing", TargetRef::None).unwrap();
        let (ok, feed) = try_it(&b, &ws);
        assert!(ok, "{feed:?}");
        let told = std::fs::read_to_string(&b.prompts).unwrap();
        assert!(!told.contains("Pick up where you left off"), "{told}");
    }

    #[test]
    fn a_check_that_could_not_run_gets_a_second_go_and_the_build_is_not_blamed() {
        let b = bench();
        let mut ws = worked_on(&b);
        std::fs::write(ws.app_dir().join("check-fails"), "1").unwrap();
        ws.manifest.awaiting_check = true;
        ws.save().unwrap();
        let (ok, _) = try_it(&b, &ws);
        assert!(ok, "one lost browser doesn't fail a finished build");

        // It keeps failing: the run stops, says the build is fine, and Try again only checks.
        let mut ws = worked_on(&b);
        ws.manifest.name = "y".into();
        ws.manifest.awaiting_check = false;
        ws.save().unwrap();
        std::fs::write(ws.app_dir().join("check-fails"), "2").unwrap();
        let (ok, feed) = try_it(&b, &ws);
        assert!(!ok);
        assert!(
            feed.iter().any(|i| matches!(i, feed::FeedItem::Error { message, .. } if message.contains("The build is finished, but it couldn't be checked"))),
            "{feed:?}"
        );
        let asked = prompts_given(&b);
        assert!(Workspace::open(&ws.root).unwrap().manifest.awaiting_check, "the builder's turn is on record");
        let (ok, _) = try_it(&b, &ws);
        assert!(ok);
        assert_eq!(prompts_given(&b), asked, "and it isn't asked to build again");
    }

    /// #736: a builder that says it built the site while app/ has no files stops after two
    /// unchanged turns, and the build check and the stop both say nothing was written. The
    /// reporter's release had no such stop, so its check ran 40+ times.
    #[test]
    fn an_empty_app_with_a_claimed_build_stops_and_says_nothing_was_written() {
        let b = bench();
        let mut ws = Workspace::create(&b.projects, "x", "A website for a bakery", TargetRef::None).unwrap();
        ws.manifest.agent_session = Some("session-1".into());
        ws.save().unwrap();
        std::fs::create_dir_all(ws.capture_dir()).unwrap();
        std::fs::create_dir_all(ws.app_dir()).unwrap();
        // The stand-in answers "Done." and writes nothing, as the reported builder did.
        let opts = RunOptions { backend: Some("Stand-in".into()), ..RunOptions::default() };
        let run = RebuildRun::start(&b.hub, Workspace::open(&ws.root).unwrap(), opts).unwrap();
        let ok = loop {
            match run.rx.recv_timeout(Duration::from_secs(60)) {
                Ok(RunEvent::Finished { ok, .. }) => break ok,
                Ok(_) => {}
                Err(e) => panic!("the run went away: {e}"),
            }
        };
        assert!(!ok);
        assert_eq!(prompts_given(&b), 2, "a third unchanged turn would spend more without progress");
        let saved = Workspace::open(&ws.root).unwrap();
        let build = saved.manifest.checks.iter().find(|c| c.check == CheckResult::BUILD).expect("the build check ran");
        assert!(build.note.contains("app/ has no files yet, so the builder hasn't written the project."), "{}", build.note);
        let feed = feed::load(&saved);
        assert!(
            feed.iter().any(|i| matches!(i, feed::FeedItem::Error { message, .. }
                if message.contains("changed no files in app/") && message.contains("app/ has no files yet"))),
            "the stop says app/ is empty: {feed:?}"
        );
    }

    /// The empty-folder wording is only for an empty app/: a folder with files gets the plain stop.
    #[test]
    fn the_stop_names_an_empty_app_and_only_then() {
        let b = bench();
        let ws = worked_on(&b);
        let empty = no_build_progress(&ws);
        assert!(empty.message.contains("app/ has no files yet"), "{empty:?}");
        assert!(empty.remediation.as_deref().is_some_and(|r| r.contains("asks for the project's files in app/")), "{empty:?}");
        std::fs::write(ws.app_dir().join("index.html"), "<h1>Bakery</h1>").unwrap();
        let written = no_build_progress(&ws);
        assert!(!written.message.contains("has no files"), "{written:?}");
        assert_eq!(written.message, "The build stopped after two builder turns changed no files in app/.");
    }
}

fn check_worker(ws: &Workspace, deep: bool) -> Worker {
    let mut s = craft_anything_engine::testing::session();
    s.settings.deep_mode = deep;
    let (tx, _rx) = std::sync::mpsc::channel();
    Worker {
        wake: Default::default(),
        engine: Arc::new(Engine::new(s)),
        tx,
        stop: CancelToken::default(),
        preview: Arc::default(),
        seen: HashSet::new(),
        inbox: Inbox::new(ws.inbox_dir()),
        editor: None,
        notes: Arc::default(),
        feed: Arc::new(Mutex::new(feed::FeedLog::open(ws))),
        assets: false,
        decompiled: false,
        usage: None,
        self_check: None,
        reset_progress_after_notes: false,
    }
}

/// A kind with its checks after the first `.1`, so a test needn't build or start anything.
struct LaterChecks(&'static dyn TargetKind, usize);

impl TargetKind for LaterChecks {
    fn id(&self) -> &'static str {
        self.0.id()
    }
    fn words(&self) -> KindWords {
        self.0.words()
    }
    fn detect(&self, _cx: &DetectCx) -> Option<u32> {
        None
    }
    fn default_stack(&self, cx: &DetectCx) -> &'static str {
        self.0.default_stack(cx)
    }
    fn goal(&self) -> &'static str {
        self.0.goal()
    }
    fn checks(&self, ws: &Workspace) -> Vec<Box<dyn Check>> {
        self.0.checks(ws).into_iter().skip(self.1).collect()
    }
}

/// The app's sheet only.
const APP_SHEET_ONLY: LaterChecks = LaterChecks(&AppKind, 3);

#[test]
fn apps_keep_the_sheet_only_when_deep_mode_decompiles_a_program() {
    let d = tempfile::tempdir().unwrap();
    let prog = Workspace::create(d.path(), "Tool", "Rebuild this app", TargetRef::Local { path: d.path().join("tool.exe") }).unwrap();
    let ids = AppKind.checks(&prog).iter().map(|c| c.id()).collect::<Vec<_>>();
    assert_eq!(ids, ["build", "app-runs", "app-screenshot", "mechanics"]);
    assert!(brief(&prog, "app", true, false, None, false, None).contains(kinds::mechanics::MECHANICS_CONTRACT));
    assert!(!brief(&prog, "app", false, false, None, false, None).contains("app/mechanics.json"), "Clean room doesn't ask for it");

    // Deep mode on a program: the sheet is checked (and missing here).
    let r = check_worker(&prog, true).run_checks(&prog, &APP_SHEET_ONLY, 1).unwrap();
    assert_eq!(r.len(), 1);
    assert!(r[0].check == "mechanics" && !r[0].passed, "{:?}", r[0]);
    // Clean room, or nothing to decompile: skipped, leaving no result.
    assert!(check_worker(&prog, false).run_checks(&prog, &APP_SHEET_ONLY, 1).unwrap().is_empty());
    let fresh = Workspace::create(d.path(), "Fresh", "A notes app", TargetRef::None).unwrap();
    assert!(check_worker(&fresh, true).run_checks(&fresh, &APP_SHEET_ONLY, 1).unwrap().is_empty());
}

#[test]
fn games_are_asked_for_the_sheet_checks_read() {
    let d = tempfile::tempdir().unwrap();
    let ws = Workspace::create(d.path(), "Arena", "Recreate this game", TargetRef::Local { path: d.path().to_path_buf() }).unwrap();
    let ids = |ws: &Workspace| GameKind.checks(ws).iter().map(|c| c.id()).collect::<Vec<_>>();
    assert_eq!(ids(&ws), ["build", "app-screenshot", "game-self-test", "mechanics", "credits"]);
    let b = brief(&ws, "game", false, false, None, false, None);
    assert!(b.contains("app/mechanics.json") && b.contains(kinds::mechanics::MECHANICS_CONTRACT), "{b}");
    let web = Workspace::create(d.path(), "Web game", "A game of this site", TargetRef::Website { url: "https://example.com".into() }).unwrap();
    assert_eq!(ids(&web), ["build", "app-screenshot", "game-self-test"], "a website's game has no sheet and no original's files");
    let web_brief = brief(&web, "game", true, true, None, false, None);
    assert!(web_brief.contains(kinds::self_test::SELF_TEST_CONTRACT) && !web_brief.contains("CREDITS.md"), "{web_brief}");
}

#[test]
fn a_game_is_ready_only_when_it_plays_and_credits_the_originals_files() {
    let d = tempfile::tempdir().unwrap();
    let ws = Workspace::create(d.path(), "Caves", "Copy this game", TargetRef::Local { path: d.path().join("game.exe") }).unwrap();
    // The self-test needs a built game to start.
    let r = check_worker(&ws, false).run_checks(&ws, &LaterChecks(&GameKind, 2), 1).unwrap();
    assert!(r[0].check == "game-self-test" && !r[0].passed && r[0].note == "There's no built program to start yet.", "{:?}", r[0]);

    // Deep mode on a program: the original's files must be credited and the README says they're private.
    const CREDITS_ONLY: LaterChecks = LaterChecks(&GameKind, 4);
    let r = check_worker(&ws, true).run_checks(&ws, &CREDITS_ONLY, 1).unwrap();
    assert_eq!(r.len(), 1);
    assert!(r[0].check == "credits" && !r[0].passed && r[0].note.contains("app/CREDITS.md") && r[0].note.contains(PRIVATE_USE_NOTE), "{:?}", r[0]);
    std::fs::create_dir_all(ws.app_dir()).unwrap();
    std::fs::write(
        ws.app_dir().join("README.md"),
        format!(
            "# Caves

{PRIVATE_USE_NOTE}
"
        ),
    )
    .unwrap();
    std::fs::write(
        ws.app_dir().join("CREDITS.md"),
        "- Content/Rock.uasset from the game's Paks folder
",
    )
    .unwrap();
    assert!(check_worker(&ws, true).run_checks(&ws, &CREDITS_ONLY, 2).unwrap()[0].passed);
    // Clean room copies none of the original's files, and a game without a target has none: skipped.
    assert!(check_worker(&ws, false).run_checks(&ws, &CREDITS_ONLY, 1).unwrap().is_empty());
    let snake = Workspace::create(d.path(), "Snake", "A snake game", TargetRef::None).unwrap();
    assert!(check_worker(&snake, true).run_checks(&snake, &CREDITS_ONLY, 1).unwrap().is_empty());
}

#[test]
fn app_check_compares_the_labelled_controls() {
    use kinds::screenshot::controls_likeness;
    let tree = |nodes: &[(&str, &str)]| json!({"nodes": nodes.iter().map(|(role, title)| json!({"role": role, "title": title})).collect::<Vec<_>>()});
    let original =
        tree(&[("Window", "Character Map"), ("Pane", "Characters to copy :"), ("Button", "&Copy"), ("Button", "Select"), ("Image", ""), ("MenuItem", "File")]);
    // Case, spacing, mnemonics and a trailing colon don't matter; unlabelled parts aren't counted.
    let same =
        tree(&[("AXWindow", "character map"), ("AXStaticText", "Characters  to copy"), ("AXButton", "Copy"), ("AXButton", "Select"), ("AXMenuItem", "File")]);
    let c = controls_likeness(&original, &same).unwrap();
    assert_eq!((c.similarity.round(), c.missing.clone()), (100.0, Vec::<String>::new()));

    let thin = tree(&[("Window", "Character Map"), ("Button", "Select"), ("Button", "Help")]);
    let c = controls_likeness(&original, &thin).unwrap();
    assert_eq!(c.similarity.round(), 50.0, "2 shared of 5 + 3: 2*2/8");
    assert_eq!(c.missing, vec!["\"Characters to copy :\" (pane)", "\"Copy\" (button)", "\"File\" (menuitem)"]);
    assert_eq!(
        c.note(),
        "Its controls are 50% alike by their labels; the original also has \"Characters to copy :\" (pane), \"Copy\" (button), \"File\" (menuitem)."
    );
    assert!(controls_likeness(&tree(&[("Pane", "Game"), ("Pane", "")]), &thin).is_none(), "a game window has nothing to compare");

    // Each control a person uses is a requirement of its own; text labels stay advisory.
    let claims = kinds::screenshot::control_claims(&original, &thin);
    let got: Vec<(&str, bool)> = claims.iter().map(|c| (c.title.as_str(), c.passed)).collect();
    assert_eq!(
        got,
        [
            ("It has the original's \"Copy\" button", false),
            ("It has the original's \"Select\" button", true),
            ("It has the original's \"File\" menuitem", false)
        ]
    );
    assert!(kinds::screenshot::control_claims(&original, &same).iter().all(|c| c.passed), "labels match whatever the platform calls the role");
}

#[test]
fn website_check_compares_the_page_structure() {
    use kinds::website::structure_likeness;
    let page = |parts: &[(&str, Option<&str>)]| json!({"outline": parts.iter().map(|(tag, role)| match role { Some(r) => json!({"tag": tag, "role": r}), None => json!({"tag": tag}) }).collect::<Vec<_>>()});
    let original =
        page(&[("header", None), ("nav", None), ("h1", None), ("p", None), ("ul", None), ("ul", None), ("form", None), ("input", None), ("footer", None)]);
    // Roles count as their element; ul and ol are both lists; paragraphs and divs aren't parts.
    let same = page(&[
        ("div", Some("banner")),
        ("div", Some("navigation")),
        ("h1", None),
        ("ol", None),
        ("ul", None),
        ("form", None),
        ("textarea", None),
        ("div", Some("contentinfo")),
    ]);
    let s = structure_likeness(&original, &same).unwrap();
    assert_eq!((s.similarity.round(), s.missing.clone()), (100.0, vec![]), "{s:?}");

    let thin = page(&[("header", None), ("h1", None), ("ul", None), ("footer", None)]);
    let s = structure_likeness(&original, &thin).unwrap();
    assert_eq!(s.similarity.round(), 67.0, "4 of 8 parts in order: 2*4/12");
    assert_eq!(s.missing, vec![("navigation bar", 1), ("form", 1), ("list", 1), ("form field", 1)]);
    assert_eq!(s.note(), "structure 67% alike; missing parts: a navigation bar, a form, a list, a form field");
    let s = structure_likeness(&original, &page(&[])).unwrap();
    assert!(s.note().contains("2 lists"), "{}", s.note());
    assert!(structure_likeness(&page(&[("p", None)]), &thin).is_none(), "nothing to compare");
}

#[test]
fn website_check_pairs_opened_states_by_kind_and_label() {
    use kinds::website::pair_states;
    let original = json!({"states": [
        {"kind": "menu", "label": "Products", "file": "state-1.png"},
        {"kind": "dialog", "label": "Sign  up", "file": "state-2.png"},
        {"kind": "tab", "label": "Pricing", "file": "state-3.png"},
        {"kind": "menu", "label": "Went elsewhere", "changed": ["address changed"]}
    ]});
    let built = json!({"states": [
        {"kind": "dialog", "label": "sign up", "file": "state-1.png"},
        {"kind": "menu", "label": "Products", "file": "state-2.png"},
        {"kind": "section", "label": "Pricing", "file": "state-3.png"}
    ]});
    let pairs: Vec<(&str, Option<&str>)> =
        pair_states(&original, &built).into_iter().map(|(o, b)| (o["file"].as_str().unwrap(), b.map(|b| b["file"].as_str().unwrap()))).collect();
    assert_eq!(
        pairs,
        vec![("state-1.png", Some("state-2.png")), ("state-2.png", Some("state-1.png")), ("state-3.png", None)],
        "unpictured states are left out"
    );
    assert!(pair_states(&json!({"states": []}), &built).is_empty());
}

#[test]
fn deeper_pages_come_from_the_captured_pages_links() {
    use kinds::website::{Route, pick_deeper_routes};
    let start = "https://shop.example.com/";
    let have = vec![Route { path: "/products".into(), slug: "products".into(), url: "https://shop.example.com/products".into() }];
    let products_page = json!({"links": [
        {"href": "https://shop.example.com/products/"},
        {"href": "https://shop.example.com/products/chairs"},
        {"href": "https://shop.example.com/products/tables"},
        {"href": "https://shop.example.com/"},
        {"href": "https://elsewhere.example.org/x"},
        {"href": "https://shop.example.com/brochure.pdf"}
    ]});
    let about_page = json!({"links": [{"href": "https://shop.example.com/products/chairs"}, {"href": "https://shop.example.com/team"}]});
    let r = pick_deeper_routes(&[products_page.clone(), about_page], start, &have, 5);
    let paths: Vec<&str> = r.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(paths, vec!["/products/chairs", "/products/tables", "/team"], "new pages only, once each, same site, no files");
    assert_eq!(pick_deeper_routes(&[products_page], start, &have, 1).len(), 1, "never more than there's room for");
}

#[test]
fn website_check_compares_the_text_and_names_what_is_missing() {
    use kinds::website::text_likeness;
    let original = json!({
        "text": "Simple pricing for teams. Plans start at 9 dollars. FAQ: can I cancel? Yes, anytime.",
        "headings": [{"level": 1, "text": "Simple pricing"}, {"level": 2, "text": "FAQ"}]
    });
    let built = json!({
        "text": "Simple pricing for teams. Plans start at 9 dollars.",
        "headings": [{"level": 1, "text": "Simple  Pricing"}]
    });
    let t = text_likeness(&original, &built).unwrap();
    assert!(t.similarity > 50.0 && t.similarity < 90.0, "{}", t.similarity);
    assert_eq!(t.missing_headings, vec!["FAQ".to_string()], "case and spacing don't matter");
    assert!(t.missing_words.contains(&"faq".to_string()) && t.missing_words.contains(&"cancel".to_string()), "{:?}", t.missing_words);
    assert!(t.note().starts_with("text ") && t.note().contains("missing headings: \"FAQ\""), "{}", t.note());
    let same = text_likeness(&original, &original).unwrap();
    assert!((same.similarity - 100.0).abs() < 1e-9 && same.missing_headings.is_empty() && same.missing_words.is_empty());
    assert!(text_likeness(&json!({"text": ""}), &built).is_none(), "nothing to compare");
}

#[test]
fn a_resumed_game_builder_hears_the_asset_choice_as_it_is_now() {
    let d = tempfile::tempdir().unwrap();
    let mut ws = Workspace::create(d.path(), "Game", "A snake game", TargetRef::None).unwrap();
    let game = kind("game");
    fn cx(ws: &Workspace, assets: bool) -> BriefCx<'_> {
        BriefCx { ws, deep: false, ghidra: false, editor: None, assets, image_model: None }
    }
    assert!(game.follow_up_sections(&cx(&ws, true)).is_empty(), "off and never used: nothing to say");
    // Turned on after the first build, with a key: the follow-up carries the asset note and budget.
    ws.set_person(|m| m.generate_assets = true).unwrap();
    let on = game.follow_up_sections(&cx(&ws, true));
    assert!(on[0].contains("assets_model_generate") && on[0].contains("$5.00"), "{on:?}");
    assert!(game.follow_up_sections(&cx(&ws, false)).is_empty(), "no key: not offered");
    // Turned off after some were made: say so.
    let rec = craft_anything_engine::workspace::AssetRecord { file: "a.glb".into(), kind: "model".into(), prompt: "a crate".into(), cost_usd: 0.5, at_ms: 1 };
    ws.record_asset(rec).unwrap();
    ws.set_person(|m| m.generate_assets = false).unwrap();
    assert!(game.follow_up_sections(&cx(&ws, true))[0].contains("now off"));
    assert!(kind("website").follow_up_sections(&cx(&ws, true)).is_empty());
}

#[test]
fn a_stopped_build_is_told_to_carry_on_not_start_over() {
    let p = continue_prompt(kind("website").as_ref());
    assert!(p.contains("stopped before it finished") && p.contains("Pick up where you left off") && p.contains("app/"));
}

#[test]
fn the_extractor_takes_the_originals_out_and_the_builder_is_told_where() {
    let b = extract_brief(Path::new("D:/Games/Cave Game"));
    assert!(b.contains("D:/Games/Cave Game") && b.contains("app/Original/") && b.contains("permission: true"), "{b}");
    assert!(b.contains("Never decrypt") && b.contains("You don't build the game") && b.contains(&format!("app/{ORIGINALS_MANIFEST}")), "{b}");
    assert!(b.contains("never write your own archive reader") && b.contains("ending in `/`"), "{b}");
    assert!(ORIGINALS_READY.contains("app/Original/MANIFEST.md") && ORIGINALS_READY.contains("CREDITS.md"));
}

#[test]
fn the_check_names_padding_colour_font_and_width_differences() {
    use kinds::website::design_notes;
    let section = |heading: &str, pad: [u64; 4], per_row: u64, column: u64| json!({"tag": "section", "heading": heading, "pad": pad, "per_row": per_row, "column": {"max_w": column, "x": 80}});
    let original = json!({
        "design": {"backgrounds": [{"value": "#12181f"}, {"value": "#0c2a2a"}], "text_colors": [{"value": "#e6ecf2"}],
                   "fonts": [{"value": "Figtree, sans-serif"}], "font_sizes": [{"value": "64px"}, {"value": "16px"}],
                   "spacing": [{"value": "96px"}, {"value": "24px"}]},
        "layouts": [
            {"width": 1440, "page_w": 1440, "type": {"h1": 64, "body": 16}, "blocks": [json!({"tag": "header", "pad": [16, 32, 16, 32], "per_row": 1}), section("Plans", [96, 32, 96, 32], 3, 1280)]},
            {"width": 390, "page_w": 390, "type": {"h1": 40}, "blocks": [section("Plans", [64, 16, 64, 16], 1, 1280)]}
        ]
    });
    // next/font renames the family and the browser rounds a colour by a step: both still match.
    let same = json!({
        "design": {"backgrounds": [{"value": "#12181e"}, {"value": "#0c2a2a"}], "text_colors": [{"value": "#e6ecf2"}],
                   "fonts": [{"value": "\"__Figtree_a1b2c3\", \"__Figtree_Fallback_a1b2c3\""}], "font_sizes": [{"value": "64px"}, {"value": "16px"}],
                   "spacing": [{"value": "24px"}, {"value": "96px"}]},
        "layouts": original["layouts"].clone()
    });
    assert!(design_notes(&original, &same).is_empty(), "{:?}", design_notes(&original, &same));
    let built = json!({
        "design": {"backgrounds": [{"value": "#ffffff"}, {"value": "#0c2a2a"}], "text_colors": [{"value": "#e6ecf2"}],
                   "fonts": [{"value": "Inter, sans-serif"}], "font_sizes": [{"value": "48px"}, {"value": "16px"}], "spacing": [{"value": "24px"}]},
        "layouts": [
            {"width": 1440, "page_w": 1440, "type": {"h1": 48, "body": 16}, "blocks": [json!({"tag": "header", "pad": [16, 32, 16, 32], "per_row": 1}), section("Plans", [48, 24, 48, 24], 1, 1200)]},
            {"width": 390, "page_w": 512, "type": {"h1": 40}, "blocks": [json!({"tag": "section", "heading": "plans ", "pad": [64, 16, 64, 16], "per_row": 1})]}
        ]
    });
    let notes = design_notes(&original, &built);
    assert_eq!(notes[0], "colours the original uses that the build doesn't: #12181f (background)");
    assert_eq!(notes[1], "main font: Figtree in the original, Inter in the build");
    assert_eq!(notes[2], "text sizes the original uses that the build doesn't: 64px");
    assert_eq!(notes[3], "spacing the original uses that the build doesn't: 96px");
    assert_eq!(
        notes[4],
        "at 1440 px: top heading 48 px, the original's 64 px; the \"Plans\" section: padding 48 24 48 24 vs the original's 96 32 96 32, 1 per row vs 3, content column 1200 px vs 1280 px"
    );
    assert_eq!(notes[5], "at 390 px: the page scrolls sideways (512 px wide); the \"Plans\" section: no content column (the original's is 1280 px)");
    assert_eq!(notes.len(), 6);
    assert!(design_notes(&json!({}), &json!({})).is_empty(), "nothing to compare");
}

#[test]
fn menus_are_studied_on_a_phone_too() {
    use kinds::website::{STATES_DIR, phone_states_dir, phone_width, pictured_states, state_dirs};
    assert_eq!(phone_width(&[1440, 768, 390]), Some(390));
    assert_eq!(phone_width(&[1440, 1024]), None);
    let d = tempfile::tempdir().unwrap();
    let ws = Workspace::create(d.path(), "Shop", "Clone it", TargetRef::Website { url: "https://shop.example.com/".into() }).unwrap();
    for (dir, width, label) in [(STATES_DIR.to_string(), 1440, "Products"), (phone_states_dir(390), 390, "Menu")] {
        let states = ws.capture_dir().join(dir);
        std::fs::create_dir_all(&states).unwrap();
        let list = json!({"width": width, "states": [{"kind": "menu", "label": label, "file": "state-1.png"}]});
        std::fs::write(states.join("states.json"), serde_json::to_vec(&list).unwrap()).unwrap();
    }
    std::fs::create_dir_all(ws.capture_dir().join("states-old")).unwrap();
    assert_eq!(state_dirs(&ws.capture_dir()), ["states", "states-390"]);
    assert_eq!(pictured_states(&ws), ["states/state-1.png (Products, menu, 1440 px)", "states-390/state-1.png (Menu, menu, 390 px)"]);
}

/// A builder whose turn lasts until its own check (asked like `rebuild_check` from another process)
/// has been answered by the run, then ends with that answer as its reply.
struct ChecksItsWork {
    root: PathBuf,
    asked: Option<std::thread::JoinHandle<Option<String>>>,
    reply: Option<String>,
}

impl Conversation for ChecksItsWork {
    fn send(&mut self, _text: &str) {
        let asks = Inbox::new(self_check::asks_dir(&self.root));
        self.asked = Some(std::thread::spawn(move || {
            let id = asks.post(craft_anything_engine::inbox::RequestKind::Question, "Check the build", "", Vec::new(), false, None).ok()?;
            asks.wait(&id, Duration::from_secs(20), &|| false).and_then(|a| a.text)
        }));
    }
    fn poll(&mut self) -> Vec<AgentEvent> {
        if self.asked.as_ref().is_some_and(|h| h.is_finished()) {
            self.reply = self.asked.take().and_then(|h| h.join().ok()).flatten();
        }
        match self.reply.take() {
            Some(text) => {
                vec![
                    AgentEvent::TextDelta { text },
                    AgentEvent::TurnDone { backend: "test".into(), input_tokens: 0, output_tokens: 0, cost_usd: None, cost_is_estimate: false },
                ]
            }
            None => Vec::new(),
        }
    }
    fn answer_consent(&mut self, _call_id: &str, _allow: bool) {}
    fn stop(&mut self) {}
    fn is_busy(&self) -> bool {
        true
    }
}

/// The builder's own `rebuild_check`, asked mid-turn, is answered by the run and isn't a round.
#[test]
fn the_run_answers_the_builders_own_check_during_its_turn() {
    let d = tempfile::tempdir().unwrap();
    let mut ws = Workspace::create(d.path(), "Arena", "Recreate this game", TargetRef::None).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let mut w = Worker {
        wake: Default::default(),
        engine: Arc::new(Engine::new(craft_anything_engine::testing::session())),
        tx,
        stop: CancelToken::default(),
        preview: Arc::default(),
        seen: HashSet::new(),
        inbox: Inbox::new(ws.inbox_dir()),
        editor: None,
        notes: Arc::default(),
        feed: Arc::new(Mutex::new(feed::FeedLog::open(&ws))),
        assets: false,
        decompiled: false,
        usage: None,
        self_check: Some((Arc::new(SheetKind), 90.0)),
        reset_progress_after_notes: false,
    };
    let mut builder = ChecksItsWork { root: ws.root.clone(), asked: None, reply: None };
    let reply = w.turn(&mut ws, &mut builder, "Build it.", None).unwrap();
    // The same check the run makes after a turn, as round 0, with what to do next.
    assert!(reply.starts_with("mechanics: failed") && reply.contains("no app/mechanics.json") && reply.contains("check again with rebuild_check"), "{reply}");
    assert!(ws.manifest.checks.is_empty(), "the builder's own check isn't a round");
    let steps: Vec<String> = rx.try_iter().filter_map(|e| if let RunEvent::Step { text, .. } = e { Some(text) } else { None }).collect();
    assert!(steps.iter().any(|s| s.contains("checking its work")) && steps.iter().any(|s| s.contains("checked its work")), "the person sees it: {steps:?}");
    assert!(Inbox::new(self_check::asks_dir(&ws.root)).pending().is_empty(), "nothing left waiting");
}

#[test]
fn every_kind_leads_tests_its_own_build_and_keeps_its_unknowns() {
    let d = tempfile::tempdir().unwrap();
    let web = TargetRef::Website { url: "https://example.com".into() };
    for (k, target, stack) in [("website", web, stacks::BUILDERS_CHOICE), ("app", TargetRef::None, "Rust + egui (eframe)"), ("game", TargetRef::None, "Godot")]
    {
        let mut ws = Workspace::create(d.path(), k, "Rebuild it", target).unwrap();
        ws.manifest.stack = stack.into();
        let b = brief(&ws, k, false, false, None, false, None);
        assert!(b.contains("you lead the investigation") && b.contains("Don't repeat a call that already answered"), "{k}: {b}");
        assert!(b.contains("core_unknown_add") && b.contains("core_unknown_edit") && b.contains("Missing evidence is never proof"), "{k}");
        assert!(b.contains("go through your checklist and core_unknowns_show"), "{k}");
        assert!(b.contains("Test it against the original with rebuild_check") && b.contains("Before you reply, go through your checklist"), "{k}");
        if k == "website" {
            assert!(b.contains("Work from these pictures and measurements.\n\nMatch it exactly"), "one clean paragraph break: {b}");
            let deep = brief(&ws, k, true, false, None, false, None);
            assert!(deep.contains("(the rendered HTML).\n\nMatch it exactly"), "one clean paragraph break: {deep}");
        }
    }
    assert!(follow_up("bigger title", "game").contains("check it with rebuild_check"));
    assert!(continue_prompt(kind("app").as_ref()).contains("check it with rebuild_check"));
}

#[test]
fn the_check_names_errors_failed_requests_and_accessibility_the_build_adds() {
    use kinds::website::health_notes;
    let original = json!({
        "health": {"errors": [{"kind": "script", "text": "ads.js blocked"}], "failed_requests": [{"url": "https://ex.com/ads/pixel.gif", "status": 404}]},
        "a11y": {"lang": "en", "unnamed_controls": 1, "images_without_alt": 0}
    });
    // The original's own problems, served from elsewhere, aren't the build's to fix.
    let same = json!({
        "health": {"errors": [{"kind": "script", "text": "ads.js blocked"}], "failed_requests": [{"url": "http://127.0.0.1:5173/ads/pixel.gif", "status": 404}]},
        "a11y": {"lang": "en", "unnamed_controls": 1, "images_without_alt": 0}
    });
    assert!(health_notes(&original, &same).is_empty(), "{:?}", health_notes(&original, &same));
    let built = json!({
        "health": {
            "errors": [{"kind": "script", "text": "TypeError: x is undefined"}, {"kind": "console", "text": "Hydration failed"}],
            "failed_requests": [{"url": "http://127.0.0.1:5173/fonts/Figtree.woff2", "error": "net::ERR_FILE_NOT_FOUND"}, {"url": "http://127.0.0.1:5173/logo.png", "status": 404}]
        },
        "a11y": {"lang": "", "unnamed_controls": 4, "images_without_alt": 2}
    });
    assert_eq!(
        health_notes(&original, &built),
        [
            "the build logs 2 error(s) the original doesn't: \"TypeError: x is undefined\"; \"Hydration failed\"",
            "2 request(s) fail in the build: /fonts/Figtree.woff2 (net::ERR_FILE_NOT_FOUND), /logo.png (404)",
            "4 controls without a name (the original has 1)",
            "2 pictures without alt text (the original has 0)",
            "the page language isn't set (the original's is en)",
        ]
    );
    assert!(health_notes(&json!({}), &json!({})).is_empty(), "older captures have nothing to compare");
}

#[test]
fn each_thing_a_page_must_do_is_a_claim_of_its_own() {
    use kinds::website::page_claims;
    let original = json!({
        "layouts": [{"width": 1440, "page_w": 1440}, {"width": 390, "page_w": 390}, {"width": 320, "page_w": 400}],
        "health": {"errors": [], "failed_requests": []},
        "a11y": {"landmarks": {"banner": 1, "navigation": 1, "main": 1, "contentinfo": 1}}
    });
    let good = json!({
        "layouts": [{"width": 1440, "page_w": 1440}, {"width": 390, "page_w": 390}, {"width": 320, "page_w": 500}],
        "health": {"errors": [], "failed_requests": []},
        "a11y": {"landmarks": {"banner": 2, "navigation": 1, "main": 1, "contentinfo": 1}}
    });
    let claims = page_claims("The start page", &original, &good);
    let titles: Vec<&str> = claims.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(
        titles,
        [
            "The start page doesn't scroll sideways at 1440 px",
            "The start page doesn't scroll sideways at 390 px",
            "The start page logs no errors the original doesn't",
            "The start page loads every file it asks for",
            "The start page has the original's landmarks",
        ],
        "no claim at a width where the original scrolls sideways too"
    );
    assert!(claims.iter().all(|c| c.passed));
    let broken = json!({
        "layouts": [{"width": 1440, "page_w": 1440}, {"width": 390, "page_w": 512}],
        "health": {"errors": [{"text": "TypeError: x"}], "failed_requests": [{"url": "http://127.0.0.1:1/logo.png", "status": 404}]},
        "a11y": {"landmarks": {"main": 1}}
    });
    let failed: Vec<(String, String)> = page_claims("/about", &original, &broken).into_iter().filter(|c| !c.passed).map(|c| (c.title, c.detail)).collect();
    assert_eq!(
        failed,
        [
            ("/about doesn't scroll sideways at 390 px".to_string(), "it's 512 px wide".to_string()),
            ("/about logs no errors the original doesn't".to_string(), "TypeError: x".to_string()),
            ("/about loads every file it asks for".to_string(), "/logo.png (404)".to_string()),
            ("/about has the original's landmarks".to_string(), "missing: banner, contentinfo, navigation".to_string()),
        ]
    );
    assert!(page_claims("x", &json!({}), &json!({})).is_empty(), "older captures make no claims");
}

#[test]
fn a_console_original_is_compared_by_its_runs_not_pictured() {
    let d = tempfile::tempdir().unwrap();
    let ws = Workspace::create(d.path(), "Tool", "Rebuild this tool", TargetRef::Local { path: d.path().join("tool.exe") }).unwrap();
    let b = brief(&ws, "app", false, false, None, false, None);
    assert!(b.contains("--screenshot") && !b.contains("capture/runs/"), "a windowed app is pictured");
    let runs = ws.capture_dir().join(kinds::runs::RUNS_DIR);
    std::fs::create_dir_all(&runs).unwrap();
    std::fs::write(
        runs.join("help.json"),
        br#"{"name": "--help", "arguments": ["--help"], "reason": "exited", "exit_code": 0, "lines": ["Usage: tool <file>"]}"#,
    )
    .unwrap();
    let b = brief(&ws, "app", false, false, None, false, None);
    assert!(b.contains(kinds::RUNS_BRIEF) && !b.contains("Give the program a `--screenshot"), "{b}");
    assert_eq!(kinds::runs::captured(&ws).len(), 1, "the runs check applies, the picture check doesn't");
}

#[test]
fn deep_mode_hands_the_builder_every_tool_and_clean_room_none_that_look_inside() {
    let d = tempfile::tempdir().unwrap();
    let web = Workspace::create(d.path(), "Site", "Clone it", TargetRef::Website { url: "https://example.com".into() }).unwrap();
    let b = brief(&web, "website", true, false, None, false, None);
    assert!(b.contains("every craft-anything tool is yours") && b.contains("capture/how-it-works.md"), "{b}");
    assert!(b.contains("web_listeners_list") && b.contains("web_coverage_measure") && b.contains("js_sources_restore"), "{b}");
    assert!(b.contains("never paste the original's code into app/") && !b.contains("enable_toolset with"), "{b}");
    let clean = brief(&web, "website", false, false, None, false, None);
    assert!(clean.contains("what a visitor of the live site sees") && clean.contains("don't open or fetch the original site"), "{clean}");
    for deep_only in ["web_page_show", "web_listeners_list", "page.html", "breakpoints", "how-it-works"] {
        assert!(!clean.contains(deep_only), "{deep_only} in a Clean room brief: {clean}");
    }
    assert!(clean.contains("never open, read, list, fetch, run or decompile its files"), "{clean}");
    let prog = Workspace::create(d.path(), "Tool", "Rebuild it", TargetRef::Local { path: d.path().join("tool.exe") }).unwrap();
    let b = brief(&prog, "app", true, false, None, false, None);
    assert!(
        b.contains("core_target_triage")
            && b.contains("native_function_pseudocode")
            && b.contains("managed_assembly_show")
            && !b.contains("web_listeners_list"),
        "{b}"
    );
    let clean = brief(&prog, "app", false, false, None, false, None);
    assert!(!clean.contains("core_target_triage") && !clean.contains("native_") && clean.contains("ask the person with core_person_ask"), "{clean}");
}

#[test]
fn a_console_original_is_rebuilt_as_a_console_program() {
    let d = tempfile::tempdir().unwrap();
    let (tool, app) = (d.path().join("tool.exe"), d.path().join("app.exe"));
    std::fs::write(&tool, craft_anything_testkit::pe64_subsystem(3)).unwrap();
    std::fs::write(&app, craft_anything_testkit::pe64_subsystem(2)).unwrap();
    assert_eq!(choose("Rebuild this tool", &TargetRef::Local { path: tool }), ("app", "Rust (console)"));
    assert_eq!(choose("Rebuild this tool", &TargetRef::Local { path: app }).1, "Rust + egui (eframe)", "a windowed program stays a windowed app");
    let steps = stacks::steps("Rust (console)", None);
    assert!(steps.contains("console program") && steps.contains("exit codes") && steps.contains("draw no window"), "{steps}");
    assert_eq!(choose("A notes tool", &TargetRef::None).1, "Rust + egui (eframe)");
}

#[test]
fn a_note_the_builder_took_at_a_step_joins_the_feed_and_isnt_sent_again() {
    let d = tempfile::tempdir().unwrap();
    let mut ws = Workspace::create(d.path(), "Site", "Clone it", TargetRef::None).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let mut w = Worker {
        wake: Default::default(),
        engine: Arc::new(Engine::new(craft_anything_engine::testing::session())),
        tx,
        stop: CancelToken::default(),
        preview: Arc::default(),
        seen: HashSet::new(),
        inbox: Inbox::new(ws.inbox_dir()),
        editor: None,
        notes: Arc::new(Mutex::new(vec![
            Note { id: 1, text: "make the header blue".into(), shown: "make the header blue".into(), pictures: Vec::new() },
            Note { id: 2, text: "add a footer".into(), shown: "add a footer".into(), pictures: Vec::new() },
        ])),
        feed: Arc::new(Mutex::new(feed::FeedLog::open(&ws))),
        assets: false,
        decompiled: false,
        usage: None,
        self_check: None,
        reset_progress_after_notes: false,
    };
    let dir = notes::notes_dir(&ws.inbox_dir());
    notes::hand_over(&dir, 1, "make the header blue").unwrap();
    notes::hand_over(&dir, 2, "add a footer").unwrap();
    // The agent's hook takes the first one at a step; the second comes in after it.
    std::fs::rename(dir.join("2.json"), dir.join("2.wait")).unwrap();
    assert_eq!(notes::claim(&dir), ["make the header blue"]);
    std::fs::rename(dir.join("2.wait"), dir.join("2.json")).unwrap();
    w.relay_read_notes(&ws);
    let shown: Vec<String> = rx.try_iter().filter_map(|e| if let RunEvent::Note { shown, .. } = e { Some(shown) } else { None }).collect();
    assert_eq!(shown, ["make the header blue"], "it joins the feed as soon as the builder took it");
    assert!(!notes::was_read(&dir, 1), "its files are gone");
    // After the turn, only the note nobody took is sent, and no hook can hand it over again.
    let follow = w.added(&mut ws).unwrap();
    assert!(follow.contains("add a footer") && !follow.contains("make the header blue"), "{follow}");
    assert!(notes::claim(&dir).is_empty());
}

/// From a live Next.js + shadcn/ui clone of a fixture page (R2.3): the builder writes its own
/// headings, so sections pair by tag in page order when the headings differ, and a nav's link
/// count isn't a grid's items per row.
#[test]
fn design_notes_pair_renamed_sections_and_leave_nav_links_alone() {
    use kinds::website::design_notes;
    let block = |tag: &str, heading: &str, pad: u64, per_row: u64| json!({"tag": tag, "heading": heading, "pad": [pad, 0, pad, 0], "per_row": per_row, "column": {"max_w": 1100}});
    let layout = |blocks: Vec<Value>| json!({"layouts": [{"width": 768, "page_w": 753, "type": {"h1": 36}, "blocks": blocks}]});
    let original = layout(vec![block("nav", "", 0, 2), block("section", "Why people switch", 48, 2), block("section", "Plans", 48, 1)]);
    let same_but_renamed = layout(vec![block("nav", "", 0, 3), block("section", "Why people like it", 48, 2), block("section", "Plans", 48, 1)]);
    assert!(design_notes(&original, &same_but_renamed).is_empty(), "{:?}", design_notes(&original, &same_but_renamed));
    let tighter = layout(vec![block("nav", "", 0, 3), block("section", "Why people like it", 24, 2), block("section", "Plans", 48, 1)]);
    let notes = design_notes(&original, &tighter);
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("\"Why people switch\" section: padding 24 0 24 0 vs the original's 48 0 48 0"), "{notes:?}");
}

#[test]
fn a_project_in_a_picked_folder_tells_the_builder_what_its_files_are() {
    use craft_anything_engine::workspace::ExistingFiles;
    let d = tempfile::tempdir().unwrap();
    let settings = craft_anything_engine::Settings::default();
    let folder = |name: &str| {
        let f = d.path().join(name);
        std::fs::create_dir_all(&f).unwrap();
        std::fs::write(f.join("index.html"), "<p>hi</p>").unwrap();
        f
    };
    let usual = project::new_project(&settings, "-", "A notes app", project::Place::In(d.path()), None, false, None).unwrap();
    assert!(!brief(&usual, "app", false, false, None, false, None).contains("folder of their own"), "a usual project says nothing about it");

    let mine = folder("mine");
    let refused = project::new_project(&settings, "-", "Add a contact form", project::Place::At(&mine, None), None, false, None);
    assert!(refused.is_err(), "a folder with files needs to know what they are");
    let ws = project::new_project(&settings, "-", "Add a contact form", project::Place::At(&mine, Some(ExistingFiles::Edit)), None, false, None).unwrap();
    let b = brief(&ws, "website", false, false, None, false, None);
    assert!(b.contains("already has their files (index.html)") && b.contains("use the workspace folder itself (./)") && b.contains("craft.json"), "{b}");

    let start = folder("start");
    let ws = project::new_project(&settings, "-", "A site like this", project::Place::At(&start, Some(ExistingFiles::StartFrom)), None, false, None).unwrap();
    let b = brief(&ws, "website", false, false, None, false, None);
    assert!(b.contains("which already had these files: index.html") && b.contains("bring into app/"), "{b}");
    assert!(!b.contains("use the workspace folder itself"));
}

#[test]
fn a_project_started_in_a_picked_folder_stays_in_the_list() {
    let mut session = craft_anything_engine::testing::session();
    let d = tempfile::tempdir().unwrap();
    let ws = project::new_project(&session.settings, "-", "A notes app", project::Place::At(&d.path().join("mine"), None), None, false, None).unwrap();
    session.remember_project(&ws.root).unwrap();
    assert_eq!(session.settings.project_folders, std::slice::from_ref(&ws.root));
    let saved = craft_anything_engine::Settings::load_from(&session.config_dir().join("config.toml"));
    assert_eq!(saved.project_folders, std::slice::from_ref(&ws.root), "saved in the session's own settings");
    let listed = Workspace::list_in(&session.settings.project_locations());
    assert!(listed.iter().any(|w| w.root == ws.root));
}

#[test]
fn a_project_edited_in_place_keeps_what_its_files_are_built_with() {
    use craft_anything_engine::workspace::ExistingFiles::{Edit, StartFrom};
    let first = |file: &str, request: &str, existing| {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(file), "").unwrap();
        let ws = Workspace::create_at(d.path(), "Mine", request, TargetRef::None, Some(existing)).unwrap();
        project::first_stack(&KindRegistry::built_in(), &ws, false)
    };
    assert_eq!(first("project.godot", "Fix my game's jump", Edit), Some("Godot"), "not the Unity its words would get");
    assert_eq!(first("requirements.txt", "Add a settings page", Edit), Some("Python"), "not the app kind's default");
    assert_eq!(first("requirements.txt", "Move it to Electron", Edit), Some("Electron"), "the person's words come first");
    assert_eq!(first("notes.md", "Fix my game's jump", Edit), Some("Unity"), "nothing to read: as for a new project");
    assert_eq!(first("project.godot", "Fix my game's jump", StartFrom), Some("Unity"), "material to start from doesn't decide");
}

/// A folder of the person's own with this file in it, as a project of `existing`.
fn own_folder(file: &str, existing: craft_anything_engine::workspace::ExistingFiles) -> (tempfile::TempDir, Workspace) {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join(file), "<p>hi</p>").unwrap();
    let ws = Workspace::create_at(d.path(), "Mine", "Add a contact form", TargetRef::None, Some(existing)).unwrap();
    (d, ws)
}

#[test]
fn a_persons_own_project_is_judged_by_its_own_build_and_tests_whatever_its_kind() {
    use craft_anything_engine::workspace::ExistingFiles::Edit;
    let (_d, ws) = own_folder("index.html", Edit);
    let r = check_worker(&ws, false).run_checks(&ws, &GameKind, 1).unwrap();
    assert_eq!(r.iter().map(|c| c.check.as_str()).collect::<Vec<_>>(), [CheckResult::BUILD, kinds::own::OwnTestsCheck::ID]);
    assert!(r.iter().all(|c| c.passed), "{r:?}");
    assert!(r[1].note.starts_with("No tests ran"), "says what wasn't run: {}", r[1].note);
}

#[test]
fn a_persons_own_project_is_asked_for_nothing_it_would_add_for_our_sake() {
    use craft_anything_engine::workspace::ExistingFiles::{Edit, StartFrom};
    let (_d, mut own) = own_folder("main.py", Edit);
    for k in ["app", "game"] {
        let b = brief(&own, k, false, false, None, false, None);
        assert!(!b.contains("--screenshot") && !b.contains("--self-test") && !b.contains("MECHANICS.md"), "{k}: {b}");
        assert!(b.contains("its own build and tests") && !b.contains(kind(k).goal()), "{k}: {b}");
    }
    own.manifest.stack = "Unity".into();
    let b = brief(&own, "game", false, false, Some(Path::new("/x/Unity")), false, None);
    assert!(!b.contains("CraftBuild") && b.contains("keep to what the project already uses") && b.contains("/x/Unity"), "{b}");
    own.manifest.generate_assets = true;
    let b = brief(&own, "game", false, false, None, true, None);
    assert!(b.contains("Assets/Generated/") && !b.contains("--screenshot"), "what the person turned on still reaches it: {b}");
    let (_d, beside) = own_folder("main.py", StartFrom);
    let b = brief(&beside, "app", false, false, None, false, None);
    assert!(b.contains("--screenshot") && !b.contains("its own build and tests"), "a new app beside the files proves itself: {b}");
}

#[test]
fn a_site_stack_with_nothing_to_recreate_is_a_website() {
    let vite = "React + TypeScript (Vite)";
    assert_eq!(kind_of("Add a contact form", &TargetRef::None, "Plain HTML and CSS"), "website");
    assert_eq!(kind_of("A calculator in react", &TargetRef::None, vite), "website", "it can't be started as a program");
    assert_eq!(kind_of("Add a high score to my game", &TargetRef::None, vite), "game", "the person's words come first");
    assert_eq!(kind_of("Add a settings page", &TargetRef::None, "Python"), "app");
    assert_eq!(kind_of("Add a settings page", &TargetRef::None, "Electron"), "app", "a window, not a site");
    assert_eq!(kind_of("Copy this tool", &TargetRef::Local { path: "/x/tool".into() }, vite), "app", "an original decides");
}

#[test]
fn the_extractors_transcript_is_one_of_craft_anythings_own_files() {
    assert!(craft_anything_engine::workspace::OWN_NAMES.contains(&brief::EXTRACT_TRANSCRIPT), "a picked folder with this name would be overwritten");
}

#[test]
fn a_project_is_built_by_the_ai_it_was_started_with_while_that_one_is_on() {
    use crate::{AiConfig, BackendConfig, BackendEntry, BackendKind};
    let d = tempfile::tempdir().unwrap();
    let mut ws = Workspace::create(d.path(), "Notes", "A notes app", TargetRef::None).unwrap();
    ws.manifest.builder = Some("Codex".into());
    let mut cfg = AiConfig::default();
    for (name, enabled) in [("Claude Code", true), ("Codex", true)] {
        let c = BackendConfig { name: name.into(), kind: BackendKind::Nvidia, model: String::new(), base_url: None, ssh_host: None, enabled };
        cfg.upsert(BackendEntry { config: c, options: Default::default() });
    }
    assert_eq!(super::project_builder(&cfg, &ws), Some("Codex"));
    assert_eq!(builder_backend(&cfg, super::project_builder(&cfg, &ws)).unwrap().config.name, "Codex");
    // Turned off (or removed): the default builds it instead of failing.
    cfg.backends.iter_mut().for_each(|b| b.config.enabled = b.config.name != "Codex");
    assert_eq!(super::project_builder(&cfg, &ws), None);
    assert_eq!(builder_backend(&cfg, super::project_builder(&cfg, &ws)).unwrap().config.name, "Claude Code");
}
