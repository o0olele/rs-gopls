use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, bounded};
use lsp_server::{Connection, ErrorCode, RequestId};
use lsp_types::Position;

use crate::protocol::{RequestResult, internal_error, send_response, serialize_query};
use crate::semantic::{Engine, HighlightInput, HighlightOutput, HighlightPreparation};
use crate::workspace::path_key;
use crate::{rpc_telemetry, telemetry};

const CANCELLED: &str = "the client cancelled the request";
const DOCUMENT_CHANGED: &str = "the document changed during highlighting";

struct Pending {
    id: RequestId,
    path: PathBuf,
    input: HighlightInput,
}

struct Active {
    id: Option<RequestId>,
    path: PathBuf,
    cancelled: Arc<AtomicBool>,
}

impl Drop for Active {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

struct Work {
    trace_id: Option<RequestId>,
    input: HighlightInput,
    cancelled: Arc<AtomicBool>,
}

pub(crate) struct Completed(Result<Option<(HighlightOutput, RequestResult)>>);

struct Worker {
    queue: Sender<Work>,
    completions: Receiver<Completed>,
}

impl Worker {
    fn new() -> Result<Self> {
        let (queue, jobs) = bounded::<Work>(1);
        let (done, completions) = bounded(1);
        thread::Builder::new()
            .name("go-highlights".into())
            .spawn(move || {
                while let Ok(job) = jobs.recv() {
                    let completed = job.run();
                    if done.send(Completed(completed)).is_err() {
                        break;
                    }
                }
            })
            .context("starting Go highlight worker")?;
        Ok(Self { queue, completions })
    }
}

impl Work {
    fn run(self) -> Result<Option<(HighlightOutput, RequestResult)>> {
        if self.cancelled.load(Ordering::Relaxed) {
            return Ok(None);
        }
        if let Some(id) = self.trace_id {
            telemetry::event!("highlight_start", { "req_id": rpc_telemetry::request_id(&id) });
        }
        let Some(output) = self.input.analyze(&self.cancelled)? else {
            return Ok(None);
        };
        if self.cancelled.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let response = serialize_query(output.results());
        Ok((!self.cancelled.load(Ordering::Relaxed)).then_some((output, response)))
    }
}

#[derive(Default)]
pub(super) struct Highlights {
    waiting: VecDeque<Pending>,
    active: Option<Active>,
    worker: Option<Worker>,
}

impl Highlights {
    pub(super) fn receiver(&self) -> Option<&Receiver<Completed>> {
        self.worker.as_ref().map(|worker| &worker.completions)
    }

    #[cfg(test)]
    pub(super) fn has_active(&self) -> bool {
        self.active.is_some()
    }

    pub(super) fn submit(
        &mut self,
        engine: &mut Engine,
        connection: &Connection,
        id: RequestId,
        path: PathBuf,
        position: Position,
    ) -> Result<()> {
        let result = match engine.prepare_highlight(&path, position) {
            Ok(HighlightPreparation::Ready(highlights)) => serialize_query(highlights),
            Ok(HighlightPreparation::Background(input)) => {
                let _ = rpc_telemetry::wait(|| "highlight_worker");
                self.waiting.push_back(Pending {
                    id,
                    path: path_key(&path),
                    input,
                });
                return self.advance(connection);
            }
            Err(error) => Err(internal_error(error)),
        };
        send_response(connection, id, result)
    }

    fn advance(&mut self, connection: &Connection) -> Result<()> {
        while self.active.is_none()
            && let Some(Pending { id, path, input }) = self.waiting.pop_front()
        {
            let cancelled = Arc::new(AtomicBool::new(false));
            let work = Work {
                trace_id: telemetry::enabled().then(|| id.clone()),
                input,
                cancelled: cancelled.clone(),
            };
            let send = (|| {
                if self.worker.is_none() {
                    self.worker = Some(Worker::new()?);
                }
                self.worker
                    .as_ref()
                    .unwrap()
                    .queue
                    .try_send(work)
                    .map_err(|_| anyhow::anyhow!("Go highlight worker is unavailable"))
            })();
            match send {
                Ok(()) => {
                    self.active = Some(Active {
                        id: Some(id),
                        path,
                        cancelled,
                    });
                }
                Err(error) => {
                    self.worker = None;
                    send_response(connection, id, Err(internal_error(error)))?;
                }
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
        if let Some(mut active) = self.active.take()
            && let Some(id) = active.id.take()
        {
            let _trace = rpc_telemetry::step(&id, false);
            let result = match completed.0 {
                Ok(Some((output, response))) => {
                    if engine.finish_highlight(output) {
                        response
                    } else {
                        Err((ErrorCode::ContentModified, DOCUMENT_CHANGED.into()))
                    }
                }
                Ok(None) => Err((ErrorCode::RequestCanceled, CANCELLED.into())),
                Err(error) => Err(internal_error(error)),
            };
            send_response(connection, id, result)?;
        }
        self.advance(connection)
    }

    pub(super) fn cancel(&mut self, connection: &Connection, id: &RequestId) -> Result<()> {
        self.cancel_matching(
            connection,
            |request, _| request == id,
            ErrorCode::RequestCanceled,
            CANCELLED,
        )
    }

    pub(super) fn document_modified(&mut self, connection: &Connection, path: &Path) -> Result<()> {
        let key = path_key(path);
        self.cancel_matching(
            connection,
            |_, path| path == key.as_path(),
            ErrorCode::ContentModified,
            DOCUMENT_CHANGED,
        )
    }

    pub(super) fn cancel_all(
        &mut self,
        connection: &Connection,
        code: ErrorCode,
        message: &str,
    ) -> Result<()> {
        self.cancel_matching(connection, |_, _| true, code, message)
    }

    fn cancel_matching(
        &mut self,
        connection: &Connection,
        mut matches: impl FnMut(&RequestId, &Path) -> bool,
        code: ErrorCode,
        message: &str,
    ) -> Result<()> {
        let mut cancelled = Vec::new();
        self.waiting.retain(|request| {
            let remove = matches(&request.id, &request.path);
            if remove {
                cancelled.push(request.id.clone());
            }
            !remove
        });
        if let Some(active) = &mut self.active
            && active
                .id
                .as_ref()
                .is_some_and(|id| matches(id, &active.path))
        {
            active.cancelled.store(true, Ordering::Relaxed);
            cancelled.push(active.id.take().unwrap());
        }
        for id in cancelled {
            send_response(connection, id, Err((code, message.into())))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use lsp_types::TextDocumentContentChangeEvent;

    use super::*;
    use crate::test_support::{TestProject, TestSession};

    const SOURCE: &str = "package p\nconst Shared = 1\nvar _ = Shared\n";
    const TARGET: Position = Position {
        line: 1,
        character: 6,
    };

    fn completed(highlights: &Highlights) -> Completed {
        highlights
            .receiver()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
    }

    #[test]
    fn cancels_queued_and_active_highlights_once_and_recovers() {
        let project = TestProject::new();
        let path = project.write("highlight.go", SOURCE);
        let mut engine = Engine::new(project.root.clone());
        engine.open(path.clone(), SOURCE.into(), 1).unwrap();
        let session = TestSession::new();
        let server = session.server();
        let mut highlights = Highlights::default();
        for id in [1, 2] {
            highlights
                .submit(&mut engine, server, id.into(), path.clone(), TARGET)
                .unwrap();
        }
        assert!(highlights.has_active());
        assert_eq!(highlights.waiting.len(), 1);

        for id in [2, 1] {
            highlights.cancel(server, &id.into()).unwrap();
            assert_eq!(
                session.response(id).error.unwrap().code,
                ErrorCode::RequestCanceled as i32
            );
        }
        assert!(highlights.waiting.is_empty());
        let active = highlights.active.as_ref().unwrap();
        assert!(active.id.is_none());
        assert!(active.cancelled.load(Ordering::Relaxed));

        let done = completed(&highlights);
        highlights.complete(&mut engine, server, done).unwrap();
        assert!(!highlights.has_active());
        session.assert_no_messages();

        highlights
            .submit(&mut engine, server, 3.into(), path, TARGET)
            .unwrap();
        assert!(
            highlights.has_active(),
            "cancelled result must stay uncached"
        );
        let done = completed(&highlights);
        highlights.complete(&mut engine, server, done).unwrap();
        assert_eq!(
            session
                .response(3)
                .result
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );
        session.assert_no_messages();
    }

    #[test]
    fn a_highlight_finished_for_an_edited_revision_is_discarded() {
        let project = TestProject::new();
        let path = project.write("highlight.go", SOURCE);
        let mut engine = Engine::new(project.root.clone());
        engine.open(path.clone(), SOURCE.into(), 1).unwrap();
        let session = TestSession::new();
        let server = session.server();
        let mut highlights = Highlights::default();
        highlights
            .submit(&mut engine, server, 1.into(), path.clone(), TARGET)
            .unwrap();
        let done = completed(&highlights);

        engine
            .edit(
                &path,
                &[TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: "package p\nconst Shared = 1\n".into(),
                }],
                2,
            )
            .unwrap();
        highlights.complete(&mut engine, server, done).unwrap();
        assert_eq!(
            session.response(1).error.unwrap().code,
            ErrorCode::ContentModified as i32
        );

        highlights
            .submit(&mut engine, server, 2.into(), path, TARGET)
            .unwrap();
        let done = completed(&highlights);
        highlights.complete(&mut engine, server, done).unwrap();
        assert_eq!(
            session
                .response(2)
                .result
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );
        session.assert_no_messages();
    }
}
