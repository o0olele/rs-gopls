mod diagnostics;
mod highlights;

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender};
use lsp_server::{Connection, ErrorCode, Request, RequestId};
use lsp_types::{
    FileChangeType, Position,
    request::{Completion, Request as _},
};

use crate::dispatch::OpenRequestScope;
use crate::protocol::{RequestResult, internal_error, send_response};
use crate::semantic::{Engine, LoadedWorkspace, QueryProgress, SharedLoadPending};
use crate::workspace::{LoadTrace, Workspace, WorkspaceLoad, path_key};
use crate::{rpc_telemetry, telemetry};

use self::diagnostics::Diagnostics;
pub(super) use self::diagnostics::{
    Completed as DiagnosticCompletion, Consumer as DiagnosticConsumer, Mode as DiagnosticMode,
    RefreshMark as DiagnosticRefreshMark,
};
pub(super) use self::highlights::Completed as HighlightCompletion;
use self::highlights::Highlights;

const WORKSPACE_CHANGED: &str = "the workspace changed during search";
const SHUTTING_DOWN: &str = "the server is shutting down";
const WATCH_WINDOW: Duration = Duration::from_millis(75);
const TEST_VIEW_IDLE_DELAY: Duration = Duration::from_millis(150);
const TEST_VIEW_PREWARM: &str = "test_view_prewarm";

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

    fn is_completion(&self) -> bool {
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
    request: Pending,
    generation: u64,
    task: SearchWork,
}

enum SearchWork {
    Semantic(Box<dyn SearchTask>),
    SharedSource(SharedLoadPending),
}

impl SearchWork {
    fn ready(&self, engine: &Engine) -> bool {
        match self {
            Self::Semantic(task) => task.ready(engine),
            Self::SharedSource(pending) => pending.ready(),
        }
    }
}

impl Searching {
    fn discard(&mut self, code: ErrorCode) {
        match &mut self.task {
            SearchWork::Semantic(task) => task.discard(code),
            SearchWork::SharedSource(_) => self.request.query.discard(code),
        }
    }
}

pub(super) struct WorkspaceResult {
    pub(super) load: WorkspaceLoad,
    pub(super) result: Result<LoadedWorkspace>,
}

#[derive(Default)]
struct WatchedFiles {
    deadline: Option<Instant>,
    changes: BTreeMap<PathBuf, (PathBuf, FileChangeType)>,
}

struct PrewarmingTests {
    cancelled: Arc<AtomicBool>,
    promoted: bool,
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        if let Some(warmup) = &self.prewarming_tests {
            warmup.cancelled.store(true, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
impl WorkspaceResult {
    pub(crate) fn for_test(
        engine: &Engine,
        load: WorkspaceLoad,
        result: Result<Workspace>,
    ) -> Self {
        Self {
            load,
            result: result
                .map(|workspace| engine.workspace_cache_snapshot(load).validate(workspace)),
        }
    }
}

#[derive(Default)]
pub(super) struct Scheduler {
    pub(super) pending: Vec<Pending>,
    pub(super) searches: VecDeque<Searching>,
    pub(super) loading: Option<WorkspaceLoad>,
    pub(super) requested: Option<WorkspaceLoad>,
    requested_from: &'static str,
    load_trace: Option<(Instant, &'static str)>,
    prewarm: Option<(WorkspaceLoad, Instant)>,
    prewarming_tests: Option<PrewarmingTests>,
    deferred_test_view: Option<WorkspaceResult>,
    workspace_file_event: bool,
    watched_files: WatchedFiles,
    diagnostics: Diagnostics,
    highlights: Highlights,
    open_requests: Vec<(Request, OpenRequestScope)>,
}

impl Scheduler {
    pub(super) fn defer_open_request(
        &mut self,
        request: Request,
        scope: OpenRequestScope,
        reason: &'static str,
    ) {
        rpc_telemetry::wait(|| reason);
        self.open_requests.push((request, scope));
    }

    pub(super) fn take_ready_open_requests(&mut self, engine: &Engine) -> Vec<Request> {
        if self.watching_files() {
            return Vec::new();
        }
        self.open_requests
            .extract_if(.., |(_, scope)| !scope.waiting(engine))
            .map(|(request, _)| request)
            .collect()
    }

    pub(super) fn watching_files(&self) -> bool {
        self.watched_files.deadline.is_some()
    }

    pub(super) fn workspace_deadline(&self) -> Option<Instant> {
        self.watched_files.deadline.or_else(|| {
            self.prewarm
                .filter(|_| self.loading.is_none())
                .map(|(_, at)| at)
        })
    }

    pub(super) fn watch_file(&mut self, path: PathBuf, change: FileChangeType, now: Instant) {
        self.record_workspace_file_event();
        self.watched_files
            .deadline
            .get_or_insert(now + WATCH_WINDOW);
        self.watched_files
            .changes
            .insert(path_key(&path), (path, change));
    }

    pub(super) fn watch_save(&mut self, path: PathBuf) {
        self.record_workspace_file_event();
        self.watched_files
            .changes
            .entry(path_key(&path))
            .or_insert((path, FileChangeType::CHANGED));
    }

    pub(super) fn record_workspace_file_event(&mut self) {
        // Capture each arrival before coalescing. The loader could have read
        // an intermediate state even if the last event leaves the graph clean.
        if self.loading.is_some() {
            self.workspace_file_event = true;
            if let Some(warmup) = &self.prewarming_tests {
                warmup.cancelled.store(true, Ordering::Relaxed);
            }
        }
    }

    pub(super) fn take_watched_files(
        &mut self,
        now: Instant,
    ) -> Option<Vec<(PathBuf, FileChangeType)>> {
        let deadline = self.watched_files.deadline?;
        if now < deadline {
            return None;
        }
        self.watched_files.deadline = None;
        Some(
            std::mem::take(&mut self.watched_files.changes)
                .into_values()
                .collect(),
        )
    }

    pub(super) fn with_diagnostic_mode(mode: DiagnosticMode) -> Self {
        let mut scheduler = Self::default();
        scheduler.diagnostics.set_mode(mode);
        scheduler
    }

    pub(super) fn note_diagnostic_edit(&mut self, path: &Path, now: Instant) {
        self.diagnostics.note_edit(path, now);
    }

    pub(super) fn interrupt_document_diagnostics(&mut self, path: &Path) {
        self.diagnostics.interrupt_for_edit(path);
    }

    pub(super) fn diagnostic_deadline(&self) -> Option<Instant> {
        self.diagnostics.next_deadline()
    }

    pub(super) fn advance_diagnostics(
        &mut self,
        engine: &mut Engine,
        connection: &Connection,
        now: Instant,
    ) -> Result<()> {
        self.diagnostics.advance(engine, connection, now)
    }

    pub(super) fn drain_diagnostic_refresh(&mut self) -> Vec<DiagnosticRefreshMark> {
        self.diagnostics.drain_refresh()
    }

    pub(super) fn diagnostic_refresh_is_current(
        &self,
        engine: &mut Engine,
        mark: &DiagnosticRefreshMark,
    ) -> bool {
        self.diagnostics.refresh_is_current(engine, mark)
    }

    pub(super) fn request_workspace_from(&mut self, load: WorkspaceLoad, trigger: &'static str) {
        if let Some(loading) = self.loading {
            if !self.workspace_file_event && loading.covers(load) {
                if let Some(warmup) = &mut self.prewarming_tests
                    && !warmup.promoted
                {
                    warmup.promoted = true;
                    telemetry::event!("workspace_load_promoted", {
                        "trigger": trigger,
                        "target_generation": load.generation,
                        "include_tests": loading.include_tests,
                    });
                }
                return;
            }
            if let Some(warmup) = &self.prewarming_tests {
                warmup.cancelled.store(true, Ordering::Relaxed);
            }
        }
        self.prewarm = None;
        if self
            .requested
            .is_some_and(|requested| requested.covers(load))
        {
            return;
        }
        self.requested = Some(load);
        self.requested_from = trigger;
    }

    pub(super) fn start_workspace_load(
        &mut self,
        engine: &mut Engine,
        results: &Sender<WorkspaceResult>,
    ) -> Result<()> {
        let current = engine.workspace_load();
        if let Some(loading) = self.loading {
            if loading.generation != current.generation
                && let Some(warmup) = &self.prewarming_tests
            {
                warmup.cancelled.store(true, Ordering::Relaxed);
                if self.requested.is_none() {
                    self.request_workspace_from(current, "workspace_changed");
                }
            }
            return Ok(());
        }
        if self.watching_files() {
            return Ok(());
        }
        let Some((load, trigger)) = self.take_workspace_load(engine, Instant::now()) else {
            return Ok(());
        };
        if trigger != TEST_VIEW_PREWARM {
            engine.prewarm_completion_sdk(true);
        }
        let roots = engine.roots().to_vec();
        let environment = engine.environment().clone();
        let track_rename_inputs = engine.track_rename_inputs;
        let cache_snapshot = engine.begin_workspace_cache_validation(load);
        let results = results.clone();
        let cancelled = (trigger == TEST_VIEW_PREWARM).then(|| Arc::new(AtomicBool::new(false)));
        let worker_cancelled = cancelled.clone();
        let in_flight_generation = Some(load.generation);
        self.load_trace = telemetry::now().map(|start| {
            telemetry::emit(
                "workspace_load_start",
                serde_json::json!({
                    "trigger": trigger,
                    "target_generation": load.generation,
                    "in_flight_generation": in_flight_generation,
                    "include_tests": load.include_tests,
                }),
            );
            (start, trigger)
        });
        let worker = thread::Builder::new()
            .name("go-package-loader".into())
            .spawn(move || {
                let _trace = LoadTrace::enter(trigger, load, in_flight_generation);
                let result = if let Some(cancelled) = &worker_cancelled {
                    Workspace::prewarm_tests(&roots, &environment, track_rename_inputs, cancelled)
                } else if track_rename_inputs {
                    Workspace::load_roots_for_rename(&roots, load.include_tests, &environment)
                } else {
                    Workspace::load_roots_in_environment(&roots, load.include_tests, &environment)
                };
                let result = result.map(|workspace| cache_snapshot.validate(workspace));
                let _ = results.send(WorkspaceResult { load, result });
            });
        if let Err(error) = worker {
            engine.discard_workspace_cache_validation(load);
            return Err(error).context("starting Go package loader");
        }
        self.prewarming_tests = cancelled.map(|cancelled| PrewarmingTests {
            cancelled,
            promoted: false,
        });
        self.loading = Some(load);
        Ok(())
    }

    fn take_workspace_load(
        &mut self,
        engine: &Engine,
        now: Instant,
    ) -> Option<(WorkspaceLoad, &'static str)> {
        if let Some(load) = self.requested.take() {
            self.prewarm = None;
            Some((load, self.requested_from))
        } else {
            let (load, deadline) = self.prewarm?;
            if now < deadline {
                return None;
            }
            self.prewarm = None;
            (engine.test_view_prewarm() == Some(load)).then_some((load, TEST_VIEW_PREWARM))
        }
    }

    fn should_defer_test_view(&self, engine: &Engine, load: WorkspaceLoad) -> bool {
        self.loading == Some(load)
            && engine.workspace_load().generation == load.generation
            && !self.workspace_file_event
            && self
                .prewarming_tests
                .as_ref()
                .is_some_and(|job| !job.promoted && !job.cancelled.load(Ordering::Relaxed))
            && (self
                .searches
                .iter()
                .any(|search| search.request.query.is_completion())
                || self
                    .open_requests
                    .iter()
                    .any(|(request, _)| request.method == Completion::METHOD))
    }

    pub(super) fn take_deferred_test_view(&mut self, engine: &Engine) -> Option<WorkspaceResult> {
        let load = self.deferred_test_view.as_ref()?.load;
        if self.watching_files() || self.should_defer_test_view(engine, load) {
            return None;
        }
        self.deferred_test_view.take()
    }

    pub(super) fn workspace_loaded(
        &mut self,
        engine: &mut Engine,
        connection: &Connection,
        update: WorkspaceResult,
    ) -> Result<bool> {
        if self.loading.is_some() && self.loading != Some(update.load) {
            return Ok(false);
        }
        let loading = self.loading == Some(update.load);
        let file_event = self.workspace_file_event;
        if loading && let Some((start, trigger)) = self.load_trace.take() {
            let current = engine.workspace_load();
            let failure = match &update.result {
                Ok(loaded) => loaded.workspace.total_load_failure().is_some(),
                Err(_) => true,
            };
            telemetry::emit(
                "workspace_load_done",
                serde_json::json!({
                    "trigger": trigger,
                    "target_generation": update.load.generation,
                    "in_flight_generation": update.load.generation,
                    "include_tests": update.load.include_tests,
                    "load_ms": telemetry::elapsed(start),
                    "failed": failure,
                    "cancelled": self.prewarming_tests.as_ref().is_some_and(|job| job.cancelled.load(Ordering::Relaxed)),
                    "stale": file_event || current.generation != update.load.generation || failure && current != update.load,
                }),
            );
        }
        if update.result.is_ok() && self.should_defer_test_view(engine, update.load) {
            self.deferred_test_view = Some(update);
            return Ok(false);
        }
        self.workspace_file_event = false;
        let warmup = if loading {
            self.loading = None;
            self.prewarming_tests.take()
        } else {
            None
        };
        let background_only = warmup.as_ref().is_some_and(|job| !job.promoted);
        if file_event {
            engine.discard_workspace_cache_validation(update.load);
            if self.requested.is_none() {
                if background_only && let Some(load) = engine.test_view_prewarm() {
                    self.prewarm = Some((load, Instant::now() + TEST_VIEW_IDLE_DELAY));
                } else {
                    self.request_workspace_from(engine.workspace_load(), "workspace_file_event");
                }
            }
            return Ok(false);
        }
        if warmup.is_some_and(|job| job.cancelled.load(Ordering::Relaxed)) {
            engine.discard_workspace_cache_validation(update.load);
            return Ok(false);
        }
        let mut installed = false;
        match update.result {
            Ok(loaded) => {
                let failure = loaded.workspace.total_load_failure().map(str::to_owned);
                if (failure.is_none() || !background_only && update.load == engine.workspace_load())
                    && (!background_only
                        || engine.can_install_background_test_view(&loaded.workspace))
                    && engine.install_loaded_workspace(update.load, loaded)
                {
                    installed = true;
                    self.revalidate_diagnostics(engine, connection)?;
                    if let Some(message) = failure {
                        // Diagnostics can use the installed failures. Graph
                        // queries retain their load error and can retry later.
                        self.fail_pending_load(engine, connection, &message)?;
                    } else {
                        let replay_start = telemetry::now();
                        let replay_count = self.pending.len() + self.diagnostics.pending_count();
                        if let Some(load) = self.diagnostics.replay(engine, connection, false)? {
                            self.request_workspace_from(load, "pending_diagnostics");
                        }
                        for pending in std::mem::take(&mut self.pending) {
                            let Pending {
                                id,
                                document_key,
                                query,
                            } = pending;
                            let _trace = rpc_telemetry::step(&id, true);
                            let progress = {
                                let _navigation = rpc_telemetry::navigation_step();
                                run_query(query.as_ref(), engine)
                            };
                            self.resolve(engine, connection, id, progress, |id| Pending {
                                id,
                                document_key,
                                query,
                            })?;
                        }
                        if let Some(start) = replay_start {
                            telemetry::emit(
                                "workspace_replay",
                                serde_json::json!({
                                    "target_generation": update.load.generation,
                                    "replay_count": replay_count,
                                    "replay_ms": telemetry::elapsed(start),
                                }),
                            );
                        }
                    }
                }
                engine.discard_workspace_cache_validation(update.load);
            }
            Err(error) if !background_only && update.load == engine.workspace_load() => {
                engine.discard_workspace_cache_validation(update.load);
                self.fail_pending_load(engine, connection, &format!("{error:#}"))?;
            }
            Err(_) => engine.discard_workspace_cache_validation(update.load),
        }
        if !self.pending.is_empty() {
            self.request_workspace_from(engine.workspace_load(), "pending_requests");
        }
        if installed && self.requested.is_none() {
            self.prewarm = engine
                .test_view_prewarm()
                .map(|load| (load, Instant::now() + TEST_VIEW_IDLE_DELAY));
        }
        Ok(installed)
    }

    fn fail_pending_load(
        &mut self,
        engine: &mut Engine,
        connection: &Connection,
        message: &str,
    ) -> Result<()> {
        if self.pending.is_empty() && self.diagnostics.pending_count() == 0 {
            eprintln!("rs-gopls: loading Go packages: {message}");
        }
        self.diagnostics.replay(engine, connection, true)?;
        for pending in std::mem::take(&mut self.pending) {
            let _trace = rpc_telemetry::step(&pending.id, false);
            rpc_telemetry::wait(|| "graph_load_failed");
            let result = pending.query.load_failed(engine, message);
            send_response(connection, pending.id, result)?;
        }
        Ok(())
    }

    pub(super) fn submit_diagnostics(
        &mut self,
        engine: &mut Engine,
        connection: &Connection,
        path: PathBuf,
        consumer: DiagnosticConsumer,
        trigger: &'static str,
    ) -> Result<()> {
        if let Some(load) = self
            .diagnostics
            .submit(engine, connection, path, consumer, false)?
        {
            self.request_workspace_from(load, trigger);
        }
        Ok(())
    }

    pub(super) fn diagnostic_completions(&self) -> Option<&Receiver<DiagnosticCompletion>> {
        self.diagnostics.receiver()
    }

    pub(super) fn submit_highlight(
        &mut self,
        engine: &mut Engine,
        connection: &Connection,
        id: RequestId,
        path: PathBuf,
        position: Position,
    ) -> Result<()> {
        self.highlights
            .submit(engine, connection, id, path, position)
    }

    pub(super) fn highlight_completions(&self) -> Option<&Receiver<HighlightCompletion>> {
        self.highlights.receiver()
    }

    pub(super) fn complete_highlight(
        &mut self,
        engine: &mut Engine,
        connection: &Connection,
        completed: HighlightCompletion,
    ) -> Result<()> {
        self.highlights.complete(engine, connection, completed)
    }

    pub(super) fn complete_diagnostics(
        &mut self,
        engine: &mut Engine,
        connection: &Connection,
        completed: DiagnosticCompletion,
    ) -> Result<()> {
        self.diagnostics.complete(engine, connection, completed)
    }

    pub(super) fn revalidate_diagnostics(
        &mut self,
        engine: &mut Engine,
        connection: &Connection,
    ) -> Result<()> {
        if let Some(load) = self.diagnostics.revalidate(engine, connection)? {
            self.request_workspace_from(load, "publish_diagnostics");
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
        let progress = {
            let _navigation = rpc_telemetry::navigation_step();
            run_query(&query, engine)
        };
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
                let trigger = rpc_telemetry::wait(|| engine.workspace_wait_reason(load));
                self.pending.push(defer(id));
                self.request_workspace_from(load, trigger);
                return Ok(());
            }
            Ok(QueryProgress::Searching(task)) => {
                self.searches.push_back(Searching {
                    request: defer(id),
                    generation: engine.workspace_load().generation,
                    task: SearchWork::Semantic(task),
                });
                return Ok(());
            }
            Ok(QueryProgress::Ready(result)) => result,
            Err(error) => match error.downcast::<SharedLoadPending>() {
                Ok(pending) => {
                    self.searches.push_back(Searching {
                        request: defer(id),
                        generation: engine.workspace_load().generation,
                        task: SearchWork::SharedSource(pending),
                    });
                    return Ok(());
                }
                Err(error) => Err(internal_error(error)),
            },
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
        let _trace = rpc_telemetry::step(&search.request.id, false);
        let result = if engine.workspace_load().generation != search.generation {
            search.discard(ErrorCode::ContentModified);
            Err((ErrorCode::ContentModified, WORKSPACE_CHANGED.to_owned()))
        } else {
            match search.task {
                SearchWork::SharedSource(_) => {
                    let Pending {
                        id,
                        document_key,
                        query,
                    } = search.request;
                    let progress = {
                        let _navigation = rpc_telemetry::navigation_step();
                        run_query(query.as_ref(), engine)
                    };
                    return self.resolve(engine, connection, id, progress, |id| Pending {
                        id,
                        document_key,
                        query,
                    });
                }
                SearchWork::Semantic(mut task) => {
                    let progress = {
                        let _navigation = rpc_telemetry::navigation_step();
                        task.advance(engine)
                    };
                    if engine.workspace_load().generation != search.generation {
                        task.discard(ErrorCode::ContentModified);
                        Err((ErrorCode::ContentModified, WORKSPACE_CHANGED.to_owned()))
                    } else {
                        match progress {
                            Ok(false) => {
                                search.task = SearchWork::Semantic(task);
                                self.searches.push_back(search);
                                return Ok(());
                            }
                            Ok(true) => task.finish(),
                            Err(error) => match error.downcast::<SharedLoadPending>() {
                                Ok(pending) => {
                                    search.task = SearchWork::SharedSource(pending);
                                    self.searches.push_back(search);
                                    return Ok(());
                                }
                                Err(error) => {
                                    task.discard(ErrorCode::InternalError);
                                    Err(internal_error(error))
                                }
                            },
                        }
                    }
                }
            }
        };
        send_response(connection, search.request.id, result)
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
            if matches(&search.request.id) {
                search.discard(code);
                drop(search.task);
                send_response(
                    connection,
                    search.request.id,
                    Err((code, message.to_owned())),
                )?;
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
        rpc_telemetry::cancelled(id);
        self.cancel_open_requests(
            connection,
            |request, _| &request.id == id,
            ErrorCode::RequestCanceled,
            "the client cancelled the request",
        )?;
        self.highlights.cancel(connection, id)?;
        self.diagnostics.cancel(connection, id)?;
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
        self.invalidate_document_queries(connection, path)
    }

    fn invalidate_document_queries(&mut self, connection: &Connection, path: &Path) -> Result<()> {
        self.cancel_open_requests(
            connection,
            |_, scope| scope.concerns(path),
            ErrorCode::ContentModified,
            "the document changed while parsing Go syntax",
        )?;
        self.watched_document_modified(connection, path)
    }

    pub(super) fn watched_document_modified(
        &mut self,
        connection: &Connection,
        path: &Path,
    ) -> Result<()> {
        self.highlights.document_modified(connection, path)?;
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

    pub(super) fn document_closed(&mut self, connection: &Connection, path: &Path) -> Result<()> {
        self.diagnostics.document_closed(connection, path)?;
        self.invalidate_document_queries(connection, path)
    }

    pub(super) fn workspace_modified(
        &mut self,
        connection: &Connection,
        load: WorkspaceLoad,
        trigger: &'static str,
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
        self.request_workspace_from(load, trigger);
        Ok(())
    }

    pub(super) fn shutdown(&mut self, connection: &Connection) -> Result<()> {
        self.cancel_all(connection, ErrorCode::RequestFailed, SHUTTING_DOWN)?;
        self.requested = None;
        self.prewarm = None;
        self.deferred_test_view = None;
        self.workspace_file_event = false;
        if let Some(warmup) = self.prewarming_tests.take() {
            warmup.cancelled.store(true, Ordering::Relaxed);
        }
        self.loading = None;
        self.load_trace = None;
        Ok(())
    }

    pub(super) fn environment_modified(
        &mut self,
        connection: &Connection,
        load: WorkspaceLoad,
    ) -> Result<()> {
        self.cancel_all(connection, ErrorCode::ContentModified, WORKSPACE_CHANGED)?;
        self.request_workspace_from(load, "didChangeConfiguration");
        Ok(())
    }

    fn cancel_all(
        &mut self,
        connection: &Connection,
        code: ErrorCode,
        message: &str,
    ) -> Result<()> {
        self.cancel_open_requests(connection, |_, _| true, code, message)?;
        self.cancel_pending(connection, |_| true, code, message)?;
        self.cancel_searches(connection, |_| true, code, message)?;
        self.highlights.cancel_all(connection, code, message)?;
        self.diagnostics.cancel_all(connection, code, message)
    }

    fn cancel_open_requests(
        &mut self,
        connection: &Connection,
        mut matches: impl FnMut(&Request, &OpenRequestScope) -> bool,
        code: ErrorCode,
        message: &str,
    ) -> Result<()> {
        for (request, _) in self
            .open_requests
            .extract_if(.., |(request, scope)| matches(request, scope))
        {
            send_response(connection, request.id, Err((code, message.to_owned())))?;
        }
        Ok(())
    }
}

fn run_query(query: &dyn DeferredQuery, engine: &mut Engine) -> Result<RequestProgress> {
    if let Some(pending) = query
        .document_path()
        .and_then(|path| engine.pending_open_dependency(path))
    {
        return Err(pending.into());
    }
    query.run(engine)
}

#[cfg(test)]
mod tests {
    mod diagnostic_protocol;
    mod workspace_reload;

    use std::cell::Cell;
    use std::collections::HashMap;
    use std::rc::Rc;
    use std::time::Duration;

    use anyhow::anyhow;
    use lsp_server::{Message, Notification, Request, Response, ResponseError};
    use lsp_types::{
        FileChangeType, Uri,
        notification::{
            Cancel, DidChangeConfiguration, DidChangeTextDocument, DidChangeWatchedFiles,
            DidChangeWorkspaceFolders, DidCloseTextDocument, DidOpenTextDocument, Exit,
            Initialized, Notification as _,
        },
        request::{
            CodeActionRequest, DocumentDiagnosticRequest, DocumentHighlightRequest,
            FoldingRangeRequest, GotoDefinition, GotoImplementation, HoverRequest, Initialize,
            InlayHintRequest, References, Request as _, SemanticTokensFullRequest, Shutdown,
            SignatureHelpRequest, WorkspaceSymbolRequest,
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
            Self::initialize(project, capabilities, Value::Null)
        }

        fn initialize(
            project: &TestProject,
            capabilities: Value,
            initialization_options: Value,
        ) -> Self {
            let (server, client) = Connection::memory();
            let mut session = Self {
                server,
                client,
                state: State::starting(),
                replies: HashMap::new(),
            };
            session.request(
                0,
                Initialize::METHOD,
                json!({
                    "rootUri": path_uri(&project.root).unwrap(),
                    "capabilities": capabilities,
                    "initializationOptions": initialization_options
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
            self.flush_watches();
        }

        fn flush_watches(&mut self) {
            let State::Running(running) = &mut self.state else {
                panic!("expected a running session");
            };
            if let Some(deadline) = running.scheduler.watched_files.deadline {
                running
                    .advance_watched_files(&self.server, deadline)
                    .unwrap();
            }
        }

        fn deliver(&mut self, load: WorkspaceLoad, result: Result<Workspace>) {
            let State::Running(running) = &mut self.state else {
                panic!("expected a running session");
            };
            let update = WorkspaceResult::for_test(&running.engine, load, result);
            running.workspace_loaded(&self.server, update).unwrap();
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
            let State::Running(running) = &mut self.state else {
                panic!("expected a running session");
            };
            running
                .scheduler
                .start_workspace_load(&mut running.engine, &sender)
                .unwrap();
            drop(sender);
            let update = receiver.recv().unwrap();
            assert!(update.result.is_ok());
            running.workspace_loaded(&self.server, update).unwrap();
        }

        fn step(&mut self) {
            let State::Running(running) = &mut self.state else {
                panic!("expected a running session");
            };
            running.advance_search(&self.server).unwrap();
        }

        fn finish_diagnostic_job(&mut self) {
            let completed = self.take_diagnostic_completion();
            self.complete_diagnostic(completed);
        }

        fn take_diagnostic_completion(&mut self) -> DiagnosticCompletion {
            self.scheduler()
                .diagnostic_completions()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
        }

        fn complete_diagnostic(&mut self, completed: DiagnosticCompletion) {
            let State::Running(running) = &mut self.state else {
                panic!("expected a running session");
            };
            running
                .scheduler
                .complete_diagnostics(&mut running.engine, &self.server, completed)
                .unwrap();
            running.collect_diagnostic_refresh(Instant::now());
        }

        fn advance_diagnostics(&mut self, now: Instant) {
            let State::Running(running) = &mut self.state else {
                panic!("expected a running session");
            };
            running.advance_diagnostics(&self.server, now).unwrap();
        }

        fn finish_searches(&mut self) {
            for _ in 0..4096 {
                let running = self.running();
                if running.scheduler.searches.is_empty() {
                    return;
                }
                if running.scheduler.ready_search(&running.engine).is_some() {
                    self.step();
                    continue;
                }
                crossbeam_channel::select! {
                    recv(running.engine.reference_wake()) -> signal => signal.unwrap(),
                    recv(running.engine.completion_wake()) -> signal => signal.unwrap(),
                    recv(running.engine.shared_disk_wake()) -> signal => signal.unwrap(),
                    default(Duration::from_secs(10)) => panic!("search worker did not wake"),
                }
            }
            panic!("searches did not finish");
        }

        fn expect_next(&mut self, id: impl Into<RequestId>) {
            assert_eq!(
                self.scheduler().searches.front().unwrap().request.id,
                id.into()
            );
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

    fn task_request(id: i32) -> Pending {
        struct NoReplay;

        impl DeferredQuery for NoReplay {
            fn run(&self, _: &mut Engine) -> Result<RequestProgress> {
                panic!("the test task cannot be restarted");
            }
        }

        Pending {
            id: id.into(),
            document_key: None,
            query: Box::new(NoReplay),
        }
    }

    #[test]
    fn definition_retries_a_shared_read_without_blocking_other_requests() {
        use crate::semantic::{CacheEvent, DefinitionKind};
        use std::sync::{Arc, atomic::AtomicBool};

        struct LateRead(PathBuf);

        impl DeferredQuery for LateRead {
            fn run(&self, _: &mut Engine) -> Result<RequestProgress> {
                Ok(QueryProgress::Searching(Box::new(Self(self.0.clone()))))
            }
        }

        impl SearchTask for LateRead {
            fn advance(&mut self, engine: &mut Engine) -> Result<bool> {
                assert!(matches!(
                    engine.definition(&self.0, Position::new(1, 5), DefinitionKind::Declaration)?,
                    QueryProgress::Ready(Some(_))
                ));
                Ok(true)
            }

            fn finish(self: Box<Self>) -> RequestResult {
                Ok(Value::Bool(true))
            }
        }

        let project = TestProject::new();
        let cold = project.write("cold.go", "package sample\nvar Cold int\n");
        let other = project.write("other.go", "package sample\nvar Other int\n");
        let mut session = Session::start(&project);
        let engine = &mut session.running().engine;
        let key = engine.diagnostic_key(&cold).unwrap();
        let input = engine.capture_diagnostics(&cold, key);
        let cold_key = path_key(&cold);
        let (entered, started) = crossbeam_channel::bounded(1);
        let (release, blocked) = crossbeam_channel::bounded(1);
        engine.set_shared_disk_hook(Some(Arc::new(move |path, event| {
            if path == cold_key && event == CacheEvent::Read {
                let _ = entered.try_send(());
                let _ = blocked.recv_timeout(Duration::from_secs(5));
            }
        })));
        let producer = thread::spawn(move || input.analyze(&AtomicBool::new(false)));
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        session.request(1, GotoDefinition::METHOD, position(&cold, 1, 5));
        session.expect_next(1);
        let State::Running(running) = &mut session.state else {
            unreachable!()
        };
        running
            .scheduler
            .submit(
                &mut running.engine,
                &session.server,
                3.into(),
                LateRead(cold.clone()),
            )
            .unwrap();
        session.step();
        session.request(2, GotoDefinition::METHOD, position(&other, 1, 5));
        assert_eq!(
            session.reply(2).result.as_ref().unwrap()["uri"],
            json!(path_uri(&other).unwrap())
        );
        session.count(2);
        release.send(()).unwrap();
        session.finish_searches();
        assert_eq!(
            session.reply(1).result.as_ref().unwrap()["uri"],
            json!(path_uri(&cold).unwrap())
        );
        assert_eq!(session.reply(3).result, Some(Value::Bool(true)));
        let _ = producer.join().unwrap().unwrap();
        session.running().engine.set_shared_disk_hook(None);
    }

    fn reference(path: &Path, line: u32, character: u32) -> Value {
        let mut params = position(path, line, character);
        params["context"] = json!({ "includeDeclaration": true });
        params
    }

    #[test]
    fn configuration_reload_cancels_old_queries_once_and_retains_unsaved_documents() {
        struct CountingSearch(Rc<Cell<usize>>);

        impl SearchTask for CountingSearch {
            fn discard(&mut self, code: ErrorCode) {
                assert!(matches!(code, ErrorCode::ContentModified));
                self.0.set(self.0.get() + 1);
            }

            fn advance(&mut self, _: &mut Engine) -> Result<bool> {
                panic!("an obsolete search advanced")
            }

            fn finish(self: Box<Self>) -> RequestResult {
                panic!("an obsolete search returned a result")
            }
        }

        let project = TestProject::new();
        let path = project.write("api.go", "package sample\nvar Disk int\n");
        let settings = json!({ "env": { "GOWORK": "", "RS_GOPLS_TEST_ENV": "initial" } });
        let mut session = Session::initialize(&project, json!({}), settings.clone());
        let inherited = crate::go_environment::GoEnvironment::capture();
        assert_eq!(
            session.running().engine.environment().get("GOWORK"),
            Some("".as_ref())
        );
        session.open(&path, "package sample\nvar Open int\n");
        let buffer = "package sample\nvar Unsaved int\n";
        session.edit(&path, 2, buffer);
        session.request(
            1,
            WorkspaceSymbolRequest::METHOD,
            json!({ "query": "Unsaved" }),
        );
        let old = session.begin_load();
        let discarded = Rc::new(Cell::new(0));
        session.scheduler().searches.push_back(Searching {
            request: task_request(2),
            generation: old.generation,
            task: SearchWork::Semantic(Box::new(CountingSearch(Rc::clone(&discarded)))),
        });

        for params in [
            json!({ "settings": settings }),
            json!({ "settings": {} }),
            json!({ "settings": null }),
            json!({ "settings": { "env": false } }),
            json!({}),
        ] {
            session.notify(DidChangeConfiguration::METHOD, params);
            assert_eq!(session.running().engine.workspace_load(), old);
            assert_eq!(session.scheduler().pending.len(), 1);
            assert_eq!(session.scheduler().searches.len(), 1);
            assert!(session.scheduler().requested.is_none());
            session.count(1);
        }

        session.notify(
            DidChangeConfiguration::METHOD,
            json!({
                "settings": { "rs-gopls": { "env": { "RS_GOPLS_TEST_ENV": "changed" } } }
            }),
        );
        let current = session.running().engine.workspace_load();
        assert_eq!(current.generation, old.generation + 1);
        let engine = &mut session.running().engine;
        assert_eq!(
            engine.environment().get("RS_GOPLS_TEST_ENV"),
            Some("changed".as_ref())
        );
        assert_eq!(engine.environment().get("GOWORK"), inherited.get("GOWORK"));
        let mut snapshot = engine.snapshot();
        let document = snapshot.document(&path).unwrap().unwrap();
        assert_eq!(document.text(), buffer);
        assert_eq!(document.version(), Some(2));
        assert_eq!(session.scheduler().requested, Some(current));
        assert_eq!(discarded.get(), 1);
        for id in [1, 2] {
            session.error(id, ErrorCode::ContentModified, WORKSPACE_CHANGED);
            session.cancel(id);
        }
        session.request(
            3,
            WorkspaceSymbolRequest::METHOD,
            json!({ "query": "Unsaved" }),
        );
        session.deliver(old, Ok(project.workspace(&["api.go"], old.include_tests)));
        assert_eq!(session.scheduler().pending.len(), 1);
        assert!(session.scheduler().searches.is_empty());
        assert_eq!(session.begin_load(), current);
        session.deliver(
            current,
            Ok(project.workspace(&["api.go"], current.include_tests)),
        );
        session.finish_searches();
        assert_eq!(
            session.reply(3).result.as_ref().unwrap()[0]["name"],
            "Unsaved"
        );
        assert_eq!(discarded.get(), 1);
        session.count(4);

        session.notify(
            DidChangeConfiguration::METHOD,
            json!({ "settings": { "env": {} } }),
        );
        assert!(*session.running().engine.environment() == inherited);
        assert_eq!(
            session.running().engine.workspace_load().generation,
            current.generation + 1
        );
    }

    #[test]
    fn configuration_reload_emits_diagnostics_only_for_the_installed_generation() {
        let project = TestProject::new();
        let text = "package sample\nvar _ = Ready\n";
        let path = project.write("api.go", text);
        project.write("dep.go", "package sample\nvar Ready int\n");
        for (capabilities, pull) in [
            (
                json!({ "textDocument": { "publishDiagnostics": {} } }),
                false,
            ),
            (
                json!({
                    "textDocument": { "diagnostic": {} },
                    "workspace": { "diagnostics": { "refreshSupport": true } }
                }),
                true,
            ),
        ] {
            let mut session = Session::with_capabilities(&project, capabilities);
            let initial = session.running().engine.workspace_load();
            assert!(session.running().engine.install_workspace(
                initial,
                project.workspace(&["api.go"], initial.include_tests)
            ));
            session.open(&path, text);
            if !pull {
                session.finish_diagnostic_job();
                let Message::Notification(notification) =
                    session.client.receiver.try_recv().unwrap()
                else {
                    panic!("expected initial diagnostics");
                };
                assert_eq!(
                    notification.params["diagnostics"][0]["code"],
                    "name.unresolved"
                );
            }
            session.scheduler().request_workspace_from(initial, "test");
            let old = session.begin_load();
            session.notify(
                DidChangeConfiguration::METHOD,
                json!({
                    "settings": { "env": { "RS_GOPLS_TEST_ENV": "changed" } }
                }),
            );
            session.deliver(old, Ok(project.workspace(&["api.go"], old.include_tests)));
            assert!(session.client.receiver.is_empty());
            let current = session.begin_load();
            session.deliver(
                current,
                Ok(project.workspace(&["api.go", "dep.go"], current.include_tests)),
            );
            if !pull {
                session.finish_diagnostic_job();
            } else {
                assert!(session.client.receiver.is_empty());
                let deadline = session.running().diagnostic_deadline().unwrap();
                session.advance_diagnostics(deadline);
            }
            match session.client.receiver.try_recv().unwrap() {
                Message::Request(request) if pull => {
                    assert_eq!(
                        request.method,
                        lsp_types::request::WorkspaceDiagnosticRefresh::METHOD
                    );
                }
                Message::Notification(notification) if !pull => {
                    assert_eq!(notification.method, "textDocument/publishDiagnostics");
                    assert_eq!(notification.params["version"], 1);
                    assert_eq!(notification.params["diagnostics"], json!([]));
                }
                other => panic!("unexpected diagnostics message: {other:?}"),
            }
            assert!(session.client.receiver.is_empty());
        }
    }

    #[test]
    fn replays_pending_diagnostics_and_actions_in_one_background_job() {
        let project = TestProject::new();
        let text = "package sample\nfunc f() { unused := 1 }\n";
        let path = project.write("api.go", text);
        let mut session = Session::start(&project);
        session.open(&path, text);
        let document = json!({ "uri": path_uri(&path).unwrap() });
        session.request(
            1,
            DocumentDiagnosticRequest::METHOD,
            json!({ "textDocument": document }),
        );
        session.request(
            2,
            CodeActionRequest::METHOD,
            json!({
                "textDocument": document,
                "range": {
                    "start": { "line": 1, "character": 0 },
                    "end": { "line": 2, "character": 0 },
                },
                "context": { "diagnostics": [], "only": ["quickfix"] },
            }),
        );
        assert_eq!(session.scheduler().diagnostics.pending_count(), 2);
        assert!(session.scheduler().diagnostic_completions().is_none());
        let load = session.begin_load();
        session.deliver(load, Ok(project.workspace(&["api.go"], load.include_tests)));
        assert!(session.client.receiver.is_empty());
        assert_eq!(session.scheduler().diagnostics.pending_count(), 0);
        session.finish_diagnostic_job();
        let report = session.reply(1).result.as_ref().unwrap();
        assert_eq!(report["items"][0]["code"], "var.unused");
        let previous = report["resultId"].as_str().unwrap().to_owned();
        assert_eq!(
            session.reply(2).result.as_ref().unwrap()[0]["title"],
            "Replace unused variable 'unused' with '_'",
        );
        session.request(
            3,
            DocumentDiagnosticRequest::METHOD,
            json!({ "textDocument": document, "previousResultId": previous }),
        );
        assert_eq!(
            session.reply(3).result.as_ref().unwrap(),
            &json!({ "kind": "unchanged", "resultId": previous }),
        );
        session.count(4);
    }

    #[test]
    fn production_pull_is_unchanged_immediately_after_test_view_upgrade() {
        let project = TestProject::new();
        let text = "package sample\nvar _ = Missing\n";
        let path = project.write("api.go", text);
        let test = project.write("api_test.go", "package sample\nvar _ = MissingTest\n");
        let mut session = Session::start(&project);
        session.open(&path, text);
        let document = json!({ "uri": path_uri(&path).unwrap() });
        session.request(
            1,
            DocumentDiagnosticRequest::METHOD,
            json!({ "textDocument": document }),
        );
        let initial = session.begin_load();
        assert!(!initial.include_tests);
        session.deliver(initial, Ok(project.workspace(&["api.go"], false)));
        session.finish_diagnostic_job();
        let result_id = session.reply(1).result.as_ref().unwrap()["resultId"]
            .as_str()
            .unwrap()
            .to_owned();
        session.request(
            2,
            DocumentDiagnosticRequest::METHOD,
            json!({ "textDocument": { "uri": path_uri(&test).unwrap() } }),
        );
        let upgrade = session.begin_load();
        assert!(upgrade.include_tests);
        let production = json!({
            "Dir": project.root, "ImportPath": "example.test/sample",
            "Name": "sample", "GoFiles": ["api.go"],
        });
        let variant = json!({
            "Dir": project.root, "ImportPath": "example.test/sample [example.test/sample.test]",
            "ForTest": "example.test/sample", "Name": "sample", "GoFiles": ["api.go", "api_test.go"],
        });
        session.deliver(
            upgrade,
            Ok(Workspace::from_test_listing(
                std::slice::from_ref(&project.root),
                &format!("{production}\n{variant}"),
                true,
            )),
        );
        session.request(
            3,
            DocumentDiagnosticRequest::METHOD,
            json!({ "textDocument": document, "previousResultId": result_id }),
        );
        assert_eq!(
            session.reply(3).result.as_ref().unwrap(),
            &json!({ "kind": "unchanged", "resultId": result_id }),
        );
        session.finish_diagnostic_job();
        assert_eq!(
            session.reply(2).result.as_ref().unwrap()["items"][0]["code"],
            "name.unresolved",
        );
        session.count(4);
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
            crate::initialize(
                params,
                crate::go_environment::GoEnvironment::capture(),
                None,
            )
            .map(|(session, _)| session.engine.roots().to_vec())
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
                request: task_request(id),
                generation: running.engine.workspace_load().generation,
                task: SearchWork::Semantic(Box::new(InvalidatingTask {
                    calls: Rc::clone(&calls),
                    manifest: manifest.clone(),
                })),
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
        let mut state = State::starting();
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
            if method == DocumentHighlightRequest::METHOD
                && let State::Running(running) = state
                && running.scheduler.highlights.has_active()
            {
                let completed = running
                    .scheduler
                    .highlight_completions()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
                running
                    .scheduler
                    .complete_highlight(&mut running.engine, session.server(), completed)
                    .unwrap();
            }
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
                WorkspaceResult::for_test(
                    &running.engine,
                    load,
                    Ok(project.workspace(&["editor.go"], load.include_tests)),
                ),
            )
            .unwrap();
        assert!(matches!(
            running.engine.snapshot().active_graph(false),
            QueryProgress::Ready(_)
        ));

        assert_eq!(paused, editor_features(&session, &mut state, &uri, 5));
    }
}
