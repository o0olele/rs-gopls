use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{self, Write};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use lsp_server::{Connection, Message, Request, RequestId, Response};
use lsp_types::{PublishDiagnosticsParams, notification::PublishDiagnostics};
use lsp_types::{notification::Notification as _, request::Request as _};
use serde_json::{Value, json};

use crate::{semantic::NavigationTrace, telemetry};

static RECEIVED: LazyLock<Mutex<HashMap<RequestId, Instant>>> = LazyLock::new(Mutex::default);
static OUTGOING: LazyLock<Mutex<HashMap<RequestId, Outgoing>>> = LazyLock::new(Mutex::default);
// The stdio writer receives these notifications in the same order as their metadata.
static DIAGNOSTIC_PUSHES: Mutex<VecDeque<Value>> = Mutex::new(VecDeque::new());
static DIAGNOSTIC_REFRESHES: LazyLock<Mutex<HashMap<RequestId, Value>>> =
    LazyLock::new(Mutex::default);

thread_local! {
    static REQUESTS: RefCell<Requests> = RefCell::default();
}

#[derive(Default)]
struct Requests {
    active: Option<Active>,
    entries: HashMap<RequestId, RequestTrace>,
}

struct Active {
    id: RequestId,
    start: Instant,
}

struct RequestTrace {
    received: Instant,
    method: &'static str,
    fields: Value,
    context: telemetry::Context,
    active_ms: f64,
    serialize_ms: f64,
    wait_reason: Option<&'static str>,
    replay_count: u32,
    waiting: Option<(Wait, Instant)>,
    workspace_wait: Duration,
    shared_source_wait: Duration,
    shared_source_replays: u32,
    shared_disk: telemetry::SharedDiskStats,
    phases: Phases,
    navigation: Option<NavigationTrace>,
}

#[derive(Clone, Copy)]
enum Wait {
    Workspace,
    SharedSource,
}

pub(super) fn request_id(id: &RequestId) -> Value {
    telemetry::request_id(id)
}

pub(super) fn received(message: &Message) {
    if let Some(start) = telemetry::now()
        && let Message::Request(request) = message
    {
        RECEIVED.lock().unwrap().insert(request.id.clone(), start);
    }
}

pub(super) fn begin(request: &Request, method: &'static str) -> Step {
    let now = Instant::now();
    let received = RECEIVED.lock().unwrap().remove(&request.id).unwrap_or(now);
    let context = telemetry::Context::request(&request.id);
    let document = request
        .params
        .pointer("/textDocument/uri")
        .or_else(|| request.params.pointer("/data/uri"))
        .and_then(Value::as_str)
        .and_then(document_path);
    let fields = json!({
        "req_id": context.req_id(),
        "method": method,
        "document": document,
        "line": request.params.pointer("/position/line").and_then(Value::as_u64),
        "char": request.params.pointer("/position/character").and_then(Value::as_u64),
        "dispatch_queue_ms": telemetry::millis(now.saturating_duration_since(received)),
    });
    telemetry::emit("rpc_recv", fields.clone());
    REQUESTS.with_borrow_mut(|requests| {
        requests.entries.insert(
            request.id.clone(),
            RequestTrace {
                received,
                method,
                fields,
                context,
                active_ms: 0.0,
                serialize_ms: 0.0,
                wait_reason: None,
                replay_count: 0,
                waiting: None,
                workspace_wait: Duration::ZERO,
                shared_source_wait: Duration::ZERO,
                shared_source_replays: 0,
                shared_disk: telemetry::SharedDiskStats::default(),
                phases: Phases::default(),
                navigation: NavigationTrace::new(request, received, document),
            },
        );
    });
    Step::enter(&request.id, now)
}

fn document_path(uri: &str) -> Option<String> {
    let path = url::Url::parse(uri).ok()?.to_file_path().ok()?;
    Some(telemetry::path(&path))
}

pub(super) struct Step {
    previous: Option<Active>,
    _context: Option<telemetry::ContextGuard>,
}

impl Step {
    fn enter(id: &RequestId, start: Instant) -> Self {
        let active = Active {
            id: id.clone(),
            start,
        };
        let (previous, context) = REQUESTS.with_borrow_mut(|requests| {
            let previous = requests.active.replace(active);
            if let Some(previous) = &previous {
                requests.account_step(previous, start);
            }
            let context = requests.entries.get_mut(id).map(|trace| {
                trace.finish_wait(start, true);
                trace.context
            });
            (previous, context)
        });
        Self {
            previous,
            _context: telemetry::enter_context(context),
        }
    }
}

impl Drop for Step {
    fn drop(&mut self) {
        REQUESTS.with_borrow_mut(|requests| {
            let now = Instant::now();
            let previous = self.previous.take().map(|mut previous| {
                previous.start = now;
                previous
            });
            if let Some(active) = std::mem::replace(&mut requests.active, previous) {
                requests.account_step(&active, now);
            }
        });
    }
}

impl Requests {
    fn account_step(&mut self, active: &Active, end: Instant) {
        let shared_disk = telemetry::take_shared_disk_stats();
        if let Some(trace) = self.entries.get_mut(&active.id) {
            trace.active_ms += telemetry::millis(end.saturating_duration_since(active.start));
            trace.shared_disk.merge(shared_disk);
        } else {
            telemetry::merge_shared_disk_stats(shared_disk);
        }
    }

    fn take_trace(&mut self, id: &RequestId, end: Instant) -> Option<RequestTrace> {
        let mut trace = self.entries.remove(id)?;
        trace.finish_wait(end, false);
        if let Some(active) = &self.active
            && &active.id == id
        {
            trace.active_ms += telemetry::millis(end.saturating_duration_since(active.start));
            trace.shared_disk.merge(telemetry::take_shared_disk_stats());
        }
        Some(trace)
    }
}

impl RequestTrace {
    fn finish_wait(&mut self, end: Instant, resumed: bool) {
        if let Some((kind, start)) = self.waiting.take() {
            let elapsed = end.saturating_duration_since(start);
            match kind {
                Wait::Workspace => self.workspace_wait += elapsed,
                Wait::SharedSource => {
                    self.shared_source_wait += elapsed;
                    self.shared_source_replays += u32::from(resumed);
                }
            }
        }
    }
}

#[derive(Default)]
struct Phases {
    active: Option<(&'static str, Instant)>,
    elapsed: BTreeMap<&'static str, f64>,
}

impl Phases {
    fn account(&mut self, end: Instant) -> Option<&'static str> {
        let (name, start) = self.active.take()?;
        *self.elapsed.entry(name).or_default() +=
            telemetry::millis(end.saturating_duration_since(start));
        Some(name)
    }
}

struct Phase {
    id: RequestId,
    previous: Option<&'static str>,
}

impl Drop for Phase {
    fn drop(&mut self) {
        REQUESTS.with_borrow_mut(|requests| {
            if let Some(trace) = requests.entries.get_mut(&self.id) {
                let now = Instant::now();
                trace.phases.account(now);
                trace.phases.active = self.previous.map(|name| (name, now));
            }
        });
    }
}

pub(crate) fn phase(name: &'static str) -> Option<impl Drop> {
    if !telemetry::enabled() {
        return None;
    }
    REQUESTS.with_borrow_mut(|requests| {
        let id = &requests.active.as_ref()?.id;
        let trace = requests.entries.get_mut(id)?;
        let now = Instant::now();
        let previous = trace.phases.account(now);
        trace.phases.active = Some((name, now));
        Some(Phase {
            id: id.clone(),
            previous,
        })
    })
}

pub(super) fn step(id: &RequestId, replay: bool) -> Option<Step> {
    if !telemetry::enabled() {
        return None;
    }
    if replay {
        REQUESTS.with_borrow_mut(|requests| {
            if let Some(trace) = requests.entries.get_mut(id) {
                trace.replay_count += 1;
            }
        });
    }
    Some(Step::enter(id, Instant::now()))
}

pub(super) fn navigation_step() -> Option<impl Drop> {
    if !telemetry::enabled() {
        return None;
    }
    REQUESTS.with_borrow(|requests| {
        let active = requests.active.as_ref()?;
        let trace = requests.entries.get(&active.id)?;
        trace.navigation.as_ref().map(NavigationTrace::step)
    })
}

pub(super) fn wait(reason: impl FnOnce() -> &'static str) -> &'static str {
    note_wait(Some(reason), None)
}

pub(super) fn wait_workspace(reason: impl FnOnce() -> &'static str) -> &'static str {
    note_wait(Some(reason), Some(Wait::Workspace))
}

pub(super) fn wait_shared_source() {
    note_wait(None::<fn() -> &'static str>, Some(Wait::SharedSource));
}

fn note_wait(reason: Option<impl FnOnce() -> &'static str>, kind: Option<Wait>) -> &'static str {
    if !telemetry::enabled() {
        return "request";
    }
    REQUESTS.with_borrow_mut(|requests| {
        if let Some(active) = &requests.active
            && let Some(trace) = requests.entries.get_mut(&active.id)
        {
            if let Some(reason) = reason {
                trace.wait_reason = Some(reason());
            }
            if let Some(kind) = kind {
                let now = Instant::now();
                trace.finish_wait(now, false);
                trace.waiting = Some((kind, now));
            }
            return trace.method;
        }
        "request"
    })
}

pub(super) fn cancelled(id: &RequestId) {
    if !telemetry::enabled() {
        return;
    }
    let fields =
        REQUESTS.with_borrow(|requests| requests.entries.get(id).map(|trace| trace.fields.clone()));
    telemetry::emit(
        "rpc_cancel",
        fields.unwrap_or_else(|| json!({ "req_id": request_id(id) })),
    );
}

pub(crate) fn diagnostic_response(id: &RequestId, fields: Value) {
    REQUESTS.with_borrow_mut(|requests| {
        if let Some(trace) = requests.entries.get_mut(id)
            && let Value::Object(fields) = fields
        {
            trace.fields.as_object_mut().unwrap().extend(fields);
        }
    });
}

pub(super) fn diagnostic_request_version(id: &RequestId, version: Option<i32>) {
    diagnostic_response(id, json!({ "doc_version": version }));
}

pub(crate) fn diagnostic_push(
    connection: &Connection,
    params: PublishDiagnosticsParams,
    fields: Option<Value>,
) -> Result<()> {
    let tracked = fields.is_some();
    if let Some(mut fields) = fields {
        fields["push_version"] = json!(params.version);
        fields["req_id"] = Value::Null;
        DIAGNOSTIC_PUSHES.lock().unwrap().push_back(fields);
    }
    let result = crate::protocol::send_notification::<PublishDiagnostics>(connection, params);
    if result.is_err() && tracked {
        DIAGNOSTIC_PUSHES.lock().unwrap().pop_back();
    }
    result
}

pub(super) fn diagnostic_refresh(
    connection: &Connection,
    request: Request,
    fields: Option<Value>,
) -> Result<()> {
    let tracked_id = fields.map(|fields| {
        let id = request.id.clone();
        let write_fields = json!({
            "req_id": fields["req_id"],
            "refresh_id": fields["refresh_id"],
        });
        DIAGNOSTIC_REFRESHES
            .lock()
            .unwrap()
            .insert(id.clone(), write_fields);
        telemetry::emit("diagnostic_refresh_send", fields);
        id
    });
    let result = connection.sender.send(request.into());
    if result.is_err()
        && let Some(id) = tracked_id
    {
        DIAGNOSTIC_REFRESHES.lock().unwrap().remove(&id);
    }
    result.context("requesting a diagnostic refresh")
}

pub(super) fn serialize<T>(encode: impl FnOnce() -> T) -> T {
    let start = telemetry::now();
    let result = encode();
    if let Some(start) = start {
        REQUESTS.with_borrow_mut(|requests| {
            if let Some(active) = &requests.active
                && let Some(trace) = requests.entries.get_mut(&active.id)
            {
                trace.serialize_ms += telemetry::elapsed(start);
            }
        });
    }
    result
}

struct Outgoing {
    received: Instant,
    fields: Value,
    exec_ms: f64,
    serialize_ms: f64,
}

pub(super) fn responding(response: &Response) {
    if !telemetry::enabled() {
        return;
    }
    let Some(mut trace) =
        REQUESTS.with_borrow_mut(|requests| requests.take_trace(&response.id, Instant::now()))
    else {
        return;
    };
    let (outcome, result_count) = match (&response.error, &response.result) {
        (Some(error), _) => (
            match error.code {
                -32800 => "cancelled_-32800".into(),
                -32801 => "content_modified_-32801".into(),
                code => format!("error_{code}"),
            },
            0,
        ),
        (None, None | Some(Value::Null)) => ("null".into(), 0),
        (None, Some(Value::Array(results))) => ("ok".into(), results.len()),
        (None, Some(_)) => ("ok".into(), 1),
    };
    if let Some(navigation) = trace.navigation {
        navigation.finish(&outcome, result_count);
    }
    if let Some(error) = &response.error {
        discard_failed_delivery(&mut trace.fields);
        if trace.method == "textDocument/diagnostic" && trace.fields.get("delivery").is_none() {
            let fields = trace.fields.as_object_mut().unwrap();
            for key in [
                "job",
                "run",
                "doc_version",
                "stage",
                "context_completeness",
                "lane",
                "result_id",
                "report_kind",
            ] {
                fields.entry(key).or_insert(Value::Null);
            }
            fields.insert("diagnostic_codes".into(), json!({}));
            fields.insert(
                "delivery".into(),
                json!(if error.code == -32801 {
                    "superseded"
                } else {
                    "discarded"
                }),
            );
        }
    }
    trace.fields["outcome"] = json!(outcome);
    trace.fields["result_count"] = json!(result_count);
    trace.fields["waited_workspace"] = json!(trace.wait_reason.is_some());
    trace.fields["wait_reason"] = json!(trace.wait_reason);
    trace.fields["replay_count"] = json!(trace.replay_count);
    trace.fields["workspace_wait_ms"] = json!(telemetry::millis(trace.workspace_wait));
    trace.fields["shared_source_wait_ms"] = json!(telemetry::millis(trace.shared_source_wait));
    trace.fields["shared_source_replay_count"] = json!(trace.shared_source_replays);
    trace.shared_disk.add_fields(&mut trace.fields);
    if !trace.phases.elapsed.is_empty() {
        trace.fields["phases_ms"] = json!(trace.phases.elapsed);
    }
    let outgoing = Outgoing {
        received: trace.received,
        fields: trace.fields,
        exec_ms: (trace.active_ms - trace.serialize_ms).max(0.0),
        serialize_ms: trace.serialize_ms,
    };
    OUTGOING
        .lock()
        .unwrap()
        .insert(response.id.clone(), outgoing);
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn nested_phases_charge_only_active_time_across_replays() {
        let time = Instant::now();
        let at = |ms| time + Duration::from_millis(ms);
        let mut phases = Phases {
            active: Some(("symbol", at(0))),
            ..Phases::default()
        };
        let outer = phases.account(at(2)).unwrap();
        phases.active = Some(("package_doc", at(2)));
        phases.account(at(5));
        phases.active = Some((outer, at(5)));
        phases.account(at(7));
        phases.active = Some(("symbol", at(20)));
        phases.account(at(21));
        assert_eq!(phases.elapsed["symbol"], 5.0);
        assert_eq!(phases.elapsed["package_doc"], 3.0);
    }

    #[test]
    fn nested_diagnostic_delivery_retains_dispatch_time() {
        let id = RequestId::from(1);
        let received = Instant::now();
        REQUESTS.with_borrow_mut(|requests| {
            requests.entries.insert(
                id.clone(),
                RequestTrace {
                    received,
                    method: "textDocument/diagnostic",
                    fields: Value::Null,
                    context: telemetry::Context::request(&id),
                    active_ms: 0.0,
                    serialize_ms: 0.0,
                    wait_reason: None,
                    replay_count: 0,
                    waiting: None,
                    workspace_wait: Duration::ZERO,
                    shared_source_wait: Duration::ZERO,
                    shared_source_replays: 0,
                    shared_disk: telemetry::SharedDiskStats::default(),
                    phases: Phases::default(),
                    navigation: None,
                },
            );
        });
        let dispatch = received + Duration::from_millis(5);
        let delivery = dispatch + Duration::from_millis(20);
        let response = delivery + Duration::from_millis(3);
        let _dispatch = Step::enter(&id, dispatch);
        let _delivery = Step::enter(&id, delivery);
        let trace =
            REQUESTS.with_borrow_mut(|requests| requests.take_trace(&id, response).unwrap());
        assert_eq!(trace.active_ms, 23.0);
        assert_eq!(
            telemetry::millis(response - trace.received) - trace.active_ms,
            5.0
        );
    }
}

pub(super) fn write_message(message: &Message, output: &mut impl Write) -> io::Result<()> {
    if !telemetry::enabled() {
        return message.write(output);
    }
    let outgoing = match message {
        Message::Response(response) => OUTGOING.lock().unwrap().remove(&response.id),
        _ => None,
    };
    let diagnostic = match message {
        Message::Notification(notification)
            if notification.method == PublishDiagnostics::METHOD =>
        {
            DIAGNOSTIC_PUSHES
                .lock()
                .unwrap()
                .pop_front()
                .map(|fields| ("diagnostic_push_write", fields))
        }
        Message::Request(request)
            if request.method == lsp_types::request::WorkspaceDiagnosticRefresh::METHOD =>
        {
            DIAGNOSTIC_REFRESHES
                .lock()
                .unwrap()
                .remove(&request.id)
                .map(|fields| ("diagnostic_refresh_write", fields))
        }
        _ => None,
    };
    let start = Instant::now();
    let mut stdout = TimedWrite {
        output,
        first_write: None,
        operation: None,
    };
    let result = message.write(&mut stdout);
    let end = Instant::now();
    drop(stdout.operation);
    if let Some(mut info) = outgoing {
        let first_write = stdout.first_write.unwrap_or(end);
        let serialize_ms = info.serialize_ms + telemetry::millis(first_write - start);
        let write_ms = telemetry::millis(end - first_write);
        let total_ms = telemetry::millis(end - info.received);
        info.fields["exec_ms"] = json!(info.exec_ms);
        info.fields["serialize_ms"] = json!(serialize_ms);
        info.fields["write_ms"] = json!(write_ms);
        info.fields["total_ms"] = json!(total_ms);
        info.fields["queue_wait_ms"] =
            json!((total_ms - info.exec_ms - serialize_ms - write_ms).max(0.0));
        info.fields["write_ok"] = json!(result.is_ok());
        if info.fields.get("delivery").is_some() {
            info.fields["write_finished_ms"] = json!(telemetry::timestamp(end));
            if result.is_err() {
                discard_failed_delivery(&mut info.fields);
            }
            telemetry::emit("diagnostic_delivery", info.fields.clone());
        }
        telemetry::emit("rpc_done", info.fields);
    }
    if let Some((event, mut fields)) = diagnostic {
        fields["write_ok"] = json!(result.is_ok());
        fields["write_ms"] = json!(telemetry::millis(end - stdout.first_write.unwrap_or(end)));
        fields["write_finished_ms"] = json!(telemetry::timestamp(end));
        if event == "diagnostic_push_write" {
            if result.is_err() {
                discard_failed_delivery(&mut fields);
            }
            telemetry::emit("diagnostic_delivery", fields.clone());
        }
        telemetry::emit(event, fields);
    }
    result
}

fn discard_failed_delivery(fields: &mut Value) {
    if matches!(
        fields.get("delivery").and_then(Value::as_str),
        Some("delivered" | "cache_hit")
    ) {
        fields["delivery"] = json!("discarded");
    }
}

struct TimedWrite<'a, W> {
    output: &'a mut W,
    first_write: Option<Instant>,
    operation: Option<telemetry::LoopOp>,
}

impl<W: Write> Write for TimedWrite<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.first_write.is_none() {
            self.first_write = Some(Instant::now());
            self.operation = telemetry::loop_start("stdout_write", 10.0);
        }
        self.output.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}
