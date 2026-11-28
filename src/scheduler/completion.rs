use std::collections::BTreeMap;
use std::path::PathBuf;
use std::thread;
use std::time::Instant;

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, bounded};
use lsp_server::{Connection, ErrorCode, RequestId};
use lsp_types::Position;

use crate::protocol::{internal_error, send_response};
use crate::semantic::completion::{
    CompletionClientCapabilities, CompletionInput, CompletionOutcome, CompletionStamp,
    CompletionWorker, StaleCompletion, Trace,
};
use crate::semantic::{Engine, SharedLoadPending};
use crate::workspace::{CancelToken, WorkspaceLoad};
use crate::{rpc_telemetry, telemetry};

pub(crate) struct Request {
    pub(crate) path: PathBuf,
    pub(crate) position: Position,
    pub(crate) caps: CompletionClientCapabilities,
}

#[derive(Default)]
pub(super) struct Completions {
    worker: Option<Worker>,
    active: Option<Active>,
    flight: Option<Flight>,
    next_sequence: u64,
}

struct Active {
    id: RequestId,
    request: Request,
    stamp: CompletionStamp,
    trace: Option<Trace>,
    local_only: bool,
    state: State,
    queued: Option<Instant>,
}

enum State {
    Watching(Option<(u64, i32)>),
    Queued,
    Running(u64),
    Workspace(WorkspaceLoad),
    OpenParse(SharedLoadPending),
}

struct Flight {
    sequence: u64,
    cancel: CancelToken,
    interrupted: Option<Instant>,
}

struct Worker {
    jobs: Sender<Job>,
    results: Receiver<Completed>,
}

struct Job {
    sequence: u64,
    input: CompletionInput,
    telemetry: Option<JobTelemetry>,
}

struct JobTelemetry {
    context: telemetry::Context,
    trace: Option<Trace>,
    queued: Instant,
    dispatched: Instant,
}

pub(crate) struct Completed {
    sequence: u64,
    outcome: CompletionOutcome,
    timing: Option<Timing>,
}

struct Timing {
    context: telemetry::Context,
    queued: Instant,
    dispatched: Instant,
    dequeued: Instant,
    started: Instant,
    finished: Instant,
    published: Instant,
    phases_ms: BTreeMap<&'static str, f64>,
    shared_disk: telemetry::SharedDiskStats,
}

impl Worker {
    fn spawn() -> Result<Self> {
        let (jobs, input) = bounded::<Job>(1);
        let (output, results) = bounded(1);
        thread::Builder::new()
            .name("go-completion".into())
            .spawn(move || {
                let mut worker = CompletionWorker::default();
                while let Ok(job) = input.recv() {
                    if output.send(job.run(&mut worker)).is_err() {
                        break;
                    }
                }
            })
            .context("starting the Go completion worker")?;
        Ok(Self { jobs, results })
    }
}

impl Job {
    fn run(self, worker: &mut CompletionWorker) -> Completed {
        let (outcome, timing) = if let Some(telemetry) = self.telemetry {
            let dequeued = Instant::now();
            let before = telemetry.trace.as_ref().map(Trace::phases);
            let (outcome, started, finished, shared_disk) =
                telemetry::with_context(Some(telemetry.context), || {
                    let started = Instant::now();
                    let outcome = worker.run(self.input);
                    (
                        outcome,
                        started,
                        Instant::now(),
                        telemetry::take_shared_disk_stats(),
                    )
                });
            let mut phases_ms = telemetry
                .trace
                .as_ref()
                .map_or_else(BTreeMap::new, Trace::phases);
            if let Some(before) = before {
                phases_ms.retain(|phase, elapsed| {
                    *elapsed -= before.get(phase).copied().unwrap_or_default();
                    *elapsed > 0.0
                });
            }
            drop(telemetry.trace);
            let timing = Timing {
                context: telemetry.context,
                queued: telemetry.queued,
                dispatched: telemetry.dispatched,
                dequeued,
                started,
                finished,
                published: Instant::now(),
                phases_ms,
                shared_disk,
            };
            (outcome, Some(timing))
        } else {
            (worker.run(self.input), None)
        };
        Completed {
            sequence: self.sequence,
            outcome,
            timing,
        }
    }
}

impl Drop for Completions {
    fn drop(&mut self) {
        self.interrupt();
    }
}

impl Completions {
    pub(super) fn receiver(&self) -> Option<&Receiver<Completed>> {
        self.worker.as_ref().map(|worker| &worker.results)
    }

    pub(super) fn worker_stopped(&mut self, connection: &Connection) -> Result<()> {
        self.worker = None;
        self.flight = None;
        if self
            .active
            .as_ref()
            .is_some_and(|active| matches!(active.state, State::Running(_)))
        {
            self.cancel(
                connection,
                |_| true,
                ErrorCode::InternalError,
                "the completion worker stopped",
            )?;
        }
        Ok(())
    }

    pub(super) fn submit(
        &mut self,
        engine: &Engine,
        connection: &Connection,
        id: RequestId,
        request: Request,
        paused: bool,
    ) -> Result<()> {
        self.cancel(
            connection,
            |_| true,
            ErrorCode::RequestCanceled,
            "a newer completion superseded the request",
        )?;
        self.active = Some(Active {
            stamp: engine.completion_stamp(&request.path),
            trace: Trace::start(&request.path, request.position),
            state: if paused {
                State::Watching(engine.open_document_identity(&request.path))
            } else {
                State::Queued
            },
            id,
            request,
            local_only: false,
            queued: telemetry::now(),
        });
        rpc_telemetry::wait(|| "completion_worker");
        self.advance(engine, connection, paused)
    }

    pub(super) fn blocks_test_prewarm(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| !matches!(active.state, State::Workspace(_)))
    }

    #[cfg(test)]
    pub(super) fn active(&self) -> bool {
        self.active.is_some()
    }

    #[cfg(test)]
    pub(super) fn waiting_for_workspace(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| matches!(active.state, State::Workspace(_)))
    }

    pub(super) fn interrupt(&mut self) {
        if let Some(flight) = &mut self.flight
            && !flight.cancel.is_cancelled()
        {
            flight.interrupted = telemetry::now();
            flight.cancel.cancel();
        }
    }

    pub(super) fn cancel(
        &mut self,
        connection: &Connection,
        matches: impl FnOnce(&RequestId) -> bool,
        code: ErrorCode,
        message: &str,
    ) -> Result<()> {
        if self
            .active
            .as_ref()
            .is_some_and(|active| matches(&active.id))
        {
            self.interrupt();
            let active = self.active.take().unwrap();
            let _step = rpc_telemetry::step(&active.id, false);
            if let Some(trace) = &active.trace {
                trace.error(code);
            }
            send_response(connection, active.id, Err((code, message.to_owned())))?;
        }
        Ok(())
    }

    // The edit or graph install can fail after requesting preemption. Retry
    // the same request if its snapshot is still current.
    pub(super) fn revalidate(&mut self, engine: &Engine, connection: &Connection) -> Result<()> {
        if self
            .active
            .as_ref()
            .is_some_and(|active| match active.state {
                State::Watching(identity) => {
                    identity != engine.open_document_identity(&active.request.path)
                }
                _ => active.stamp != engine.completion_stamp(&active.request.path),
            })
        {
            return self.cancel(
                connection,
                |_| true,
                ErrorCode::ContentModified,
                &StaleCompletion.to_string(),
            );
        }
        if let (Some(active), Some(flight)) = (&mut self.active, &self.flight)
            && matches!(active.state, State::Running(sequence) if sequence == flight.sequence)
            && flight.cancel.is_cancelled()
        {
            active.queue();
        }
        Ok(())
    }

    pub(super) fn workspace_installed(
        &mut self,
        engine: &Engine,
        connection: &Connection,
        load: WorkspaceLoad,
        failed: bool,
    ) -> Result<()> {
        if let Some(active) = &mut self.active {
            match active.state {
                State::Watching(_) => {}
                State::Workspace(waiting) => {
                    active.stamp = engine.completion_stamp(&active.request.path);
                    if load.covers(waiting) {
                        active.local_only = failed;
                        active.queue();
                    }
                }
                State::Queued => {
                    active.stamp = engine.completion_stamp(&active.request.path);
                }
                State::OpenParse(_) | State::Running(_) => self.revalidate(engine, connection)?,
            }
        }
        Ok(())
    }

    pub(super) fn workspace_failed(&mut self, load: WorkspaceLoad) {
        if let Some(active) = &mut self.active
            && matches!(active.state, State::Workspace(waiting) if load.covers(waiting))
        {
            active.local_only = true;
            active.queue();
        }
    }

    pub(super) fn workspace_invalidated(
        &mut self,
        connection: &Connection,
        message: &str,
    ) -> Result<()> {
        if self
            .active
            .as_ref()
            .is_some_and(|active| !matches!(active.state, State::Watching(_)))
        {
            self.cancel(connection, |_| true, ErrorCode::ContentModified, message)?;
        }
        Ok(())
    }

    pub(super) fn advance(
        &mut self,
        engine: &Engine,
        connection: &Connection,
        paused: bool,
    ) -> Result<()> {
        self.revalidate(engine, connection)?;
        let Some(active) = &mut self.active else {
            return Ok(());
        };
        if paused || self.flight.is_some() {
            return Ok(());
        }
        match &active.state {
            State::Queued => {}
            State::Watching(_) => {
                active.stamp = engine.completion_stamp(&active.request.path);
                active.queue();
            }
            State::OpenParse(pending) if pending.ready() => active.queue(),
            _ => return Ok(()),
        }
        let _step = rpc_telemetry::step(&active.id, false);
        if self.worker.is_none() {
            match Worker::spawn() {
                Ok(worker) => self.worker = Some(worker),
                Err(error) => {
                    let active = self.active.take().unwrap();
                    return send_response(connection, active.id, Err(internal_error(error)));
                }
            }
        }
        self.next_sequence += 1;
        let sequence = self.next_sequence;
        let cancel = CancelToken::default();
        let input = engine.capture_completion(
            &active.request.path,
            active.request.position,
            active.request.caps,
            active.trace.clone(),
            cancel.clone(),
            active.local_only,
        );
        let job = Job {
            sequence,
            input,
            telemetry: active.queued.map(|queued| JobTelemetry {
                context: telemetry::Context::request(&active.id).with_job_seq(sequence),
                trace: active.trace.clone(),
                queued,
                dispatched: Instant::now(),
            }),
        };
        // A submission occupies the slot until its result is received. Thus
        // a live worker always has room for one message here.
        if self.worker.as_ref().unwrap().jobs.try_send(job).is_err() {
            self.worker = None;
            return self.cancel(
                connection,
                |_| true,
                ErrorCode::InternalError,
                "the completion worker stopped",
            );
        }
        active.state = State::Running(sequence);
        self.flight = Some(Flight {
            sequence,
            cancel,
            interrupted: None,
        });
        Ok(())
    }

    pub(super) fn complete(
        &mut self,
        engine: &Engine,
        connection: &Connection,
        completed: Completed,
    ) -> Result<Option<(WorkspaceLoad, &'static str)>> {
        let observed = completed.timing.as_ref().map(|_| Instant::now());
        if self
            .flight
            .as_ref()
            .is_none_or(|flight| flight.sequence != completed.sequence)
        {
            return Ok(None);
        }
        self.revalidate(engine, connection)?;
        let flight = self.flight.take().unwrap();
        if let Some(observed) = observed {
            completed.record(&flight, observed);
        }
        let Some(active) = &mut self.active else {
            return Ok(None);
        };
        if !matches!(active.state, State::Running(sequence) if sequence == completed.sequence) {
            return Ok(None);
        }
        let _trace = rpc_telemetry::step(&active.id, false);
        let response = match completed.outcome {
            CompletionOutcome::Ready(value) => Ok(value),
            CompletionOutcome::NeedWorkspace(load) => {
                let trigger = rpc_telemetry::wait_workspace(|| engine.workspace_wait_reason(load));
                active.state = State::Workspace(load);
                return Ok(Some((load, trigger)));
            }
            CompletionOutcome::NeedOpenParse(pending) => {
                rpc_telemetry::wait(|| "open_document_parse");
                active.state = State::OpenParse(pending);
                return Ok(None);
            }
            CompletionOutcome::Cancelled => Err((
                ErrorCode::RequestCanceled,
                "the completion request was cancelled".into(),
            )),
            CompletionOutcome::Failed(error) if error.is::<StaleCompletion>() => {
                Err((ErrorCode::ContentModified, error.to_string()))
            }
            CompletionOutcome::Failed(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::Interrupted) =>
            {
                Err((ErrorCode::ContentModified, error.to_string()))
            }
            CompletionOutcome::Failed(error) => Err(internal_error(error)),
        };
        let active = self.active.take().unwrap();
        if let Some(trace) = &active.trace {
            match &response {
                Err((code, _)) => trace.error(*code),
                Ok(_) if active.local_only => trace.outcome("completed with local syntax"),
                Ok(_) => trace.outcome("completed"),
            }
        }
        send_response(connection, active.id, response)?;
        Ok(None)
    }
}

impl Active {
    fn queue(&mut self) {
        self.state = State::Queued;
        self.queued = telemetry::now();
    }
}

impl Completed {
    fn record(&self, flight: &Flight, observed: Instant) {
        let Some(timing) = &self.timing else {
            return;
        };
        let outcome = match self.outcome {
            CompletionOutcome::Ready(_) => "ready",
            CompletionOutcome::NeedWorkspace(_) => "workspace",
            CompletionOutcome::NeedOpenParse(_) => "open_parse",
            CompletionOutcome::Cancelled => "cancelled",
            CompletionOutcome::Failed(_) => "failed",
        };
        telemetry::with_context(Some(timing.context), || {
            let mut fields = serde_json::json!({
                "queue_ms": telemetry::millis(timing.dequeued.saturating_duration_since(timing.queued)),
                "pending_ms": telemetry::millis(timing.dispatched.saturating_duration_since(timing.queued)),
                "worker_queue_ms": telemetry::millis(timing.dequeued.saturating_duration_since(timing.dispatched)),
                "worker_ms": telemetry::millis(timing.finished.saturating_duration_since(timing.started)),
                "result_queue_ms": telemetry::millis(observed.saturating_duration_since(timing.published)),
                "cancel_to_exit_ms": flight.interrupted
                    .and_then(|interrupted| timing.finished.checked_duration_since(interrupted))
                    .map(telemetry::millis),
                "phases_ms": timing.phases_ms,
                "outcome": outcome,
            });
            timing.shared_disk.add_fields(&mut fields);
            telemetry::emit("completion_worker", fields);
        });
    }
}
