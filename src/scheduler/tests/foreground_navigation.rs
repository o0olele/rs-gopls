use std::sync::Arc;

use lsp_types::request::GotoTypeDefinition;

use super::*;
use crate::semantic::{CacheEvent, DefinitionKind};

fn install(session: &mut Session, project: &TestProject, files: &[&str]) {
    let engine = &mut session.running().engine;
    let load = engine.workspace_load();
    assert!(engine.install_workspace(load, project.workspace(files, load.include_tests)));
}

fn block_load(
    session: &mut Session,
    path: &Path,
    wait_for: CacheEvent,
) -> (Receiver<()>, Sender<()>, Receiver<thread::ThreadId>) {
    let key = path_key(path);
    let (entered, started) = crossbeam_channel::bounded(1);
    let (release, blocked) = crossbeam_channel::bounded(1);
    let (worker, producer) = crossbeam_channel::bounded(1);
    session
        .running()
        .engine
        .set_shared_disk_hook(Some(Arc::new(move |path, event| {
            if path == key && event == wait_for {
                let _ = worker.try_send(thread::current().id());
                let _ = entered.try_send(());
                let _ = blocked.recv_timeout(Duration::from_secs(5));
            }
        })));
    (started, release, producer)
}

fn wait_for_load(session: &mut Session, started: &Receiver<()>) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while started.try_recv().is_err() {
        assert!(Instant::now() < deadline, "cold source load did not start");
        session.step();
        thread::yield_now();
    }
}

#[test]
fn cold_navigation_requests_produce_in_background_and_preserve_utf16_results() {
    for (method, block_origin, block_event) in [
        (GotoDefinition::METHOD, false, CacheEvent::Read),
        (GotoTypeDefinition::METHOD, true, CacheEvent::Read),
        (HoverRequest::METHOD, false, CacheEvent::Read),
        (HoverRequest::METHOD, false, CacheEvent::Parse),
    ] {
        let project = TestProject::new();
        let (text, dependency) = if block_event == CacheEvent::Parse {
            (
                "package sample\nfunc f() { _ = \"😀\"; _ = V }\n",
                "package sample\n// V describes an emoji 😀.\nvar V = value()\nfunc value() int { return 0 }\n",
            )
        } else {
            (
                "package sample\nfunc f() { _ = \"😀\"; _ = T{} }\n",
                "package sample\n// T describes an emoji 😀.\ntype T struct{}\n",
            )
        };
        let origin = project.write("a.go", text);
        let definition = project.write("b.go", dependency);
        let unrelated = project.write("other.go", "package sample\nvar Other int\n");

        let options = if method == GotoTypeDefinition::METHOD {
            DefinitionKind::Type
        } else {
            DefinitionKind::Declaration
        };
        let mut baseline = Session::start(&project);
        install(&mut baseline, &project, &["a.go", "b.go", "other.go"]);
        let engine = &mut baseline.running().engine;
        let caret = Position::new(1, 25);
        let expected = if method == HoverRequest::METHOD {
            let QueryProgress::Ready(value) = engine.hover(&origin, caret).unwrap() else {
                panic!("expected the baseline hover");
            };
            serde_json::to_value(value).unwrap()
        } else {
            let QueryProgress::Ready(value) = engine.definition(&origin, caret, options).unwrap()
            else {
                panic!("expected the baseline location");
            };
            let (path, range) = value.unwrap();
            assert_eq!(path, definition);
            assert_eq!(range.start, Position::new(2, 5));
            assert_eq!(range.end, Position::new(2, 6));
            json!({"uri": path_uri(&path).unwrap(), "range": range})
        };

        let mut session = Session::start(&project);
        if !block_origin {
            session.open(&origin, text);
        }
        session.open(&unrelated, "package sample\nvar Other int\n");
        install(&mut session, &project, &["a.go", "b.go", "other.go"]);
        let blocked = if block_origin { &origin } else { &definition };
        let (started, release, producer) = block_load(&mut session, blocked, block_event);
        session.request(1, method, position(&origin, caret.line, caret.character));
        session.expect_next(1);
        wait_for_load(&mut session, &started);
        assert_ne!(
            producer.try_recv().unwrap(),
            thread::current().id(),
            "the main loop must not produce a cold source artifact",
        );
        session.count(1);

        session.edit(&unrelated, 2, "package sample\nvar Edited int\n");
        session.request(2, GotoDefinition::METHOD, position(&unrelated, 1, 5));
        assert_eq!(
            session.reply(2).result.as_ref().unwrap()["range"]["end"]["character"],
            10,
        );
        session.count(2);

        release.send(()).unwrap();
        session.finish_searches();
        assert_eq!(
            session.reply(1).result.as_ref().unwrap(),
            &expected,
            "{method}"
        );
        session.count(3);
    }
}

#[test]
fn foreground_navigation_delivers_one_terminal_response_after_invalidation() {
    for action in [
        "cancel",
        "edit",
        "graph",
        "delete",
        "unreadable",
        "during parse",
        "watch epoch",
    ] {
        let project = TestProject::new();
        let path = project.write("cold.go", "package sample\nvar Cold int\n");
        let mut session = Session::start(&project);
        install(&mut session, &project, &["cold.go"]);
        let event = if action == "during parse" {
            CacheEvent::Parse
        } else {
            CacheEvent::Read
        };
        let (started, release, _) = block_load(&mut session, &path, event);
        session.request(1, GotoDefinition::METHOD, position(&path, 1, 5));
        wait_for_load(&mut session, &started);
        session.count(1);

        let code = match action {
            "cancel" => {
                session.cancel(1);
                ErrorCode::RequestCanceled
            }
            "edit" => {
                session.open(&path, "package sample\nvar Changed int\n");
                ErrorCode::ContentModified
            }
            "graph" => {
                let original = session.running().engine.workspace_load();
                install(&mut session, &project, &["cold.go"]);
                assert_eq!(session.running().engine.workspace_load(), original);
                ErrorCode::ContentModified
            }
            "delete" => {
                std::fs::remove_file(&path).unwrap();
                ErrorCode::RequestFailed
            }
            "unreadable" => {
                std::fs::write(&path, [0xFF, 0xFF, 0xFF]).unwrap();
                ErrorCode::RequestFailed
            }
            "during parse" => {
                std::fs::write(&path, "package sample\n// new content\nvar Changed int\n").unwrap();
                ErrorCode::ContentModified
            }
            "watch epoch" => {
                let other = project.write("other.go", "package sample\nvar Other int\n");
                session
                    .running()
                    .engine
                    .file_changed(&other, FileChangeType::CREATED);
                ErrorCode::ContentModified
            }
            _ => unreachable!(),
        };
        release.send(()).unwrap();
        session.finish_searches();
        let response = session.reply(1);
        let error = response.error.as_ref().unwrap();
        // A file replacement can also exhaust the loader's revision checks.
        assert!(
            error.code == code as i32
                || (matches!(action, "delete" | "unreadable")
                    && error.code == ErrorCode::ContentModified as i32),
            "{action}: {response:?}",
        );
        session.step();
        session.count(2);
    }
}

#[test]
fn navigation_waits_for_the_watched_change_before_using_warm_disk_syntax() {
    let project = TestProject::new();
    let path = project.write("cold.go", "package sample\nvar Name int\n");
    let mut session = Session::start(&project);
    install(&mut session, &project, &["cold.go"]);
    session.request(1, GotoDefinition::METHOD, position(&path, 1, 5));
    session.finish_searches();
    assert_eq!(
        session.reply(1).result.as_ref().unwrap()["range"]["start"]["line"],
        1
    );

    std::fs::write(&path, "package sample\n// moved\nvar Name int\n").unwrap();
    session.notify(
        DidChangeWatchedFiles::METHOD,
        json!({"changes": [{"uri": path_uri(&path).unwrap(), "type": FileChangeType::CHANGED}]}),
    );
    session.request(2, GotoDefinition::METHOD, position(&path, 2, 5));
    assert_eq!(session.scheduler().open_requests.len(), 1);
    session.step();
    session.count(2);
    session.flush_watches();
    session.finish_searches();
    assert_eq!(
        session.reply(2).result.as_ref().unwrap()["range"]["start"]["line"],
        2
    );
    session.count(3);
}
