use std::fs::OpenOptions;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, select_biased};
use serde_json::{Value, json};

use crate::go_environment::{EnvironmentOverrides, GoEnvironment};

#[cfg(test)]
mod tests;

pub(crate) const FILE_ENV: &str = "RS_GOPLS_LOG_FILE";
pub(crate) const REDACT_ENV: &str = "RS_GOPLS_LOG_REDACT";
const CAPACITY: usize = 4096;
const FLUSH_INTERVAL: Duration = Duration::from_millis(50);
static ENABLED: AtomicBool = AtomicBool::new(false);
static LOGGER: OnceLock<Logger> = OnceLock::new();
static SESSION_CLOCK: OnceLock<SessionClock> = OnceLock::new();

struct SessionClock {
    origin: Instant,
    started_at_unix_ms: u64,
}

impl SessionClock {
    fn capture() -> Self {
        Self {
            origin: Instant::now(),
            started_at_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
        }
    }
}

struct Entry {
    event: &'static str,
    ts_ms: f64,
    fields: Value,
}

struct Logger {
    events: Sender<Entry>,
    stop: Sender<()>,
    writer: Mutex<Option<JoinHandle<()>>>,
    dropped: std::sync::Arc<AtomicU64>,
    origin: Instant,
    source: &'static str,
    redact: bool,
    roots: Mutex<Vec<PathBuf>>,
}

#[inline]
pub(crate) fn enabled() -> bool {
    ENABLED.load(Relaxed)
}

macro_rules! event {
    ($name:expr, $fields:tt) => {
        if $crate::telemetry::enabled() {
            $crate::telemetry::emit($name, serde_json::json!($fields));
        }
    };
}
pub(crate) use event;

pub(super) struct Session;

impl Session {
    pub(super) fn start() -> Self {
        SESSION_CLOCK.get_or_init(SessionClock::capture);
        if let Some(file) = std::env::var_os(FILE_ENV).filter(|path| !path.is_empty()) {
            start(
                file.into(),
                std::env::var_os(REDACT_ENV).is_some_and(|value| value == "1"),
                "process_env",
            );
        }
        Self
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(logger) = LOGGER.get() {
            ENABLED.store(false, Relaxed);
            let _ = logger.stop.try_send(());
            if let Some(writer) = logger.writer.lock().unwrap().take() {
                let _ = writer.join();
            }
        }
    }
}

pub(super) fn start_options(overrides: &mut EnvironmentOverrides) {
    let file = overrides.remove(FILE_ENV);
    let redact = overrides.remove(REDACT_ENV);
    if !enabled()
        && let Some(file) = file.filter(|value| !value.is_empty())
    {
        let redact = redact
            .map(|value| value == "1")
            .unwrap_or_else(|| std::env::var_os(REDACT_ENV).is_some_and(|value| value == "1"));
        start(file.into(), redact, "initialization_options");
    }
}

fn start(file: PathBuf, redact: bool, source: &'static str) {
    let clock = SESSION_CLOCK.get_or_init(SessionClock::capture);
    match Logger::spawn(file, clock.origin, redact, source, CAPACITY) {
        Ok(logger) => {
            logger.send(
                "session_start",
                json!({
                    "pid": std::process::id(),
                    "version": env!("CARGO_PKG_VERSION"),
                    "started_at_unix_ms": clock.started_at_unix_ms,
                    "source": source,
                    "redact": redact,
                }),
            );
            if LOGGER.set(logger).is_ok() {
                ENABLED.store(true, Relaxed);
            }
        }
        Err(error) => report_writer_error(&error),
    }
}

pub(super) fn session_config(roots: &[PathBuf], environment: &GoEnvironment) {
    if !enabled() {
        return;
    }
    let logger = LOGGER.get().unwrap();
    *logger.roots.lock().unwrap() = roots
        .iter()
        .map(|root| crate::workspace::path_key(root))
        .collect();
    let module = environment
        .get("GO111MODULE")
        .and_then(|mode| mode.to_str());
    let module = match module {
        Some(mode @ ("on" | "off" | "auto" | "")) => mode,
        Some(_) => "unknown",
        None => "",
    };
    let gopath_segments = environment
        .get("GOPATH")
        .into_iter()
        .flat_map(std::env::split_paths)
        .filter(|path| !path.as_os_str().is_empty())
        .count();
    logger.send(
        "session_config",
        json!({
            "workspace_folders": roots.len(),
            "go111module": module,
            "gopath_segments": gopath_segments,
            "source": logger.source,
        }),
    );
}

pub(crate) fn emit(event: &'static str, fields: Value) {
    if let Some(logger) = LOGGER.get() {
        logger.send(event, fields);
    }
}

pub(crate) fn now() -> Option<Instant> {
    enabled().then(Instant::now)
}

pub(crate) fn elapsed(start: Instant) -> f64 {
    millis(start.elapsed())
}

pub(super) fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

pub(crate) fn path(path: &Path) -> String {
    let key = crate::workspace::path_key(path);
    let logger = LOGGER.get().unwrap();
    let roots = logger.roots.lock().unwrap();
    let relative = roots
        .iter()
        .filter_map(|root| key.strip_prefix(root).ok())
        .min_by_key(|path| path.components().count());
    let short = match relative {
        Some(relative) => relative,
        None if logger.redact => &key,
        None => key
            .file_name()
            .map(Path::new)
            .unwrap_or_else(|| Path::new("")),
    };
    let short = if short.as_os_str().is_empty() {
        ".".to_owned()
    } else {
        short.to_string_lossy().replace('\\', "/")
    };
    if logger.redact { hash(&short) } else { short }
}

pub(super) fn hash(value: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("h:{hash:016x}")
}

pub(crate) struct LoopOp {
    op: &'static str,
    threshold_ms: f64,
    start: Instant,
}

pub(crate) fn loop_start(op: &'static str, threshold_ms: f64) -> Option<LoopOp> {
    let start = now()?;
    emit("loop_op_start", json!({ "op": op }));
    Some(LoopOp {
        op,
        threshold_ms,
        start,
    })
}

impl Drop for LoopOp {
    fn drop(&mut self) {
        let duration_ms = elapsed(self.start);
        if duration_ms >= self.threshold_ms {
            emit(
                "loop_long_op",
                json!({ "op": self.op, "duration_ms": duration_ms }),
            );
        }
    }
}

impl Logger {
    fn spawn(
        file: PathBuf,
        origin: Instant,
        redact: bool,
        source: &'static str,
        capacity: usize,
    ) -> io::Result<Self> {
        let (events, receiver) = bounded(capacity);
        let (stop, stopped) = bounded(1);
        let dropped = std::sync::Arc::new(AtomicU64::new(0));
        let writer_dropped = dropped.clone();
        let writer = thread::Builder::new()
            .name("go-diagnostic-log".into())
            .spawn(move || {
                if let Err(error) = write_log(&file, receiver, stopped, &writer_dropped, origin) {
                    report_writer_error(&error);
                }
            })?;
        Ok(Self {
            events,
            stop,
            writer: Mutex::new(Some(writer)),
            dropped,
            origin,
            source,
            redact,
            roots: Mutex::new(Vec::new()),
        })
    }

    fn send(&self, event: &'static str, fields: Value) {
        let entry = Entry {
            event,
            ts_ms: elapsed(self.origin),
            fields,
        };
        if matches!(self.events.try_send(entry), Err(TrySendError::Full(_))) {
            self.dropped.fetch_add(1, Relaxed);
        }
    }
}

fn report_writer_error(error: &io::Error) {
    eprintln!("rs-gopls: diagnostic log unavailable ({})", error.kind());
}

fn write_log(
    file: &Path,
    events: Receiver<Entry>,
    stopped: Receiver<()>,
    dropped: &AtomicU64,
    origin: Instant,
) -> io::Result<()> {
    if let Some(parent) = file
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new().create(true).append(true).open(file)?;
    let mut output = BufWriter::with_capacity(64 * 1024, file);
    let mut last_ts = 0.0;
    let mut reported_drops = 0;
    let mut last_flush = Instant::now();
    loop {
        let entry = select_biased! {
            recv(stopped) -> _ => break,
            recv(events) -> entry => match entry {
                Ok(entry) => entry,
                Err(_) => break,
            },
        };
        let flush = write_entry(&mut output, entry, &mut last_ts)?;
        if flush || events.is_empty() || last_flush.elapsed() >= FLUSH_INTERVAL {
            write_drops(
                &mut output,
                dropped,
                origin,
                &mut last_ts,
                &mut reported_drops,
            )?;
            output.flush()?;
            last_flush = Instant::now();
        }
    }
    while let Ok(entry) = events.try_recv() {
        write_entry(&mut output, entry, &mut last_ts)?;
    }
    write_drops(
        &mut output,
        dropped,
        origin,
        &mut last_ts,
        &mut reported_drops,
    )?;
    output.flush()
}

fn write_drops(
    output: &mut impl Write,
    dropped: &AtomicU64,
    origin: Instant,
    last_ts: &mut f64,
    reported: &mut u64,
) -> io::Result<()> {
    let total = dropped.load(Relaxed);
    if total != *reported {
        write_entry(
            output,
            Entry {
                event: "log_dropped",
                ts_ms: elapsed(origin),
                fields: json!({ "dropped": total, "since_last": total - *reported }),
            },
            last_ts,
        )?;
        *reported = total;
    }
    Ok(())
}

fn write_entry(output: &mut impl Write, entry: Entry, last_ts: &mut f64) -> io::Result<bool> {
    let Entry {
        event,
        ts_ms,
        mut fields,
    } = entry;
    let flush = event.ends_with("_start")
        || event == "workspace_load_cmd"
            && fields.get("phase").and_then(Value::as_str) == Some("cmd_start");
    *last_ts = last_ts.max(ts_ms);
    fields["event"] = json!(event);
    fields["ts_ms"] = json!(*last_ts);
    serde_json::to_writer(&mut *output, &fields)?;
    output.write_all(b"\n")?;
    Ok(flush)
}
