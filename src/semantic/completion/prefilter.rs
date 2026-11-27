use std::collections::HashSet;
use std::fs::File;
use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};

use crate::semantic::index::text_fingerprint;
pub(in crate::semantic) use crate::workspace::DiagnosticFileStamp as Stamp;

use super::imports::SymbolMatcher;
use super::sdk_names::{Names, scan_names};

pub(in crate::semantic) const SOURCE_LIMIT: usize = 8;
pub(in crate::semantic) const BYTE_LIMIT: usize = 16 * 1024 * 1024;

#[derive(Clone)]
pub(in crate::semantic) struct FileNames {
    pub(in crate::semantic) stamp: Stamp,
    pub(in crate::semantic) fingerprint: u64,
    pub(in crate::semantic) names: Option<Arc<Names>>,
    pub(in crate::semantic) elide_bodies: bool,
}

impl FileNames {
    pub(in crate::semantic) fn new(stamp: Stamp, text: &str, elide_bodies: bool) -> Self {
        Self {
            stamp,
            fingerprint: text_fingerprint(text),
            names: scan_names(text).map(Arc::new),
            elide_bodies,
        }
    }

    pub(super) fn matches(&self, matcher: &SymbolMatcher<'_>) -> bool {
        if matcher.types_only {
            return self.names.as_ref().is_none_or(|names| {
                names.types.iter().any(|name| {
                    crate::semantic::resolver::is_exported(name)
                        && matcher.match_symbol(name).is_some()
                })
            });
        }
        self.may_export(|name| matcher.match_symbol(name).is_some())
    }

    pub(in crate::semantic) fn may_export(&self, matches: impl Fn(&str) -> bool) -> bool {
        self.names
            .as_ref()
            .is_none_or(|names| names.exported.iter().any(|name| matches(name)))
    }

    pub(in crate::semantic) fn may_declare_type(&self, name: &str) -> bool {
        self.names.as_ref().is_none_or(|names| {
            names
                .types
                .binary_search_by(|entry| entry.as_str().cmp(name))
                .is_ok()
        })
    }

    pub(in crate::semantic) fn may_declare(&self, name: &str) -> bool {
        self.names.as_ref().is_none_or(|names| {
            let names = if crate::semantic::resolver::is_exported(name) {
                &names.exported
            } else {
                &names.private
            };
            names
                .binary_search_by(|entry| entry.as_str().cmp(name))
                .is_ok()
        })
    }

    pub(in crate::semantic) fn local_matches(
        &self,
        matches: impl Fn(&str) -> bool,
        skipped: &mut HashSet<String>,
    ) -> bool {
        let Some(names) = &self.names else {
            return true;
        };
        let declarations = || names.exported.iter().chain(&names.private);
        if declarations().any(|name| matches(name)) {
            true
        } else {
            skipped.extend(declarations().cloned());
            false
        }
    }
}

pub(super) struct Input {
    pub(super) key: PathBuf,
    pub(super) path: PathBuf,
    pub(super) project: bool,
    pub(super) elide_bodies: bool,
    pub(super) matcher: SymbolMatcher<'static>,
}

pub(super) struct Output {
    pub(super) key: PathBuf,
    pub(super) project: bool,
    pub(super) read: bool,
    pub(super) names: Option<FileNames>,
    pub(super) decision: Decision,
}

pub(super) enum Decision {
    Text(String),
    Skip,
    Deferred,
}

pub(super) struct Scanner {
    wake: Sender<()>,
    busy: Arc<AtomicBool>,
    work: Option<Sender<Work>>,
}

struct Work {
    input: Input,
    state: Arc<BatchState>,
}

struct BatchState {
    busy: Arc<AtomicBool>,
    wake: Sender<()>,
    left: AtomicUsize,
    bytes: AtomicUsize,
    cancelled: AtomicBool,
    sender: Sender<Output>,
}

pub(super) struct Batch {
    state: Arc<BatchState>,
    receiver: Receiver<Output>,
}

impl Scanner {
    pub(super) fn new(wake: Sender<()>) -> Self {
        Self {
            wake,
            busy: Arc::new(AtomicBool::new(false)),
            work: None,
        }
    }

    pub(super) fn busy(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }

    pub(super) fn submit(&mut self, inputs: Vec<Input>) -> Option<Batch> {
        debug_assert!(!inputs.is_empty() && inputs.len() <= SOURCE_LIMIT);
        debug_assert!(!self.busy());
        if self.work.is_none() {
            self.work = start_workers();
        }
        let worker = self.work.as_ref()?;
        let (sender, receiver) = bounded(inputs.len());
        let state = Arc::new(BatchState {
            busy: Arc::clone(&self.busy),
            wake: self.wake.clone(),
            left: AtomicUsize::new(inputs.len()),
            bytes: AtomicUsize::new(0),
            cancelled: AtomicBool::new(false),
            sender,
        });
        self.busy.store(true, Ordering::Release);
        for input in inputs {
            worker
                .send(Work {
                    input,
                    state: Arc::clone(&state),
                })
                .ok()?;
        }
        Some(Batch { state, receiver })
    }
}

impl Batch {
    pub(super) fn is_ready(&self) -> bool {
        self.state.left.load(Ordering::Acquire) == 0
    }

    pub(super) fn results(&self) -> Vec<Output> {
        let mut results = self.receiver.try_iter().collect::<Vec<_>>();
        results.sort_unstable_by(|left, right| left.key.cmp(&right.key));
        results
    }
}

impl Drop for Batch {
    fn drop(&mut self) {
        self.state.cancelled.store(true, Ordering::Relaxed);
    }
}

impl Drop for BatchState {
    fn drop(&mut self) {
        self.busy.store(false, Ordering::Release);
        let _ = self.wake.try_send(());
    }
}

fn start_workers() -> Option<Sender<Work>> {
    let (sender, receiver) = unbounded::<Work>();
    let workers = thread::available_parallelism()
        .map_or(1, usize::from)
        .min(4);
    thread::Builder::new()
        .name("go-completion-prefilter".into())
        .spawn(move || {
            thread::scope(|scope| {
                for _ in 0..workers {
                    let receiver = &receiver;
                    scope.spawn(move || {
                        for Work { input, state } in receiver {
                            if !state.cancelled.load(Ordering::Relaxed) {
                                let _ = state.sender.send(scan(input, &state.bytes));
                            }
                            if state.left.fetch_sub(1, Ordering::AcqRel) == 1 {
                                let _ = state.wake.try_send(());
                            }
                        }
                    });
                }
            });
        })
        .ok()?;
    Some(sender)
}

fn scan(input: Input, budget: &AtomicUsize) -> Output {
    let mut output = Output {
        key: input.key,
        project: input.project,
        read: false,
        names: None,
        decision: Decision::Skip,
    };
    let Ok(mut file) = File::open(input.path) else {
        return output;
    };
    let Ok(metadata) = file.metadata() else {
        output.decision = Decision::Deferred;
        return output;
    };
    let Ok(size) = usize::try_from(metadata.len()) else {
        output.decision = Decision::Deferred;
        return output;
    };
    if budget
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
            used.checked_add(size).filter(|total| *total <= BYTE_LIMIT)
        })
        .is_err()
    {
        output.decision = Decision::Deferred;
        return output;
    }
    output.read = true;
    match read_exact_text(&mut file, size) {
        Ok(Some(text)) => {
            let names = FileNames::new(metadata.into(), &text, input.elide_bodies);
            let matched = names.matches(&input.matcher);
            output.names = Some(names);
            if matched {
                output.decision = Decision::Text(text);
                return output;
            }
        }
        Ok(None) => output.decision = Decision::Deferred,
        Err(_) => {}
    }
    budget.fetch_sub(size, Ordering::Relaxed);
    output
}

/// If the file changed size during the read, retry outside the pending text
/// budget. Reading one extra byte into the stack keeps that budget exact.
fn read_exact_text(file: &mut File, size: usize) -> io::Result<Option<String>> {
    let mut bytes = vec![0; size];
    if let Err(error) = file.read_exact(&mut bytes) {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            return Ok(None);
        }
        return Err(error);
    }
    if file.read(&mut [0])? != 0 {
        return Ok(None);
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}
