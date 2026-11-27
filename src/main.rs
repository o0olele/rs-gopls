mod dispatch;
mod go_environment;
mod protocol;
mod rpc_telemetry;
mod scheduler;
mod scope;
mod semantic;
#[cfg(test)]
mod symbol_request_tests;
mod symbols;
mod telemetry;
#[cfg(test)]
mod test_support;
mod transport;
mod workspace;
mod workspace_roots;

use std::{
    collections::HashSet,
    env,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use crossbeam_channel::{bounded, never, select_biased, unbounded};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request};
use lsp_types::{
    CancelParams, DidChangeTextDocumentParams, DidChangeWatchedFilesParams,
    DidChangeWorkspaceFoldersParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, InitializeParams, NumberOrString, OneOf, PositionEncodingKind,
    PublishDiagnosticsParams, ServerCapabilities, ServerInfo, TextDocumentSyncCapability,
    TextDocumentSyncKind, TextDocumentSyncOptions, TextDocumentSyncSaveOptions, Uri,
    WorkspaceFolder, WorkspaceFoldersServerCapabilities, WorkspaceServerCapabilities,
    notification::{
        Cancel, DidChangeConfiguration, DidChangeTextDocument, DidChangeWatchedFiles,
        DidChangeWorkspaceFolders, DidCloseTextDocument, DidOpenTextDocument, DidSaveTextDocument,
        Exit, Initialized, Notification as _, PublishDiagnostics,
    },
    request::{Initialize, Request as _, Shutdown, WorkspaceDiagnosticRefresh},
};
use serde_json::Value;

use go_environment::{EnvironmentOverrides, GoEnvironment, environment_overrides};
use protocol::{
    RequestResult, file_path, invalid_params, path_uri, send_notification, send_response,
};
use scheduler::{DiagnosticConsumer, Scheduler, WorkspaceResult};
use semantic::Engine;
use workspace::path_key;

#[cfg(test)]
use lsp_server::{RequestId, Response};
#[cfg(test)]
use lsp_types::request::{DocumentSymbolRequest, WorkspaceSymbolRequest};

enum State {
    Starting(GoEnvironment),
    Running(Box<Running>),
    Shutdown,
}

impl State {
    fn starting() -> Self {
        Self::Starting(GoEnvironment::capture())
    }
}

struct Running {
    engine: Engine,
    watch_files: bool,
    client_features: dispatch::ClientFeatures,
    diagnostic_updates: DiagnosticUpdates,
    scheduler: Scheduler,
}

enum DiagnosticUpdates {
    None,
    Publish,
    Refresh { sequence: u64 },
}

impl Running {
    fn workspace_loaded(&mut self, connection: &Connection, update: WorkspaceResult) -> Result<()> {
        if self
            .scheduler
            .workspace_loaded(&mut self.engine, connection, update)?
        {
            self.workspace_diagnostics_changed(connection)?;
        }
        Ok(())
    }

    fn publish_document(&mut self, connection: &Connection, path: &Path, uri: Uri) -> Result<()> {
        if matches!(self.diagnostic_updates, DiagnosticUpdates::Publish)
            && self.engine.is_open(path)
        {
            self.scheduler.submit_diagnostics(
                &mut self.engine,
                connection,
                path.to_owned(),
                DiagnosticConsumer::Push(uri),
                "publish_diagnostics",
            )?;
        }
        Ok(())
    }

    fn publish_open_documents(&mut self, connection: &Connection) -> Result<()> {
        if matches!(self.diagnostic_updates, DiagnosticUpdates::Publish) {
            for path in self.engine.open_document_paths() {
                if let Err(error) = self.publish_document(connection, &path, path_uri(&path)?) {
                    eprintln!("rs-gopls: diagnostics for {}: {error:#}", path.display());
                }
            }
        }
        Ok(())
    }

    fn workspace_diagnostics_changed(&mut self, connection: &Connection) -> Result<()> {
        if let DiagnosticUpdates::Refresh { sequence } = &mut self.diagnostic_updates {
            *sequence += 1;
            let id = format!("rs-gopls/diagnostic-refresh/{sequence}");
            connection
                .sender
                .send(
                    Request::new(id.into(), WorkspaceDiagnosticRefresh::METHOD.to_owned(), ())
                        .into(),
                )
                .context("requesting a diagnostic refresh")
        } else {
            self.publish_open_documents(connection)
        }
    }

    fn advance_search(&mut self, connection: &Connection) -> Result<()> {
        self.scheduler.advance_search(&mut self.engine, connection)
    }
}

// Keep the existing in-memory symbol request harness attached to the session.
#[cfg(test)]
impl std::ops::Deref for Running {
    type Target = Scheduler;

    fn deref(&self) -> &Self::Target {
        &self.scheduler
    }
}

#[cfg(test)]
impl std::ops::DerefMut for Running {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.scheduler
    }
}

fn main() -> Result<()> {
    let mut args = env::args_os().skip(1);
    if args.next().is_some_and(|arg| arg == "version") {
        let name = env!("CARGO_PKG_NAME");
        let version = env!("CARGO_PKG_VERSION");
        match args.next() {
            None => println!("{name} {version}"),
            Some(flag) if flag == "-json" && args.next().is_none() => {
                let info = serde_json::json!({ "Main": { "Path": name, "Version": version } });
                println!("{info}");
            }
            _ => anyhow::bail!("usage: {name} version [-json]"),
        }
        return Ok(());
    }

    let _telemetry = telemetry::Session::start();
    let (connection, threads) = transport::stdio()?;
    let result = serve(&connection);
    drop(connection);
    result?;
    threads.join().context("joining LSP transport threads")
}

fn serve(connection: &Connection) -> Result<()> {
    let mut state = State::starting();
    let (load_sender, load_receiver) = unbounded();
    let (search_token, search_ready) = bounded(1);
    let search_idle = never();
    let diagnostic_idle = never();
    let highlight_idle = never();
    search_token
        .send(())
        .context("scheduling navigation searches")?;
    loop {
        let (
            search_receiver,
            completion_wake,
            reference_wake,
            implementation_wake,
            diagnostic_receiver,
            highlight_receiver,
        ) = match &state {
            State::Running(running) => (
                if running.scheduler.ready_search(&running.engine).is_some()
                    || running.engine.implementation_index_ready()
                {
                    &search_ready
                } else {
                    &search_idle
                },
                running.engine.completion_wake(),
                running.engine.reference_wake(),
                running.engine.implementation_wake(),
                running
                    .scheduler
                    .diagnostic_completions()
                    .unwrap_or(&diagnostic_idle),
                running
                    .scheduler
                    .highlight_completions()
                    .unwrap_or(&highlight_idle),
            ),
            _ => (
                &search_idle,
                &search_idle,
                &search_idle,
                &search_idle,
                &diagnostic_idle,
                &highlight_idle,
            ),
        };
        select_biased! {
            recv(connection.receiver) -> message => {
                let Ok(message) = message else {
                    break;
                };
                match message {
                    Message::Request(request) => {
                        let initializing = matches!(state, State::Starting(_)) && request.method == Initialize::METHOD;
                        handle_request(connection, &mut state, request)?;
                        if initializing && let State::Running(running) = &state {
                            running.engine.prewarm_completion_sdk(false);
                        }
                    }
                    Message::Notification(notification) if notification.method == Exit::METHOD => {
                        ensure!(
                            matches!(state, State::Shutdown),
                            "exit arrived before shutdown"
                        );
                        break;
                    }
                    Message::Notification(notification) => {
                        if let State::Running(running) = &mut state {
                            let Notification { method, params } = notification;
                            if method == Initialized::METHOD && std::mem::take(&mut running.watch_files) {
                                register_file_watches(connection)?;
                            } else if let Err(error) = handle_notification(connection, running, &method, params) {
                                eprintln!("rs-gopls: {method}: {error:#}");
                            }
                        }
                    }
                    Message::Response(_) => {}
                }
            }
            recv(load_receiver) -> update => {
                if let State::Running(running) = &mut state {
                    running.workspace_loaded(connection, update.context("receiving Go package metadata")?)?;
                }
            }
            recv(diagnostic_receiver) -> completed => {
                if let State::Running(running) = &mut state {
                    let completed = completed.context("receiving Go diagnostics")?;
                    running.scheduler.complete_diagnostics(&mut running.engine, connection, completed)?;
                }
            }
            recv(highlight_receiver) -> completed => {
                if let State::Running(running) = &mut state {
                    let completed = completed.context("receiving Go document highlights")?;
                    running.scheduler.complete_highlight(&mut running.engine, connection, completed)?;
                }
            }
            recv(completion_wake) -> _ => {}
            recv(reference_wake) -> _ => {}
            recv(implementation_wake) -> _ => {}
            recv(search_receiver) -> ready => {
                ready.context("receiving navigation search work")?;
                if let State::Running(running) = &mut state {
                    if running.scheduler.ready_search(&running.engine).is_some() {
                        running.advance_search(connection)?;
                    } else {
                        running.engine.advance_implementation_index()?;
                    }
                }
                search_token.send(()).context("scheduling further navigation searches")?;
            }
        }
        if let State::Running(running) = &mut state {
            running
                .scheduler
                .start_workspace_load(&mut running.engine, &load_sender)?;
        }
    }
    Ok(())
}

fn handle_request(connection: &Connection, state: &mut State, request: Request) -> Result<()> {
    let mut overrides = None;
    if matches!(state, State::Starting(_)) && request.method == Initialize::METHOD {
        overrides = request
            .params
            .get("initializationOptions")
            .and_then(environment_overrides);
        if let Some(overrides) = &mut overrides {
            telemetry::start_options(overrides);
        }
    }
    let _trace = if telemetry::enabled() {
        let method = match request.method.as_str() {
            Initialize::METHOD => Initialize::METHOD,
            Shutdown::METHOD => Shutdown::METHOD,
            method => dispatch::telemetry_method(method).unwrap_or("unknown"),
        };
        Some(rpc_telemetry::begin(&request, method))
    } else {
        None
    };
    let Request { id, method, params } = request;
    let result: RequestResult = match (&mut *state, method.as_str()) {
        (State::Starting(environment), Initialize::METHOD) => {
            initialize(params, environment.clone(), overrides)
                .map(|(running, capabilities)| {
                    *state = State::Running(Box::new(running));
                    capabilities
                })
                .map_err(invalid_params)
        }
        (State::Starting(_), _) => Err((
            ErrorCode::ServerNotInitialized,
            "initialize must be called first".into(),
        )),
        (State::Shutdown, _) => Err((
            ErrorCode::InvalidRequest,
            "server has already shut down".into(),
        )),
        (State::Running(..), Initialize::METHOD) => Err((
            ErrorCode::InvalidRequest,
            "server is already initialized".into(),
        )),
        (State::Running(running), Shutdown::METHOD) => {
            running.scheduler.shutdown(connection)?;
            *state = State::Shutdown;
            Ok(Value::Null)
        }
        (State::Running(running), _) => {
            return dispatch::handle(
                connection,
                &mut running.engine,
                &mut running.scheduler,
                running.client_features,
                Request { id, method, params },
            );
        }
    };
    send_response(connection, id, result)
}

fn initialize(
    params: Value,
    mut environment: GoEnvironment,
    overrides: Option<EnvironmentOverrides>,
) -> Result<(Running, Value)> {
    // lsp-types 0.97 deserializes workspace.diagnostic, while LSP 3.17 uses workspace.diagnostics.
    let refresh_support = params
        .pointer("/capabilities/workspace/diagnostics/refreshSupport")
        .and_then(Value::as_bool);
    let params = serde_json::from_value::<InitializeParams>(params)
        .context("invalid initialize parameters")?;
    let watch_files = params
        .capabilities
        .workspace
        .as_ref()
        .and_then(|workspace| workspace.did_change_watched_files.as_ref())
        .is_some_and(|watches| watches.dynamic_registration == Some(true));
    let client_features = dispatch::ClientFeatures::from_initialize(&params);
    let document = params.capabilities.text_document.as_ref();
    let pull = document.and_then(|d| d.diagnostic.as_ref()).is_some();
    let publish = document
        .and_then(|d| d.publish_diagnostics.as_ref())
        .is_some();
    let refresh = refresh_support.or_else(|| {
        params
            .capabilities
            .workspace
            .as_ref()
            .and_then(|w| w.diagnostic.as_ref())
            .and_then(|d| d.refresh_support)
    }) == Some(true);
    let diagnostic_updates = match (pull, refresh, publish) {
        (true, true, _) => DiagnosticUpdates::Refresh { sequence: 0 },
        (false, _, true) => DiagnosticUpdates::Publish,
        _ => DiagnosticUpdates::None,
    };
    if let Some(overrides) = overrides {
        environment = environment.with_overrides(overrides);
    }
    let roots = workspace_roots(params)?;
    telemetry::session_config(&roots, &environment);

    let capabilities = ServerCapabilities {
        position_encoding: Some(PositionEncodingKind::UTF16),
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::INCREMENTAL),
                save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                ..Default::default()
            },
        )),
        workspace: Some(WorkspaceServerCapabilities {
            workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                supported: Some(true),
                change_notifications: Some(OneOf::Left(true)),
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let result = rpc_telemetry::serialize(|| {
        serde_json::json!({
            "capabilities": dispatch::capabilities(capabilities, client_features),
            "serverInfo": ServerInfo {
                name: env!("CARGO_PKG_NAME").into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }
        })
    });
    Ok((
        Running {
            watch_files,
            client_features,
            diagnostic_updates,
            engine: Engine::with_environment(roots, environment),
            scheduler: Scheduler::default(),
        },
        result,
    ))
}

#[allow(deprecated)]
fn workspace_roots(params: InitializeParams) -> Result<Vec<PathBuf>> {
    let mut roots = Vec::new();
    add_workspace_folders(&mut roots, params.workspace_folders.unwrap_or_default())?;
    if roots.is_empty() {
        let root = match (params.root_uri, params.root_path) {
            (Some(uri), _) => file_path(&uri)?,
            (None, Some(path)) => env::current_dir()?.join(path),
            (None, None) => env::current_dir()
                .context("getting the current directory for a client without a workspace")?,
        };
        roots.push(workspace_directory(root)?);
    }
    Ok(roots)
}

fn add_workspace_folders(roots: &mut Vec<PathBuf>, folders: Vec<WorkspaceFolder>) -> Result<()> {
    let mut seen: HashSet<_> = roots.iter().map(|root| path_key(root)).collect();
    for folder in folders {
        let path = workspace_directory(file_path(&folder.uri)?)?;
        if seen.insert(path_key(&path)) {
            roots.push(path);
        }
    }
    Ok(())
}

fn workspace_directory(path: PathBuf) -> Result<PathBuf> {
    ensure!(
        path.is_dir(),
        "workspace is not a directory: {}",
        path.display()
    );
    Ok(path)
}

fn register_file_watches(connection: &Connection) -> Result<()> {
    let watchers: Vec<_> = [
        "**/*.go",
        "**/go.mod",
        "**/go.sum",
        "**/go.work",
        "**/go.work.sum",
    ]
    .into_iter()
    .map(|pattern| serde_json::json!({ "globPattern": pattern }))
    .collect();
    let request = Request::new(
        "rs-gopls/watch-go-files".to_owned().into(),
        "client/registerCapability".to_owned(),
        serde_json::json!({
            "registrations": [{
                "id": "rs-gopls/watch-go-files",
                "method": DidChangeWatchedFiles::METHOD,
                "registerOptions": { "watchers": watchers }
            }]
        }),
    );
    connection
        .sender
        .send(request.into())
        .context("registering Go file watches")
}

fn handle_notification(
    connection: &Connection,
    running: &mut Running,
    method: &str,
    params: Value,
) -> Result<()> {
    match method {
        DidChangeConfiguration::METHOD => {
            let overrides = params
                .get("settings")
                .and_then(environment_overrides)
                .and_then(|mut overrides| {
                    let explicit_empty = overrides.is_empty();
                    overrides.remove(telemetry::FILE_ENV);
                    overrides.remove(telemetry::REDACT_ENV);
                    (explicit_empty || !overrides.is_empty()).then_some(overrides)
                });
            let env_changed =
                overrides.is_some_and(|overrides| running.engine.update_environment(overrides));
            telemetry::event!("notification", {
                "method": DidChangeConfiguration::METHOD, "environment_changed": env_changed,
            });
            if env_changed {
                telemetry::session_config(running.engine.roots(), running.engine.environment());
                running
                    .scheduler
                    .environment_modified(connection, running.engine.workspace_load())?;
            }
            Ok(())
        }
        DidChangeWorkspaceFolders::METHOD => {
            let params: DidChangeWorkspaceFoldersParams = serde_json::from_value(params)?;
            telemetry::event!("notification", {
                "method": DidChangeWorkspaceFolders::METHOD,
                "added_count": params.event.added.len(), "removed_count": params.event.removed.len(),
            });
            let removed = params
                .event
                .removed
                .iter()
                .map(|folder| file_path(&folder.uri).map(|path| path_key(&path)))
                .collect::<Result<HashSet<_>>>()?;
            let mut roots = running.engine.roots().to_vec();
            roots.retain(|root| !removed.contains(&path_key(root)));
            add_workspace_folders(&mut roots, params.event.added)?;
            if running.engine.update_roots(roots) {
                telemetry::session_config(running.engine.roots(), running.engine.environment());
                running.scheduler.workspace_modified(
                    connection,
                    running.engine.workspace_load(),
                    "didChangeWorkspaceFolders",
                )?;
                running
                    .scheduler
                    .revalidate_diagnostics(&mut running.engine, connection)?;
                running.workspace_diagnostics_changed(connection)?;
            }
            Ok(())
        }
        DidChangeWatchedFiles::METHOD => {
            let params: DidChangeWatchedFilesParams = serde_json::from_value(params)?;
            let changed_file_count = params.changes.len();
            telemetry::event!("notification", {
                "method": DidChangeWatchedFiles::METHOD, "changed_file_count": changed_file_count,
            });
            let before = running.engine.workspace_load();
            for change in params.changes {
                if let Ok(path) = file_path(&change.uri) {
                    running.engine.file_changed_from(
                        &path,
                        change.typ,
                        "watched_files",
                        changed_file_count,
                    );
                    if !running.engine.is_open(&path) {
                        running.scheduler.document_modified(connection, &path)?;
                    }
                }
            }
            let after = running.engine.workspace_load();
            if after != before {
                running
                    .scheduler
                    .workspace_modified(connection, after, "watched_files")?;
            }
            running
                .scheduler
                .revalidate_diagnostics(&mut running.engine, connection)?;
            running.publish_open_documents(connection)
        }
        DidOpenTextDocument::METHOD => {
            let params: DidOpenTextDocumentParams = serde_json::from_value(params)?;
            let document = params.text_document;
            let path = file_path(&document.uri)?;
            telemetry::event!("notification", {
                "method": DidOpenTextDocument::METHOD, "document": telemetry::path(&path),
                "version": document.version,
            });
            running
                .engine
                .open(path.clone(), document.text, document.version)?;
            running.scheduler.document_modified(connection, &path)?;
            running
                .scheduler
                .revalidate_diagnostics(&mut running.engine, connection)?;
            running.publish_document(connection, &path, document.uri)
        }
        DidChangeTextDocument::METHOD => {
            let params: DidChangeTextDocumentParams = serde_json::from_value(params)?;
            let path = file_path(&params.text_document.uri)?;
            telemetry::event!("notification", {
                "method": DidChangeTextDocument::METHOD, "document": telemetry::path(&path),
                "version": params.text_document.version, "change_count": params.content_changes.len(),
            });
            running
                .engine
                .edit(&path, &params.content_changes, params.text_document.version)?;
            running.scheduler.document_modified(connection, &path)?;
            running
                .scheduler
                .revalidate_diagnostics(&mut running.engine, connection)?;
            running.publish_document(connection, &path, params.text_document.uri)
        }
        DidCloseTextDocument::METHOD => {
            let params: DidCloseTextDocumentParams = serde_json::from_value(params)?;
            let path = file_path(&params.text_document.uri)?;
            telemetry::event!("notification", {
                "method": DidCloseTextDocument::METHOD, "document": telemetry::path(&path),
            });
            let was_open = running.engine.is_open(&path);
            running.engine.close(&path);
            running.scheduler.document_modified(connection, &path)?;
            running
                .scheduler
                .revalidate_diagnostics(&mut running.engine, connection)?;
            if was_open && matches!(running.diagnostic_updates, DiagnosticUpdates::Publish) {
                send_notification::<PublishDiagnostics>(
                    connection,
                    PublishDiagnosticsParams::new(params.text_document.uri, Vec::new(), None),
                )?;
            }
            Ok(())
        }
        DidSaveTextDocument::METHOD => {
            let params: DidSaveTextDocumentParams = serde_json::from_value(params)?;
            let path = file_path(&params.text_document.uri)?;
            telemetry::event!("notification", {
                "method": DidSaveTextDocument::METHOD, "document": telemetry::path(&path),
            });
            let before = running.engine.workspace_load();
            running.engine.file_changed_from(
                &path,
                lsp_types::FileChangeType::CHANGED,
                "didSave",
                1,
            );
            running.scheduler.document_modified(connection, &path)?;
            let after = running.engine.workspace_load();
            if after != before {
                running
                    .scheduler
                    .workspace_modified(connection, after, "didSave")?;
            }
            running
                .scheduler
                .revalidate_diagnostics(&mut running.engine, connection)?;
            Ok(())
        }
        Cancel::METHOD => {
            let params: CancelParams = serde_json::from_value(params)?;
            let id = match params.id {
                NumberOrString::Number(value) => value.into(),
                NumberOrString::String(value) => value.into(),
            };
            telemetry::event!("notification", {
                "method": Cancel::METHOD, "req_id": rpc_telemetry::request_id(&id),
            });
            running.scheduler.cancel(connection, &id)
        }
        _ => Ok(()),
    }
}
