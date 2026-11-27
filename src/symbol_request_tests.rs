use serde_json::json;

use super::*;
use crate::test_support::{TestProject, TestSession};

fn request(
    session: &TestSession,
    state: &mut State,
    id: impl Into<RequestId>,
    method: &str,
    params: Value,
) {
    session.request(id, method, params, |server, request| {
        handle_request(server, state, request)
    });
}

fn start(project: &TestProject, capabilities: Value) -> (TestSession, State) {
    let session = TestSession::new();
    let mut state = State::starting();
    request(
        &session,
        &mut state,
        0,
        Initialize::METHOD,
        json!({
            "rootUri": path_uri(&project.root).unwrap(),
            "capabilities": capabilities,
        }),
    );
    let Response { result, error, .. } = session.response(0);
    let init = result.unwrap_or_else(|| panic!("initialize failed: {error:?}"));
    assert_eq!(init["capabilities"]["documentSymbolProvider"], true);
    assert_eq!(init["capabilities"]["workspaceSymbolProvider"], true);
    (session, state)
}

fn running(state: &mut State) -> &mut Running {
    match state {
        State::Running(running) => running,
        _ => panic!("server is not initialized"),
    }
}

fn notifications(state: &mut State) -> impl FnOnce(&Connection, Notification) -> Result<()> + '_ {
    |server, notification| {
        handle_notification(
            server,
            running(state),
            &notification.method,
            notification.params,
        )
    }
}

fn open(session: &TestSession, state: &mut State, uri: &Uri, text: &str) {
    session.open_go_document(uri, text, notifications(state));
}

fn change(session: &TestSession, state: &mut State, uri: &Uri, text: &str) {
    session.replace_document(uri, 2, text, notifications(state));
}

fn finish_searches(session: &TestSession, state: &mut State) {
    let running = running(state);
    for _ in 0..1024 {
        if running.searches.is_empty() {
            return;
        }
        running.advance_search(session.server()).unwrap();
    }
    panic!("workspace symbol searches did not complete");
}

#[test]
fn negotiates_document_symbols_and_serves_empty_workspace_query_immediately() {
    let project = TestProject::new();
    let path = project.write(
        "source.go",
        "package sample\ntype Box struct{ Field int }\n",
    );
    let uri = path_uri(&path).unwrap();
    for hierarchical in [None, Some(false), Some(true)] {
        let caps = match hierarchical {
            None => json!({}),
            Some(supported) => {
                json!({ "textDocument": {"documentSymbol": {"hierarchicalDocumentSymbolSupport": supported}} })
            }
        };
        let (session, mut state) = start(&project, caps);
        request(
            &session,
            &mut state,
            1,
            DocumentSymbolRequest::METHOD,
            json!({"textDocument": {"uri": uri}}),
        );
        let outline = session.response(1).result.unwrap();
        assert_eq!(outline[0]["name"], "Box");
        if hierarchical == Some(true) {
            assert_eq!(outline[0]["children"][0]["name"], "Field");
            assert!(outline[0].get("selectionRange").is_some());
        } else {
            assert_eq!(outline[1]["name"], "Field");
            assert_eq!(outline[1]["containerName"], "Box");
            assert_eq!(outline[1]["location"]["uri"], uri.as_str());
        }
        request(
            &session,
            &mut state,
            2,
            WorkspaceSymbolRequest::METHOD,
            json!({"query": " \t\n "}),
        );
        assert_eq!(session.response(2).result.unwrap(), json!([]));
        let running = running(&mut state);
        assert!(!running.engine.workspace_load().include_tests);
        assert!(
            running.searches.is_empty()
                && running.requested.is_none()
                && running.pending.is_empty()
        );
    }
}

#[test]
fn waits_for_latest_test_graph_and_preserves_pending_queries_across_edits() {
    let project = TestProject::new();
    let path = project.write("disk.go", "package sample\nvar Original int\n");
    let uri = path_uri(&path).unwrap();
    let (session, mut state) = start(&project, json!({}));
    open(
        &session,
        &mut state,
        &uri,
        "package sample\nvar Original int\n",
    );
    let regular = running(&mut state).engine.workspace_load();
    running(&mut state).loading = Some(regular);
    request(
        &session,
        &mut state,
        1,
        WorkspaceSymbolRequest::METHOD,
        json!({"query": "Needle"}),
    );
    request(
        &session,
        &mut state,
        "cancel-me".to_owned(),
        WorkspaceSymbolRequest::METHOD,
        json!({"query": "Needle"}),
    );
    assert_eq!(running(&mut state).pending.len(), 2);
    for _ in 0..2 {
        session.notify(
            Cancel::METHOD,
            json!({"id": "cancel-me"}),
            notifications(&mut state),
        );
    }
    assert_eq!(
        session.response("cancel-me".to_owned()).error.unwrap().code,
        -32800
    );
    session.assert_no_messages();

    let State::Running(server) = &mut state else {
        unreachable!()
    };
    let update = WorkspaceResult::for_test(
        &server.engine,
        regular,
        Ok(project.workspace(&["disk.go"], false)),
    );
    server.workspace_loaded(session.server(), update).unwrap();
    let pending = running(&mut state);
    assert_eq!(pending.pending.len(), 1);
    let old_test_graph = pending.requested.take().unwrap();
    assert!(old_test_graph.include_tests);
    pending.loading = Some(old_test_graph);
    change(
        &session,
        &mut state,
        &uri,
        "package sample\n\nvar NeedleBuffer int\n",
    );
    let created = project.write("created.go", "package sample\nvar NeedleDisk int\n");
    session.notify(
        DidChangeWatchedFiles::METHOD,
        json!({"changes": [{"uri": path_uri(&created).unwrap(), "type": 1}]}),
        notifications(&mut state),
    );
    let State::Running(server) = &mut state else {
        unreachable!()
    };
    let update = WorkspaceResult::for_test(
        &server.engine,
        old_test_graph,
        Ok(project.workspace(&["disk.go"], true)),
    );
    server.workspace_loaded(session.server(), update).unwrap();
    let pending = running(&mut state);
    assert_eq!(pending.pending.len(), 1);
    assert!(pending.searches.is_empty());
    session.assert_no_messages();
    let current_test_graph = pending.requested.take().unwrap();
    assert!(
        current_test_graph.generation > old_test_graph.generation
            && current_test_graph.include_tests
    );
    let update = WorkspaceResult::for_test(
        &pending.engine,
        current_test_graph,
        Ok(project.workspace(&["disk.go", "created.go"], true)),
    );
    pending.workspace_loaded(session.server(), update).unwrap();
    finish_searches(&session, &mut state);
    let result = session.response(1).result.unwrap();
    assert_eq!(result.as_array().unwrap().len(), 2);
    assert_eq!(result[0]["name"], "NeedleBuffer");
    assert_eq!(result[0]["location"]["range"]["start"]["line"], 2);
    assert_eq!(result[1]["name"], "NeedleDisk");
    session.assert_no_messages();
}

#[test]
fn cancels_cold_and_warm_searches_without_affecting_concurrent_requests() {
    const FILES: usize = 140;
    let project = TestProject::new();
    let files: Vec<_> = (0..FILES).map(|index| format!("s{index:03}.go")).collect();
    for (index, name) in files.iter().enumerate() {
        project.write(name, &format!("package sample\nvar Token{index:03} int\n"));
    }
    let file_names = files.iter().map(String::as_str).collect::<Vec<_>>();
    let (session, mut state) = start(&project, json!({}));
    let workspace = project.workspace(&file_names, true);
    let running_server = running(&mut state);
    assert!(
        running_server
            .engine
            .install_workspace(running_server.engine.workspace_load(), workspace)
    );
    let symbol_request = |state: &mut State, id, query| {
        request(
            &session,
            state,
            id,
            WorkspaceSymbolRequest::METHOD,
            json!({"query": query}),
        );
    };
    let cancel = |state: &mut State, id| {
        session.notify(Cancel::METHOD, json!({"id": id}), notifications(state));
        assert_eq!(session.response(id).error.unwrap().code, -32800);
        session.assert_no_messages();
    };

    symbol_request(&mut state, 1, "Token");
    symbol_request(&mut state, 2, "Token");
    running(&mut state)
        .advance_search(session.server())
        .unwrap();
    assert_eq!(running(&mut state).searches.len(), 2);
    cancel(&mut state, 1);
    finish_searches(&session, &mut state);
    let result = session.response(2).result.unwrap();
    assert_eq!(result.as_array().unwrap().len(), FILES);
    assert_eq!(result[0]["name"], "Token000");

    symbol_request(&mut state, 3, "Token");
    symbol_request(&mut state, 4, "Token");
    running(&mut state)
        .advance_search(session.server())
        .unwrap();
    assert_eq!(running(&mut state).searches.len(), 2);
    cancel(&mut state, 3);
    finish_searches(&session, &mut state);
    assert_eq!(session.response(4).result.unwrap(), result);

    let uri = path_uri(&project.root.join(&files[0])).unwrap();
    open(
        &session,
        &mut state,
        &uri,
        "package sample\nvar Token000 int\n",
    );
    symbol_request(&mut state, 5, "Token");
    running(&mut state)
        .advance_search(session.server())
        .unwrap();
    change(
        &session,
        &mut state,
        &uri,
        "package sample\nvar Replacement int\n",
    );
    assert_eq!(session.response(5).error.unwrap().code, -32801);
    assert!(running(&mut state).searches.is_empty());
    symbol_request(&mut state, 6, "Replacement");
    finish_searches(&session, &mut state);
    let result = session.response(6).result.unwrap();
    assert_eq!(result.as_array().unwrap().len(), 1);
    assert_eq!(result[0]["location"]["uri"], uri.as_str());
    session.assert_no_messages();
}
