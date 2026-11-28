use std::cmp::Reverse;
use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, bounded};
use lsp_server::{Connection, ErrorCode, RequestId};
use lsp_types::{
    CodeActionKind, DocumentDiagnosticReportKind, FullDocumentDiagnosticReport,
    PublishDiagnosticsParams, Range, UnchangedDocumentDiagnosticReport, Uri,
};
use serde_json::{Value, json};

use crate::protocol::{internal_error, send_response, serialize_query};
use crate::semantic::diagnostics::{
    ContextCompleteness, DiagnosticInput, DiagnosticProvenance, DiagnosticReport,
    DiagnosticSnapshotKey, DiagnosticStage, DiagnosticsPreparation, Stages,
};
use crate::semantic::{DiagnosticIo, Engine};
use crate::workspace::{WorkspaceLoad, path_key};
use crate::{rpc_telemetry, telemetry};

const EDIT_DELAY: Duration = Duration::from_millis(25);
const MAX_EDIT_WAIT: Duration = Duration::from_millis(100);
const STAGE2_EDIT_DELAY: Duration = Duration::from_millis(200);
const STAGE2_BUDGET: Duration = Duration::from_millis(25);
const STAGE2_INTERACTIVE_BURST: usize = 3;
const EDIT_ACTIVITY: Duration = Duration::from_secs(2);
const PULL_BURST: Duration = Duration::from_millis(75);
const LARGE_DOCUMENT_BYTES: usize = 512 * 1024;
const DIAGNOSTICS_CHANGED: &str = "the document or package changed during diagnostics";
const CANCELLED: &str = "the client cancelled the request";
const CONTEXT_UNAVAILABLE: &str = "Go package context is unavailable for complete diagnostics";

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

    fn takes_preview(&self, mode: Mode, partial: bool) -> bool {
        match (self, mode) {
            (Self::Pull { .. }, Mode::Pull { refresh }) => refresh,
            (Self::Push(_), Mode::Push { versioned }) => partial || versioned,
            _ => false,
        }
    }

    fn finish_actions(
        self,
        engine: &mut Engine,
        connection: &Connection,
        path: &Path,
        key: &DiagnosticSnapshotKey,
    ) -> Result<()> {
        let Self::Actions {
            id,
            uri,
            range,
            only,
        } = self
        else {
            unreachable!("expected a code action request");
        };
        let actions = engine
            .code_actions_for_diagnostic_key(path, &uri, range, only.as_deref(), key)
            .map_err(internal_error)
            .and_then(serialize_query);
        send_response(connection, id, actions)
    }

    fn finish_report(
        self,
        connection: &Connection,
        report: &DiagnosticReport,
        fields: Option<Value>,
    ) -> Result<()> {
        match self {
            Self::Pull { id, previous } => {
                if let Some(fields) = fields {
                    rpc_telemetry::diagnostic_response(&id, fields);
                }
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
            Self::Push(uri) => rpc_telemetry::diagnostic_push(
                connection,
                PublishDiagnosticsParams::new(
                    uri,
                    report.to_lsp_diagnostics(),
                    report.snapshot.version,
                ),
                fields,
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

    fn error_for(
        self,
        connection: &Connection,
        error: (ErrorCode, String),
        key: &DiagnosticSnapshotKey,
        attempt: Option<DiagnosticProvenance>,
    ) -> Result<()> {
        if telemetry::enabled()
            && let Some(id) = self.id()
        {
            let mut fields = error_fields(key, attempt);
            let (delivery, reason) = error_disposition(error.0);
            fields["delivery"] = json!(delivery);
            fields["reason"] = json!(reason);
            fields["transport"] = json!(self.transport());
            rpc_telemetry::diagnostic_response(id, fields);
        }
        self.error(connection, error)
    }

    fn transport(&self) -> &'static str {
        match self {
            Self::Pull { .. } => "pull",
            Self::Push(_) => "push",
            Self::Actions { .. } => "code_action",
        }
    }
}

#[derive(Clone, Copy, Default)]
pub(crate) enum Mode {
    #[default]
    None,
    Pull {
        refresh: bool,
    },
    Push {
        versioned: bool,
    },
}

struct Pending {
    path: PathBuf,
    waiting_for: PendingInput,
    subscriber: Subscriber,
}

enum PendingInput {
    Workspace(DiagnosticSnapshotKey),
    OpenParse(Option<(u64, i32)>),
}

struct Subscriber {
    consumer: Consumer,
    priority: Option<Priority>,
}

struct Job {
    path: PathBuf,
    subscribers: Vec<Subscriber>,
    running: Option<Running>,
    stage: DiagnosticStage,
    attempted_preview: bool,
    preview: Option<Preview>,
    continue_pull: bool,
    waiting_for_graph: bool,
    large: bool,
    order: u64,
    run: u64,
    queue: Option<QueueTimes>,
    last_attempt: Option<DiagnosticProvenance>,
}

impl Job {
    fn runnable(&self) -> bool {
        self.running.is_none() && (!self.waiting_for_graph || self.stage == DiagnosticStage::Stage1)
    }

    fn needs_work(&self) -> bool {
        self.continue_pull || !self.subscribers.is_empty()
    }

    fn foreground(&self) -> Option<Priority> {
        self.subscribers.iter().filter_map(|sub| sub.priority).max()
    }

    fn last_interaction(&self) -> Option<u64> {
        self.subscribers
            .iter()
            .filter(|sub| sub.consumer.id().is_some())
            .filter_map(|sub| sub.priority.map(|priority| priority.request_order))
            .max()
    }

    fn subscribe(&mut self, subscriber: Subscriber) {
        if !subscriber.consumer.is_push()
            || !self.subscribers.iter().any(|sub| sub.consumer.is_push())
        {
            self.subscribers.push(subscriber);
        }
    }

    fn preview_at(&self) -> Option<Instant> {
        self.preview.as_ref().map(|preview| preview.ready_at)
    }

    fn discard_preview(&mut self, delivery: &'static str, reason: &'static str) {
        if let Some(preview) = self.preview.take() {
            preview.log_disposition(delivery, reason);
        }
    }
}

struct Preview {
    ready_at: Instant,
    fields: Option<Value>,
}

impl Preview {
    fn new(ready_at: Instant, report: &DiagnosticReport) -> Self {
        Self {
            ready_at,
            fields: telemetry::enabled().then(|| report_fields(report)),
        }
    }

    fn log_disposition(self, delivery: &'static str, reason: &'static str) {
        if let Some(fields) = self.fields {
            log_disposition(fields, delivery, reason);
        }
    }
}

#[derive(Clone, Copy)]
struct QueueTimes {
    enqueued_at: Instant,
    context_ready_at: Option<Instant>,
}

impl QueueTimes {
    fn new(waiting_for_context: bool) -> Option<Self> {
        let enqueued_at = telemetry::now()?;
        Some(Self {
            enqueued_at,
            context_ready_at: (!waiting_for_context).then_some(enqueued_at),
        })
    }

    fn delays(self, eligible_at: Instant, start: Instant) -> [Duration; 4] {
        let context_ready_at = self.context_ready_at.unwrap_or(eligible_at);
        [
            start.saturating_duration_since(self.enqueued_at),
            context_ready_at.saturating_duration_since(self.enqueued_at),
            eligible_at.saturating_duration_since(context_ready_at),
            start.saturating_duration_since(eligible_at),
        ]
    }
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
struct Priority {
    edited_at: Option<Instant>,
    request_order: u64,
}

#[derive(Eq, Ord, PartialEq, PartialOrd)]
enum QueueRank {
    Background(Reverse<u64>),
    Interaction(u64, Reverse<u64>),
    BackgroundTurn(Reverse<u64>),
    Preview(Option<Priority>, Reverse<u64>),
}

struct EditWindow {
    first: Instant,
    last: Instant,
    started: bool,
}

impl EditWindow {
    fn ready_at(&self, stage: DiagnosticStage) -> Option<Instant> {
        match stage {
            DiagnosticStage::Stage1 => {
                (!self.started).then(|| (self.last + EDIT_DELAY).min(self.first + MAX_EDIT_WAIT))
            }
            DiagnosticStage::Stage2 => Some(self.last + STAGE2_EDIT_DELAY),
        }
    }
}

struct Running {
    ticket: u64,
    cancelled: Arc<AtomicBool>,
    cancel_requested_at: Option<Arc<Mutex<Option<Instant>>>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(requested) = &self.cancel_requested_at {
            let mut requested = requested.lock().unwrap();
            *requested = Some(Instant::now());
            self.cancelled.store(true, Ordering::Relaxed);
        } else {
            self.cancelled.store(true, Ordering::Relaxed);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Lane {
    Fast,
    Reserve,
    Background,
    Background2,
    LargePreview,
    Large,
}

impl Lane {
    const ALL: [Self; 6] = [
        Self::Fast,
        Self::Reserve,
        Self::Background,
        Self::Background2,
        Self::LargePreview,
        Self::Large,
    ];
    const STAGE2: [Self; 2] = [Self::Background, Self::Background2];

    fn index(self) -> usize {
        self as usize
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Fast => "foreground",
            Self::Reserve => "reserve",
            Self::Background => "background",
            Self::Background2 => "background_2",
            Self::LargePreview => "large_preview",
            Self::Large => "large",
        }
    }
}

impl DiagnosticProvenance {
    fn fields(self, key: &DiagnosticSnapshotKey) -> Value {
        let document = telemetry::path(&key.path_key);
        json!({
            "job": self.job,
            "run": self.run,
            "path": document,
            "document": document,
            "doc_version": key.version,
            "stage": self.stage.as_str(),
            "context_completeness": if key.partial { "partial" } else { "full" },
            "partial": key.partial,
            "lane": self.lane,
        })
    }
}

struct WorkTrace {
    attempt: DiagnosticProvenance,
    queue: QueueTimes,
    eligible_at: Instant,
    cancel_requested_at: Arc<Mutex<Option<Instant>>>,
}

impl WorkTrace {
    fn started(&self, key: &DiagnosticSnapshotKey, start: Instant) -> Value {
        let mut fields = self.attempt.fields(key);
        let [total, context, merge, runnable] = self.queue.delays(self.eligible_at, start);
        fields["enqueued_ms"] = json!(telemetry::timestamp(self.queue.enqueued_at));
        fields["worker_started_ms"] = json!(telemetry::timestamp(start));
        fields["queue_wait_ms"] = json!(telemetry::millis(total));
        fields["context_wait_ms"] = json!(telemetry::millis(context));
        fields["merge_wait_ms"] = json!(telemetry::millis(merge));
        fields["runnable_queue_wait_ms"] = json!(telemetry::millis(runnable));
        fields
    }

    fn cancellation_at(&self, exited_at: Instant) -> Option<Instant> {
        self.cancel_requested_at
            .lock()
            .unwrap()
            .filter(|&requested| requested <= exited_at)
    }
}

// The slot stays occupied until its own ticket returns, even after the job
// drops its Running handle and requests cancellation.
struct Slot {
    ticket: u64,
    cancelled: Arc<AtomicBool>,
}

struct Work {
    key: DiagnosticSnapshotKey,
    ticket: u64,
    input: DiagnosticInput,
    stage: DiagnosticStage,
    cancelled: Arc<AtomicBool>,
    trace: Option<WorkTrace>,
}

pub(crate) struct Completed {
    key: DiagnosticSnapshotKey,
    ticket: u64,
    lane: Lane,
    report: Result<Option<DiagnosticReport>>,
    ready_at: Instant,
    attempt: Option<DiagnosticProvenance>,
    published_at: Option<Instant>,
    #[cfg(test)]
    stage: DiagnosticStage,
}

impl Completed {
    fn log_disposition(&self, delivery: &'static str, reason: &'static str) {
        if self.attempt.is_none() || !telemetry::enabled() {
            return;
        }
        if let Ok(Some(report)) = &self.report {
            log_report_disposition(report, delivery, reason);
        } else {
            log_attempt_disposition(&self.key, self.attempt, delivery, reason);
        }
    }
}

#[cfg(test)]
impl Completed {
    pub(crate) fn set_ready_at(&mut self, at: Instant) {
        self.ready_at = at;
    }

    pub(crate) fn stage(&self) -> DiagnosticStage {
        self.stage
    }
}

struct Pool {
    send: [Sender<Work>; Lane::ALL.len()],
    receive: Receiver<Completed>,
}

impl Pool {
    fn new() -> Result<Self> {
        let channels: [_; Lane::ALL.len()] = std::array::from_fn(|_| bounded::<Work>(1));
        let send = channels.each_ref().map(|(send, _)| send.clone());
        let (done, receive) = bounded(Lane::ALL.len());
        for (lane, (_, queue)) in Lane::ALL.into_iter().zip(channels) {
            let done = done.clone();
            thread::Builder::new()
                .name(format!("go-diagnostics-{lane:?}"))
                .spawn(move || {
                    while let Ok(work) = queue.recv() {
                        let timing = work.trace.as_ref().map(|trace| {
                            let start = Instant::now();
                            let fields = trace.started(&work.key, start);
                            telemetry::emit("diagnostic_worker_start", fields.clone());
                            (start, fields)
                        });
                        let io = timing
                            .as_ref()
                            .map(|_| DiagnosticIo::start(&work.key.path_key));
                        let mut stages = timing.as_ref().map(|_| Stages::default());
                        let mut shared_disk = telemetry::SharedDiskStats::default();
                        let mut report = if work.cancelled.load(Ordering::Relaxed) {
                            Ok(None)
                        } else {
                            let context = work
                                .trace
                                .as_ref()
                                .map(|trace| telemetry::Context::diagnostic(trace.attempt));
                            telemetry::with_context(context, || {
                                let report = run_analysis(|| {
                                    work.input.analyze_with_stages(
                                        work.stage,
                                        &work.cancelled,
                                        &mut stages,
                                    )
                                });
                                if context.is_some() {
                                    shared_disk = telemetry::take_shared_disk_stats();
                                }
                                report
                            })
                        };
                        let ready_at = Instant::now();
                        let attempt = work.trace.as_ref().map(|trace| trace.attempt);
                        if let (Some((start, mut fields)), Some(trace), Some(io)) =
                            (timing, work.trace, io)
                        {
                            let requested_at = trace.cancellation_at(ready_at);
                            fields["analysis_ms"] = json!(telemetry::millis(ready_at - start));
                            fields["worker_exited_ms"] = json!(telemetry::timestamp(ready_at));
                            fields["cancelled"] = json!(requested_at.is_some());
                            fields["cancel_requested_ms"] =
                                json!(requested_at.and_then(telemetry::timestamp));
                            fields["cancel_lag_ms"] =
                                json!(requested_at.map(|at| telemetry::millis(ready_at - at)));
                            let cold = io.counts();
                            fields["cold_dep_reads"] = json!(cold.reads);
                            fields["cold_dep_parses"] = json!(cold.parses);
                            if let Ok(Some(report)) = &mut report {
                                report.provenance = Some(trace.attempt);
                                fields["context_completeness"] =
                                    json!(report.context_completeness.as_str());
                                fields["report_stage"] = json!(report.diagnostic_stage.as_str());
                                fields["result_id"] = json!(telemetry::hash(&report.result_id));
                                fields["diagnostic_codes"] =
                                    telemetry::diagnostic_codes(&report.items);
                            } else {
                                fields["diagnostic_codes"] = json!({});
                            }
                            if let Some(stages) = stages {
                                let mut stage_fields = fields.clone();
                                stage_fields["partial"] =
                                    json!(stage_fields["context_completeness"] == "partial");
                                stages.log(stage_fields);
                            }
                            shared_disk.add_fields(&mut fields);
                            telemetry::emit("diagnostic_worker_done", fields);
                        }
                        if done
                            .send(Completed {
                                key: work.key,
                                ticket: work.ticket,
                                lane,
                                report,
                                ready_at,
                                attempt,
                                published_at: attempt.and_then(|_| telemetry::now()),
                                #[cfg(test)]
                                stage: work.stage,
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

fn run_analysis(
    analyze: impl FnOnce() -> Result<Option<DiagnosticReport>>,
) -> Result<Option<DiagnosticReport>> {
    catch_unwind(AssertUnwindSafe(analyze))
        .unwrap_or_else(|_| Err(anyhow::anyhow!("Go diagnostic analysis panicked")))
}

#[derive(Clone)]
pub(crate) struct RefreshMark {
    key: DiagnosticSnapshotKey,
    result_id: String,
    ticket: u64,
    telemetry: Option<Arc<Value>>,
}

impl RefreshMark {
    pub(crate) fn path(&self) -> &Path {
        &self.key.path_key
    }

    pub(crate) fn telemetry_fields(&self) -> Value {
        self.telemetry.as_deref().cloned().unwrap_or(Value::Null)
    }
}

struct Delivery {
    report: Arc<DiagnosticReport>,
    push_uri: Option<Uri>,
    replacement: Option<RefreshMark>,
}

#[derive(Default)]
pub(super) struct Diagnostics {
    mode: Mode,
    pending: Vec<Pending>,
    jobs: HashMap<DiagnosticSnapshotKey, Job>,
    pool: Option<Pool>,
    slots: [Option<Slot>; Lane::ALL.len()],
    sequence: u64,
    stage2_interactive_streak: usize,
    edits: HashMap<PathBuf, EditWindow>,
    last_pull: Option<Instant>,
    focus: Option<PathBuf>,
    delivered: HashMap<PathBuf, Delivery>,
    refresh: HashMap<PathBuf, RefreshMark>,
}

impl Diagnostics {
    pub(super) fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    pub(super) fn receiver(&self) -> Option<&Receiver<Completed>> {
        self.pool.as_ref().map(|pool| &pool.receive)
    }

    pub(super) fn pending_count(&self) -> usize {
        self.pending.len()
    }

    pub(super) fn note_edit(&mut self, path: &Path, now: Instant) {
        let window = self.edits.entry(path_key(path)).or_insert(EditWindow {
            first: now,
            last: now,
            started: false,
        });
        if window.started || now > window.last + EDIT_DELAY {
            window.first = now;
            window.started = false;
        }
        window.last = now;
    }

    pub(super) fn interrupt_for_edit(&mut self, path: &Path) {
        let path = path_key(path);
        for (key, job) in &mut self.jobs {
            if key.path_key == path {
                job.running = None;
            }
        }
    }

    pub(super) fn next_deadline(&self) -> Option<Instant> {
        let now = Instant::now();
        let stage2_available = self.has_stage2_capacity();
        self.jobs
            .iter()
            .filter_map(|(key, job)| {
                let runnable = (job.runnable() && self.free_lane(job, stage2_available).is_some())
                    .then(|| self.edit_deadline(key, job.stage).unwrap_or(now));
                runnable.into_iter().chain(job.preview_at()).min()
            })
            .min()
    }

    pub(super) fn drain_refresh(&mut self) -> Vec<RefreshMark> {
        self.refresh.drain().map(|(_, mark)| mark).collect()
    }

    pub(super) fn refresh_is_current(&self, engine: &mut Engine, mark: &RefreshMark) -> bool {
        matches!(self.mode, Mode::Pull { refresh: true })
            && engine.is_open(mark.path())
            && self.delivered.get(mark.path()).is_some_and(|delivery| {
                delivery.replacement.as_ref().is_some_and(|replacement| {
                    replacement.ticket == mark.ticket && delivery.report.result_id != mark.result_id
                })
            })
            && engine
                .diagnostic_report_for_key(&mark.key)
                .is_some_and(|report| {
                    report.diagnostic_stage == DiagnosticStage::Stage2
                        && report.result_id == mark.result_id
                })
            && is_current(engine, mark.path(), &mark.key)
    }

    pub(super) fn submit(
        &mut self,
        engine: &mut Engine,
        connection: &Connection,
        path: PathBuf,
        consumer: Consumer,
        fallback: bool,
    ) -> Result<Option<WorkspaceLoad>> {
        self.submit_at(engine, connection, path, consumer, fallback, Instant::now())
    }

    fn submit_at(
        &mut self,
        engine: &mut Engine,
        connection: &Connection,
        path: PathBuf,
        consumer: Consumer,
        fallback: bool,
        now: Instant,
    ) -> Result<Option<WorkspaceLoad>> {
        let subscriber = self.subscriber(&path, consumer, now);
        let requested = self.submit_subscriber(engine, connection, path, subscriber, fallback)?;
        self.start_ready(engine, connection, now)?;
        Ok(requested)
    }

    fn submit_subscriber(
        &mut self,
        engine: &mut Engine,
        connection: &Connection,
        path: PathBuf,
        subscriber: Subscriber,
        fallback: bool,
    ) -> Result<Option<WorkspaceLoad>> {
        let consumer = &subscriber.consumer;
        if consumer.is_push() && (!matches!(self.mode, Mode::Push { .. }) || !engine.is_open(&path))
        {
            return Ok(None);
        }
        let enqueue_push = !consumer.is_push()
            || !self.pending.iter().any(|pending| {
                pending.subscriber.consumer.is_push() && path_key(&pending.path) == path_key(&path)
            });
        if engine.pending_document_affects(&path) {
            if enqueue_push {
                self.pending.push(Pending {
                    waiting_for: PendingInput::OpenParse(engine.open_document_identity(&path)),
                    path,
                    subscriber,
                });
            }
            return Ok(None);
        }
        let allow_partial = consumer.takes_preview(self.mode, true) && engine.is_open(&path);
        let preparation = engine.prepare_diagnostics(&path, allow_partial || fallback);
        match preparation {
            Ok(DiagnosticsPreparation::Pending(load)) => {
                let _ = rpc_telemetry::wait_workspace(|| engine.workspace_wait_reason(load));
                if enqueue_push {
                    match engine.diagnostic_key(&path) {
                        Ok(key) => self.pending.push(Pending {
                            path,
                            waiting_for: PendingInput::Workspace(key),
                            subscriber,
                        }),
                        Err(error) => subscriber
                            .consumer
                            .error(connection, internal_error(error))?,
                    }
                }
                Ok(Some(load))
            }
            Ok(DiagnosticsPreparation::Ready(report)) => {
                let key = report.snapshot.clone();
                self.deliver_current(engine, connection, &path, &key, subscriber.consumer, true)?;
                Ok((!fallback && key.partial).then(|| engine.workspace_load()))
            }
            Ok(DiagnosticsPreparation::Cold(key)) => {
                let preview =
                    !fallback && allow_partial && consumer.takes_preview(self.mode, key.partial);
                if preview
                    && self.jobs.get(&key).is_some_and(|job| job.continue_pull)
                    && matches!(consumer, Consumer::Pull { .. })
                    && engine.diagnostic_report_for_key(&key).is_some()
                {
                    self.deliver_current(
                        engine,
                        connection,
                        &path,
                        &key,
                        subscriber.consumer,
                        true,
                    )?;
                    return Ok(key.partial.then(|| engine.workspace_load()));
                }
                let waiting_for_graph = key.partial && !fallback;
                let stage = if preview {
                    DiagnosticStage::Stage1
                } else {
                    DiagnosticStage::Stage2
                };
                if let Some(job) = self.jobs.get_mut(&key) {
                    job.subscribe(subscriber);
                    if preview && !job.attempted_preview {
                        // A QuickFix can arrive first. Keep its consumer attached
                        // while running the newly requested preview independently.
                        job.running = None;
                        job.stage = DiagnosticStage::Stage1;
                        job.attempted_preview = true;
                        job.queue = QueueTimes::new(false);
                    }
                    job.waiting_for_graph &= waiting_for_graph;
                    if job.runnable() {
                        resolve_queue_context(&mut job.queue);
                    }
                } else {
                    self.sequence += 1;
                    let large = engine.diagnostic_document_len(&path) >= LARGE_DOCUMENT_BYTES;
                    self.jobs.insert(
                        key,
                        Job {
                            path,
                            subscribers: vec![subscriber],
                            running: None,
                            stage,
                            attempted_preview: preview,
                            preview: None,
                            continue_pull: false,
                            waiting_for_graph,
                            large,
                            order: self.sequence,
                            run: self.sequence,
                            queue: QueueTimes::new(
                                waiting_for_graph && stage == DiagnosticStage::Stage2,
                            ),
                            last_attempt: None,
                        },
                    );
                }
                Ok(waiting_for_graph.then(|| engine.workspace_load()))
            }
            Err(error) => {
                subscriber
                    .consumer
                    .error(connection, internal_error(error))?;
                Ok(None)
            }
        }
    }

    fn subscriber(&mut self, path: &Path, consumer: Consumer, now: Instant) -> Subscriber {
        let priority = self.priority(&path_key(path), &consumer, now);
        Subscriber { consumer, priority }
    }

    fn priority(&mut self, path: &Path, consumer: &Consumer, now: Instant) -> Option<Priority> {
        let recent_edit = self
            .edits
            .get(path)
            .filter(|edit| now < edit.last + EDIT_ACTIVITY);
        let interactive = match consumer {
            Consumer::Pull { .. } => {
                let first_in_burst = self.last_pull.is_none_or(|last| now >= last + PULL_BURST);
                self.last_pull = Some(now);
                if recent_edit.is_some() || first_in_burst || self.focus.is_none() {
                    self.focus = Some(path.to_owned());
                }
                self.focus.as_deref() == Some(path)
            }
            Consumer::Actions { .. } => true,
            Consumer::Push(_) => false,
        };
        (interactive || recent_edit.is_some()).then(|| {
            self.sequence += 1;
            Priority {
                edited_at: recent_edit.map(|edit| edit.last),
                request_order: self.sequence,
            }
        })
    }

    fn edit_deadline(
        &self,
        key: &DiagnosticSnapshotKey,
        stage: DiagnosticStage,
    ) -> Option<Instant> {
        self.edits.get(&key.path_key)?.ready_at(stage)
    }

    fn has_stage2_capacity(&self) -> bool {
        Lane::STAGE2
            .into_iter()
            .chain([Lane::Large])
            .filter(|lane| self.slots[lane.index()].is_some())
            .count()
            < Lane::STAGE2.len()
    }

    fn free_lane(&self, job: &Job, stage2_available: bool) -> Option<Lane> {
        let free = |lane: Lane| self.slots[lane.index()].is_none();
        match (job.stage, job.large) {
            (DiagnosticStage::Stage1, true) => {
                free(Lane::LargePreview).then_some(Lane::LargePreview)
            }
            (DiagnosticStage::Stage1, false) if free(Lane::Fast) => Some(Lane::Fast),
            (DiagnosticStage::Stage1, false) => {
                // Only cancellation on the fast worker opens the Stage 1 reserve.
                let abandoned = self.slots[Lane::Fast.index()]
                    .as_ref()
                    .is_some_and(|slot| slot.cancelled.load(Ordering::Relaxed));
                (abandoned && free(Lane::Reserve)).then_some(Lane::Reserve)
            }
            // Large jobs keep their isolated worker while sharing the Stage 2 cap.
            (DiagnosticStage::Stage2, true) => {
                (stage2_available && free(Lane::Large)).then_some(Lane::Large)
            }
            (DiagnosticStage::Stage2, false) if stage2_available => {
                Lane::STAGE2.into_iter().find(|&lane| free(lane))
            }
            (DiagnosticStage::Stage2, false) => None,
        }
    }

    pub(super) fn advance(
        &mut self,
        engine: &mut Engine,
        connection: &Connection,
        now: Instant,
    ) -> Result<()> {
        self.deliver_previews(engine, connection, now)?;
        self.start_ready(engine, connection, now)
    }

    fn start_ready(
        &mut self,
        engine: &mut Engine,
        connection: &Connection,
        now: Instant,
    ) -> Result<()> {
        while let Some((key, lane)) = self.next_job(now) {
            if is_current(engine, &self.jobs[&key].path, &key) {
                self.start(engine, connection, key, lane)?;
            } else {
                let mut job = self.jobs.remove(&key).unwrap();
                job.discard_preview("superseded", "content_modified");
                fail_attempt(
                    connection,
                    job.subscribers,
                    ErrorCode::ContentModified,
                    DIAGNOSTICS_CHANGED,
                    &key,
                    job.last_attempt,
                )?;
            }
        }
        Ok(())
    }

    fn next_job(&self, now: Instant) -> Option<(DiagnosticSnapshotKey, Lane)> {
        let stage2_available = self.has_stage2_capacity();
        self.jobs
            .iter()
            .filter(|(key, job)| {
                job.runnable()
                    && self
                        .edit_deadline(key, job.stage)
                        .is_none_or(|ready_at| now >= ready_at)
            })
            .filter_map(|(key, job)| {
                self.free_lane(job, stage2_available)
                    .map(|lane| (key, job, lane))
            })
            .max_by_key(|(_, job, _)| {
                let order = Reverse(job.order);
                if job.stage == DiagnosticStage::Stage1 {
                    QueueRank::Preview(job.foreground(), order)
                } else if let Some(last) = job.last_interaction() {
                    QueueRank::Interaction(last, order)
                } else if self.stage2_interactive_streak >= STAGE2_INTERACTIVE_BURST {
                    // Give the oldest eligible background job a turn after a burst.
                    QueueRank::BackgroundTurn(order)
                } else {
                    QueueRank::Background(order)
                }
            })
            .map(|(key, _, lane)| (key.clone(), lane))
    }

    fn start(
        &mut self,
        engine: &Engine,
        connection: &Connection,
        key: DiagnosticSnapshotKey,
        lane: Lane,
    ) -> Result<()> {
        let job = &self.jobs[&key];
        self.sequence += 1;
        let ticket = self.sequence;
        let cancelled = Arc::new(AtomicBool::new(false));
        let trace = job.queue.map(|queue| {
            let queue_ready = queue.context_ready_at.unwrap_or_else(Instant::now);
            let eligible_at = self
                .edit_deadline(&key, job.stage)
                .map_or(queue_ready, |ready_at| queue_ready.max(ready_at));
            WorkTrace {
                attempt: DiagnosticProvenance {
                    job: ticket,
                    run: job.run,
                    lane: lane.as_str(),
                    stage: job.stage,
                },
                queue,
                eligible_at,
                cancel_requested_at: Arc::new(Mutex::new(None)),
            }
        });
        let attempt = trace.as_ref().map(|trace| trace.attempt);
        let cancel_requested_at = trace
            .as_ref()
            .map(|trace| trace.cancel_requested_at.clone());
        let work = Work {
            ticket,
            input: engine.capture_diagnostics(&job.path, key.clone()),
            stage: job.stage,
            key: key.clone(),
            cancelled: cancelled.clone(),
            trace,
        };
        let send = (|| {
            if self.pool.is_none() {
                self.pool = Some(Pool::new()?);
            }
            self.pool
                .as_ref()
                .expect("diagnostic pool was started")
                .send[lane.index()]
            .try_send(work)
            .map_err(|_| anyhow::anyhow!("diagnostic worker queue is unavailable"))
        })();
        match send {
            Ok(()) => {
                let job = self.jobs.get_mut(&key).unwrap();
                if job.stage == DiagnosticStage::Stage1 {
                    if let Some(edit) = self.edits.get_mut(&key.path_key) {
                        edit.started = true;
                    }
                } else if job.last_interaction().is_some() {
                    self.stage2_interactive_streak =
                        (self.stage2_interactive_streak + 1).min(STAGE2_INTERACTIVE_BURST);
                } else {
                    self.stage2_interactive_streak = 0;
                }
                self.slots[lane.index()] = Some(Slot {
                    ticket,
                    cancelled: cancelled.clone(),
                });
                job.last_attempt = attempt;
                job.running = Some(Running {
                    ticket,
                    cancelled,
                    cancel_requested_at,
                });
            }
            Err(error) => {
                let mut job = self.jobs.remove(&key).unwrap();
                job.discard_preview("discarded", "worker_unavailable");
                let (code, message) = internal_error(error);
                fail(connection, job.subscribers, code, &message)?;
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
        if let (Some(attempt), Some(published_at)) = (completed.attempt, completed.published_at) {
            let wait = published_at.elapsed();
            telemetry::with_context(Some(telemetry::Context::diagnostic(attempt)), || {
                telemetry::event!("diagnostic_worker_result", {
                    "result_queue_ms": telemetry::millis(wait),
                });
            });
        }
        self.apply_completion(engine, connection, completed)?;
        self.start_ready(engine, connection, Instant::now())
    }

    fn apply_completion(
        &mut self,
        engine: &mut Engine,
        connection: &Connection,
        completed: Completed,
    ) -> Result<()> {
        let slot = &mut self.slots[completed.lane.index()];
        if slot
            .as_ref()
            .is_none_or(|slot| slot.ticket != completed.ticket)
        {
            completed.log_disposition("discarded", "slot_replaced");
            return Ok(());
        }
        *slot = None;
        let key = &completed.key;
        let Some(job) = self.jobs.get(key).filter(|job| {
            job.running
                .as_ref()
                .is_some_and(|running| running.ticket == completed.ticket)
        }) else {
            if completed.attempt.is_some() {
                if is_current(engine, &key.path_key, key) {
                    completed.log_disposition("discarded", "attempt_cancelled");
                } else {
                    completed.log_disposition("superseded", "content_modified");
                }
            }
            return Ok(());
        };
        if job
            .preview_at()
            .is_some_and(|deadline| completed.ready_at > deadline)
        {
            self.deliver_preview(engine, connection, key)?;
        }
        let Some(mut job) = self.jobs.remove(key) else {
            completed.log_disposition("discarded", "inactive_job");
            return Ok(());
        };
        job.running = None;
        if !is_current(engine, &job.path, key) {
            job.discard_preview("superseded", "content_modified");
            completed.log_disposition("superseded", "content_modified");
            return fail_attempt(
                connection,
                job.subscribers,
                ErrorCode::ContentModified,
                DIAGNOSTICS_CHANGED,
                key,
                completed.attempt,
            );
        }
        let Completed {
            key,
            report,
            ready_at,
            ticket,
            attempt,
            ..
        } = completed;
        if job.stage == DiagnosticStage::Stage1 {
            match report {
                Ok(Some(report))
                    if report.snapshot == key
                        && report.diagnostic_stage == DiagnosticStage::Stage1
                        && (key.partial
                            || report.context_completeness == ContextCompleteness::Complete
                            || !report.items.is_empty()) =>
                {
                    if job
                        .subscribers
                        .iter()
                        .any(|sub| sub.consumer.takes_preview(self.mode, key.partial))
                    {
                        let budget = if key.partial && matches!(self.mode, Mode::Push { .. }) {
                            Duration::ZERO
                        } else {
                            STAGE2_BUDGET
                        };
                        job.preview = Some(Preview::new(ready_at + budget, &report));
                    } else {
                        log_report_disposition(&report, "discarded", "preview_not_requested");
                    }
                    engine.install_diagnostic_report(report);
                }
                Ok(Some(report)) => {
                    log_report_disposition(&report, "discarded", "preview_unverified")
                }
                _ => log_attempt_disposition(&key, attempt, "discarded", "preview_unavailable"),
            }
            job.stage = DiagnosticStage::Stage2;
            self.sequence += 1;
            job.order = self.sequence;
            job.queue = QueueTimes::new(job.waiting_for_graph);
            self.jobs.insert(key, job);
            return Ok(());
        }
        match report {
            Ok(Some(report)) if report.snapshot == key => {
                let complete = report.diagnostic_stage == DiagnosticStage::Stage2;
                job.discard_preview("superseded", "stage2_ready");
                engine.install_diagnostic_report(report);
                let mut attempted_delivery = false;
                for Subscriber { consumer, .. } in job.subscribers {
                    if complete
                        || matches!(consumer, Consumer::Actions { .. })
                        || (key.version.is_some() && consumer.takes_preview(self.mode, key.partial))
                    {
                        attempted_delivery = true;
                        self.deliver_current(engine, connection, &job.path, &key, consumer, false)?;
                    } else {
                        consumer.error_for(
                            connection,
                            (ErrorCode::RequestFailed, CONTEXT_UNAVAILABLE.into()),
                            &key,
                            attempt,
                        )?;
                    }
                }
                let refresh_queued =
                    job.continue_pull && self.queue_refresh(engine, &job.path, key.clone(), ticket);
                if !attempted_delivery
                    && !refresh_queued
                    && let Some(report) = engine.diagnostic_report_for_key(&key)
                {
                    let reason = if complete {
                        "preview_current"
                    } else {
                        "context_unavailable"
                    };
                    log_report_disposition(report, "discarded", reason);
                }
                Ok(())
            }
            Ok(_) => {
                job.discard_preview("discarded", "report_unavailable");
                log_attempt_disposition(&key, attempt, "discarded", "report_unavailable");
                fail_attempt(
                    connection,
                    job.subscribers,
                    ErrorCode::RequestCanceled,
                    CANCELLED,
                    &key,
                    attempt,
                )
            }
            Err(error) => {
                job.discard_preview("discarded", "analysis_failed");
                log_attempt_disposition(&key, attempt, "discarded", "analysis_failed");
                let (code, message) = internal_error(error);
                fail_attempt(connection, job.subscribers, code, &message, &key, attempt)
            }
        }
    }

    fn deliver_previews(
        &mut self,
        engine: &mut Engine,
        connection: &Connection,
        now: Instant,
    ) -> Result<()> {
        let ready: Vec<_> = self
            .jobs
            .iter()
            .filter(|(_, job)| job.preview_at().is_some_and(|at| now >= at))
            .map(|(key, _)| key.clone())
            .collect();
        for key in ready {
            self.deliver_preview(engine, connection, &key)?;
        }
        Ok(())
    }

    fn deliver_preview(
        &mut self,
        engine: &mut Engine,
        connection: &Connection,
        key: &DiagnosticSnapshotKey,
    ) -> Result<()> {
        let mut job = self.jobs.remove(key).unwrap();
        if !is_current(engine, &job.path, key) {
            job.discard_preview("superseded", "content_modified");
            return fail_attempt(
                connection,
                job.subscribers,
                ErrorCode::ContentModified,
                DIAGNOSTICS_CHANGED,
                key,
                job.last_attempt,
            );
        }
        let mut attempted = false;
        if engine
            .diagnostic_report_for_key(key)
            .is_some_and(|report| report.diagnostic_stage == DiagnosticStage::Stage1)
        {
            let mode = self.mode;
            for Subscriber { consumer, .. } in job.subscribers.extract_if(.., |sub| {
                !sub.consumer.is_push() && sub.consumer.takes_preview(mode, key.partial)
            }) {
                attempted = true;
                job.continue_pull |=
                    self.deliver_current(engine, connection, &job.path, key, consumer, false)?;
            }
            if let Some(Consumer::Push(uri)) = job
                .subscribers
                .iter()
                .map(|sub| &sub.consumer)
                .find(|consumer| consumer.is_push() && consumer.takes_preview(mode, key.partial))
            {
                attempted = true;
                self.deliver_current(
                    engine,
                    connection,
                    &job.path,
                    key,
                    Consumer::Push(uri.clone()),
                    false,
                )?;
            }
        }
        if attempted {
            job.preview = None;
        } else {
            job.discard_preview("discarded", "preview_unavailable");
        }
        if job.needs_work() {
            self.jobs.insert(key.clone(), job);
        }
        Ok(())
    }

    // The caller checked this snapshot during the current main-loop turn.
    // Disk documents can still change externally between two deliveries.
    fn deliver_current(
        &mut self,
        engine: &mut Engine,
        connection: &Connection,
        path: &Path,
        key: &DiagnosticSnapshotKey,
        consumer: Consumer,
        cache_hit: bool,
    ) -> Result<bool> {
        let _trace = consumer.id().and_then(|id| rpc_telemetry::step(id, false));
        if key.version.is_none() && !is_current(engine, path, key) {
            consumer.error_for(
                connection,
                (ErrorCode::ContentModified, DIAGNOSTICS_CHANGED.into()),
                key,
                None,
            )?;
            return Ok(false);
        }
        if matches!(consumer, Consumer::Actions { .. }) {
            if telemetry::enabled()
                && let Some(report) = engine.diagnostic_report_for_key(key)
                && let Some(id) = consumer.id()
            {
                let fields = delivery_fields(report, &consumer, cache_hit);
                rpc_telemetry::diagnostic_response(id, fields);
            }
            consumer.finish_actions(engine, connection, path, key)?;
            return Ok(false);
        }
        let Some(report) = engine.shared_diagnostic_report(key) else {
            consumer.error_for(
                connection,
                internal_error(anyhow::anyhow!("the diagnostic report expired")),
                key,
                None,
            )?;
            return Ok(false);
        };
        let push_uri = match &consumer {
            Consumer::Push(uri) if key.version.is_some() => Some(uri.clone()),
            Consumer::Push(_) => return Ok(false),
            _ => None,
        };
        let unchanged_push = push_uri.is_some()
            && self.delivered.get(&key.path_key).is_some_and(|previous| {
                previous.push_uri == push_uri
                    && previous.report.snapshot.same_document(key)
                    && previous.report.same_visible_content(&report)
            });
        if unchanged_push {
            log_report_disposition(&report, "superseded", "unchanged_push");
        } else {
            let fields =
                telemetry::enabled().then(|| delivery_fields(&report, &consumer, cache_hit));
            consumer.finish_report(connection, &report, fields)?;
        }
        if key.version.is_some() {
            self.delivered.insert(
                key.path_key.clone(),
                Delivery {
                    report,
                    push_uri,
                    replacement: None,
                },
            );
            self.refresh.remove(&key.path_key);
            engine.mark_diagnostic_report_delivered(key);
            engine.release_diagnostic_replacement(&key.path_key);
        }
        Ok(!unchanged_push)
    }

    fn queue_refresh(
        &mut self,
        engine: &mut Engine,
        path: &Path,
        key: DiagnosticSnapshotKey,
        ticket: u64,
    ) -> bool {
        if !matches!(self.mode, Mode::Pull { refresh: true }) || !is_current(engine, path, &key) {
            return false;
        }
        if let Some(report) = engine.shared_diagnostic_report(&key)
            && report.diagnostic_stage == DiagnosticStage::Stage2
            && let Some(previous) = self.delivered.get_mut(&key.path_key)
            && previous.report.diagnostic_stage == DiagnosticStage::Stage1
            && (previous.report.snapshot == key
                || (previous.report.snapshot.partial
                    && previous.report.snapshot.same_document(&key)))
            && previous.report.result_id != report.result_id
            && !previous.report.same_visible_content(&report)
        {
            let mark = RefreshMark {
                telemetry: telemetry::enabled().then(|| Arc::new(report_fields(&report))),
                key,
                result_id: report.result_id.clone(),
                ticket,
            };
            previous.replacement = Some(mark.clone());
            self.refresh.insert(mark.key.path_key.clone(), mark);
            engine.retain_diagnostic_replacement(report);
            return true;
        }
        false
    }

    pub(super) fn replay(
        &mut self,
        engine: &mut Engine,
        connection: &Connection,
        fallback: bool,
    ) -> Result<Option<WorkspaceLoad>> {
        if fallback {
            for job in self.jobs.values_mut() {
                job.waiting_for_graph = false;
                resolve_queue_context(&mut job.queue);
            }
        }
        let mut requested = None;
        let ready: Vec<_> = self
            .pending
            .extract_if(.., |pending| {
                matches!(pending.waiting_for, PendingInput::Workspace(_))
            })
            .collect();
        for Pending {
            path, subscriber, ..
        } in ready
        {
            let _trace = subscriber
                .consumer
                .id()
                .and_then(|id| rpc_telemetry::step(id, !fallback));
            if fallback {
                let _ = rpc_telemetry::wait(|| "graph_load_failed");
            }
            requested = self
                .submit_subscriber(engine, connection, path, subscriber, fallback)?
                .or(requested);
        }
        self.start_ready(engine, connection, Instant::now())?;
        Ok(requested)
    }

    pub(super) fn revalidate(
        &mut self,
        engine: &mut Engine,
        connection: &Connection,
    ) -> Result<Option<WorkspaceLoad>> {
        self.revalidate_at(engine, connection, Instant::now())
    }

    pub(super) fn revalidate_before_replay(
        &mut self,
        engine: &mut Engine,
        connection: &Connection,
    ) -> Result<Option<WorkspaceLoad>> {
        self.revalidate_jobs(engine, connection, Instant::now())
    }

    fn revalidate_at(
        &mut self,
        engine: &mut Engine,
        connection: &Connection,
        now: Instant,
    ) -> Result<Option<WorkspaceLoad>> {
        let requested = self.revalidate_jobs(engine, connection, now)?;
        self.start_ready(engine, connection, now)?;
        Ok(requested)
    }

    fn revalidate_jobs(
        &mut self,
        engine: &mut Engine,
        connection: &Connection,
        now: Instant,
    ) -> Result<Option<WorkspaceLoad>> {
        for pending in self
            .pending
            .extract_if(.., |pending| match &pending.waiting_for {
                PendingInput::OpenParse(identity) => {
                    engine.open_document_identity(&pending.path) != *identity
                }
                PendingInput::Workspace(key) if key.version.is_some() => {
                    engine.open_document_identity(&pending.path)
                        != key.open_generation().zip(key.version)
                }
                PendingInput::Workspace(key) => !engine
                    .diagnostic_key(&pending.path)
                    .is_ok_and(|current| key.same_document(&current)),
            })
        {
            fail_pending(
                connection,
                pending,
                ErrorCode::ContentModified,
                DIAGNOSTICS_CHANGED,
            )?;
        }
        let stale: Vec<_> = self
            .jobs
            .iter()
            .filter_map(|(key, job)| {
                let current = current_key(engine, &job.path);
                (current.as_ref() != Some(key)).then(|| (key.clone(), current))
            })
            .collect();
        let stale: Vec<_> = stale
            .into_iter()
            .map(|(key, current)| {
                let mut job = self.jobs.remove(&key).unwrap();
                job.running = None;
                (key, current, job)
            })
            .collect();
        let mut requested = None;
        for (key, current, mut job) in stale {
            if key.partial
                && let Some(current) = current
                && !current.partial
                && key.same_document(&current)
            {
                // The loader supplied context for a request against the same
                // synchronized text. The new snapshot can finish its report.
                job.discard_preview("superseded", "context_ready");
                self.resume_with_graph(engine, connection, current, job)?;
                continue;
            }
            job.discard_preview("superseded", "content_modified");
            for Subscriber { consumer, .. } in job.subscribers {
                if consumer.is_push() && engine.is_open(&job.path) {
                    let subscriber = self.subscriber(&job.path, consumer, now);
                    requested = self
                        .submit_subscriber(engine, connection, job.path.clone(), subscriber, false)?
                        .or(requested);
                } else {
                    consumer.error_for(
                        connection,
                        (ErrorCode::ContentModified, DIAGNOSTICS_CHANGED.into()),
                        &key,
                        job.last_attempt,
                    )?;
                }
            }
        }
        let ready: Vec<_> = self
            .pending
            .extract_if(.., |pending| {
                matches!(pending.waiting_for, PendingInput::OpenParse(_))
                    && !engine.pending_document_affects(&pending.path)
            })
            .collect();
        for Pending {
            path, subscriber, ..
        } in ready
        {
            requested = self
                .submit_subscriber(engine, connection, path, subscriber, false)?
                .or(requested);
        }
        let mut republish = Vec::new();
        for delivery in self.delivered.values_mut() {
            let key = &delivery.report.snapshot;
            if delivery.push_uri.is_none() && delivery.replacement.is_none() {
                continue;
            }
            let current = current_key(engine, &key.path_key);
            if let Some(replacement) = &delivery.replacement
                && current.as_ref() != Some(&replacement.key)
            {
                self.refresh.remove(&key.path_key);
                engine.release_diagnostic_replacement(&key.path_key);
                delivery.replacement = None;
            }
            if let (Some(uri), Some(current)) = (&delivery.push_uri, current)
                && current != *key
                && current.same_document(key)
                && !self
                    .jobs
                    .get(&current)
                    .is_some_and(|job| job.subscribers.iter().any(|sub| sub.consumer.is_push()))
            {
                republish.push((key.path_key.clone(), uri.clone()));
            }
        }
        for (path, uri) in republish {
            let subscriber = self.subscriber(&path, Consumer::Push(uri), now);
            requested = self
                .submit_subscriber(engine, connection, path, subscriber, false)?
                .or(requested);
        }
        Ok(requested)
    }

    fn resume_with_graph(
        &mut self,
        engine: &mut Engine,
        connection: &Connection,
        key: DiagnosticSnapshotKey,
        mut job: Job,
    ) -> Result<()> {
        if engine
            .diagnostic_report_for_key(&key)
            .is_some_and(|report| report.diagnostic_stage == DiagnosticStage::Stage2)
        {
            for Subscriber { consumer, .. } in job.subscribers {
                self.deliver_current(engine, connection, &job.path, &key, consumer, true)?;
            }
            if job.continue_pull {
                self.sequence += 1;
                self.queue_refresh(engine, &job.path, key, self.sequence);
            }
        } else if let Some(existing) = self.jobs.get_mut(&key) {
            existing.continue_pull |= job.continue_pull;
            for subscriber in job.subscribers {
                existing.subscribe(subscriber);
            }
        } else {
            if job.stage != DiagnosticStage::Stage2 {
                job.queue = QueueTimes::new(false);
            }
            job.stage = DiagnosticStage::Stage2;
            job.waiting_for_graph = false;
            resolve_queue_context(&mut job.queue);
            self.jobs.insert(key, job);
        }
        Ok(())
    }

    pub(super) fn cancel(&mut self, connection: &Connection, id: &RequestId) -> Result<()> {
        for pending in self
            .pending
            .extract_if(.., |pending| pending.subscriber.consumer.id() == Some(id))
        {
            fail_pending(connection, pending, ErrorCode::RequestCanceled, CANCELLED)?;
        }
        let mut cancelled = Vec::new();
        self.jobs.retain(|key, job| {
            cancelled.extend(
                job.subscribers
                    .extract_if(.., |sub| sub.consumer.id() == Some(id))
                    .map(|sub| (key.clone(), sub.consumer, job.last_attempt)),
            );
            let keep = job.needs_work();
            if !keep {
                job.discard_preview("discarded", "request_cancelled");
            }
            keep
        });
        for (key, consumer, attempt) in cancelled {
            consumer.error_for(
                connection,
                (ErrorCode::RequestCanceled, CANCELLED.into()),
                &key,
                attempt,
            )?;
        }
        Ok(())
    }

    pub(super) fn document_closed(&mut self, connection: &Connection, path: &Path) -> Result<()> {
        let path = path_key(path);
        for pending in self
            .pending
            .extract_if(.., |pending| path_key(&pending.path) == path)
        {
            fail_pending(
                connection,
                pending,
                ErrorCode::ContentModified,
                DIAGNOSTICS_CHANGED,
            )?;
        }
        self.delivered.remove(&path);
        self.refresh.remove(&path);
        self.edits.remove(&path);
        if self.focus.as_ref() == Some(&path) {
            self.focus = None;
        }
        for (key, mut job) in self.jobs.extract_if(|key, _| key.path_key == path) {
            job.discard_preview("superseded", "document_closed");
            fail_attempt(
                connection,
                job.subscribers,
                ErrorCode::ContentModified,
                DIAGNOSTICS_CHANGED,
                &key,
                job.last_attempt,
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
            fail_pending(connection, pending, code, message)?;
        }
        for (key, mut job) in self.jobs.drain() {
            let (delivery, reason) = error_disposition(code);
            job.discard_preview(delivery, reason);
            fail_attempt(
                connection,
                job.subscribers,
                code,
                message,
                &key,
                job.last_attempt,
            )?;
        }
        self.refresh.clear();
        for delivery in self.delivered.values_mut() {
            delivery.replacement = None;
        }
        self.edits.clear();
        Ok(())
    }
}

fn resolve_queue_context(queue: &mut Option<QueueTimes>) {
    if let Some(queue) = queue
        && queue.context_ready_at.is_none()
    {
        queue.context_ready_at = Some(Instant::now());
    }
}

fn report_fields(report: &DiagnosticReport) -> Value {
    let provenance = report.provenance;
    let mut fields = telemetry::diagnostic_report_fields(
        report,
        provenance.map(|source| source.job),
        provenance.map(|source| source.lane),
    );
    fields["run"] = json!(provenance.map(|source| source.run));
    fields["report_stage"] = json!(report.diagnostic_stage.as_str());
    if let Some(attempt) = provenance {
        fields["stage"] = json!(attempt.stage.as_str());
    }
    fields
}

fn delivery_fields(report: &DiagnosticReport, consumer: &Consumer, cache_hit: bool) -> Value {
    let mut fields = report_fields(report);
    fields["transport"] = json!(consumer.transport());
    fields["delivery"] = json!(if cache_hit { "cache_hit" } else { "delivered" });
    fields["report_kind"] = match consumer {
        Consumer::Pull { previous, .. } if previous.as_ref() == Some(&report.result_id) => {
            json!("unchanged")
        }
        Consumer::Pull { .. } | Consumer::Push(_) => json!("full"),
        Consumer::Actions { .. } => Value::Null,
    };
    fields
}

fn error_fields(key: &DiagnosticSnapshotKey, attempt: Option<DiagnosticProvenance>) -> Value {
    let mut fields = if let Some(attempt) = attempt {
        attempt.fields(key)
    } else {
        json!({
            "document": telemetry::path(&key.path_key),
            "doc_version": key.version,
            "context_completeness": if key.partial { "partial" } else { "full" },
            "job": null,
            "run": null,
            "stage": null,
            "lane": null,
        })
    };
    fields["result_id"] = Value::Null;
    fields["report_kind"] = Value::Null;
    fields["diagnostic_codes"] = json!({});
    fields
}

fn log_disposition(mut fields: Value, delivery: &'static str, reason: &'static str) {
    fields["req_id"] = Value::Null;
    fields["delivery"] = json!(delivery);
    fields["reason"] = json!(reason);
    telemetry::emit("diagnostic_delivery", fields);
}

fn log_report_disposition(report: &DiagnosticReport, delivery: &'static str, reason: &'static str) {
    if telemetry::enabled() {
        log_disposition(report_fields(report), delivery, reason);
    }
}

fn log_attempt_disposition(
    key: &DiagnosticSnapshotKey,
    attempt: Option<DiagnosticProvenance>,
    delivery: &'static str,
    reason: &'static str,
) {
    if telemetry::enabled() && attempt.is_some() {
        log_disposition(error_fields(key, attempt), delivery, reason);
    }
}

fn fail_pending(
    connection: &Connection,
    pending: Pending,
    code: ErrorCode,
    message: &str,
) -> Result<()> {
    let consumer = pending.subscriber.consumer;
    match pending.waiting_for {
        PendingInput::Workspace(key) => {
            consumer.error_for(connection, (code, message.into()), &key, None)
        }
        PendingInput::OpenParse(identity) => {
            if telemetry::enabled()
                && let Some(id) = consumer.id()
            {
                let (delivery, reason) = error_disposition(code);
                rpc_telemetry::diagnostic_response(
                    id,
                    json!({
                        "document": telemetry::path(&pending.path),
                        "doc_version": identity.map(|(_, version)| version),
                        "job": null,
                        "run": null,
                        "stage": null,
                        "context_completeness": null,
                        "lane": null,
                        "result_id": null,
                        "report_kind": null,
                        "diagnostic_codes": {},
                        "delivery": delivery,
                        "reason": reason,
                        "transport": consumer.transport(),
                    }),
                );
            }
            consumer.error(connection, (code, message.into()))
        }
    }
}

fn error_disposition(code: ErrorCode) -> (&'static str, &'static str) {
    match code {
        ErrorCode::ContentModified => ("superseded", "content_modified"),
        ErrorCode::RequestCanceled => ("discarded", "request_cancelled"),
        _ => ("discarded", "report_unavailable"),
    }
}

fn is_current(engine: &mut Engine, path: &Path, key: &DiagnosticSnapshotKey) -> bool {
    current_key(engine, path).as_ref() == Some(key)
}

fn current_key(engine: &mut Engine, path: &Path) -> Option<DiagnosticSnapshotKey> {
    (!engine.pending_document_affects(path))
        .then(|| engine.diagnostic_key(path).ok())
        .flatten()
}

fn fail(
    connection: &Connection,
    subscribers: Vec<Subscriber>,
    code: ErrorCode,
    message: &str,
) -> Result<()> {
    for Subscriber { consumer, .. } in subscribers {
        consumer.error(connection, (code, message.into()))?;
    }
    Ok(())
}

fn fail_attempt(
    connection: &Connection,
    subscribers: Vec<Subscriber>,
    code: ErrorCode,
    message: &str,
    key: &DiagnosticSnapshotKey,
    attempt: Option<DiagnosticProvenance>,
) -> Result<()> {
    for Subscriber { consumer, .. } in subscribers {
        consumer.error_for(connection, (code, message.into()), key, attempt)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use lsp_server::{Message, Response};
    use lsp_types::{Position, TextDocumentContentChangeEvent};

    use super::*;
    use crate::protocol::path_uri;
    use crate::semantic::diagnostics::{
        ContextCompleteness, DiagnosticCode, DiagnosticItem, DiagnosticSeverity,
    };
    use crate::test_support::TestProject;

    #[test]
    fn diagnostic_queue_and_cancellation_clocks_are_independent() {
        let enqueued_at = Instant::now();
        let after = |millis| enqueued_at + Duration::from_millis(millis);
        let trace = WorkTrace {
            attempt: DiagnosticProvenance {
                job: 2,
                run: 1,
                lane: "foreground",
                stage: DiagnosticStage::Stage1,
            },
            queue: QueueTimes {
                enqueued_at,
                context_ready_at: Some(after(2)),
            },
            eligible_at: after(5),
            cancel_requested_at: Arc::new(Mutex::new(Some(after(11)))),
        };
        assert_eq!(
            trace.queue.delays(trace.eligible_at, after(9)),
            [9, 2, 3, 4].map(Duration::from_millis),
        );
        assert_eq!(trace.cancellation_at(after(10)), None);
        assert_eq!(trace.cancellation_at(after(13)), Some(after(11)));
    }

    struct Fixture {
        project: TestProject,
        engine: Engine,
        server: Connection,
        client: Connection,
        diagnostics: Diagnostics,
        queues: [Receiver<Work>; Lane::ALL.len()],
        now: Instant,
    }

    impl Fixture {
        fn new(mode: Mode) -> Self {
            let project = TestProject::new();
            let engine = Engine::new(project.root.clone());
            let (server, client) = Connection::memory();
            let channels: [_; Lane::ALL.len()] = std::array::from_fn(|_| bounded(1));
            let send = channels.each_ref().map(|(send, _)| send.clone());
            let queues = channels.map(|(_, receiver)| receiver);
            let (_, receive) = bounded(1);
            let diagnostics = Diagnostics {
                mode,
                pool: Some(Pool { send, receive }),
                ..Diagnostics::default()
            };
            Self {
                project,
                engine,
                server,
                client,
                diagnostics,
                queues,
                now: Instant::now(),
            }
        }

        fn open(&mut self, name: &str, text: &str, version: i32) -> PathBuf {
            let path = self.project.write(name, text);
            self.engine
                .open(path.clone(), text.into(), version)
                .unwrap();
            path
        }

        fn install_graph(&mut self, files: &[&str]) {
            let load = self.engine.workspace_load();
            assert!(
                self.engine
                    .install_workspace(load, self.project.workspace(files, false))
            );
        }

        fn submit(&mut self, path: &Path, consumer: Consumer) -> Option<WorkspaceLoad> {
            self.diagnostics
                .submit_at(
                    &mut self.engine,
                    &self.server,
                    path.to_owned(),
                    consumer,
                    false,
                    self.now,
                )
                .unwrap()
        }

        fn work(&self, lane: Lane) -> Work {
            self.queues[lane.index()].try_recv().unwrap()
        }

        fn no_work(&self, lane: Lane) -> bool {
            self.queues[lane.index()].is_empty()
        }

        fn edit(&mut self, path: &Path, text: &str, version: i32) {
            self.diagnostics.interrupt_for_edit(path);
            self.engine
                .edit(
                    path,
                    &[TextDocumentContentChangeEvent {
                        range: None,
                        range_length: None,
                        text: text.into(),
                    }],
                    version,
                )
                .unwrap();
            self.diagnostics.note_edit(path, self.now);
            self.diagnostics
                .revalidate_at(&mut self.engine, &self.server, self.now)
                .unwrap();
        }

        fn finish(&mut self, lane: Lane, work: Work, items: Vec<DiagnosticItem>) {
            let report = DiagnosticReport::at_stage(
                work.key.clone(),
                items,
                work.stage,
                ContextCompleteness::Complete,
            );
            self.finish_work(lane, work, Some(report));
        }

        fn finish_work(&mut self, lane: Lane, work: Work, report: Option<DiagnosticReport>) {
            self.complete(Completed {
                key: work.key,
                ticket: work.ticket,
                lane,
                report: Ok(report),
                ready_at: self.now,
                attempt: None,
                published_at: None,
                stage: work.stage,
            });
        }

        fn run(&mut self, lane: Lane, work: Work) {
            let report = work.input.analyze_at_stage(work.stage, &work.cancelled);
            self.complete(Completed {
                key: work.key,
                ticket: work.ticket,
                lane,
                report,
                ready_at: self.now,
                attempt: None,
                published_at: None,
                stage: work.stage,
            });
        }

        fn complete(&mut self, completed: Completed) {
            self.diagnostics
                .apply_completion(&mut self.engine, &self.server, completed)
                .unwrap();
            self.diagnostics
                .start_ready(&mut self.engine, &self.server, self.now)
                .unwrap();
        }

        fn advance(&mut self, elapsed: Duration) {
            self.now += elapsed;
            self.diagnostics
                .advance(&mut self.engine, &self.server, self.now)
                .unwrap();
        }

        fn response(&self, id: i32) -> Response {
            let Message::Response(response) = self.client.receiver.try_recv().unwrap() else {
                panic!("expected a diagnostic response");
            };
            assert_eq!(response.id, id.into());
            response
        }

        fn push(&self) -> serde_json::Value {
            let Message::Notification(push) = self.client.receiver.try_recv().unwrap() else {
                panic!("expected a diagnostic publication");
            };
            assert_eq!(push.method, "textDocument/publishDiagnostics");
            push.params
        }
    }

    fn pull(id: i32, previous: Option<String>) -> Consumer {
        Consumer::Pull {
            id: id.into(),
            previous,
        }
    }

    fn actions(id: i32, path: &Path) -> Consumer {
        Consumer::Actions {
            id: id.into(),
            uri: path_uri(path).unwrap(),
            range: Range::default(),
            only: Some(vec![CodeActionKind::QUICKFIX]),
        }
    }

    fn unresolved(name: &str) -> DiagnosticItem {
        DiagnosticItem::new(
            DiagnosticCode::NameUnresolved,
            DiagnosticSeverity::Error,
            Range::new(Position::new(1, 8), Position::new(1, 15)),
            format!("Undefined {name}"),
        )
    }

    #[test]
    fn blocked_stage2_and_large_work_do_not_take_fast_or_reserve() {
        let mut fixture = Fixture::new(Mode::Pull { refresh: true });
        let text = "package sample\nvar _ = Missing\n";
        let large_text = format!("package sample\n//{}\n", " ".repeat(LARGE_DOCUMENT_BYTES));
        let large = fixture.open("large.go", &large_text, 1);
        let large_preview = fixture.open("large_preview.go", &large_text, 1);
        for _ in 0..2 {
            let parsed = fixture.engine.open_parse_wake().recv().unwrap();
            fixture.engine.complete_open_parse(parsed).unwrap();
        }
        let background = fixture.open("background.go", text, 1);
        let edited = fixture.open("edited.go", text, 1);
        fixture.install_graph(&["large.go", "large_preview.go", "background.go", "edited.go"]);

        assert!(fixture.submit(&large, actions(1, &large)).is_none());
        let large_work = fixture.work(Lane::Large);
        assert_eq!(large_work.stage, DiagnosticStage::Stage2);
        assert!(
            fixture
                .submit(&background, actions(2, &background))
                .is_none()
        );
        let background_work = fixture.work(Lane::Background);
        assert_eq!(background_work.stage, DiagnosticStage::Stage2);
        fixture.submit(&large_preview, pull(5, None));
        let large_fast_report = fixture.work(Lane::LargePreview);
        assert_eq!(large_fast_report.stage, DiagnosticStage::Stage1);
        assert!(fixture.no_work(Lane::Fast));

        fixture.edit(&edited, text, 2);
        fixture.submit(&edited, pull(3, None));
        assert!(fixture.no_work(Lane::Fast));
        fixture.advance(EDIT_DELAY);
        let old = fixture.work(Lane::Fast);
        assert_eq!(old.stage, DiagnosticStage::Stage1);

        fixture.edit(&edited, text, 3);
        fixture.submit(&edited, pull(4, None));
        assert_eq!(fixture.response(3).error.unwrap().code, -32801);
        assert!(old.cancelled.load(Ordering::Relaxed));
        assert!(!large_work.cancelled.load(Ordering::Relaxed));
        assert!(!background_work.cancelled.load(Ordering::Relaxed));
        assert!(fixture.no_work(Lane::Reserve));
        fixture.advance(EDIT_DELAY);
        let current = fixture.work(Lane::Reserve);
        assert_eq!(current.stage, DiagnosticStage::Stage1);
        assert_eq!(current.key.version, Some(3));
        fixture.finish(Lane::Reserve, current, vec![unresolved("Missing")]);
        fixture.advance(STAGE2_BUDGET);
        let preview = fixture.response(4).result.unwrap();
        assert_eq!(preview["items"][0]["message"], "Undefined Missing");

        let stale_key = old.key.clone();
        fixture.finish(Lane::Fast, old, vec![unresolved("Stale")]);
        assert!(
            fixture
                .engine
                .diagnostic_report_for_key(&stale_key)
                .is_none()
        );
        assert!(fixture.client.receiver.is_empty());
        fixture.advance(STAGE2_EDIT_DELAY - EDIT_DELAY - STAGE2_BUDGET);
        assert!(fixture.no_work(Lane::Background2));
        assert!(fixture.no_work(Lane::Fast));
        fixture.finish(Lane::Background, background_work, vec![]);
        assert_eq!(fixture.response(2).result.unwrap(), json!([]));
        let promoted = fixture.work(Lane::Background);
        assert_eq!(promoted.key.version, Some(3));
        assert_eq!(promoted.stage, DiagnosticStage::Stage2);
        fixture.finish(Lane::Background, promoted, vec![unresolved("Missing")]);
        assert!(fixture.diagnostics.drain_refresh().is_empty());
        assert!(
            fixture.client.receiver.is_empty(),
            "each pull receives one response"
        );

        fixture.finish(Lane::LargePreview, large_fast_report, vec![]);
        fixture.advance(STAGE2_BUDGET);
        assert_eq!(fixture.response(5).result.unwrap()["items"], json!([]));
        assert!(fixture.no_work(Lane::Background));
        fixture.finish(Lane::Large, large_work, vec![]);
        assert_eq!(fixture.response(1).result.unwrap(), json!([]));
        let large_terminal = fixture.work(Lane::Large);
        assert_eq!(large_terminal.key.path_key, path_key(&large_preview));
        assert_eq!(large_terminal.stage, DiagnosticStage::Stage2);
        fixture.finish(Lane::Large, large_terminal, vec![]);
        assert!(fixture.diagnostics.drain_refresh().is_empty());
        assert!(fixture.client.receiver.is_empty());
    }

    #[test]
    fn continuous_edits_coalesce_to_the_latest_version_with_a_bounded_wait() {
        let mut fixture = Fixture::new(Mode::Push { versioned: true });
        let text = "package sample\nvar _ = Missing\n";
        let path = fixture.open("a.go", text, 1);
        fixture.install_graph(&["a.go"]);
        let start = fixture.now;

        for version in 2..=6 {
            fixture.edit(&path, text, version);
            fixture.submit(&path, Consumer::Push(path_uri(&path).unwrap()));
            assert!(fixture.no_work(Lane::Fast));
            fixture.advance(Duration::from_millis(20));
        }
        assert_eq!(fixture.now, start + MAX_EDIT_WAIT);
        let work = fixture.work(Lane::Fast);
        assert_eq!(work.key.version, Some(6));
        fixture.finish(Lane::Fast, work, vec![unresolved("Missing")]);

        assert!(fixture.no_work(Lane::Background));
        assert!(fixture.no_work(Lane::Background2));
        assert!(fixture.client.receiver.is_empty());
        fixture.advance(STAGE2_BUDGET);
        assert_eq!(fixture.push()["version"], 6);
        let before_stage2 =
            start + Duration::from_millis(80) + STAGE2_EDIT_DELAY - Duration::from_millis(1);
        fixture.advance(before_stage2 - fixture.now);
        assert!(fixture.no_work(Lane::Background));
        fixture.advance(Duration::from_millis(1));
        let old = fixture.work(Lane::Background);
        assert_eq!(old.key.version, Some(6));
        assert_eq!(old.stage, DiagnosticStage::Stage2);

        fixture.edit(&path, text, 7);
        assert!(old.cancelled.load(Ordering::Relaxed));
        let stale_key = old.key.clone();
        let previous = fixture
            .engine
            .diagnostic_report_for_key(&stale_key)
            .cloned();
        fixture.finish(Lane::Background, old, vec![unresolved("Stale")]);
        assert_eq!(
            fixture.engine.diagnostic_report_for_key(&stale_key),
            previous.as_ref()
        );

        fixture.advance(EDIT_DELAY);
        let latest = fixture.work(Lane::Fast);
        assert_eq!(latest.key.version, Some(7));
        fixture.finish(Lane::Fast, latest, vec![unresolved("Missing")]);
        fixture.advance(STAGE2_BUDGET);
        assert_eq!(fixture.push()["version"], 7);
        fixture.advance(STAGE2_EDIT_DELAY - EDIT_DELAY - STAGE2_BUDGET);
        let terminal = fixture.work(Lane::Background);
        assert_eq!(terminal.key.version, Some(7));
        fixture.finish(Lane::Background, terminal, vec![unresolved("Missing")]);
        assert!(fixture.client.receiver.is_empty());
    }

    #[test]
    fn client_cancellation_wakes_already_queued_foreground_work() {
        let mut fixture = Fixture::new(Mode::Pull { refresh: true });
        let first = fixture.open("a.go", "package sample\n", 1);
        let next = fixture.open("b.go", "package sample\n", 1);
        fixture.install_graph(&["a.go", "b.go"]);

        fixture.submit(&first, pull(1, None));
        let blocked = fixture.work(Lane::Fast);
        fixture.submit(&next, pull(2, None));
        assert!(fixture.diagnostics.next_deadline().is_none());
        assert!(fixture.no_work(Lane::Reserve));

        fixture
            .diagnostics
            .cancel(&fixture.server, &1.into())
            .unwrap();
        assert!(blocked.cancelled.load(Ordering::Relaxed));
        assert_eq!(fixture.response(1).error.unwrap().code, -32800);
        assert!(fixture.diagnostics.next_deadline().is_some());
        fixture.advance(Duration::ZERO);
        let ready = fixture.work(Lane::Reserve);
        assert_eq!(ready.key.path_key, path_key(&next));
        fixture.finish(Lane::Reserve, ready, vec![]);
        fixture.advance(STAGE2_BUDGET);
        assert_eq!(fixture.response(2).result.unwrap()["items"], json!([]));

        fixture.finish(Lane::Fast, blocked, vec![]);
        assert!(fixture.client.receiver.is_empty());
    }

    #[test]
    fn coalesces_pull_and_actions_and_cancels_each_subscriber_once() {
        let mut fixture = Fixture::new(Mode::Pull { refresh: false });
        let path = fixture.open("a.go", "package sample\nfunc f() { unused := 1 }\n", 1);
        fixture.install_graph(&["a.go"]);
        fixture.submit(&path, pull(1, None));
        fixture.submit(
            &path,
            Consumer::Actions {
                id: 2.into(),
                uri: path_uri(&path).unwrap(),
                range: Range::new(Position::new(1, 0), Position::new(2, 0)),
                only: Some(vec![CodeActionKind::QUICKFIX]),
            },
        );
        fixture.submit(&path, pull(3, None));
        let work = fixture.work(Lane::Background);
        assert_eq!(work.stage, DiagnosticStage::Stage2);
        assert!(fixture.no_work(Lane::Fast));
        assert!(fixture.no_work(Lane::Background2));
        fixture
            .diagnostics
            .cancel(&fixture.server, &1.into())
            .unwrap();
        assert!(!work.cancelled.load(Ordering::Relaxed));
        assert_eq!(fixture.response(1).error.unwrap().code, -32800);

        fixture.run(Lane::Background, work);
        assert_eq!(
            fixture.response(2).result.unwrap()[0]["title"],
            "Replace unused variable 'unused' with '_'"
        );
        let full = fixture.response(3).result.unwrap();
        let previous = full["resultId"].as_str().unwrap().to_owned();
        fixture.submit(&path, pull(4, Some(previous)));
        assert_eq!(fixture.response(4).result.unwrap()["kind"], "unchanged");
        fixture
            .diagnostics
            .cancel(&fixture.server, &4.into())
            .unwrap();
        assert!(fixture.client.receiver.is_empty());
    }

    #[test]
    fn stage2_runs_two_jobs_and_balances_latest_subscriptions_with_background_work() {
        let mut fixture = Fixture::new(Mode::Push { versioned: false });
        let text = "package sample\n";
        let first = fixture.open("first.go", text, 1);
        let second = fixture.open("second.go", text, 1);
        let ordinary = fixture.open("ordinary.go", text, 1);
        let older = fixture.open("older.go", text, 1);
        let newer = fixture.open("newer.go", text, 1);
        fixture.install_graph(&[
            "first.go",
            "second.go",
            "ordinary.go",
            "older.go",
            "newer.go",
        ]);

        fixture.submit(&first, actions(1, &first));
        fixture.submit(&second, actions(2, &second));
        let first_work = fixture.work(Lane::Background);
        let second_work = fixture.work(Lane::Background2);
        assert_ne!(first_work.ticket, second_work.ticket);
        assert_eq!(first_work.stage, DiagnosticStage::Stage2);
        assert_eq!(second_work.stage, DiagnosticStage::Stage2);

        fixture.submit(&ordinary, Consumer::Push(path_uri(&ordinary).unwrap()));
        fixture.submit(&older, pull(3, None));
        fixture.advance(PULL_BURST);
        fixture.submit(&newer, pull(4, None));
        fixture.advance(PULL_BURST);
        fixture.submit(&older, pull(5, None));
        fixture
            .diagnostics
            .cancel(&fixture.server, &5.into())
            .unwrap();
        assert_eq!(fixture.response(5).error.unwrap().code, -32800);
        assert!(fixture.no_work(Lane::Fast));
        assert!(fixture.no_work(Lane::Reserve));
        assert!(fixture.diagnostics.next_deadline().is_none());

        fixture.finish(Lane::Background2, second_work, vec![]);
        assert_eq!(fixture.response(2).result.unwrap(), json!([]));
        let newest = fixture.work(Lane::Background2);
        assert_eq!(newest.key.path_key, path_key(&newer));

        fixture.finish(Lane::Background2, newest, vec![]);
        assert_eq!(fixture.response(4).result.unwrap()["items"], json!([]));
        let background_turn = fixture.work(Lane::Background2);
        assert_eq!(background_turn.key.path_key, path_key(&ordinary));

        fixture.finish(Lane::Background2, background_turn, vec![]);
        assert_eq!(fixture.push()["uri"], json!(path_uri(&ordinary).unwrap()));
        let queued = fixture.work(Lane::Background2);
        assert_eq!(queued.key.path_key, path_key(&older));
        fixture.finish(Lane::Background2, queued, vec![]);
        assert_eq!(fixture.response(3).result.unwrap()["items"], json!([]));

        fixture.finish(Lane::Background, first_work, vec![]);
        assert_eq!(fixture.response(1).result.unwrap(), json!([]));
        assert!(fixture.client.receiver.is_empty());
        assert!(fixture.diagnostics.slots.iter().all(Option::is_none));
    }

    #[test]
    fn resent_terminal_report_consumes_refresh_and_close_invalidates_old_generation() {
        let mut fixture = Fixture::new(Mode::Pull { refresh: true });
        let text = "package sample\nvar _ = Missing\n";
        let path = fixture.open("a.go", text, 1);
        fixture.install_graph(&["a.go"]);
        fixture.submit(&path, pull(1, None));
        let fast = fixture.work(Lane::Fast);
        fixture.finish(Lane::Fast, fast, vec![]);
        let terminal = fixture.work(Lane::Background);
        fixture.advance(STAGE2_BUDGET);
        let previous = fixture.response(1).result.unwrap()["resultId"]
            .as_str()
            .unwrap()
            .to_owned();

        fixture.advance(Duration::from_millis(1));
        fixture.finish(Lane::Background, terminal, vec![unresolved("Missing")]);
        let marks = fixture.diagnostics.drain_refresh();
        let [mark] = &marks[..] else {
            panic!("expected a single refresh mark")
        };
        assert!(
            fixture
                .diagnostics
                .refresh_is_current(&mut fixture.engine, mark)
        );
        fixture.submit(&path, pull(2, Some(previous)));
        let full = fixture.response(2).result.unwrap();
        assert_eq!(full["kind"], "full");
        assert_eq!(full["items"][0]["code"], "name.unresolved");
        assert!(
            !fixture
                .diagnostics
                .refresh_is_current(&mut fixture.engine, mark)
        );

        fixture.engine.close(&path);
        fixture
            .diagnostics
            .document_closed(&fixture.server, &path)
            .unwrap();
        let reopened = fixture.open("a.go", text, 1);
        assert!(
            !fixture
                .diagnostics
                .refresh_is_current(&mut fixture.engine, mark)
        );
        fixture.submit(
            &reopened,
            pull(3, Some(full["resultId"].as_str().unwrap().to_owned())),
        );
        let preview = fixture.work(Lane::Fast);
        fixture.finish(Lane::Fast, preview, vec![unresolved("Missing")]);
        let complete = fixture.work(Lane::Background);
        fixture.finish(Lane::Background, complete, vec![unresolved("Missing")]);
        assert_eq!(fixture.response(3).result.unwrap()["kind"], "full");
        assert!(fixture.diagnostics.drain_refresh().is_empty());
    }

    #[test]
    fn late_completion_still_delivers_preview_and_equivalent_terminal_keeps_identity() {
        let mut fixture = Fixture::new(Mode::Pull { refresh: true });
        let path = fixture.open("a.go", "package sample\nvar _ = Missing\n", 1);
        fixture.install_graph(&["a.go"]);
        fixture.submit(&path, pull(1, None));
        let fast = fixture.work(Lane::Fast);
        fixture.finish(Lane::Fast, fast, vec![unresolved("Missing")]);
        let terminal = fixture.work(Lane::Background);

        // The completion is handled first although the timer is also ready.
        fixture.now += STAGE2_BUDGET + Duration::from_millis(1);
        fixture.finish(Lane::Background, terminal, vec![unresolved("Missing")]);
        let full = fixture.response(1).result.unwrap();
        assert_eq!(full["items"][0]["code"], "name.unresolved");
        assert!(fixture.diagnostics.drain_refresh().is_empty());
        fixture.submit(
            &path,
            pull(2, Some(full["resultId"].as_str().unwrap().to_owned())),
        );
        assert_eq!(fixture.response(2).result.unwrap()["kind"], "unchanged");
        assert!(fixture.client.receiver.is_empty());
    }

    #[test]
    fn partial_pull_survives_graph_install_when_synchronized_text_is_unchanged() {
        let mut fixture = Fixture::new(Mode::Pull { refresh: true });
        let path = fixture.open("a.go", "package sample\nvar _ = Missing\n", 1);
        assert!(fixture.submit(&path, pull(1, None)).is_some());
        let partial = fixture.work(Lane::Fast);
        assert!(partial.key.partial);
        fixture.install_graph(&["a.go"]);
        fixture
            .diagnostics
            .revalidate_at(&mut fixture.engine, &fixture.server, fixture.now)
            .unwrap();
        assert!(partial.cancelled.load(Ordering::Relaxed));
        assert!(fixture.client.receiver.is_empty());
        let full = fixture.work(Lane::Background);
        assert!(!full.key.partial);
        assert_eq!(full.stage, DiagnosticStage::Stage2);
        fixture.finish(Lane::Fast, partial, vec![]);
        assert!(fixture.client.receiver.is_empty());
        fixture.finish(Lane::Background, full, vec![unresolved("Missing")]);
        assert_eq!(
            fixture.response(1).result.unwrap()["items"][0]["code"],
            "name.unresolved"
        );
        assert!(fixture.client.receiver.is_empty());
    }

    #[test]
    fn graph_replay_prioritizes_the_latest_subscribers_across_resumed_jobs() {
        let mut fixture = Fixture::new(Mode::Pull { refresh: true });
        let text = "package sample\n";
        let partial_path = fixture.open("partial.go", text, 1);
        let earlier_path = fixture.open("earlier.go", text, 1);
        let latest_path = fixture.open("latest.go", text, 1);
        assert!(fixture.submit(&partial_path, pull(1, None)).is_some());
        let partial = fixture.work(Lane::Fast);
        assert!(partial.key.partial);
        assert!(
            fixture
                .submit(&earlier_path, actions(2, &earlier_path))
                .is_some()
        );
        assert!(
            fixture
                .submit(&latest_path, actions(3, &latest_path))
                .is_some()
        );
        assert_eq!(fixture.diagnostics.pending_count(), 2);

        fixture.install_graph(&["partial.go", "earlier.go", "latest.go"]);
        fixture
            .diagnostics
            .revalidate_before_replay(&mut fixture.engine, &fixture.server)
            .unwrap();
        assert!(partial.cancelled.load(Ordering::Relaxed));
        fixture
            .diagnostics
            .replay(&mut fixture.engine, &fixture.server, false)
            .unwrap();
        assert!(fixture.no_work(Lane::Reserve));
        let latest = fixture.work(Lane::Background);
        let earlier = fixture.work(Lane::Background2);
        assert_eq!(latest.key.path_key, path_key(&latest_path));
        assert_eq!(earlier.key.path_key, path_key(&earlier_path));

        fixture.finish(Lane::Fast, partial, vec![]);
        assert!(fixture.client.receiver.is_empty());
        fixture.finish(Lane::Background, latest, vec![]);
        assert_eq!(fixture.response(3).result.unwrap(), json!([]));
        let promoted = fixture.work(Lane::Background);
        assert_eq!(promoted.key.path_key, path_key(&partial_path));
        assert!(!promoted.key.partial);
        assert_eq!(promoted.stage, DiagnosticStage::Stage2);
        fixture.finish(Lane::Background, promoted, vec![]);
        assert_eq!(fixture.response(1).result.unwrap()["items"], json!([]));
        fixture.finish(Lane::Background2, earlier, vec![]);
        assert_eq!(fixture.response(2).result.unwrap(), json!([]));
        assert!(fixture.client.receiver.is_empty());
    }

    #[test]
    fn unversioned_push_publishes_cold_syntax_and_deduplicates_equivalent_graph_promotion() {
        let mut fixture = Fixture::new(Mode::Push { versioned: false });
        let path = fixture.open("a.go", "package sample\n", 1);
        assert!(
            fixture
                .submit(&path, Consumer::Push(path_uri(&path).unwrap()))
                .is_some()
        );
        let cold = fixture.work(Lane::Fast);
        assert_eq!(cold.stage, DiagnosticStage::Stage1);
        fixture.run(Lane::Fast, cold);
        fixture.advance(STAGE2_BUDGET);
        assert_eq!(fixture.push()["diagnostics"], serde_json::json!([]));

        fixture.install_graph(&["a.go"]);
        fixture
            .diagnostics
            .revalidate_at(&mut fixture.engine, &fixture.server, fixture.now)
            .unwrap();
        let warm = fixture.work(Lane::Background);
        assert_eq!(warm.stage, DiagnosticStage::Stage2);
        assert!(!warm.key.partial);
        fixture.run(Lane::Background, warm);
        assert!(fixture.client.receiver.is_empty());
        let delivery = &fixture.diagnostics.delivered[&path_key(&path)];
        assert_eq!(delivery.report.diagnostic_stage, DiagnosticStage::Stage2);
    }

    #[test]
    fn cold_sibling_withholds_unverified_preview_but_terminal_uses_stage2_pool() {
        let mut fixture = Fixture::new(Mode::Pull { refresh: true });
        let path = fixture.open("a.go", "package sample\nvar _ = Missing\n", 1);
        fixture
            .project
            .write("b.go", "package sample\nvar Declared = 1\n");
        fixture.install_graph(&["a.go", "b.go"]);
        fixture.submit(&path, pull(1, None));
        let fast = fixture.work(Lane::Fast);
        fixture.run(Lane::Fast, fast);
        fixture.advance(STAGE2_BUDGET);
        assert!(fixture.client.receiver.is_empty());
        assert!(fixture.no_work(Lane::Fast));
        let terminal = fixture.work(Lane::Background);
        assert_eq!(terminal.stage, DiagnosticStage::Stage2);
        fixture.run(Lane::Background, terminal);
        assert_eq!(
            fixture.response(1).result.unwrap()["items"][0]["code"],
            "name.unresolved"
        );
    }

    #[test]
    fn pending_refresh_keeps_terminal_report_through_report_cache_eviction() {
        let mut fixture = Fixture::new(Mode::Pull { refresh: true });
        let path = fixture.open("a.go", "package sample\nvar _ = Missing\n", 1);
        fixture.install_graph(&["a.go"]);
        fixture.submit(&path, pull(1, None));
        let fast = fixture.work(Lane::Fast);
        let key = fast.key.clone();
        fixture.finish(Lane::Fast, fast, vec![]);
        let terminal = fixture.work(Lane::Background);
        fixture.advance(STAGE2_BUDGET);
        let preview = fixture.response(1).result.unwrap()["resultId"]
            .as_str()
            .unwrap()
            .to_owned();
        fixture.finish(Lane::Background, terminal, vec![unresolved("Missing")]);
        let mark = fixture.diagnostics.drain_refresh().pop().unwrap();

        for index in 0..300 {
            let mut unrelated = key.clone();
            unrelated.path_key = path.with_file_name(format!("unrelated{index}.go"));
            fixture
                .engine
                .install_diagnostic_report(DiagnosticReport::new(unrelated, vec![], false));
        }
        assert!(
            fixture
                .diagnostics
                .refresh_is_current(&mut fixture.engine, &mark)
        );
        fixture.submit(&path, pull(2, Some(preview)));
        let result = fixture.response(2).result.unwrap();
        assert_eq!(result["kind"], "full");
        assert_eq!(result["items"][0]["code"], "name.unresolved");
        assert!(
            !fixture
                .diagnostics
                .refresh_is_current(&mut fixture.engine, &mark)
        );
    }

    #[test]
    fn analysis_panic_returns_the_slot_and_the_next_request_completes() {
        let mut fixture = Fixture::new(Mode::Pull { refresh: false });
        let path = fixture.open("a.go", "package sample\n", 1);
        fixture.install_graph(&["a.go"]);
        fixture.submit(&path, pull(1, None));
        let work = fixture.work(Lane::Background);
        fixture.complete(Completed {
            key: work.key,
            ticket: work.ticket,
            lane: Lane::Background,
            report: run_analysis(|| panic!("injected analysis panic")),
            ready_at: fixture.now,
            attempt: None,
            published_at: None,
            stage: work.stage,
        });
        assert_eq!(
            fixture.response(1).error.unwrap().code,
            ErrorCode::InternalError as i32
        );
        fixture.submit(&path, pull(2, None));
        let next = fixture.work(Lane::Background);
        fixture.run(Lane::Background, next);
        assert_eq!(
            fixture.response(2).result.unwrap()["items"],
            serde_json::json!([])
        );
    }

    #[test]
    fn two_cancelled_stage2_loads_release_their_own_slots() {
        use crate::semantic::CacheEvent;

        let mut fixture = Fixture::new(Mode::Pull { refresh: false });
        fixture.diagnostics.pool = None;
        let active = fixture.open("active.go", "package sample\nvar _ = Missing\n", 3);
        let first = fixture.project.write("first.go", "package sample\n");
        let second = fixture.project.write("second.go", "package sample\n");
        fixture.install_graph(&["active.go"]);
        let first_key = path_key(&first);
        let second_key = path_key(&second);
        let (started, starts) = bounded(2);
        let (release, blocked) = bounded::<()>(0);
        fixture
            .engine
            .set_shared_disk_hook(Some(Arc::new(move |path, event| {
                let key = path_key(path);
                if event == CacheEvent::Read && (key == first_key || key == second_key) {
                    started.send(key).unwrap();
                    let _ = blocked.recv();
                }
            })));

        for (id, path, lane) in [(1, first, Lane::Background), (2, second, Lane::Background2)] {
            fixture.now += PULL_BURST;
            fixture.submit(&path, pull(id, None));
            assert_eq!(
                starts.recv_timeout(Duration::from_secs(3)).unwrap(),
                path_key(&path)
            );
            assert!(fixture.diagnostics.slots[lane.index()].is_some());
            fixture
                .diagnostics
                .cancel(&fixture.server, &id.into())
                .unwrap();
            assert_eq!(fixture.response(id).error.unwrap().code, -32800);
        }

        fixture.now += PULL_BURST;
        fixture.submit(&active, pull(3, None));
        assert!(fixture.client.receiver.is_empty());
        let completions = fixture.diagnostics.receiver().unwrap().clone();
        while fixture.client.receiver.is_empty() {
            fixture.complete(completions.recv_timeout(Duration::from_secs(3)).unwrap());
        }
        let report = fixture.response(3).result.unwrap();
        assert_eq!(report["items"][0]["code"], "name.unresolved");
        drop(release);
    }

    #[test]
    fn suppressed_preview_keeps_new_pulls_waiting_for_one_complete_report() {
        let mut fixture = Fixture::new(Mode::Pull { refresh: true });
        let path = fixture.open("a.go", "package sample\nvar _ = Missing\n", 1);
        fixture.install_graph(&["a.go"]);
        fixture.submit(&path, pull(1, None));
        let preview = fixture.work(Lane::Fast);
        fixture.finish_work(Lane::Fast, preview, None);

        let complete = fixture.work(Lane::Background);
        assert_eq!(complete.stage, DiagnosticStage::Stage2);
        fixture.submit(&path, pull(2, None));
        fixture.advance(STAGE2_BUDGET);
        assert!(fixture.no_work(Lane::Fast));
        assert!(fixture.client.receiver.is_empty());
        fixture.finish(Lane::Background, complete, vec![unresolved("Missing")]);
        assert_eq!(fixture.response(1).result, fixture.response(2).result);
        assert!(fixture.diagnostics.drain_refresh().is_empty());
        assert!(fixture.client.receiver.is_empty());
    }

    #[test]
    fn dependency_change_invalidates_a_push_for_the_same_document_version() {
        let mut fixture = Fixture::new(Mode::Push { versioned: false });
        let path = fixture.open("a.go", "package sample\nvar _ = Name\n", 1);
        let dependency = fixture.open("b.go", "package sample\nvar Name int\n", 1);
        fixture.install_graph(&["a.go", "b.go"]);
        fixture.submit(&path, Consumer::Push(path_uri(&path).unwrap()));
        let old = fixture.work(Lane::Background);
        let old_key = old.key.clone();

        fixture.edit(&dependency, "package sample\nvar Other int\n", 2);
        assert!(old.cancelled.load(Ordering::Relaxed));
        let current = fixture.work(Lane::Background2);
        fixture.finish(Lane::Background, old, vec![]);
        assert!(fixture.engine.diagnostic_report_for_key(&old_key).is_none());
        assert!(fixture.client.receiver.is_empty());
        assert!(old_key.same_document(&current.key));
        assert_ne!(old_key, current.key);
        fixture.finish(Lane::Background2, current, vec![unresolved("Name")]);
        let published = fixture.push();
        assert_eq!(published["version"], 1);
        assert_eq!(published["diagnostics"][0]["message"], "Undefined Name");
        fixture.submit(&path, Consumer::Push(path_uri(&path).unwrap()));
        assert!(fixture.client.receiver.is_empty());
    }

    #[test]
    fn pending_pull_only_fails_when_its_document_revision_changes() {
        let mut fixture = Fixture::new(Mode::Pull { refresh: false });
        let path = fixture.open("a.go", "package sample\n", 1);
        assert!(fixture.submit(&path, pull(1, None)).is_some());
        assert_eq!(fixture.diagnostics.pending_count(), 1);
        fixture
            .diagnostics
            .revalidate_at(&mut fixture.engine, &fixture.server, fixture.now)
            .unwrap();
        assert_eq!(fixture.diagnostics.pending_count(), 1);
        assert!(fixture.client.receiver.is_empty());

        fixture.edit(&path, "package sample\nvar Changed int\n", 2);
        assert_eq!(fixture.response(1).error.unwrap().code, -32801);
        assert_eq!(fixture.diagnostics.pending_count(), 0);
        fixture
            .diagnostics
            .cancel(&fixture.server, &1.into())
            .unwrap();
        assert!(fixture.client.receiver.is_empty());
    }
}
