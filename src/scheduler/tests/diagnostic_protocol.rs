use lsp_types::request::WorkspaceDiagnosticRefresh;

use super::*;

const CHANGED: &str = "package sample\nfunc f() { _ = Missing }\nfunc g() { unused := 1 }\n";
const EQUIVALENT: &str = "package sample\nfunc f() { _ = Missing }\n";
const CLEAN: &str = "package sample\nfunc f() {}\n";

fn pull_capabilities(refresh: bool, push: bool) -> Value {
    let mut caps = json!({
        "workspace": { "diagnostics": { "refreshSupport": refresh } },
        "textDocument": { "diagnostic": {} },
    });
    if push {
        caps["textDocument"]["publishDiagnostics"] = json!({ "versionSupport": true });
    }
    caps
}

fn push_capabilities(versioned: bool) -> Value {
    json!({ "textDocument": { "publishDiagnostics": { "versionSupport": versioned } } })
}

fn loaded(project: &TestProject, files: &[&str], capabilities: Value) -> Session {
    let mut session = Session::with_capabilities(project, capabilities);
    let engine = &mut session.running().engine;
    let load = engine.workspace_load();
    assert!(engine.install_workspace(load, project.workspace(files, load.include_tests)));
    session
}

fn pull(session: &mut Session, id: i32, path: &Path, previous: Option<&Value>) {
    let mut params = json!({ "textDocument": { "uri": path_uri(path).unwrap() } });
    if let Some(previous) = previous {
        params["previousResultId"] = previous.clone();
    }
    session.request(id, DocumentDiagnosticRequest::METHOD, params);
}

fn codes(items: &Value) -> Vec<&str> {
    let mut codes: Vec<_> = items
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["code"].as_str().unwrap())
        .collect();
    codes.sort_unstable();
    codes
}

fn delivery_deadline(session: &mut Session) -> Instant {
    let State::Running(running) = &mut session.state else {
        unreachable!();
    };
    let deadline = running.scheduler.diagnostic_deadline().unwrap();
    running
        .scheduler
        .advance_diagnostics(&mut running.engine, &session.server, deadline)
        .unwrap();
    running.collect_diagnostic_refresh(deadline);
    deadline
}

fn finish_stage1(session: &mut Session) {
    let mut completed = session.take_diagnostic_completion();
    completed.set_ready_at(Instant::now() + Duration::from_secs(1));
    session.complete_diagnostic(completed);
}

fn finish_stage2_within_budget(session: &mut Session) {
    let mut completed = session.take_diagnostic_completion();
    if let Some(deadline) = session.running().scheduler.diagnostic_deadline() {
        completed.set_ready_at(deadline - Duration::from_millis(1));
    }
    session.complete_diagnostic(completed);
}

fn refresh_deadline(session: &mut Session) -> Instant {
    session
        .running()
        .diagnostic_refresh
        .as_ref()
        .unwrap()
        .deadline()
        .unwrap()
}

fn take_refresh(session: &Session) -> Request {
    let Message::Request(request) = session.client.receiver.try_recv().unwrap() else {
        panic!("expected a workspace diagnostic refresh");
    };
    assert_eq!(request.method, WorkspaceDiagnosticRefresh::METHOD);
    assert!(request.params.is_null());
    request
}

fn ack(session: &mut Session, id: &RequestId) {
    session
        .running()
        .diagnostic_refresh
        .as_mut()
        .unwrap()
        .response(&Response::new_ok(id.clone(), ()));
}

fn publication(session: &Session, path: &Path, version: Option<i32>) -> Value {
    let Message::Notification(notification) = session.client.receiver.try_recv().unwrap() else {
        panic!("expected a diagnostic publication");
    };
    assert_eq!(notification.method, "textDocument/publishDiagnostics");
    assert_eq!(notification.params["uri"], json!(path_uri(path).unwrap()));
    assert_eq!(notification.params["version"], json!(version));
    notification.params["diagnostics"].clone()
}

fn close(session: &mut Session, path: &Path) {
    session.notify(
        DidCloseTextDocument::METHOD,
        json!({ "textDocument": { "uri": path_uri(path).unwrap() } }),
    );
}

#[test]
fn pull_fast_report_is_replaced_by_a_cached_terminal_report_after_refresh() {
    let project = TestProject::new();
    let path = project.write("api.go", CHANGED);
    for dual in [false, true] {
        let mut session = loaded(&project, &["api.go"], pull_capabilities(true, dual));
        session.open(&path, CHANGED);
        assert!(session.client.receiver.is_empty());
        pull(&mut session, 1, &path, None);
        finish_stage1(&mut session);
        assert!(session.client.receiver.is_empty());
        delivery_deadline(&mut session);
        let stage1 = session.reply(1).result.clone().unwrap();
        assert_eq!(stage1["kind"], "full");
        assert_eq!(codes(&stage1["items"]), ["name.unresolved"]);
        session.finish_diagnostic_job();

        let refresh = refresh_deadline(&mut session);
        session.advance_diagnostics(refresh - Duration::from_millis(1));
        assert!(session.client.receiver.is_empty());
        session.advance_diagnostics(refresh);
        let request = take_refresh(&session);

        // A Pull can consume the complete cached report while the refresh ACK is pending.
        pull(&mut session, 2, &path, Some(&stage1["resultId"]));
        let stage2 = session.reply(2).result.clone().unwrap();
        assert_eq!(stage2["kind"], "full");
        assert_eq!(codes(&stage2["items"]), ["name.unresolved", "var.unused"]);
        assert_ne!(stage2["resultId"], stage1["resultId"]);
        pull(&mut session, 3, &path, Some(&stage2["resultId"]));
        assert_eq!(
            session.reply(3).result.as_ref().unwrap(),
            &json!({ "kind": "unchanged", "resultId": stage2["resultId"] })
        );
        ack(&mut session, &request.id);
        close(&mut session, &path);
        session.count(4);
    }
}

#[test]
fn terminal_reports_within_budget_and_equivalent_promotions_need_no_refresh() {
    let project = TestProject::new();
    for (text, stage2_within_budget, expected) in [
        (CHANGED, true, &["name.unresolved", "var.unused"][..]),
        (EQUIVALENT, false, &["name.unresolved"][..]),
        (CLEAN, false, &[][..]),
    ] {
        let path = project.write("api.go", text);
        let mut session = loaded(&project, &["api.go"], pull_capabilities(true, false));
        session.open(&path, text);
        pull(&mut session, 1, &path, None);
        finish_stage1(&mut session);
        if stage2_within_budget {
            finish_stage2_within_budget(&mut session);
        } else {
            delivery_deadline(&mut session);
        }
        let first = session.reply(1).result.clone().unwrap();
        assert_eq!(codes(&first["items"]), expected);
        if !stage2_within_budget {
            session.finish_diagnostic_job();
        }
        pull(&mut session, 2, &path, Some(&first["resultId"]));
        assert_eq!(
            session.reply(2).result.as_ref().unwrap(),
            &json!({ "kind": "unchanged", "resultId": first["resultId"] })
        );
        assert!(session.running().diagnostic_deadline().is_none());
        session.count(3);
    }
}

#[test]
fn versioned_push_replaces_same_version_and_others_publish_a_single_complete_report() {
    let project = TestProject::new();
    let path = project.write("api.go", CHANGED);
    for (versioned, text, early, terminal) in [
        (
            true,
            CHANGED,
            true,
            Some(&["name.unresolved", "var.unused"][..]),
        ),
        (true, EQUIVALENT, true, None),
        (true, CHANGED, false, None),
        (false, CHANGED, false, None),
    ] {
        let mut session = loaded(&project, &["api.go"], push_capabilities(versioned));
        session.open(&path, text);
        if versioned {
            finish_stage1(&mut session);
        } else {
            session.finish_diagnostic_job();
        }
        if early {
            assert!(session.client.receiver.is_empty());
            delivery_deadline(&mut session);
            assert_eq!(
                codes(&publication(&session, &path, Some(1))),
                ["name.unresolved"]
            );
            session.finish_diagnostic_job();
            if let Some(terminal) = terminal {
                assert_eq!(codes(&publication(&session, &path, Some(1))), terminal);
            }
        } else {
            if versioned {
                assert!(session.client.receiver.is_empty());
                finish_stage2_within_budget(&mut session);
            }
            assert_eq!(
                codes(&publication(&session, &path, Some(1))),
                ["name.unresolved", "var.unused"]
            );
        }
        assert!(session.client.receiver.is_empty());
        close(&mut session, &path);
        assert!(codes(&publication(&session, &path, None)).is_empty());
        session.open(&path, CLEAN);
        if versioned {
            finish_stage1(&mut session);
            finish_stage2_within_budget(&mut session);
        } else {
            session.finish_diagnostic_job();
        }
        assert!(codes(&publication(&session, &path, Some(1))).is_empty());
        assert!(session.client.receiver.is_empty());
    }
}

#[test]
fn pull_without_refresh_returns_only_terminal_and_no_capability_disables_active_delivery() {
    let project = TestProject::new();
    let path = project.write("api.go", CHANGED);
    for capabilities in [
        pull_capabilities(false, false),
        pull_capabilities(false, true),
        json!({ "workspace": { "diagnostics": { "refreshSupport": true } } }),
    ] {
        let mut session = loaded(&project, &["api.go"], capabilities);
        let load = session.running().engine.workspace_load();
        session.deliver(load, Ok(project.workspace(&["api.go"], load.include_tests)));
        session.open(&path, CHANGED);
        assert!(session.running().diagnostic_deadline().is_none());
        assert!(session.scheduler().diagnostic_completions().is_none());
        assert!(session.client.receiver.is_empty());
        pull(&mut session, 1, &path, None);
        session.finish_diagnostic_job();
        let result = session.reply(1).result.clone().unwrap();
        assert_eq!(codes(&result["items"]), ["name.unresolved", "var.unused"]);
        pull(&mut session, 2, &path, Some(&result["resultId"]));
        assert_eq!(
            session.reply(2).result.as_ref().unwrap()["kind"],
            "unchanged"
        );
        close(&mut session, &path);
        session.count(3);
    }
}

#[test]
fn refresh_marks_are_consumed_or_discarded_before_send() {
    let project = TestProject::new();
    let path = project.write("api.go", CHANGED);
    for invalidate in ["pull", "edit", "close"] {
        let mut session = loaded(&project, &["api.go"], pull_capabilities(true, false));
        session.open(&path, CHANGED);
        pull(&mut session, 1, &path, None);
        finish_stage1(&mut session);
        delivery_deadline(&mut session);
        let fast = session.reply(1).result.clone().unwrap();
        session.finish_diagnostic_job();
        let refresh = refresh_deadline(&mut session);
        match invalidate {
            "pull" => {
                pull(&mut session, 2, &path, Some(&fast["resultId"]));
                assert_eq!(
                    codes(&session.reply(2).result.as_ref().unwrap()["items"]).len(),
                    2
                );
            }
            "edit" => session.edit(&path, 2, CLEAN),
            "close" => close(&mut session, &path),
            _ => unreachable!(),
        }
        session.advance_diagnostics(refresh);
        assert!(session.client.receiver.is_empty());
        assert!(
            session
                .running()
                .diagnostic_refresh
                .as_ref()
                .unwrap()
                .deadline()
                .is_none()
        );
    }
}

#[test]
fn global_refresh_merges_document_and_workspace_marks_while_pulls_keep_running() {
    let project = TestProject::new();
    let files = ["a.go", "b.go", "c.go"];
    let paths = files.map(|name| {
        let function = &name[..1];
        project.write(
            name,
            &format!("package sample\nfunc {function}() {{ unused := 1; _ = Missing }}\n"),
        )
    });
    let mut session = loaded(&project, &files, pull_capabilities(true, false));
    for path in &paths {
        session.open(path, &std::fs::read_to_string(path).unwrap());
    }
    let slow_stage2 = |session: &mut Session, path: &Path, id| {
        pull(session, id, path, None);
        finish_stage1(session);
        delivery_deadline(session);
        let fast = session.reply(id).result.clone().unwrap();
        assert_eq!(codes(&fast["items"]), ["name.unresolved"]);
        session.finish_diagnostic_job();
        fast
    };
    let a = slow_stage2(&mut session, &paths[0], 1);
    let b = slow_stage2(&mut session, &paths[1], 2);
    let load = session.running().engine.workspace_load();
    session.deliver(load, Ok(project.workspace(&files, load.include_tests)));
    let first_due = refresh_deadline(&mut session);
    session.advance_diagnostics(first_due);
    let refresh = take_refresh(&session);
    let c = slow_stage2(&mut session, &paths[2], 3);

    // A change received after the refresh was sent is applied before any
    // resulting re-pull. The other documents can use cached Stage 2 while ACK waits.
    session.edit(&paths[0], 2, "package sample\nfunc a() {}\n");
    pull(&mut session, 4, &paths[0], Some(&a["resultId"]));
    delivery_deadline(&mut session);
    finish_stage1(&mut session);
    finish_stage2_within_budget(&mut session);
    assert!(codes(&session.reply(4).result.as_ref().unwrap()["items"]).is_empty());
    pull(&mut session, 5, &paths[1], Some(&b["resultId"]));
    assert_eq!(
        codes(&session.reply(5).result.as_ref().unwrap()["items"]).len(),
        2
    );

    let second_due = first_due + Duration::from_millis(250);
    session.advance_diagnostics(second_due);
    assert!(session.client.receiver.is_empty());
    ack(&mut session, &refresh.id);
    let deadline = refresh_deadline(&mut session).max(second_due);
    session.advance_diagnostics(deadline);
    let second = take_refresh(&session);
    assert_ne!(second.id, refresh.id);
    pull(&mut session, 6, &paths[2], Some(&c["resultId"]));
    assert_eq!(
        codes(&session.reply(6).result.as_ref().unwrap()["items"]).len(),
        2
    );
    ack(&mut session, &second.id);
    session.count(7);
}

#[test]
fn late_workers_and_out_of_order_requests_keep_open_generations_and_versions_monotonic() {
    let project = TestProject::new();
    let path = project.write("api.go", CHANGED);
    let mut session = loaded(&project, &["api.go"], pull_capabilities(true, false));
    session.open(&path, CHANGED);
    pull(&mut session, 4, &path, None);
    pull(&mut session, 2, &path, None);
    let stale = session.take_diagnostic_completion();
    session.cancel(4);
    session.edit(&path, 2, CLEAN);
    session.edit(&path, 1, CHANGED); // Older sync notifications cannot roll the buffer back.
    assert_eq!(session.reply(4).error.as_ref().unwrap().code, -32800);
    assert_eq!(session.reply(2).error.as_ref().unwrap().code, -32801);
    pull(&mut session, 1, &path, None);
    session.complete_diagnostic(stale);
    delivery_deadline(&mut session);
    finish_stage1(&mut session);
    finish_stage2_within_budget(&mut session);
    let clean = session.reply(1).result.clone().unwrap();
    assert!(codes(&clean["items"]).is_empty());

    close(&mut session, &path);
    session.open(&path, EQUIVALENT);
    pull(&mut session, 3, &path, Some(&clean["resultId"]));
    finish_stage1(&mut session);
    finish_stage2_within_budget(&mut session);
    let reopened = session.reply(3).result.as_ref().unwrap();
    assert_eq!(reopened["kind"], "full");
    assert_eq!(codes(&reopened["items"]), ["name.unresolved"]);
    assert_ne!(reopened["resultId"], clean["resultId"]);
    session.count(5);
}

#[test]
fn dependency_changes_invalidate_a_report_at_the_same_lsp_version() {
    let project = TestProject::new();
    let body = "package sample\nfunc f() { _ = Exported }\n";
    let dependency = "package sample\nvar Exported = 1\n";
    let path = project.write("api.go", body);
    let other = project.write("dep.go", dependency);
    let mut session = loaded(
        &project,
        &["api.go", "dep.go"],
        pull_capabilities(false, false),
    );
    session.open(&path, body);
    session.open(&other, dependency);
    pull(&mut session, 1, &path, None);
    let old = session.take_diagnostic_completion();
    session.edit(&other, 2, "package sample\nvar Renamed = 1\n");
    assert_eq!(session.reply(1).error.as_ref().unwrap().code, -32801);
    pull(&mut session, 2, &path, None);
    session.complete_diagnostic(old);
    session.finish_diagnostic_job();
    let missing = session.reply(2).result.clone().unwrap();
    assert_eq!(codes(&missing["items"]), ["name.unresolved"]);
    session.edit(&other, 3, dependency);
    pull(&mut session, 3, &path, Some(&missing["resultId"]));
    session.finish_diagnostic_job();
    let resolved = session.reply(3).result.as_ref().unwrap();
    assert_eq!(resolved["kind"], "full");
    assert!(codes(&resolved["items"]).is_empty());
    session.count(4);
}

#[test]
fn unsaved_dependency_edit_republishes_previously_delivered_push_at_the_same_version() {
    let project = TestProject::new();
    let body = "package sample\nfunc f() { _ = Exported }\n";
    let dependency = "package sample\nvar Exported = 1\n";
    let path = project.write("api.go", body);
    let other = project.write("dep.go", dependency);
    let mut session = loaded(&project, &["api.go", "dep.go"], push_capabilities(false));
    session
        .running()
        .engine
        .open(other.clone(), dependency.into(), 1)
        .unwrap();
    session.open(&path, body);
    session.finish_diagnostic_job();
    assert!(codes(&publication(&session, &path, Some(1))).is_empty());

    session.edit(&other, 2, "package sample\nvar Renamed = 1\n");
    session.finish_diagnostic_job();
    assert_eq!(
        codes(&publication(&session, &path, Some(1))),
        ["name.unresolved"]
    );
    delivery_deadline(&mut session);
    session.finish_diagnostic_job();
    assert!(codes(&publication(&session, &other, Some(2))).is_empty());

    session.edit(&other, 3, dependency);
    session.finish_diagnostic_job();
    assert!(codes(&publication(&session, &path, Some(1))).is_empty());
    delivery_deadline(&mut session);
    session.finish_diagnostic_job();
    assert!(codes(&publication(&session, &other, Some(3))).is_empty());
    assert!(session.client.receiver.is_empty());
}

#[test]
fn serve_wakes_for_refresh_and_answers_pull_before_the_client_replies() {
    let project = TestProject::new();
    project.write("go.mod", "module example.test/sample\n\ngo 1.22\n");
    let path = project.write("api.go", CHANGED);
    let uri = path_uri(&path).unwrap();
    let (server, client) = Connection::memory();
    let server = std::thread::spawn(move || serve(&server));
    let send = |id: i32, method: &str, params: Value| {
        client
            .sender
            .send(Request::new(id.into(), method.into(), params).into())
            .unwrap();
    };
    let receive = || {
        client
            .receiver
            .recv_timeout(Duration::from_secs(15))
            .unwrap()
    };
    send(
        1,
        Initialize::METHOD,
        json!({
            "rootUri": path_uri(&project.root).unwrap(),
            "capabilities": pull_capabilities(true, false),
        }),
    );
    assert!(matches!(receive(), Message::Response(reply) if reply.id == 1.into()));
    client
        .sender
        .send(
            Notification::new(
                DidOpenTextDocument::METHOD.into(),
                json!({
                    "textDocument": { "uri": uri, "languageId": "go", "version": 1, "text": CHANGED },
                }),
            )
            .into(),
        )
        .unwrap();
    send(
        2,
        DocumentDiagnosticRequest::METHOD,
        json!({ "textDocument": { "uri": uri } }),
    );
    let mut first = None;
    let mut refresh = None;
    while first.is_none() || refresh.is_none() {
        match receive() {
            Message::Response(reply) if reply.id == 2.into() => {
                assert!(first.replace(reply.result.unwrap()).is_none());
            }
            Message::Request(request) if request.method == WorkspaceDiagnosticRefresh::METHOD => {
                assert!(refresh.replace(request.id).is_none());
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }
    send(
        3,
        DocumentDiagnosticRequest::METHOD,
        json!({ "textDocument": { "uri": uri } }),
    );
    let Message::Response(reply) = receive() else {
        panic!("expected a Pull response with refresh ACK outstanding");
    };
    assert_eq!(reply.id, 3.into());
    assert!(codes(&reply.result.unwrap()["items"]).contains(&"name.unresolved"));

    client
        .sender
        .send(
            Response::new_err(
                refresh.unwrap(),
                ErrorCode::RequestFailed as i32,
                "test refresh response".into(),
            )
            .into(),
        )
        .unwrap();
    send(4, Shutdown::METHOD, Value::Null);
    assert!(matches!(receive(), Message::Response(reply) if reply.id == 4.into()));
    client
        .sender
        .send(Notification::new(Exit::METHOD.into(), Value::Null).into())
        .unwrap();
    server.join().unwrap().unwrap();
    assert!(client.receiver.is_empty());
}
