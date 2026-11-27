use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::thread;

use anyhow::{Context, Result};
use crossbeam_channel::Sender;
use lsp_server::{Connection, ErrorCode, RequestId};

use crate::protocol::{RequestResult, internal_error, send_response};
use crate::semantic::{Engine, QueryProgress};
use crate::workspace::{Workspace, WorkspaceLoad, path_key};

const WORKSPACE_CHANGED: &str = "the workspace changed during search";
const SHUTTING_DOWN: &str = "the server is shutting down";

pub(super) type RequestProgress = QueryProgress<RequestResult, Box<dyn SearchTask>>;

pub(super) trait SearchTask {
    fn ready(&self, _engine: &Engine) -> bool {
        true
    }

    fn discard(&mut self, _code: ErrorCode) {}

    fn advance(&mut self, engine: &mut Engine) -> Result<bool>;
    fn finish(self: Box<Self>) -> RequestResult;
}

pub(super) trait DeferredQuery {
    fn run(&self, engine: &mut Engine) -> Result<RequestProgress>;

    fn document_path(&self) -> Option<&Path> {
        None
    }

    fn requires_stable_workspace(&self) -> bool {
        false
    }

    fn load_failed(&self, _engine: &mut Engine, message: &str) -> RequestResult {
        Err((ErrorCode::RequestFailed, message.to_owned()))
    }

    fn discard(&mut self, _code: ErrorCode) {}
}

pub(super) struct Pending {
    id: RequestId,
    document_key: Option<PathBuf>,
    query: Box<dyn DeferredQuery>,
}

pub(super) struct Searching {
    id: RequestId,
    generation: u64,
    task: Box<dyn SearchTask>,
}

pub(super) struct WorkspaceResult {
    pub(super) load: WorkspaceLoad,
    pub(super) result: Result<Workspace>,
}

#[derive(Default)]
pub(super) struct Scheduler {
    pub(super) pending: Vec<Pending>,
    pub(super) searches: VecDeque<Searching>,
    pub(super) loading: Option<WorkspaceLoad>,
    pub(super) requested: Option<WorkspaceLoad>,
}

impl Scheduler {
    pub(super) fn request_workspace(&mut self, load: WorkspaceLoad) {
        if self.loading != Some(load) {
            self.requested = Some(load);
        }
    }

    pub(super) fn start_workspace_load(
        &mut self,
        engine: &Engine,
        results: &Sender<WorkspaceResult>,
    ) -> Result<()> {
        if self.loading.is_some() {
            return Ok(());
        }
        let Some(load) = self.requested.take() else {
            return Ok(());
        };
        engine.prewarm_completion_sdk(true);
        let roots = engine.roots().to_vec();
        let track_rename_inputs = engine.track_rename_inputs;
        let results = results.clone();
        thread::Builder::new()
            .name("go-package-loader".into())
            .spawn(move || {
                let result = if track_rename_inputs {
                    Workspace::load_roots_for_rename(&roots, load.include_tests)
                } else {
                    Workspace::load_roots(&roots, load.include_tests)
                };
                let _ = results.send(WorkspaceResult { load, result });
            })
            .context("starting Go package loader")?;
        self.loading = Some(load);
        Ok(())
    }

    pub(super) fn workspace_loaded(
        &mut self,
        engine: &mut Engine,
        connection: &Connection,
        update: WorkspaceResult,
    ) -> Result<bool> {
        if self.loading == Some(update.load) {
            self.loading = None;
        }
        let mut installed = false;
        match update.result {
            Ok(workspace) => {
                let failure = workspace.total_load_failure().map(str::to_owned);
                if (failure.is_none() || update.load == engine.workspace_load())
                    && engine.install_workspace(update.load, workspace)
                {
                    installed = true;
                    if let Some(message) = failure {
                        // Diagnostics can use the installed failures. Graph
                        // queries retain their load error and can retry later.
                        self.fail_pending_load(engine, connection, &message)?;
                    } else {
                        for pending in std::mem::take(&mut self.pending) {
                            let Pending {
                                id,
                                document_key,
                                query,
                            } = pending;
                            let progress = query.run(engine);
                            self.resolve(engine, connection, id, progress, |id| Pending {
                                id,
                                document_key,
                                query,
                            })?;
                        }
                    }
                }
            }
            Err(error) if update.load == engine.workspace_load() => {
                self.fail_pending_load(engine, connection, &format!("{error:#}"))?;
            }
            Err(_) => {}
        }
        if !self.pending.is_empty() {
            self.request_workspace(engine.workspace_load());
        }
        Ok(installed)
    }

    fn fail_pending_load(
        &mut self,
        engine: &mut Engine,
        connection: &Connection,
        message: &str,
    ) -> Result<()> {
        if self.pending.is_empty() {
            eprintln!("rs-gopls: loading Go packages: {message}");
        }
        for pending in std::mem::take(&mut self.pending) {
            let result = pending.query.load_failed(engine, message);
            send_response(connection, pending.id, result)?;
        }
        Ok(())
    }

    pub(super) fn submit(
        &mut self,
        engine: &mut Engine,
        connection: &Connection,
        id: RequestId,
        query: impl DeferredQuery + 'static,
    ) -> Result<()> {
        let progress = query.run(engine);
        self.resolve(engine, connection, id, progress, |id| Pending {
            id,
            document_key: query.document_path().map(path_key),
            query: Box::new(query),
        })
    }

    fn resolve(
        &mut self,
        engine: &Engine,
        connection: &Connection,
        id: RequestId,
        progress: Result<RequestProgress>,
        defer: impl FnOnce(RequestId) -> Pending,
    ) -> Result<()> {
        let result = match progress {
            Ok(QueryProgress::Pending(load)) => {
                self.pending.push(defer(id));
                self.request_workspace(load);
                return Ok(());
            }
            Ok(QueryProgress::Searching(task)) => {
                self.searches.push_back(Searching {
                    id,
                    generation: engine.workspace_load().generation,
                    task,
                });
                return Ok(());
            }
            Ok(QueryProgress::Ready(result)) => result,
            Err(error) => Err(internal_error(error)),
        };
        send_response(connection, id, result)
    }

    pub(super) fn advance_search(
        &mut self,
        engine: &mut Engine,
        connection: &Connection,
    ) -> Result<()> {
        let Some(next) = self.ready_search(engine) else {
            return Ok(());
        };
        let mut search = self.searches.remove(next).unwrap();
        let current_generation = engine.workspace_load().generation;
        let result = if current_generation == search.generation {
            let progress = search.task.advance(engine);
            if engine.workspace_load().generation != search.generation {
                search.task.discard(ErrorCode::ContentModified);
                Err((ErrorCode::ContentModified, WORKSPACE_CHANGED.to_owned()))
            } else {
                match progress {
                    Ok(false) => {
                        self.searches.push_back(search);
                        return Ok(());
                    }
                    Ok(true) => search.task.finish(),
                    Err(error) => {
                        search.task.discard(ErrorCode::InternalError);
                        Err(internal_error(error))
                    }
                }
            }
        } else {
            search.task.discard(ErrorCode::ContentModified);
            Err((ErrorCode::ContentModified, WORKSPACE_CHANGED.to_owned()))
        };
        send_response(connection, search.id, result)
    }

    pub(super) fn ready_search(&self, engine: &Engine) -> Option<usize> {
        let generation = engine.workspace_load().generation;
        self.searches
            .iter()
            .position(|search| search.generation != generation || search.task.ready(engine))
    }

    fn cancel_searches(
        &mut self,
        connection: &Connection,
        mut matches: impl FnMut(&RequestId) -> bool,
        code: ErrorCode,
        message: &str,
    ) -> Result<()> {
        for mut search in std::mem::take(&mut self.searches) {
            if matches(&search.id) {
                search.task.discard(code);
                drop(search.task);
                send_response(connection, search.id, Err((code, message.to_owned())))?;
            } else {
                self.searches.push_back(search);
            }
        }
        Ok(())
    }

    fn cancel_pending(
        &mut self,
        connection: &Connection,
        matches: impl FnMut(&mut Pending) -> bool,
        code: ErrorCode,
        message: &str,
    ) -> Result<()> {
        for mut request in self.pending.extract_if(.., matches) {
            request.query.discard(code);
            drop(request.query);
            send_response(connection, request.id, Err((code, message.to_owned())))?;
        }
        Ok(())
    }

    pub(super) fn cancel(&mut self, connection: &Connection, id: &RequestId) -> Result<()> {
        self.cancel_pending(
            connection,
            |request| &request.id == id,
            ErrorCode::RequestCanceled,
            "the client cancelled the request",
        )?;
        self.cancel_searches(
            connection,
            |active| active == id,
            ErrorCode::RequestCanceled,
            "the client cancelled the search",
        )
    }

    pub(super) fn document_modified(&mut self, connection: &Connection, path: &Path) -> Result<()> {
        if !self.pending.is_empty() {
            let key = path_key(path);
            self.cancel_pending(
                connection,
                |request| {
                    request.query.requires_stable_workspace()
                        || request.document_key.as_ref() == Some(&key)
                },
                ErrorCode::ContentModified,
                "the document changed while loading Go packages",
            )?;
        }
        self.cancel_searches(
            connection,
            |_| true,
            ErrorCode::ContentModified,
            "the document changed during search",
        )
    }

    pub(super) fn workspace_modified(
        &mut self,
        connection: &Connection,
        load: WorkspaceLoad,
    ) -> Result<()> {
        self.cancel_pending(
            connection,
            |request| request.query.requires_stable_workspace(),
            ErrorCode::ContentModified,
            WORKSPACE_CHANGED,
        )?;
        self.cancel_searches(
            connection,
            |_| true,
            ErrorCode::ContentModified,
            WORKSPACE_CHANGED,
        )?;
        self.request_workspace(load);
        Ok(())
    }

    pub(super) fn shutdown(&mut self, connection: &Connection) -> Result<()> {
        self.cancel_pending(
            connection,
            |_| true,
            ErrorCode::RequestFailed,
            SHUTTING_DOWN,
        )?;
        self.cancel_searches(
            connection,
            |_| true,
            ErrorCode::RequestFailed,
            SHUTTING_DOWN,
        )?;
        self.requested = None;
        self.loading = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::rc::Rc;

    use anyhow::anyhow;
    use lsp_server::{Message, Notification, Request, Response, ResponseError};
    use lsp_types::{
        FileChangeType, Uri,
        notification::{
            Cancel, DidChangeTextDocument, DidChangeWatchedFiles, DidChangeWorkspaceFolders,
            DidCloseTextDocument, DidOpenTextDocument, Exit, Initialized, Notification as _,
        },
        request::{
            DocumentHighlightRequest, FoldingRangeRequest, GotoImplementation, HoverRequest,
            Initialize, InlayHintRequest, References, Request as _, SemanticTokensFullRequest,
            Shutdown, SignatureHelpRequest, WorkspaceSymbolRequest,
        },
    };
    use serde_json::{Value, json};

    use super::*;
    use crate::protocol::path_uri;
    use crate::test_support::{TestProject, TestSession};
    use crate::{State, handle_notification, handle_request, serve};

    struct Session {
        server: Connection,
        client: Connection,
        state: State,
        replies: HashMap<RequestId, Response>,
    }

    impl Session {
        fn start(project: &TestProject) -> Self {
            Self::with_capabilities(project, json!({}))
        }

        fn with_capabilities(project: &TestProject, capabilities: Value) -> Self {
            let (server, client) = Connection::memory();
            let mut session = Self {
                server,
                client,
                state: State::Starting,
                replies: HashMap::new(),
            };
            session.request(
                0,
                Initialize::METHOD,
                json!({
                    "rootUri": path_uri(&project.root).unwrap(), "capabilities": capabilities
                }),
            );
            assert!(session.reply(0).result.is_some());
            session
        }

        fn running(&mut self) -> &mut crate::Running {
            let State::Running(running) = &mut self.state else {
                panic!("expected a running session");
            };
            running
        }

        fn scheduler(&mut self) -> &mut Scheduler {
            &mut self.running().scheduler
        }

        fn request(&mut self, id: impl Into<RequestId>, method: &str, params: Value) {
            handle_request(
                &self.server,
                &mut self.state,
                Request::new(id.into(), method.into(), params),
            )
            .unwrap();
        }

        fn notify(&mut self, method: &str, params: Value) {
            let State::Running(running) = &mut self.state else {
                panic!("expected a running session");
            };
            handle_notification(&self.server, running, method, params).unwrap();
        }

        fn cancel(&mut self, id: impl Into<RequestId>) {
            self.notify(Cancel::METHOD, json!({ "id": id.into() }));
        }

        fn open(&mut self, path: &Path, text: &str) {
            self.notify(DidOpenTextDocument::METHOD, json!({
                "textDocument": {
                    "uri": path_uri(path).unwrap(), "languageId": "go", "version": 1, "text": text
                }
            }));
        }

        fn edit(&mut self, path: &Path, version: i32, text: &str) {
            self.notify(
                DidChangeTextDocument::METHOD,
                json!({
                    "textDocument": { "uri": path_uri(path).unwrap(), "version": version },
                    "contentChanges": [{ "text": text }]
                }),
            );
        }

        fn watch(&mut self, path: &Path, change: FileChangeType) {
            self.notify(
                DidChangeWatchedFiles::METHOD,
                json!({
                    "changes": [{ "uri": path_uri(path).unwrap(), "type": change }]
                }),
            );
        }

        fn deliver(&mut self, load: WorkspaceLoad, result: Result<Workspace>) {
            let State::Running(running) = &mut self.state else {
                panic!("expected a running session");
            };
            running
                .workspace_loaded(&self.server, WorkspaceResult { load, result })
                .unwrap();
        }

        fn begin_load(&mut self) -> WorkspaceLoad {
            let scheduler = self.scheduler();
            assert!(scheduler.loading.is_none());
            let load = scheduler.requested.take().expect("a load was scheduled");
            scheduler.loading = Some(load);
            load
        }

        fn finish_requested_load(&mut self) {
            let (sender, receiver) = crossbeam_channel::bounded(1);
            let running = self.running();
            running
                .scheduler
                .start_workspace_load(&running.engine, &sender)
                .unwrap();
            drop(sender);
            let update = receiver.recv().unwrap();
            self.deliver(update.load, Ok(update.result.unwrap()));
        }

        fn step(&mut self) {
            let State::Running(running) = &mut self.state else {
                panic!("expected a running session");
            };
            running.advance_search(&self.server).unwrap();
        }

        fn finish_searches(&mut self) {
            for _ in 0..4096 {
                if self.scheduler().searches.is_empty() {
                    return;
                }
                self.step();
            }
            panic!("searches did not finish");
        }

        fn expect_next(&mut self, id: impl Into<RequestId>) {
            assert_eq!(self.scheduler().searches.front().unwrap().id, id.into());
        }

        fn collect(&mut self) {
            for message in self.client.receiver.try_iter() {
                let Message::Response(response) = message else {
                    panic!("expected a response");
                };
                let id = response.id.clone();
                assert!(
                    self.replies.insert(id.clone(), response).is_none(),
                    "duplicate response for {id}"
                );
            }
        }

        fn reply(&mut self, id: impl Into<RequestId>) -> &Response {
            self.collect();
            &self.replies[&id.into()]
        }

        fn count(&mut self, expected: usize) {
            self.collect();
            assert_eq!(self.replies.len(), expected);
        }

        fn error(&mut self, id: impl Into<RequestId>, code: ErrorCode, message: &str) {
            let error = self.reply(id).error.as_ref().unwrap();
            assert_eq!(error.code, code as i32);
            assert_eq!(error.message, message);
        }
    }

    fn position(path: &Path, line: u32, character: u32) -> Value {
        json!({
            "textDocument": { "uri": path_uri(path).unwrap() },
            "position": { "line": line, "character": character }
        })
    }

    fn reference(path: &Path, line: u32, character: u32) -> Value {
        let mut params = position(path, line, character);
        params["context"] = json!({ "includeDeclaration": true });
        params
    }

    #[test]
    fn rename_requires_versioned_edits_and_negotiates_prepare_separately() {
        let project = TestProject::new();
        for (document_changes, prepare, expected) in [
            (false, true, Value::Null),
            (true, false, json!(true)),
            (true, true, json!({"prepareProvider": true})),
        ] {
            let mut session = Session::with_capabilities(
                &project,
                json!({
                    "workspace": {"workspaceEdit": {"documentChanges": document_changes}},
                    "textDocument": {"rename": {"prepareSupport": prepare}}
                }),
            );
            let init = session.reply(0).result.as_ref().unwrap();
            assert_eq!(init["capabilities"]["renameProvider"], expected);
            // Malformed arguments distinguish registration from an unsupported method.
            for (id, method, supported) in [
                (1, "textDocument/rename", document_changes),
                (2, "textDocument/prepareRename", document_changes && prepare),
            ] {
                session.request(id, method, Value::Null);
                let code = if supported {
                    ErrorCode::InvalidParams
                } else {
                    ErrorCode::MethodNotFound
                };
                assert_eq!(session.reply(id).error.as_ref().unwrap().code, code as i32);
            }
        }
    }

    #[test]
    fn pending_rename_is_cancelled_once_when_another_input_changes() {
        let project = TestProject::new();
        let source = project.write("target.go", "package sample\nfunc Target() {}\n");
        let other = project.write("usage.go", "package sample\nfunc Use() { Target() }\n");
        let mut session = Session::with_capabilities(
            &project,
            json!({
                "workspace": {"workspaceEdit": {"documentChanges": true}}
            }),
        );
        let mut params = position(&source, 1, 6);
        params["newName"] = json!("Updated");
        session.request(1, "textDocument/rename", params.clone());
        assert_eq!(session.scheduler().pending.len(), 1);
        let load = session.begin_load();
        session.cancel(1);
        assert_eq!(
            session.reply(1).error.as_ref().unwrap().code,
            ErrorCode::RequestCanceled as i32
        );
        session.request(2, "textDocument/rename", params);
        session.open(&other, "package sample\nfunc Use() {}\n");
        assert_eq!(
            session.reply(2).error.as_ref().unwrap().code,
            ErrorCode::ContentModified as i32
        );
        session.deliver(
            load,
            Ok(project.workspace(&["target.go", "usage.go"], true)),
        );
        session.finish_searches();
        session.count(3); // initialize and one response to each rename
    }

    #[test]
    fn adding_a_folder_cancels_pending_rename_and_allows_rename_in_the_new_root() {
        let first = TestProject::new();
        first.write("go.mod", "module example.test/one\n\ngo 1.22\n");
        let unrelated = first.write("file.go", "package one\nvar Target = 1\n");
        let second = TestProject::new();
        second.write("go.mod", "module example.test/two\n\ngo 1.22\n");
        let source = second.write("file.go", "package two\nvar Target = 2\nvar use = Target\n");
        let mut session = Session::with_capabilities(
            &first,
            json!({"workspace": {"workspaceEdit": {"documentChanges": true}}}),
        );
        let rename = |path| {
            let mut request = position(path, 1, 5);
            request["newName"] = json!("Updated");
            request
        };
        session.request(1, "textDocument/rename", rename(&unrelated));
        let folder = json!({"name": "two", "uri": path_uri(&second.root).unwrap()});
        session.notify(
            DidChangeWorkspaceFolders::METHOD,
            json!({"event": {"added": [folder], "removed": []}}),
        );
        session.error(1, ErrorCode::ContentModified, WORKSPACE_CHANGED);
        session.request(2, "textDocument/rename", rename(&source));
        session.finish_requested_load();
        session.finish_searches();
        let edit = session.reply(2).result.as_ref().unwrap();
        let changes = edit["documentChanges"].as_array().unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(
            changes[0]["textDocument"]["uri"],
            json!(path_uri(&source).unwrap())
        );
        assert_eq!(changes[0]["edits"].as_array().unwrap().len(), 2);
        session.count(3);
    }

    #[test]
    fn pending_rename_refuses_a_replaced_disk_target_at_the_same_cursor() {
        let project = TestProject::new();
        let source = project.write("target.go", "package sample\nvar First int\n");
        let mut session = Session::with_capabilities(
            &project,
            json!({
                "workspace": {"workspaceEdit": {"documentChanges": true}}
            }),
        );
        let mut arguments = position(&source, 1, 5);
        arguments["newName"] = json!("Updated");
        session.request(1, "textDocument/rename", arguments);
        let load = session.begin_load();
        project.write("target.go", "package sample\nvar Other int\n");
        session.deliver(load, Ok(project.workspace(&["target.go"], true)));
        session.finish_searches();
        assert_eq!(
            session.reply(1).error.as_ref().unwrap().code,
            ErrorCode::ContentModified as i32
        );
        session.count(2);
    }

    #[test]
    fn active_rename_detects_a_disk_change_without_a_watch_notification() {
        let project = TestProject::new();
        let source = project.write("target.go", "package sample\nvar Target int\n");
        let mut files = vec![String::from("target.go")];
        for index in 0..96 {
            let name = format!("use_{index}.go");
            project.write(&name, &format!("package sample\nvar U{index} = Target\n"));
            files.push(name);
        }
        let mut session = Session::with_capabilities(
            &project,
            json!({
                "workspace": {"workspaceEdit": {"documentChanges": true}}
            }),
        );
        let mut arguments = position(&source, 1, 5);
        arguments["newName"] = json!("Updated");
        session.request(1, "textDocument/rename", arguments);
        let load = session.begin_load();
        let names = files.iter().map(String::as_str).collect::<Vec<_>>();
        session.deliver(load, Ok(project.workspace(&names, true)));
        session.step();
        assert_eq!(session.scheduler().searches.len(), 1);
        project.write("target.go", "package sample\n// shifted\nvar Target int\n");
        session.finish_searches();
        assert_eq!(
            session.reply(1).error.as_ref().unwrap().code,
            ErrorCode::ContentModified as i32
        );
        session.count(2);
    }

    #[test]
    fn initializes_with_valid_distinct_folders_and_legacy_fallbacks() {
        let first = TestProject::new();
        let second = TestProject::new();
        let uri = path_uri(&first.root).unwrap();
        let a = json!({"name": "one", "uri": uri});
        let b = json!({"name": "two", "uri": path_uri(&second.root).unwrap()});
        let roots = |mut params: Value| {
            params["capabilities"] = json!({});
            crate::initialize(params).map(|(session, _)| session.engine.roots().to_vec())
        };
        assert_eq!(
            roots(json!({"workspaceFolders": [b, a, b], "rootUri": "untitled:ignored"})).unwrap(),
            [second.root.clone(), first.root.clone()]
        );
        assert_eq!(
            roots(json!({"workspaceFolders": [], "rootUri": uri, "rootPath": second.root}))
                .unwrap(),
            [first.root.clone()]
        );
        assert_eq!(
            roots(json!({"rootPath": second.root})).unwrap(),
            [second.root.clone()]
        );
        assert_eq!(
            roots(json!({})).unwrap(),
            [std::env::current_dir().unwrap()]
        );
        let file = first.write("file.go", "package sample\n");
        for invalid in [
            path_uri(&file).unwrap().to_string(),
            "untitled:folder".into(),
        ] {
            assert!(
                roots(json!({"workspaceFolders": [a, {"name":"invalid", "uri":invalid}]})).is_err()
            );
        }
    }

    #[test]
    fn folder_changes_reload_the_graph_and_rescope_open_drafts_and_searches() {
        let first = TestProject::new();
        first.write("go.mod", "module example.test/first\n\ngo 1.22\n");
        first.write("file.go", "package first\nvar FolderSymbolOne int\n");
        let second = TestProject::new();
        second.write("go.work", "go 1.22\n\nuse ./member\n");
        second.write("member/go.mod", "module example.test/second\n\ngo 1.22\n");
        second.write(
            "member/file.go",
            "package second\nvar FolderSymbolTwo int\n",
        );
        let a = json!({"name":"one", "uri":path_uri(&first.root).unwrap()});
        let b = json!({"name":"two", "uri":path_uri(&second.root).unwrap()});
        let folders = DidChangeWorkspaceFolders::METHOD;
        let mut session = Session::start(&first);
        let symbols = |session: &mut Session, id| {
            session.request(
                id,
                WorkspaceSymbolRequest::METHOD,
                json!({"query":"FolderSymbol"}),
            );
        };
        let names = |session: &mut Session, id| {
            session.finish_searches();
            let mut names: Vec<_> = session
                .reply(id)
                .result
                .as_ref()
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .map(|symbol| symbol["name"].as_str().unwrap().to_owned())
                .collect();
            names.sort();
            names
        };
        symbols(&mut session, 1);
        let stale = session.begin_load();
        session.open(
            &second.root.join("draft.go"),
            "package draft\nvar FolderSymbolDraft int\n",
        );
        session.notify(folders, json!({"event":{"removed":[], "added":[a, b, b]}}));
        let current = session.running().engine.workspace_load();
        assert_eq!(current.generation, stale.generation + 1);
        assert_eq!(session.scheduler().requested, Some(current));
        session.deliver(stale, Ok(first.workspace(&["file.go"], true)));
        session.count(1);
        session.finish_requested_load();
        assert_eq!(
            names(&mut session, 1),
            ["FolderSymbolDraft", "FolderSymbolOne", "FolderSymbolTwo"]
        );
        session.open(
            &second.root.join("late.go"),
            "package draft\nvar FolderSymbolLate int\n",
        );
        symbols(&mut session, 2);
        assert_eq!(
            names(&mut session, 2),
            [
                "FolderSymbolDraft",
                "FolderSymbolLate",
                "FolderSymbolOne",
                "FolderSymbolTwo"
            ]
        );

        session.notify(folders, json!({"event":{"removed":[], "added":[b]}}));
        assert_eq!(session.running().engine.workspace_load(), current);
        assert!(session.scheduler().requested.is_none());
        let State::Running(running) = &mut session.state else {
            unreachable!()
        };
        let missing =
            json!({"name":"missing", "uri":path_uri(&second.root.join("absent")).unwrap()});
        assert!(
            handle_notification(
                &session.server,
                running,
                folders,
                json!({"event":{"removed":[a], "added":[missing]}})
            )
            .is_err()
        );
        assert_eq!(
            running.engine.roots(),
            [first.root.clone(), second.root.clone()]
        );
        assert_eq!(running.engine.workspace_load(), current);

        symbols(&mut session, 3);
        session.notify(folders, json!({"event":{"removed":[b], "added":[]}}));
        session.error(3, ErrorCode::ContentModified, WORKSPACE_CHANGED);
        symbols(&mut session, 4);
        session.finish_requested_load();
        assert_eq!(names(&mut session, 4), ["FolderSymbolOne"]);
        session.notify(folders, json!({"event":{"removed":[a], "added":[]}}));
        assert!(session.running().engine.roots().is_empty());
        symbols(&mut session, 5);
        session.finish_requested_load();
        assert!(names(&mut session, 5).is_empty());
        session.notify(folders, json!({"event":{"removed":[], "added":[b]}}));
        symbols(&mut session, 6);
        session.finish_requested_load();
        assert_eq!(
            names(&mut session, 6),
            ["FolderSymbolDraft", "FolderSymbolLate", "FolderSymbolTwo"]
        );
    }

    #[test]
    fn interleaves_three_real_searches_and_cancels_waiting_indexing_and_scanning() {
        // A single step always leaves index work (8 files) and scan work (128 files).
        const FILES: usize = 140;
        let project = TestProject::new();
        let api = project.write(
            "api.go",
            "package sample\ntype Contract interface { Render() }\nvar Needle int\n",
        );
        let mut files = vec!["api.go".to_owned()];
        for index in 0..FILES {
            let file = format!("s{index:03}.go");
            let ty = format!("Concrete{index:03}");
            project.write(&file, &format!(
                "package sample\ntype {ty} struct{{}}\nfunc ({ty}) Render() {{}}\nvar _ = Needle\n"
            ));
            files.push(file);
        }
        let names: Vec<_> = files.iter().map(String::as_str).collect();
        let mut session = Session::start(&project);
        let implementation = "implementation".to_owned();
        let waiting = "waiting".to_owned();
        let warm_scan = "warm-scan".to_owned();
        session.request(1, References::METHOD, reference(&api, 2, 4));
        session.request(
            implementation.clone(),
            GotoImplementation::METHOD,
            position(&api, 1, 5),
        );
        session.request(
            2,
            WorkspaceSymbolRequest::METHOD,
            json!({ "query": "Concrete" }),
        );
        session.request(waiting.clone(), References::METHOD, reference(&api, 2, 4));
        assert_eq!(session.scheduler().pending.len(), 4);
        session.cancel(waiting.clone());
        session.cancel(waiting.clone());
        session.error(
            waiting,
            ErrorCode::RequestCanceled,
            "the client cancelled the request",
        );
        session.count(2);

        let load = session.begin_load();
        assert!(load.include_tests);
        session.deliver(load, Ok(project.workspace(&names, true)));
        assert!(session.scheduler().pending.is_empty());
        session.expect_next(1);
        session.step();
        session.expect_next(implementation.clone());
        session.step();
        session.expect_next(2);
        session.step();
        session.expect_next(1);
        assert_eq!(session.scheduler().searches.len(), 3);
        session.cancel(2);
        session.cancel(2);
        session.error(
            2,
            ErrorCode::RequestCanceled,
            "the client cancelled the search",
        );
        session.count(3);
        session.request(
            3,
            WorkspaceSymbolRequest::METHOD,
            json!({ "query": "Concrete" }),
        );
        session.finish_searches();

        let references = session
            .reply(1)
            .result
            .as_ref()
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(references.len(), FILES + 1);
        assert_eq!(references[0]["uri"], path_uri(&api).unwrap().as_str());
        assert_eq!(
            references[0]["range"]["start"],
            json!({ "line": 2, "character": 4 })
        );
        let implementations = session
            .reply(implementation.clone())
            .result
            .as_ref()
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(implementations.len(), FILES);
        assert_eq!(
            implementations[0]["uri"],
            path_uri(&project.root.join("s000.go")).unwrap().as_str()
        );
        assert_eq!(
            implementations[0]["range"]["start"],
            json!({ "line": 1, "character": 5 })
        );
        let symbols = session.reply(3).result.clone().unwrap();
        assert_eq!(symbols.as_array().unwrap().len(), FILES);
        assert_eq!(symbols[0]["name"], "Concrete000");
        session.cancel(implementation);
        session.cancel(3);
        session.count(6);

        session.request(
            warm_scan.clone(),
            WorkspaceSymbolRequest::METHOD,
            json!({ "query": "Concrete" }),
        );
        session.request(
            4,
            WorkspaceSymbolRequest::METHOD,
            json!({ "query": "Concrete" }),
        );
        session.request(5, References::METHOD, reference(&api, 2, 4));
        session.step();
        session.expect_next(4);
        assert_eq!(session.scheduler().searches.len(), 3);
        session.cancel(warm_scan.clone());
        session.error(
            warm_scan,
            ErrorCode::RequestCanceled,
            "the client cancelled the search",
        );
        session.step();
        session.expect_next(5);
        session.step();
        session.cancel(5);
        session.error(
            5,
            ErrorCode::RequestCanceled,
            "the client cancelled the search",
        );
        session.finish_searches();
        assert_eq!(session.reply(4).result.as_ref(), Some(&symbols));
        session.count(9);
    }

    #[test]
    fn reruns_pending_queries_on_the_latest_graph_and_cancels_affected_requests() {
        let project = TestProject::new();
        let original = "package sample\nvar NeedleOld int\n";
        let editable = project.write("edit.go", original);
        let other = project.write("other.go", "package sample\nvar NeedleOther int\n");
        let mut session = Session::start(&project);
        session.open(&editable, original);
        let regular = session.running().engine.workspace_load();
        session.scheduler().loading = Some(regular);
        session.request(1, References::METHOD, reference(&editable, 1, 4));
        session.request(2, References::METHOD, reference(&other, 1, 4));
        session.request(
            3,
            WorkspaceSymbolRequest::METHOD,
            json!({ "query": "Needle" }),
        );
        session.deliver(
            regular,
            Ok(project.workspace(&["edit.go", "other.go"], false)),
        );
        assert_eq!(session.scheduler().pending.len(), 3);
        let old_tests = session.begin_load();
        assert_eq!(old_tests.generation, regular.generation);
        assert!(old_tests.include_tests);
        session.deliver(regular, Err(anyhow!("old regular-view failure")));
        assert_eq!(session.scheduler().loading, Some(old_tests));
        session.count(1);

        session.edit(&editable, 2, "package sample\n\nvar NeedleBuffer int\n");
        session.error(
            1,
            ErrorCode::ContentModified,
            "the document changed while loading Go packages",
        );
        assert_eq!(session.scheduler().pending.len(), 2);
        let flat = project.write("created.go", "package sample\nvar NeedleDisk int\n");
        session.watch(&flat, FileChangeType::CREATED);
        let current = session.running().engine.workspace_load();
        assert!(current.generation > old_tests.generation);
        assert_eq!(session.scheduler().requested, Some(current));
        session.deliver(
            old_tests,
            Ok(project.workspace(&["edit.go", "other.go"], true)),
        );
        assert_eq!(session.begin_load(), current);
        session.deliver(old_tests, Err(anyhow!("old generation failure")));
        assert_eq!(session.scheduler().loading, Some(current));
        assert_eq!(session.scheduler().pending.len(), 2);
        assert!(session.scheduler().searches.is_empty());
        session.count(2);

        let files = ["edit.go", "other.go", "created.go"];
        session.deliver(current, Ok(project.workspace(&files, true)));
        session.finish_searches();
        let references = session
            .reply(2)
            .result
            .as_ref()
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(references.len(), 1);
        assert_eq!(references[0]["uri"], path_uri(&other).unwrap().as_str());
        let symbols = session
            .reply(3)
            .result
            .as_ref()
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(
            symbols
                .iter()
                .map(|symbol| symbol["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["NeedleBuffer", "NeedleDisk", "NeedleOther"]
        );
        assert_eq!(symbols[0]["location"]["range"]["start"]["line"], 2);

        session.request(4, References::METHOD, reference(&other, 1, 4));
        session.edit(&editable, 3, "package sample\nvar LatestBuffer int\n");
        session.error(
            4,
            ErrorCode::ContentModified,
            "the document changed during search",
        );
        session.request(
            5,
            WorkspaceSymbolRequest::METHOD,
            json!({ "query": "Latest" }),
        );
        // Deleting a file with an open overlay invalidates the graph and leaves the buffer open.
        std::fs::remove_file(&editable).unwrap();
        session.watch(&editable, FileChangeType::DELETED);
        session.error(5, ErrorCode::ContentModified, WORKSPACE_CHANGED);
        let refreshed = session.begin_load();
        assert!(refreshed.generation > current.generation);
        session.deliver(
            refreshed,
            Ok(project.workspace(&["other.go", "created.go"], true)),
        );
        session.request(
            6,
            WorkspaceSymbolRequest::METHOD,
            json!({ "query": "Needle" }),
        );
        session.finish_searches();
        assert_eq!(
            session
                .reply(6)
                .result
                .as_ref()
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );
        session.count(7);
    }

    #[test]
    fn sends_load_fallbacks_and_cleans_up_pending_and_active_requests_at_shutdown() {
        let project = TestProject::new();
        let path = project.write("api.go", "package sample\nfunc f() { _ = len(\"x\") }\n");
        let mut session = Session::start(&project);
        let signature = "signature".to_owned();
        session.request(1, HoverRequest::METHOD, position(&path, 1, 15));
        session.request(
            signature.clone(),
            SignatureHelpRequest::METHOD,
            position(&path, 1, 19),
        );
        session.request(2, WorkspaceSymbolRequest::METHOD, json!({ "query": "f" }));
        assert_eq!(session.scheduler().pending.len(), 3);
        let load = session.begin_load();
        session.deliver(load, Err(anyhow!("Go package loading failed")));
        let hover = session.reply(1).result.as_ref().unwrap();
        assert_eq!(
            hover["contents"]["value"].as_str().unwrap().lines().nth(1),
            Some("func len(v Type) int")
        );
        assert_eq!(
            session.reply(signature).result.as_ref().unwrap()["signatures"][0]["label"],
            "func len(v Type) int"
        );
        session.error(2, ErrorCode::RequestFailed, "Go package loading failed");
        assert!(session.scheduler().pending.is_empty());
        assert!(session.scheduler().loading.is_none());

        session.request(3, WorkspaceSymbolRequest::METHOD, json!({ "query": "f" }));
        let retry = session.begin_load();
        assert_eq!(retry, load);
        session.deliver(retry, Ok(project.workspace(&["api.go"], true)));
        // Invalidate directly so both states are present before shutdown runs.
        session
            .running()
            .engine
            .file_changed(&project.root.join("go.mod"), FileChangeType::CHANGED);
        let pending = "pending-at-shutdown".to_owned();
        session.request(
            pending.clone(),
            WorkspaceSymbolRequest::METHOD,
            json!({ "query": "f" }),
        );
        session.begin_load();
        assert_eq!(session.scheduler().pending.len(), 1);
        assert_eq!(session.scheduler().searches.len(), 1);
        session.request(4, Shutdown::METHOD, Value::Null);
        session.error(pending, ErrorCode::RequestFailed, SHUTTING_DOWN);
        session.error(3, ErrorCode::RequestFailed, SHUTTING_DOWN);
        assert_eq!(session.reply(4).result, Some(Value::Null));
        assert!(matches!(session.state, State::Shutdown));
        session.request(5, WorkspaceSymbolRequest::METHOD, json!({ "query": "f" }));
        session.error(5, ErrorCode::InvalidRequest, "server has already shut down");
        session.count(8);
    }

    #[test]
    fn checks_generation_before_advancing_and_before_publishing() {
        struct InvalidatingTask {
            calls: Rc<Cell<usize>>,
            manifest: PathBuf,
        }

        impl SearchTask for InvalidatingTask {
            fn advance(&mut self, engine: &mut Engine) -> Result<bool> {
                self.calls.set(self.calls.get() + 1);
                engine.file_changed(&self.manifest, FileChangeType::CHANGED);
                Ok(true)
            }

            fn finish(self: Box<Self>) -> RequestResult {
                panic!("stale search published a result");
            }
        }

        let project = TestProject::new();
        let manifest = project.root.join("go.mod");
        let mut session = Session::start(&project);
        let calls = Rc::new(Cell::new(0));
        for (id, invalidate_before_step, expected_calls) in [(1, true, 0), (2, false, 1)] {
            let running = session.running();
            running.scheduler.searches.push_back(Searching {
                id: id.into(),
                generation: running.engine.workspace_load().generation,
                task: Box::new(InvalidatingTask {
                    calls: Rc::clone(&calls),
                    manifest: manifest.clone(),
                }),
            });
            if invalidate_before_step {
                running
                    .engine
                    .file_changed(&manifest, FileChangeType::CHANGED);
            }
            session.step();
            assert_eq!(calls.get(), expected_calls);
            session.error(id, ErrorCode::ContentModified, WORKSPACE_CHANGED);
        }
        session.finish_searches();
        session.count(3);
    }

    #[test]
    fn event_loop_preserves_capabilities_file_watches_and_lifecycle_errors() {
        let project = TestProject::new();
        let (server, client) = Connection::memory();
        let send_request = |id: i32, method: &str, params| {
            client
                .sender
                .send(Request::new(id.into(), method.into(), params).into())
                .unwrap();
        };
        let send_notification = |method: &str| {
            client
                .sender
                .send(Notification::new(method.into(), Value::Null).into())
                .unwrap();
        };
        send_request(1, WorkspaceSymbolRequest::METHOD, json!({ "query": "" }));
        send_request(
            2,
            Initialize::METHOD,
            json!({
                "rootUri": path_uri(&project.root).unwrap(),
                "capabilities": { "workspace": { "didChangeWatchedFiles": { "dynamicRegistration": true } } }
            }),
        );
        send_notification(Initialized::METHOD);
        send_request(3, Initialize::METHOD, Value::Null);
        send_request(4, "unsupported", Value::Null);
        send_request(
            5,
            HoverRequest::METHOD,
            json!({
                "textDocument": { "uri": "untitled:buffer" },
                "position": { "line": 0, "character": 0 }
            }),
        );
        send_request(6, WorkspaceSymbolRequest::METHOD, json!({ "query": " " }));
        send_request(7, Shutdown::METHOD, Value::Null);
        send_request(8, WorkspaceSymbolRequest::METHOD, json!({ "query": "" }));
        send_notification(Exit::METHOD);
        serve(&server).unwrap();

        let response = |id: i32| {
            let Message::Response(response) = client.receiver.try_recv().unwrap() else {
                panic!("expected an LSP response for {id}");
            };
            assert_eq!(response.id, id.into());
            response
        };
        let error = |id, code: ErrorCode, message: &str| {
            let ResponseError {
                code: actual,
                message: actual_message,
                data,
            } = response(id).error.unwrap();
            assert_eq!(actual, code as i32);
            assert_eq!(actual_message, message);
            assert!(data.is_none());
        };
        error(
            1,
            ErrorCode::ServerNotInitialized,
            "initialize must be called first",
        );
        let initialize = response(2).result.unwrap();
        assert_eq!(
            initialize["capabilities"],
            json!({
                "positionEncoding": "utf-16",
                "textDocumentSync": { "openClose": true, "change": 2, "save": true },
                "definitionProvider": true,
                "typeDefinitionProvider": true,
                "implementationProvider": true,
                "referencesProvider": true,
                "hoverProvider": true,
                "signatureHelpProvider": { "triggerCharacters": ["(", ","], "retriggerCharacters": [","] },
                "completionProvider": {
                    "triggerCharacters": ["."],
                    "resolveProvider": true,
                    "completionItem": { "labelDetailsSupport": true }
                },
                "documentSymbolProvider": true,
                "workspaceSymbolProvider": true,
                "semanticTokensProvider": {
                    "legend": {
                        "tokenTypes": ["namespace", "type", "struct", "interface", "typeParameter", "parameter", "variable", "property", "function", "method", "label"],
                        "tokenModifiers": ["declaration", "readonly", "defaultLibrary", "modification"]
                    },
                    "full": true
                },
                "documentHighlightProvider": true,
                "inlayHintProvider": true,
                "foldingRangeProvider": true,
                "diagnosticProvider": { "identifier": "rs-gopls", "interFileDependencies": true, "workspaceDiagnostics": false },
                "callHierarchyProvider": true,
                "typeHierarchyProvider": true,
                "documentFormattingProvider": true,
                "codeActionProvider": { "codeActionKinds": ["quickfix", "source.organizeImports"] },
                "workspace": { "workspaceFolders": { "supported": true, "changeNotifications": true } }
            })
        );
        let Message::Request(registration) = client.receiver.try_recv().unwrap() else {
            panic!("expected watched file registration");
        };
        assert_eq!(registration.method, "client/registerCapability");
        assert_eq!(
            registration.params,
            json!({
                "registrations": [{
                    "id": "rs-gopls/watch-go-files",
                    "method": DidChangeWatchedFiles::METHOD,
                    "registerOptions": { "watchers": [
                        { "globPattern": "**/*.go" }, { "globPattern": "**/go.mod" },
                        { "globPattern": "**/go.sum" }, { "globPattern": "**/go.work" },
                        { "globPattern": "**/go.work.sum" }
                    ] }
                }]
            })
        );
        error(
            3,
            ErrorCode::InvalidRequest,
            "server is already initialized",
        );
        error(
            4,
            ErrorCode::MethodNotFound,
            "unsupported request: unsupported",
        );
        error(
            5,
            ErrorCode::InvalidParams,
            "expected a file URI, got untitled:buffer",
        );
        assert_eq!(response(6).result, Some(json!([])));
        assert_eq!(response(7).result, Some(Value::Null));
        error(8, ErrorCode::InvalidRequest, "server has already shut down");
        assert!(client.receiver.is_empty());

        let (server, client) = Connection::memory();
        client
            .sender
            .send(Notification::new(Exit::METHOD.into(), Value::Null).into())
            .unwrap();
        assert_eq!(
            serve(&server).unwrap_err().to_string(),
            "exit arrived before shutdown"
        );
    }

    const EDITOR_SOURCE: &str = concat!(
        "package sample\r\n",
        "func plus(value int) int {\r\n",
        "\treturn value\r\n",
        "}\r\n",
        "func use() {\r\n",
        "\t_ = \"\u{1f600}\"; _ = plus(1)\r\n",
        "}\r\n",
    );

    fn editor_session(project: &TestProject, lines_only: Option<bool>) -> (TestSession, State) {
        let session = TestSession::new();
        let mut state = State::Starting;
        let capabilities = lines_only.map_or_else(
            || json!({}),
            |lines_only| json!({ "textDocument": { "foldingRange": { "lineFoldingOnly": lines_only } } }),
        );
        session.request(
            0,
            Initialize::METHOD,
            json!({ "rootUri": path_uri(&project.root).unwrap(), "capabilities": capabilities }),
            |connection, request| handle_request(connection, &mut state, request),
        );
        assert!(session.response(0).error.is_none());
        (session, state)
    }

    fn editor_features(
        session: &TestSession,
        state: &mut State,
        uri: &Uri,
        first_id: i32,
    ) -> [Value; 4] {
        let document = json!({ "uri": uri });
        let requests = [
            (
                SemanticTokensFullRequest::METHOD,
                json!({ "textDocument": document }),
            ),
            (
                DocumentHighlightRequest::METHOD,
                json!({ "textDocument": document, "position": { "line": 1, "character": 10 } }),
            ),
            (
                InlayHintRequest::METHOD,
                json!({ "textDocument": document, "range": {
                    "start": { "line": 0, "character": 0 }, "end": { "line": 6, "character": 1 }
                } }),
            ),
            (
                FoldingRangeRequest::METHOD,
                json!({ "textDocument": document }),
            ),
        ];
        for (index, (method, params)) in requests.into_iter().enumerate() {
            session.request(
                first_id + index as i32,
                method,
                params,
                |connection, request| handle_request(connection, state, request),
            );
        }
        let results = std::array::from_fn(|index| {
            let response = session.response(first_id + index as i32);
            assert!(response.error.is_none(), "{response:?}");
            response.result.unwrap()
        });
        session.assert_no_messages();
        results
    }

    #[test]
    fn editor_requests_use_utf16_and_negotiate_line_folding() {
        let project = TestProject::new();
        let path = project.write("editor.go", EDITOR_SOURCE);
        let uri = path_uri(&path).unwrap();
        for lines_only in [None, Some(false), Some(true)] {
            let (session, mut state) = editor_session(&project, lines_only);
            let [tokens, highlights, hints, folds] = editor_features(&session, &mut state, &uri, 1);
            let mut line = 0;
            let mut character = 0;
            let mut found_call = false;
            let data = tokens["data"].as_array().unwrap();
            assert_eq!(data.len() % 5, 0);
            for token in data.chunks_exact(5) {
                let delta_line = token[0].as_u64().unwrap();
                line += delta_line;
                if delta_line != 0 {
                    character = 0;
                }
                character += token[1].as_u64().unwrap();
                if line == 5 && character == 15 {
                    assert_eq!(&token[2..], &[json!(4), json!(8), json!(0)]);
                    found_call = true;
                }
            }
            assert!(found_call, "{tokens}");
            assert_eq!(
                highlights,
                json!([
                    { "range": { "start": { "line": 1, "character": 10 }, "end": { "line": 1, "character": 15 } }, "kind": 2 },
                    { "range": { "start": { "line": 2, "character": 8 }, "end": { "line": 2, "character": 13 } }, "kind": 2 }
                ])
            );
            assert_eq!(
                hints,
                json!([
                    { "position": { "line": 5, "character": 20 }, "label": "value:", "kind": 2, "paddingRight": true }
                ])
            );
            let expected = if lines_only == Some(true) {
                json!([{ "startLine": 1, "endLine": 2 }, { "startLine": 4, "endLine": 5 }])
            } else {
                json!([
                    { "startLine": 1, "startCharacter": 25, "endLine": 3, "endCharacter": 1 },
                    { "startLine": 4, "startCharacter": 11, "endLine": 6, "endCharacter": 1 }
                ])
            };
            assert_eq!(folds, expected);
        }
    }

    #[test]
    fn editor_requests_use_unsaved_snapshots_and_restore_disk_after_close() {
        let project = TestProject::new();
        let path = project.write("editor.go", EDITOR_SOURCE);
        let uri = path_uri(&path).unwrap();
        let (session, mut state) = editor_session(&project, None);
        let disk = editor_features(&session, &mut state, &uri, 1);

        let State::Running(running) = &mut state else {
            unreachable!()
        };
        session.open_go_document(&uri, EDITOR_SOURCE, |connection, notification| {
            handle_notification(
                connection,
                running,
                &notification.method,
                notification.params,
            )
        });
        assert_eq!(editor_features(&session, &mut state, &uri, 5), disk);

        let edited = EDITOR_SOURCE
            .replace("value", "edited")
            .replace("\treturn edited\r\n}", "\treturn edited }\r\n")
            + "var unfinished =\r\n";
        let State::Running(running) = &mut state else {
            unreachable!()
        };
        session.replace_document(&uri, 2, &edited, |connection, notification| {
            handle_notification(
                connection,
                running,
                &notification.method,
                notification.params,
            )
        });
        let unsaved = editor_features(&session, &mut state, &uri, 9);
        assert!(
            unsaved
                .iter()
                .zip(disk.iter())
                .all(|(draft, original)| draft != original)
        );
        assert_eq!(unsaved[2][0]["label"], "edited:");
        assert_eq!(unsaved[3][0]["endLine"], 2);

        let saved = EDITOR_SOURCE.replace("value", "savedArg");
        std::fs::write(&path, &saved).unwrap();
        let State::Running(running) = &mut state else {
            unreachable!()
        };
        session.notify(
            DidCloseTextDocument::METHOD,
            json!({ "textDocument": { "uri": uri } }),
            |connection, notification| {
                handle_notification(
                    connection,
                    running,
                    &notification.method,
                    notification.params,
                )
            },
        );
        let closed = editor_features(&session, &mut state, &uri, 13);
        assert_eq!(closed[2][0]["label"], "savedArg:");
        assert_eq!(closed[3][0]["endLine"], 3);
        let (fresh, mut fresh_state) = editor_session(&project, None);
        assert_eq!(closed, editor_features(&fresh, &mut fresh_state, &uri, 1));
    }

    #[test]
    fn editor_requests_finish_while_the_package_graph_is_loading() {
        let project = TestProject::new();
        let path = project.write("editor.go", EDITOR_SOURCE);
        let uri = path_uri(&path).unwrap();
        let (session, mut state) = editor_session(&project, None);
        let State::Running(running) = &mut state else {
            unreachable!()
        };
        let load = running.engine.workspace_load();
        running.scheduler.loading = Some(load);
        let paused = editor_features(&session, &mut state, &uri, 1);
        assert!(
            paused
                .iter()
                .enumerate()
                .all(|(index, value)| if index == 0 {
                    value["data"]
                        .as_array()
                        .is_some_and(|data| !data.is_empty())
                } else {
                    value.as_array().is_some_and(|data| !data.is_empty())
                })
        );

        let State::Running(running) = &mut state else {
            unreachable!()
        };
        assert!(running.scheduler.pending.is_empty());
        assert!(running.scheduler.requested.is_none());
        assert_eq!(running.scheduler.loading, Some(load));
        running
            .workspace_loaded(
                session.server(),
                WorkspaceResult {
                    load,
                    result: Ok(project.workspace(&["editor.go"], load.include_tests)),
                },
            )
            .unwrap();
        assert!(matches!(
            running.engine.snapshot().active_graph(false),
            QueryProgress::Ready(_)
        ));

        assert_eq!(paused, editor_features(&session, &mut state, &uri, 5));
    }
}
