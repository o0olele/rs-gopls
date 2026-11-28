use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::{GoEnvironment, initialize, serve_state};

const ORIGIN: &str = "package sample\nvar Anchor int\nvar _ = Ex\n";

fn hold_checkpoint(cancel: &AtomicBool, observed: &Sender<()>, blocked: &Receiver<()>) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = observed.try_send(());
        }
        if !matches!(
            blocked.recv_timeout(Duration::from_millis(1)),
            Err(crossbeam_channel::RecvTimeoutError::Timeout)
        ) || Instant::now() >= deadline
        {
            return;
        }
    }
}

#[test]
fn completion_worker_does_not_block_live_navigation_edits_or_ready_diagnostics() {
    for stage in ["local_candidates", "expand_file", "render"] {
        let project = TestProject::new();
        let origin = project.write("api.go", ORIGIN);
        let other_text = "package sample\nvar Other int\nfunc report() { unused := 1 }\n";
        let other = project.write("other.go", other_text);
        let dependency = project.write("dep/lib.go", "package dep\nvar Exported int\n");
        let root = project.root.clone();
        let origin_key = path_key(&origin);
        let (server, client) = Connection::memory();
        let (entered, started) = crossbeam_channel::bounded(1);
        let (observed, cancelled) = crossbeam_channel::bounded(1);
        let (release, blocked) = crossbeam_channel::bounded(1);
        let server_thread = thread::spawn(move || -> Result<()> {
            let (mut running, _) = initialize(
                json!({
                    "rootUri": path_uri(&root).unwrap(),
                    "capabilities": {"textDocument": {"diagnostic": {}}},
                }),
                GoEnvironment::capture(),
                None,
            )?;
            let production = json!({
                "Dir": root, "ImportPath": "example.test/sample", "Name": "sample",
                "GoFiles": ["api.go", "other.go"],
            });
            let imports = json!({
                "Dir": dependency.parent().unwrap(), "ImportPath": "example.test/dep",
                "Name": "dep", "GoFiles": ["lib.go"],
            });
            let workspace =
                Workspace::from_test_listing(&[root], &format!("{production}\n{imports}"), false);
            assert!(
                running
                    .engine
                    .install_workspace(running.engine.workspace_load(), workspace)
            );
            running.engine.use_empty_completion_sdk();
            let loop_thread = thread::current().id();
            let first = AtomicBool::new(true);
            running
                .engine
                .set_completion_hook(Some(Arc::new(move |path, current, cancel| {
                    if current != stage
                        || path_key(path) != origin_key
                        || !first.swap(false, Ordering::Relaxed)
                    {
                        return;
                    }
                    assert_ne!(loop_thread, thread::current().id());
                    entered.send(()).unwrap();
                    hold_checkpoint(cancel, &observed, &blocked);
                })));
            serve_state(&server, State::Running(Box::new(running)))
        });

        for (path, text) in [(&origin, ORIGIN), (&other, other_text)] {
            client.sender.send(Notification::new(DidOpenTextDocument::METHOD.into(), json!({
                "textDocument": {"uri": path_uri(path).unwrap(), "languageId": "go", "version": 1, "text": text},
            })).into()).unwrap();
        }
        let request = |id, method: &str, params| {
            client
                .sender
                .send(Request::new(id, method.into(), params).into())
                .unwrap();
        };
        let next = || {
            let message = client
                .receiver
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
            let Message::Response(response) = message else {
                panic!("{stage}: expected a response, got {message:?}");
            };
            response
        };
        request(1.into(), Completion::METHOD, position(&origin, 2, 10));
        started.recv_timeout(Duration::from_secs(10)).expect(stage);
        request(2.into(), GotoDefinition::METHOD, position(&other, 1, 5));
        request(
            3.into(),
            DocumentDiagnosticRequest::METHOD,
            json!({
                "textDocument": {"uri": path_uri(&other).unwrap()},
            }),
        );
        let mut foreground = HashMap::new();
        for _ in 0..2 {
            let response = next();
            foreground.insert(response.id, response.result.unwrap());
        }
        assert_eq!(
            foreground[&RequestId::from(2)]["range"]["end"]["character"],
            9
        );
        assert_eq!(foreground[&RequestId::from(3)]["kind"], "full");
        assert!(cancelled.is_empty());

        client
            .sender
            .send(
                Notification::new(
                    DidChangeTextDocument::METHOD.into(),
                    json!({
                        "textDocument": {"uri": path_uri(&origin).unwrap(), "version": 2},
                        "contentChanges": [{"text": ORIGIN.replace("Anchor", "Current")}],
                    }),
                )
                .into(),
            )
            .unwrap();
        request(4.into(), GotoDefinition::METHOD, position(&origin, 1, 5));
        let stale = next();
        assert_eq!(stale.id, 1.into());
        assert_eq!(stale.error.unwrap().code, ErrorCode::ContentModified as i32);
        let change = next();
        assert_eq!(change.id, 4.into());
        assert_eq!(change.result.unwrap()["range"]["end"]["character"], 11);
        cancelled.recv_timeout(Duration::from_secs(1)).expect(stage);
        release.send(()).unwrap();

        request(5.into(), Completion::METHOD, position(&origin, 2, 10));
        let latest = next();
        assert_eq!(latest.id, 5.into(), "{stage}: duplicate terminal response");
        assert!(
            latest
                .result
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["label"] == "Exported")
        );
        request(6.into(), Shutdown::METHOD, Value::Null);
        let shutdown = next();
        assert_eq!(shutdown.id, 6.into());
        client
            .sender
            .send(Notification::new(Exit::METHOD.into(), Value::Null).into())
            .unwrap();
        server_thread.join().unwrap().unwrap();
        assert!(client.receiver.is_empty());
    }
}

#[test]
fn latest_completion_replaces_the_only_pending_request_and_worker_cancels_once() {
    let project = TestProject::new();
    let text = "package sample\nvar Exported int\nvar _ = Ex\n";
    let path = project.write("api.go", text);
    let mut session = Session::start(&project);
    session.open(&path, text);
    let load = session.running().engine.workspace_load();
    session.deliver(load, Ok(project.workspace(&["api.go"], false)));
    session.running().engine.use_empty_completion_sdk();
    let (entered, started) = crossbeam_channel::bounded(1);
    let (observed, cancelled) = crossbeam_channel::bounded(1);
    let (release, blocked) = crossbeam_channel::bounded(1);
    let first = AtomicBool::new(true);
    session
        .running()
        .engine
        .set_completion_hook(Some(Arc::new(move |_, stage, cancel| {
            if stage != "local_candidates" || !first.swap(false, Ordering::Relaxed) {
                return;
            }
            entered.send(()).unwrap();
            hold_checkpoint(cancel, &observed, &blocked);
        })));
    session.request(1, Completion::METHOD, position(&path, 2, 10));
    started.recv_timeout(Duration::from_secs(10)).unwrap();
    session.request(2, Completion::METHOD, position(&path, 2, 10));
    session.request(3, Completion::METHOD, position(&path, 2, 10));
    session.error(
        1,
        ErrorCode::RequestCanceled,
        "a newer completion superseded the request",
    );
    session.error(
        2,
        ErrorCode::RequestCanceled,
        "a newer completion superseded the request",
    );
    cancelled.recv_timeout(Duration::from_secs(1)).unwrap();
    session.cancel(3);
    session.cancel(3);
    session.error(
        3,
        ErrorCode::RequestCanceled,
        "the client cancelled the request",
    );
    session.count(4);

    session.request(4, Completion::METHOD, position(&path, 2, 10));
    release.send(()).unwrap();
    session.step_completion();
    session.count(4);
    session.step_completion();
    let items = session
        .reply(4)
        .result
        .as_ref()
        .unwrap()
        .as_array()
        .unwrap();
    assert!(items.iter().any(|item| item["label"] == "Exported"));
    session.count(5);
}

#[test]
fn completion_resumes_after_workspace_failure_without_losing_local_candidates() {
    let project = TestProject::new();
    let text = "package sample\nvar Exported int\nvar _ = Ex\n";
    let path = project.write("api.go", text);
    let mut session = Session::start(&project);
    session.open(&path, text);
    session.request(1, Completion::METHOD, position(&path, 2, 10));
    session.step_completion();
    assert!(session.scheduler().completion_waiting_for_workspace());
    session.count(1);
    let load = session.begin_load();
    session.deliver(load, Err(anyhow!("test workspace load failed")));
    session.step_completion();
    let items = session
        .reply(1)
        .result
        .as_ref()
        .unwrap()
        .as_array()
        .unwrap();
    assert!(items.iter().any(|item| item["label"] == "Exported"));
    session.count(2);
}
