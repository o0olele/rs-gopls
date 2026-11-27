use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

use lsp_server::{Message, Request, RequestId, Response};
use serde_json::{Value, json};

use crate::{semantic::NavigationTrace, telemetry};

static RECEIVED: LazyLock<Mutex<HashMap<RequestId, Instant>>> = LazyLock::new(Mutex::default);
static OUTGOING: LazyLock<Mutex<HashMap<RequestId, Outgoing>>> = LazyLock::new(Mutex::default);

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
    active_ms: f64,
    serialize_ms: f64,
    wait_reason: Option<&'static str>,
    replay_count: u32,
    navigation: Option<NavigationTrace>,
}

pub(super) fn request_id(id: &RequestId) -> Value {
    match json!(id) {
        Value::String(id) => json!(telemetry::hash(&id)),
        id => id,
    }
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
    let fields = json!({
        "req_id": request_id(&request.id),
        "method": method,
        "document": request.params.pointer("/textDocument/uri")
            .and_then(Value::as_str).and_then(document_path),
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
                active_ms: 0.0,
                serialize_ms: 0.0,
                wait_reason: None,
                replay_count: 0,
                navigation: NavigationTrace::new(request, received),
            },
        );
    });
    Step::enter(&request.id)
}

fn document_path(uri: &str) -> Option<String> {
    let path = url::Url::parse(uri).ok()?.to_file_path().ok()?;
    Some(telemetry::path(&path))
}

pub(super) struct Step {
    previous: Option<Active>,
}

impl Step {
    fn enter(id: &RequestId) -> Self {
        let active = Active {
            id: id.clone(),
            start: Instant::now(),
        };
        let previous = REQUESTS.with_borrow_mut(|requests| requests.active.replace(active));
        Self { previous }
    }
}

impl Drop for Step {
    fn drop(&mut self) {
        REQUESTS.with_borrow_mut(|requests| {
            if let Some(active) = std::mem::replace(&mut requests.active, self.previous.take())
                && let Some(trace) = requests.entries.get_mut(&active.id)
            {
                trace.active_ms += telemetry::elapsed(active.start);
            }
        });
    }
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
    Some(Step::enter(id))
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
    if !telemetry::enabled() {
        return "request";
    }
    REQUESTS.with_borrow_mut(|requests| {
        if let Some(active) = &requests.active
            && let Some(trace) = requests.entries.get_mut(&active.id)
        {
            trace.wait_reason = Some(reason());
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
    let Some(mut trace) = REQUESTS.with_borrow_mut(|requests| {
        let mut trace = requests.entries.remove(&response.id)?;
        if let Some(active) = &requests.active
            && active.id == response.id
        {
            trace.active_ms += telemetry::elapsed(active.start);
        }
        Some(trace)
    }) else {
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
    trace.fields["outcome"] = json!(outcome);
    trace.fields["result_count"] = json!(result_count);
    trace.fields["waited_workspace"] = json!(trace.wait_reason.is_some());
    trace.fields["wait_reason"] = json!(trace.wait_reason);
    trace.fields["replay_count"] = json!(trace.replay_count);
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

pub(super) fn write_message(message: &Message, output: &mut impl Write) -> io::Result<()> {
    if !telemetry::enabled() {
        return message.write(output);
    }
    let outgoing = match message {
        Message::Response(response) => OUTGOING.lock().unwrap().remove(&response.id),
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
        telemetry::emit("rpc_done", info.fields);
    }
    result
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
