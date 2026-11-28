use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::Result;
use crossbeam_channel::{Receiver, Sender, bounded};
use tree_sitter::Parser;

use crate::semantic::index::{NavigationText, text_fingerprint};
use crate::semantic::shared_disk::{FileObservation, SharedDiskCache};
use crate::telemetry;
pub(in crate::semantic) use crate::workspace::DiagnosticFileStamp as Stamp;

use super::imports::SymbolMatcher;
use super::sdk_names::{Names, scan_names, scan_names_cancellable};

pub(in crate::semantic) const SOURCE_LIMIT: usize = 8;
pub(in crate::semantic) const BYTE_LIMIT: usize = 16 * 1024 * 1024;

#[derive(Clone)]
pub(in crate::semantic) struct FileNames {
    pub(in crate::semantic) stamp: Stamp,
    observation: Option<FileObservation>,
    pub(in crate::semantic) sdk_generation: Option<u64>,
    pub(in crate::semantic) fingerprint: u64,
    pub(in crate::semantic) names: Option<Arc<Names>>,
}

impl FileNames {
    #[cfg(test)]
    pub(in crate::semantic) fn new(stamp: Stamp, text: &str) -> Self {
        Self {
            stamp,
            observation: None,
            sdk_generation: None,
            fingerprint: text_fingerprint(text),
            names: scan_names(text).map(Arc::new),
        }
    }

    pub(super) fn sdk(observation: FileObservation, text: &str, watched: bool) -> Self {
        Self {
            stamp: observation.stamp,
            observation: Some(observation),
            sdk_generation: watched.then_some(0),
            fingerprint: text_fingerprint(text),
            names: scan_names(text).map(Arc::new),
        }
    }

    pub(in crate::semantic) fn from_read(
        text: &NavigationText,
        cancelled: Option<&AtomicBool>,
    ) -> Result<Option<Self>> {
        let Some(ticket) = &text.read_ticket else {
            return Ok(None);
        };
        let observation = ticket.observation();
        Ok(Some(Self {
            stamp: observation.stamp,
            observation: Some(observation),
            sdk_generation: None,
            fingerprint: ticket.fingerprint(),
            names: scan_names_cancellable(&text.text, cancelled)?.map(Arc::new),
        }))
    }

    pub(in crate::semantic) fn matches_disk(&self, path: &Path) -> bool {
        self.matches_observation(FileObservation::read(path).ok())
    }

    pub(in crate::semantic) fn matches_observation(
        &self,
        observed: Option<FileObservation>,
    ) -> bool {
        observed.is_some_and(|observed| {
            self.observation
                .map_or(observed.stamp == self.stamp, |expected| {
                    expected == observed
                })
        })
    }

    pub(super) fn matches(
        &self,
        matcher: &SymbolMatcher<'_>,
        cancelled: Option<&AtomicBool>,
    ) -> bool {
        if matcher.types_only {
            return self.names.as_ref().is_none_or(|names| {
                names.types.iter().any(|name| {
                    cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed))
                        || crate::semantic::resolver::is_exported(name)
                            && matcher.match_symbol(name).is_some()
                })
            });
        }
        self.may_export(|name| matcher.match_symbol(name).is_some(), cancelled)
    }

    pub(in crate::semantic) fn may_export(
        &self,
        matches: impl Fn(&str) -> bool,
        cancelled: Option<&AtomicBool>,
    ) -> bool {
        self.names.as_ref().is_none_or(|names| {
            names.exported.iter().any(|name| {
                cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) || matches(name)
            })
        })
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
        cancelled: Option<&AtomicBool>,
    ) -> bool {
        let Some(names) = &self.names else {
            return true;
        };
        let declarations = || names.exported.iter().chain(&names.private);
        if declarations()
            .any(|name| cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) || matches(name))
        {
            true
        } else {
            skipped.extend(
                declarations()
                    .take_while(|_| !cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)))
                    .cloned(),
            );
            false
        }
    }
}

pub(super) struct Input {
    pub(super) key: PathBuf,
    pub(super) path: PathBuf,
    pub(super) project: bool,
    // With a cached name match, warm the declaration artifact without rescanning.
    pub(super) matcher: Option<SymbolMatcher<'static>>,
}

pub(super) struct Output {
    pub(super) key: PathBuf,
    pub(super) project: bool,
    pub(super) read: bool,
    pub(super) names: Option<FileNames>,
    pub(super) decision: Decision,
}

pub(super) enum Decision {
    Text(NavigationText),
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
    disk: Arc<SharedDiskCache>,
    context: Option<telemetry::Context>,
    stats: Option<Mutex<telemetry::SharedDiskStats>>,
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

    pub(super) fn submit(
        &mut self,
        inputs: Vec<Input>,
        disk: Arc<SharedDiskCache>,
    ) -> Option<Batch> {
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
            disk,
            context: telemetry::context(),
            stats: telemetry::enabled().then(Mutex::default),
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
        if let Some(stats) = &self.state.stats {
            telemetry::merge_shared_disk_stats(std::mem::take(&mut *stats.lock().unwrap()));
        }
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
        if let Some(stats) = &mut self.stats {
            let stats = std::mem::take(stats.get_mut().unwrap());
            if stats != telemetry::SharedDiskStats::default() {
                telemetry::with_context(self.context, || telemetry::merge_shared_disk_stats(stats));
            }
        }
    }
}

fn start_workers() -> Option<Sender<Work>> {
    let (sender, receiver) = bounded::<Work>(SOURCE_LIMIT);
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
                        let mut parser = Parser::new();
                        parser
                            .set_language(&tree_sitter_go::LANGUAGE.into())
                            .expect("Go grammar must match the tree-sitter runtime");
                        for Work { input, state } in receiver {
                            if !state.cancelled.load(Ordering::Relaxed) {
                                let output = telemetry::with_context(state.context, || {
                                    let output = scan(input, &state, &mut parser);
                                    if let Some(stats) = &state.stats {
                                        stats
                                            .lock()
                                            .unwrap()
                                            .merge(telemetry::take_shared_disk_stats());
                                    }
                                    output
                                });
                                let _ = state.sender.send(output);
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

fn scan(input: Input, state: &BatchState, parser: &mut Parser) -> Output {
    let mut output = Output {
        key: input.key,
        project: input.project,
        read: false,
        names: None,
        decision: Decision::Skip,
    };
    let metadata_start = telemetry::now();
    let Ok(observation) = FileObservation::read(&input.path) else {
        return output;
    };
    let metadata_ms = metadata_start.map(telemetry::elapsed);
    let Ok(size) = usize::try_from(observation.stamp.revision_parts().1) else {
        output.decision = Decision::Deferred;
        return output;
    };
    if state
        .bytes
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
            used.checked_add(size).filter(|total| *total <= BYTE_LIMIT)
        })
        .is_err()
    {
        output.decision = Decision::Deferred;
        return output;
    }
    let read = state.disk.read_for_prefilter(
        &input.path,
        &output.key,
        observation,
        &state.cancelled,
        metadata_ms,
    );
    output.decision = read
        .ok()
        .flatten()
        .map_or(Decision::Deferred, |(text, producer)| {
            let (names, matched) = if let Some(matcher) = &input.matcher {
                let Ok(Some(names)) = FileNames::from_read(&text, Some(&state.cancelled)) else {
                    return Decision::Deferred;
                };
                let matched = names.matches(matcher, Some(&state.cancelled));
                (Some(names), matched)
            } else {
                // The bounded scanner and SharedDiskCache pools allow matching
                // cold files to produce in parallel. Consumption stays ordered.
                let _ = state.disk.declarations_snapshot(
                    parser,
                    &input.path,
                    text.clone(),
                    Some(&state.cancelled),
                    None,
                );
                (None, true)
            };
            if !state.disk.validates_read(&input.path, &text) {
                return Decision::Deferred;
            }
            output.read = producer;
            output.names = names;
            if matched {
                Decision::Text(text.capture())
            } else {
                Decision::Skip
            }
        });
    if matches!(output.decision, Decision::Text(_)) {
        return output;
    }
    state.bytes.fetch_sub(size, Ordering::Relaxed);
    output
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::semantic::shared_disk::CacheEvent;
    use crate::test_support::TestProject;
    use crate::workspace::path_key;

    const TIMEOUT: Duration = Duration::from_secs(5);

    fn input(path: PathBuf) -> Input {
        Input {
            key: path_key(&path),
            path,
            project: true,
            matcher: Some(SymbolMatcher::for_test("Exported")),
        }
    }

    fn wait_until(wake: &Receiver<()>, ready: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + TIMEOUT;
        while !ready() {
            if wake.recv_deadline(deadline).is_err() {
                return ready();
            }
        }
        true
    }

    #[test]
    fn cancelled_prefilter_detaches_from_a_shared_read() {
        let project = TestProject::new();
        let path = project.write("package.go", "package p\nvar Exported int\n");
        let disk = Arc::new(SharedDiskCache::default());
        let (wake, ready) = bounded(1);
        let mut scanner = Scanner::new(wake);
        let (entered, started) = bounded(1);
        let (subscribed, joined) = bounded(1);
        let (release, blocked) = bounded(1);
        disk.set_hook(Some(Arc::new(move |_, event| match event {
            CacheEvent::Read => {
                let _ = entered.try_send(());
                let _ = blocked.recv_timeout(TIMEOUT * 3);
            }
            CacheEvent::WaitRead => {
                let _ = subscribed.try_send(());
            }
            _ => {}
        })));

        let batch = scanner
            .submit(vec![input(path.clone())], disk.clone())
            .unwrap();
        started.recv_timeout(TIMEOUT).unwrap();
        let text = thread::scope(|scope| {
            let consumer = scope.spawn(|| disk.read(&path, Some(&AtomicBool::new(false)), None));
            let joined = joined.recv_timeout(TIMEOUT);
            drop(batch);
            let detached = wait_until(&ready, || !scanner.busy());
            let _ = release.try_send(());
            joined.unwrap();
            assert!(
                detached,
                "cancelled prefilter kept waiting for its producer"
            );
            consumer.join().unwrap().unwrap()
        });
        disk.set_hook(None);

        let batch = scanner
            .submit(vec![input(path.clone())], disk.clone())
            .unwrap();
        assert!(wait_until(&ready, || batch.is_ready()));
        let [result] = batch.results().try_into().ok().unwrap();
        let Decision::Text(prefetched) = result.decision else {
            panic!("expected shared text for matching declaration");
        };
        assert!(Arc::ptr_eq(&prefetched.text, &text.text));
        assert!(prefetched.read_ticket.as_ref().unwrap().is_captured());
        assert!(!text.read_ticket.as_ref().unwrap().is_captured());
        assert!(disk.validates_read(&path, &prefetched));
        assert!(!result.read);
        assert_eq!(disk.counts(&path), (1, 0));
    }

    #[test]
    fn matching_text_reservations_stay_within_the_batch_budget() {
        let project = TestProject::new();
        let header = "package p\nvar Exported int\n//";
        let text = format!("{header}{}", " ".repeat(BYTE_LIMIT / 2 - header.len() + 1));
        let first = project.write("first.go", &text);
        let second = project.write("second.go", &text);
        let disk = Arc::new(SharedDiskCache::default());
        let (wake, ready) = bounded(1);
        let mut scanner = Scanner::new(wake);
        let batch = scanner
            .submit(
                vec![input(first.clone()), input(second.clone())],
                disk.clone(),
            )
            .unwrap();
        assert!(wait_until(&ready, || batch.is_ready()));
        let results = batch.results();
        assert_eq!(results.len(), 2);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result.decision, Decision::Text(_)))
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result.decision, Decision::Deferred))
                .count(),
            1
        );
        assert_eq!(batch.state.bytes.load(Ordering::Relaxed), text.len());
        assert_eq!(disk.counts(&first).0 + disk.counts(&second).0, 1);
    }
}
