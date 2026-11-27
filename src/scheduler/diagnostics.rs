use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, bounded};
use lsp_server::{Connection, ErrorCode, RequestId};
use lsp_types::{
    CodeActionKind, DocumentDiagnosticReportKind, FullDocumentDiagnosticReport,
    PublishDiagnosticsParams, Range, UnchangedDocumentDiagnosticReport, Uri,
    notification::PublishDiagnostics,
};

use crate::protocol::{internal_error, send_notification, send_response, serialize_query};
use crate::semantic::Engine;
use crate::semantic::diagnostics::{
    DiagnosticInput, DiagnosticReport, DiagnosticSnapshotKey, DiagnosticsPreparation,
};
use crate::workspace::{WorkspaceLoad, path_key};
use crate::{rpc_telemetry, telemetry};

const WORKERS: usize = 2;
const DIAGNOSTICS_CHANGED: &str = "the document or package changed during diagnostics";
const CANCELLED: &str = "the client cancelled the request";

pub(crate) enum Consumer {
    Pull {
        id: RequestId,
        previous: Option<String>,
    },
    Push(Uri),
    Actions {
        id: RequestId,
        uri: Uri,
        range: Range,
        only: Option<Vec<CodeActionKind>>,
    },
}

impl Consumer {
    fn id(&self) -> Option<&RequestId> {
        match self {
            Self::Pull { id, .. } | Self::Actions { id, .. } => Some(id),
            Self::Push(_) => None,
        }
    }

    fn is_push(&self) -> bool {
        matches!(self, Self::Push(_))
    }

    fn finish(
        self,
        engine: &mut Engine,
        connection: &Connection,
        path: &Path,
        key: &DiagnosticSnapshotKey,
    ) -> Result<()> {
        if let Self::Actions {
            id,
            uri,
            range,
            only,
        } = self
        {
            let actions = engine
                .code_actions_for_diagnostic_key(path, &uri, range, only.as_deref(), key)
                .map_err(internal_error)
                .and_then(serialize_query);
            send_response(connection, id, actions)
        } else {
            let report = engine
                .diagnostic_report_for_key(key)
                .expect("the diagnostic report has been cached");
            self.finish_report(connection, report)
        }
    }

    fn finish_report(self, connection: &Connection, report: &DiagnosticReport) -> Result<()> {
        match self {
            Self::Pull { id, previous } => {
                let result = if previous.as_ref() == Some(&report.result_id) {
                    DocumentDiagnosticReportKind::Unchanged(UnchangedDocumentDiagnosticReport {
                        result_id: report.result_id.clone(),
                    })
                } else {
                    DocumentDiagnosticReportKind::Full(FullDocumentDiagnosticReport {
                        result_id: Some(report.result_id.clone()),
                        items: report.to_lsp_diagnostics(),
                    })
                };
                send_response(connection, id, serialize_query(result))
            }
            Self::Push(uri) => send_notification::<PublishDiagnostics>(
                connection,
                PublishDiagnosticsParams::new(
                    uri,
                    report.to_lsp_diagnostics(),
                    report.snapshot.version,
                ),
            ),
            Self::Actions { .. } => unreachable!("code actions need an engine"),
        }
    }

    fn error(self, connection: &Connection, error: (ErrorCode, String)) -> Result<()> {
        if let Some(id) = self.id() {
            send_response(connection, id.clone(), Err(error))
        } else {
            if matches!(error.0, ErrorCode::InternalError) {
                eprintln!("rs-gopls: diagnostics: {}", error.1);
            }
            Ok(())
        }
    }
}

struct Pending {
    path: PathBuf,
    consumer: Consumer,
}

struct Job {
    path: PathBuf,
    consumers: Vec<Consumer>,
    running: Option<Running>,
}

struct Running {
    ticket: u64,
    cancelled: Arc<AtomicBool>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

struct Work {
    key: DiagnosticSnapshotKey,
    ticket: u64,
    input: DiagnosticInput,
    cancelled: Arc<AtomicBool>,
}

pub(crate) struct Completed {
    key: DiagnosticSnapshotKey,
    ticket: u64,
    report: Result<Option<DiagnosticReport>>,
}

struct Pool {
    send: Sender<Work>,
    receive: Receiver<Completed>,
}

impl Pool {
    fn new() -> Result<Self> {
        let (send, queue) = bounded::<Work>(WORKERS);
        let (done, receive) = bounded(WORKERS);
        for worker in 0..WORKERS {
            let queue = queue.clone();
            let done = done.clone();
            thread::Builder::new()
                .name(format!("go-diagnostics-{worker}"))
                .spawn(move || {
                    while let Ok(work) = queue.recv() {
                        let start = telemetry::now();
                        if start.is_some() {
                            telemetry::emit(
                                "diagnostic_worker_start",
                                serde_json::json!({
                                    "job": work.ticket,
                                    "partial": work.key.partial,
                                }),
                            );
                        }
                        let report = if work.cancelled.load(Ordering::Relaxed) {
                            Ok(None)
                        } else {
                            work.input.analyze(&work.cancelled)
                        };
                        if let Some(start) = start {
                            telemetry::emit(
                                "diagnostic_worker_done",
                                serde_json::json!({
                                    "job": work.ticket,
                                    "analysis_ms": telemetry::elapsed(start),
                                    "cancelled": work.cancelled.load(Ordering::Relaxed),
                                }),
                            );
                        }
                        if done
                            .send(Completed {
                                key: work.key,
                                ticket: work.ticket,
                                report,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                })
                .context("starting Go diagnostic worker")?;
        }
        Ok(Self { send, receive })
    }
}

#[derive(Default)]
pub(super) struct Diagnostics {
    pending: Vec<Pending>,
    jobs: HashMap<DiagnosticSnapshotKey, Job>,
    queue: VecDeque<DiagnosticSnapshotKey>,
    pool: Option<Pool>,
    active: usize,
    next_ticket: u64,
}

impl Diagnostics {
    pub(super) fn receiver(&self) -> Option<&Receiver<Completed>> {
        self.pool.as_ref().map(|pool| &pool.receive)
    }

    pub(super) fn pending_count(&self) -> usize {
        self.pending.len()
    }

    pub(super) fn submit(
        &mut self,
        engine: &mut Engine,
        connection: &Connection,
        path: PathBuf,
        consumer: Consumer,
        fallback: bool,
    ) -> Result<Option<WorkspaceLoad>> {
        let allow_partial = consumer.is_push() || fallback;
        match engine.prepare_diagnostics(&path, allow_partial) {
            Ok(DiagnosticsPreparation::Pending(load)) => {
                let _ = rpc_telemetry::wait(|| engine.workspace_wait_reason(load));
                self.pending.push(Pending { path, consumer });
                Ok(Some(load))
            }
            Ok(DiagnosticsPreparation::Ready(report)) => {
                let retry = !fallback && report.is_partial;
                if matches!(consumer, Consumer::Actions { .. }) {
                    let key = report.snapshot.clone();
                    consumer.finish(engine, connection, &path, &key)?;
                } else {
                    consumer.finish_report(connection, report)?;
                }
                Ok(retry.then(|| engine.workspace_load()))
            }
            Ok(DiagnosticsPreparation::Cold(key)) => {
                let retry = !fallback && key.partial;
                if let Some(job) = self.jobs.get_mut(&key) {
                    if !consumer.is_push() || !job.consumers.iter().any(Consumer::is_push) {
                        job.consumers.push(consumer);
                    }
                } else {
                    self.jobs.insert(
                        key.clone(),
                        Job {
                            path,
                            consumers: vec![consumer],
                            running: None,
                        },
                    );
                    if self.active < WORKERS {
                        self.start(engine, connection, key)?;
                    } else {
                        // Only the workers retain source snapshots. Queued
                        // subscribers need a path and key until a slot opens.
                        self.queue.push_back(key);
                    }
                }
                Ok(retry.then(|| engine.workspace_load()))
            }
            Err(error) => {
                consumer.error(connection, internal_error(error))?;
                Ok(None)
            }
        }
    }

    fn start(
        &mut self,
        engine: &Engine,
        connection: &Connection,
        key: DiagnosticSnapshotKey,
    ) -> Result<()> {
        self.next_ticket += 1;
        let ticket = self.next_ticket;
        let cancelled = Arc::new(AtomicBool::new(false));
        let work = Work {
            key: key.clone(),
            ticket,
            input: engine.capture_diagnostics(&self.jobs[&key].path, key.clone()),
            cancelled: cancelled.clone(),
        };
        let send = (|| {
            if self.pool.is_none() {
                self.pool = Some(Pool::new()?);
            }
            self.pool
                .as_ref()
                .expect("diagnostic pool was started")
                .send
                .try_send(work)
                .map_err(|_| anyhow::anyhow!("diagnostic worker queue is unavailable"))
        })();
        match send {
            Ok(()) => {
                self.active += 1;
                self.jobs.get_mut(&key).unwrap().running = Some(Running { ticket, cancelled });
            }
            Err(error) => {
                let job = self.jobs.remove(&key).unwrap();
                let (code, message) = internal_error(error);
                fail(connection, job.consumers, code, &message)?;
            }
        }
        Ok(())
    }

    fn advance(&mut self, engine: &mut Engine, connection: &Connection) -> Result<()> {
        while self.active < WORKERS
            && let Some(key) = self.queue.pop_front()
        {
            if engine
                .diagnostic_key(&self.jobs[&key].path)
                .is_ok_and(|current| current == key)
            {
                self.start(engine, connection, key)?;
            } else {
                let job = self.jobs.remove(&key).unwrap();
                fail(
                    connection,
                    job.consumers,
                    ErrorCode::ContentModified,
                    DIAGNOSTICS_CHANGED,
                )?;
            }
        }
        Ok(())
    }

    pub(super) fn complete(
        &mut self,
        engine: &mut Engine,
        connection: &Connection,
        completed: Completed,
    ) -> Result<()> {
        self.active -= 1;
        if self
            .jobs
            .get(&completed.key)
            .and_then(|job| job.running.as_ref())
            .is_some_and(|running| running.ticket == completed.ticket)
        {
            let job = self.jobs.remove(&completed.key).unwrap();
            if !engine
                .diagnostic_key(&job.path)
                .is_ok_and(|current| current == completed.key)
            {
                fail(
                    connection,
                    job.consumers,
                    ErrorCode::ContentModified,
                    DIAGNOSTICS_CHANGED,
                )?;
            } else {
                match completed.report {
                    Ok(Some(report)) if report.snapshot == completed.key => {
                        engine.install_diagnostic_report(report);
                        for consumer in job.consumers {
                            let _trace =
                                consumer.id().and_then(|id| rpc_telemetry::step(id, false));
                            consumer.finish(engine, connection, &job.path, &completed.key)?;
                        }
                    }
                    Ok(_) => {
                        fail(
                            connection,
                            job.consumers,
                            ErrorCode::RequestCanceled,
                            CANCELLED,
                        )?;
                    }
                    Err(error) => {
                        let (code, message) = internal_error(error);
                        fail(connection, job.consumers, code, &message)?;
                    }
                }
            }
        }
        self.advance(engine, connection)
    }

    pub(super) fn replay(
        &mut self,
        engine: &mut Engine,
        connection: &Connection,
        fallback: bool,
    ) -> Result<Option<WorkspaceLoad>> {
        let mut requested = None;
        for Pending { path, consumer } in std::mem::take(&mut self.pending) {
            let _trace = consumer
                .id()
                .and_then(|id| rpc_telemetry::step(id, !fallback));
            if fallback {
                let _ = rpc_telemetry::wait(|| "graph_load_failed");
            }
            requested = self
                .submit(engine, connection, path, consumer, fallback)?
                .or(requested);
        }
        Ok(requested)
    }

    pub(super) fn revalidate(
        &mut self,
        engine: &mut Engine,
        connection: &Connection,
    ) -> Result<Option<WorkspaceLoad>> {
        let mut push = Vec::new();
        self.jobs.retain(|key, job| {
            let current = engine
                .diagnostic_key(&job.path)
                .is_ok_and(|current| &current == key);
            if !current {
                job.running = None;
                push.push((job.path.clone(), std::mem::take(&mut job.consumers)));
            }
            current
        });
        self.queue.retain(|key| self.jobs.contains_key(key));
        let mut requested = None;
        for (path, consumers) in push {
            for consumer in consumers {
                if consumer.is_push() && engine.is_open(&path) {
                    requested = self
                        .submit(engine, connection, path.clone(), consumer, false)?
                        .or(requested);
                } else {
                    consumer.error(
                        connection,
                        (ErrorCode::ContentModified, DIAGNOSTICS_CHANGED.into()),
                    )?;
                }
            }
        }
        Ok(requested)
    }

    pub(super) fn cancel(&mut self, connection: &Connection, id: &RequestId) -> Result<()> {
        for pending in self
            .pending
            .extract_if(.., |pending| pending.consumer.id() == Some(id))
        {
            pending
                .consumer
                .error(connection, (ErrorCode::RequestCanceled, CANCELLED.into()))?;
        }
        let mut cancelled = Vec::new();
        self.jobs.retain(|_, job| {
            cancelled.extend(
                job.consumers
                    .extract_if(.., |consumer| consumer.id() == Some(id)),
            );
            !job.consumers.is_empty()
        });
        self.queue.retain(|key| self.jobs.contains_key(key));
        fail(connection, cancelled, ErrorCode::RequestCanceled, CANCELLED)
    }

    pub(super) fn document_modified(&mut self, connection: &Connection, path: &Path) -> Result<()> {
        let key = path_key(path);
        for pending in self
            .pending
            .extract_if(.., |pending| path_key(&pending.path) == key)
        {
            pending.consumer.error(
                connection,
                (ErrorCode::ContentModified, DIAGNOSTICS_CHANGED.into()),
            )?;
        }
        Ok(())
    }

    pub(super) fn cancel_all(
        &mut self,
        connection: &Connection,
        code: ErrorCode,
        message: &str,
    ) -> Result<()> {
        for pending in std::mem::take(&mut self.pending) {
            pending.consumer.error(connection, (code, message.into()))?;
        }
        for (_, job) in self.jobs.drain() {
            fail(connection, job.consumers, code, message)?;
        }
        self.queue.clear();
        Ok(())
    }
}

fn fail(
    connection: &Connection,
    consumers: Vec<Consumer>,
    code: ErrorCode,
    message: &str,
) -> Result<()> {
    for consumer in consumers {
        consumer.error(connection, (code, message.into()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use lsp_server::{Message, Response};
    use lsp_types::{Position, TextDocumentContentChangeEvent};

    use super::*;
    use crate::protocol::path_uri;
    use crate::test_support::TestProject;

    fn open(engine: &mut Engine, project: &TestProject, name: &str, text: &str) -> PathBuf {
        let path = project.write(name, text);
        engine.open(path.clone(), text.into(), 1).unwrap();
        path
    }

    fn pull(id: i32, previous: Option<String>) -> Consumer {
        Consumer::Pull {
            id: id.into(),
            previous,
        }
    }

    fn response(client: &Connection, id: i32) -> Response {
        let Message::Response(response) = client.receiver.try_recv().unwrap() else {
            panic!("expected a diagnostic response");
        };
        assert_eq!(response.id, id.into());
        response
    }

    fn receive(diagnostics: &Diagnostics) -> Completed {
        diagnostics
            .receiver()
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
    }

    #[test]
    fn coalesces_push_pull_and_quickfix_and_preserves_each_previous_result() {
        let project = TestProject::new();
        let (server, client) = Connection::memory();
        let mut engine = Engine::new(project.root.clone());
        let path = open(
            &mut engine,
            &project,
            "sample.go",
            "package sample\nfunc f() { unused := 1 }\n",
        );
        let load = engine.workspace_load();
        assert!(engine.install_workspace(load, project.workspace(&["sample.go"], false)));
        let DiagnosticsPreparation::Cold(key) = engine.prepare_diagnostics(&path, false).unwrap()
        else {
            panic!("expected cold diagnostic input");
        };
        let result_id = engine
            .capture_diagnostics(&path, key.clone())
            .analyze(&AtomicBool::new(false))
            .unwrap()
            .unwrap()
            .result_id;
        let uri = path_uri(&path).unwrap();
        let mut diagnostics = Diagnostics::default();
        for consumer in [
            pull(1, None),
            Consumer::Push(uri.clone()),
            Consumer::Actions {
                id: 2.into(),
                uri,
                range: Range::new(Position::new(1, 0), Position::new(2, 0)),
                only: Some(vec![CodeActionKind::QUICKFIX]),
            },
            pull(3, Some(result_id.clone())),
            pull(4, Some("stale".into())),
        ] {
            assert_eq!(
                diagnostics
                    .submit(&mut engine, &server, path.clone(), consumer, false)
                    .unwrap(),
                None,
            );
        }
        assert!(client.receiver.is_empty());
        assert_eq!(diagnostics.active, 1);
        assert_eq!(diagnostics.jobs[&key].consumers.len(), 5);
        let cancelled = diagnostics.jobs[&key]
            .running
            .as_ref()
            .unwrap()
            .cancelled
            .clone();
        diagnostics.cancel(&server, &1.into()).unwrap();
        assert!(!cancelled.load(Ordering::Relaxed));
        assert_eq!(
            response(&client, 1).error.unwrap().code,
            ErrorCode::RequestCanceled as i32,
        );
        let completed = receive(&diagnostics);
        diagnostics
            .complete(&mut engine, &server, completed)
            .unwrap();
        let Message::Notification(push) = client.receiver.try_recv().unwrap() else {
            panic!("expected diagnostic publication");
        };
        assert_eq!(push.method, "textDocument/publishDiagnostics");
        assert_eq!(push.params["version"], 1);
        assert_eq!(push.params["diagnostics"][0]["code"], "var.unused");
        assert_eq!(
            response(&client, 2).result.unwrap()[0]["title"],
            "Replace unused variable 'unused' with '_'",
        );
        let unchanged = response(&client, 3).result.unwrap();
        assert_eq!(unchanged["kind"], "unchanged");
        assert_eq!(unchanged["resultId"], result_id);
        assert_eq!(response(&client, 4).result.unwrap()["kind"], "full");
        assert_eq!(diagnostics.active, 0);
        assert!(diagnostics.jobs.is_empty());
        assert!(client.receiver.is_empty());
        diagnostics
            .submit(&mut engine, &server, path, pull(5, Some(result_id)), false)
            .unwrap();
        assert_eq!(response(&client, 5).result.unwrap()["kind"], "unchanged");
        assert_eq!(diagnostics.active, 0);
    }

    #[test]
    fn last_cancel_reclaims_queued_and_running_slots_without_duplicate_replies() {
        let project = TestProject::new();
        let (server, client) = Connection::memory();
        let mut engine = Engine::new(project.root.clone());
        let paths = ["a.go", "b.go", "c.go"]
            .map(|name| open(&mut engine, &project, name, "package sample\n"));
        let load = engine.workspace_load();
        assert!(
            engine.install_workspace(load, project.workspace(&["a.go", "b.go", "c.go"], false))
        );
        let mut diagnostics = Diagnostics::default();
        for (id, path) in (1..).zip(&paths) {
            diagnostics
                .submit(&mut engine, &server, path.clone(), pull(id, None), false)
                .unwrap();
        }
        assert_eq!(diagnostics.active, WORKERS);
        assert_eq!(diagnostics.queue.len(), 1);
        let key = engine.diagnostic_key(&paths[0]).unwrap();
        let cancelled = diagnostics.jobs[&key]
            .running
            .as_ref()
            .unwrap()
            .cancelled
            .clone();
        diagnostics.cancel(&server, &3.into()).unwrap();
        diagnostics.cancel(&server, &1.into()).unwrap();
        assert!(cancelled.load(Ordering::Relaxed));
        assert!(diagnostics.queue.is_empty());
        assert!(!diagnostics.jobs.contains_key(&key));
        for id in [3, 1] {
            assert_eq!(
                response(&client, id).error.unwrap().code,
                ErrorCode::RequestCanceled as i32,
            );
        }
        diagnostics
            .submit(&mut engine, &server, paths[0].clone(), pull(4, None), false)
            .unwrap();
        assert_eq!(diagnostics.queue.front(), Some(&key));
        while diagnostics.active != 0 {
            let completed = receive(&diagnostics);
            diagnostics
                .complete(&mut engine, &server, completed)
                .unwrap();
        }
        let mut responses = HashMap::new();
        for message in client.receiver.try_iter() {
            let Message::Response(response) = message else {
                panic!("expected diagnostic responses");
            };
            assert!(
                responses
                    .insert(response.id, response.result.unwrap())
                    .is_none()
            );
        }
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[&2.into()]["kind"], "full");
        assert_eq!(responses[&4.into()]["kind"], "full");
        diagnostics.cancel(&server, &4.into()).unwrap();
        assert!(diagnostics.jobs.is_empty());
        assert!(client.receiver.is_empty());
    }

    #[test]
    fn edited_input_cancels_old_subscribers_and_publishes_only_current_result() {
        let project = TestProject::new();
        let (server, client) = Connection::memory();
        let mut engine = Engine::new(project.root.clone());
        let path = open(
            &mut engine,
            &project,
            "sample.go",
            "package sample\nvar Value = Missing\n",
        );
        let load = engine.workspace_load();
        assert!(engine.install_workspace(load, project.workspace(&["sample.go"], false)));
        let mut diagnostics = Diagnostics::default();
        diagnostics
            .submit(&mut engine, &server, path.clone(), pull(1, None), false)
            .unwrap();
        diagnostics
            .submit(
                &mut engine,
                &server,
                path.clone(),
                Consumer::Push(path_uri(&path).unwrap()),
                false,
            )
            .unwrap();
        let stale = receive(&diagnostics);
        engine
            .edit(
                &path,
                &[TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: "package sample\nvar Value = 1\n".into(),
                }],
                2,
            )
            .unwrap();
        diagnostics.revalidate(&mut engine, &server).unwrap();
        assert_eq!(
            response(&client, 1).error.unwrap().code,
            ErrorCode::ContentModified as i32,
        );
        diagnostics.complete(&mut engine, &server, stale).unwrap();
        assert!(client.receiver.is_empty());
        let current = receive(&diagnostics);
        diagnostics.complete(&mut engine, &server, current).unwrap();
        let Message::Notification(push) = client.receiver.try_recv().unwrap() else {
            panic!("expected diagnostic publication after edit");
        };
        assert_eq!(push.params["version"], 2);
        assert_eq!(push.params["diagnostics"], serde_json::json!([]));
        assert!(client.receiver.is_empty());
        assert!(diagnostics.jobs.is_empty());
    }
}
