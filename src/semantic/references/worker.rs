use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Select, Sender, TrySendError, bounded};
use tree_sitter::Parser;

use super::ReferenceObservations;
use super::candidates::{CandidateInput, CandidateList};
use super::large_file::{
    AnchorWork, CandidateDiscovery, CandidateResult, CandidateWork, DiscoveredCandidate,
    ReferenceAnchorResult, contains_spelling,
};
use crate::semantic::index::NavigationText;
use crate::semantic::navigation_trace::{NavigationProbe, record};
use crate::semantic::shared_disk::{PreparedDeclarations, SharedDiskCache};
use crate::semantic::source::check_cancelled;
use crate::telemetry;

const POOL_SIZE: usize = 4;
const QUEUE_SIZE: usize = 16;
const CANCEL_POLL: Duration = Duration::from_millis(2);
pub(super) const BATCH_SIZE: usize = 12;

pub(super) enum Work {
    Candidates(CandidateInput),
    Anchor(AnchorWork),
    Discover(CandidateDiscovery),
    Verify(CandidateWork),
    #[cfg(test)]
    Test(Box<dyn FnOnce() -> Result<WorkOutput> + Send>),
}

pub(super) enum WorkOutput {
    Candidates(CandidateList),
    Anchor(ReferenceAnchorResult),
    Discovered(DiscoveredCandidate),
    Verified(CandidateResult),
}

impl Work {
    fn run(
        self,
        parser: &mut Parser,
        dependencies: &ReferenceDependencyPool,
        cancelled: &Arc<AtomicBool>,
    ) -> Result<WorkOutput> {
        check_cancelled(cancelled)?;
        let result = match self {
            Self::Candidates(input) => WorkOutput::Candidates(input.run(cancelled)?),
            Self::Anchor(input) => {
                WorkOutput::Anchor(input.run(parser, dependencies, cancelled)?)
            }
            Self::Discover(input) => WorkOutput::Discovered(input.run(cancelled)?),
            Self::Verify(input) => {
                WorkOutput::Verified(input.run(parser, dependencies, cancelled)?)
            }
            #[cfg(test)]
            Self::Test(run) => run()?,
        };
        check_cancelled(cancelled)?;
        Ok(result)
    }
}

struct Job {
    work: Work,
    cancelled: Arc<AtomicBool>,
    done: JobDone,
    context: Option<telemetry::Context>,
}

struct JobDone {
    result: Option<Sender<Result<WorkOutput>>>,
    signal: Option<Sender<()>>,
}

impl JobDone {
    fn send(&mut self, result: Result<WorkOutput>) {
        if let Some(sender) = self.result.take() {
            let _ = sender.try_send(result);
        }
    }
}

impl Drop for JobDone {
    fn drop(&mut self) {
        if self.result.is_some() {
            self.send(Err(anyhow::anyhow!("Go reference worker abandoned a job")));
        }
        if let Some(signal) = &self.signal {
            let _ = signal.try_send(());
        }
    }
}

pub(super) struct PendingWork {
    jobs: VecDeque<Job>,
    results: VecDeque<Receiver<Result<WorkOutput>>>,
    completed: Vec<WorkOutput>,
    cancelled: Arc<AtomicBool>,
}

impl PendingWork {
    pub(super) fn new(work: impl IntoIterator<Item = Work>) -> Self {
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut jobs = VecDeque::new();
        let mut results = VecDeque::new();
        let context = telemetry::context();
        for work in work {
            let (done, result) = bounded(1);
            jobs.push_back(Job {
                work,
                done: JobDone {
                    result: Some(done),
                    signal: None,
                },
                cancelled: cancelled.clone(),
                context,
            });
            results.push_back(result);
        }
        Self {
            completed: Vec::with_capacity(jobs.len()),
            jobs,
            results,
            cancelled,
        }
    }

    pub(super) fn ready(&self, worker: &ReferenceWorker) -> bool {
        (!self.jobs.is_empty() && worker.queue.as_ref().is_none_or(|queue| !queue.is_full()))
            || self.results.front().is_none_or(|result| {
                let mut select = Select::new();
                select.recv(result);
                select.try_ready().is_ok()
            })
    }

    pub(super) fn poll(&mut self, worker: &mut ReferenceWorker) -> Result<Option<Vec<WorkOutput>>> {
        if !self.jobs.is_empty() {
            let signal = worker.signal.clone();
            let queue = worker.queue()?;
            while let Some(mut job) = self.jobs.pop_front() {
                job.done.signal.get_or_insert_with(|| signal.clone());
                match queue.try_send(job) {
                    Ok(()) => {}
                    Err(TrySendError::Full(job)) => {
                        self.jobs.push_front(job);
                        break;
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        anyhow::bail!("Go reference workers exited")
                    }
                }
            }
        }
        while let Some(result) = self.results.front() {
            match result.try_recv() {
                Ok(result) => {
                    self.completed.push(result?);
                    self.results.pop_front();
                }
                Err(crossbeam_channel::TryRecvError::Empty) => return Ok(None),
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    anyhow::bail!("Go reference worker dropped a result")
                }
            }
        }
        Ok(Some(std::mem::take(&mut self.completed)))
    }
}

impl Drop for PendingWork {
    fn drop(&mut self) {
        self.cancelled.store(true, Relaxed);
    }
}

pub(in crate::semantic) struct ReferenceWorker {
    queue: Option<Sender<Job>>,
    shared_disk: Arc<SharedDiskCache>,
    signal: Sender<()>,
    wake: Receiver<()>,
}

impl ReferenceWorker {
    pub(in crate::semantic) fn new(shared_disk: Arc<SharedDiskCache>) -> Self {
        let (signal, wake) = bounded(1);
        Self {
            queue: None,
            shared_disk,
            signal,
            wake,
        }
    }

    pub(in crate::semantic) fn wake(&self) -> &Receiver<()> {
        &self.wake
    }

    fn queue(&mut self) -> Result<&Sender<Job>> {
        if self.queue.is_none() {
            let (queue, jobs) = bounded::<Job>(QUEUE_SIZE);
            let dependencies = ReferenceDependencyPool::new(self.shared_disk.clone())?;
            for index in 0..POOL_SIZE {
                let mut parser = go_parser()?;
                let jobs = jobs.clone();
                let signal = self.signal.clone();
                let dependencies = dependencies.clone();
                thread::Builder::new()
                    .name(format!("go-references-{index}"))
                    .spawn(move || {
                        while let Ok(Job {
                            work,
                            cancelled,
                            mut done,
                            context,
                        }) = jobs.recv()
                        {
                            let _ = signal.try_send(());
                            let result = telemetry::with_context(context, || {
                                catch_unwind(AssertUnwindSafe(|| {
                                    work.run(&mut parser, &dependencies, &cancelled)
                                }))
                                .unwrap_or_else(|_| {
                                    parser.reset();
                                    Err(anyhow::anyhow!("Go reference worker panicked"))
                                })
                            });
                            done.send(result);
                        }
                    })
                    .context("starting Go reference worker")?;
            }
            self.queue = Some(queue);
        }
        Ok(self.queue.as_ref().unwrap())
    }
}

fn go_parser() -> Result<Parser> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_go::LANGUAGE.into())?;
    Ok(parser)
}

struct DependencyJob {
    path: PathBuf,
    text: Option<NavigationText>,
    index: usize,
    cancelled: Arc<AtomicBool>,
    spelling: Arc<str>,
    done: Sender<DependencyResult>,
    trace: bool,
    context: Option<telemetry::Context>,
}

struct DependencyResult {
    index: usize,
    source: Result<DependencySource>,
    probe: NavigationProbe,
}

pub(in crate::semantic) enum DependencySource {
    Ready(PreparedDeclarations),
    Absent(NavigationText),
}

#[derive(Clone)]
pub(in crate::semantic) struct ReferenceDependencyPool(Sender<DependencyJob>);

pub(in crate::semantic) struct ReferenceDependencies<'a> {
    pub(in crate::semantic) pool: &'a ReferenceDependencyPool,
    pub(in crate::semantic) cancelled: &'a Arc<AtomicBool>,
    pub(in crate::semantic) observations: &'a ReferenceObservations,
}

impl ReferenceDependencyPool {
    fn new(shared_disk: Arc<SharedDiskCache>) -> Result<Self> {
        let (queue, jobs) = bounded::<DependencyJob>(QUEUE_SIZE);
        // Resolution workers wait on these jobs. Keep their execution capacity
        // independent so a full verification wave cannot starve its dependencies.
        let threads = thread::available_parallelism()
            .map_or(4, usize::from)
            .clamp(4, 8);
        for index in 0..threads {
            let mut parser = go_parser()?;
            let jobs = jobs.clone();
            let shared_disk = shared_disk.clone();
            thread::Builder::new()
                .name(format!("go-reference-dependencies-{index}"))
                .spawn(move || {
                    while let Ok(job) = jobs.recv() {
                        let probe = NavigationProbe::default();
                        let source = telemetry::with_context(job.context, || {
                            catch_unwind(AssertUnwindSafe(|| {
                                let trace = job.trace.then_some(&probe);
                                let text = match job.text {
                                    Some(text) => {
                                        record(trace, |counts| counts.text_hits += 1);
                                        text
                                    }
                                    None => {
                                        shared_disk.read(&job.path, Some(&job.cancelled), trace)?
                                    }
                                };
                                if !contains_spelling(&text.text, &job.spelling, &job.cancelled)?
                                    || shared_disk
                                        .reference_identifiers(&text, &job.cancelled)?
                                        .offsets(&job.spelling)
                                        .is_some_and(|offsets| offsets.is_empty())
                                {
                                    return Ok(DependencySource::Absent(text));
                                }
                                shared_disk
                                    .load_declarations(
                                        &mut parser,
                                        &job.path,
                                        Some(text),
                                        Some(&job.cancelled),
                                        trace,
                                    )
                                    .map(DependencySource::Ready)
                            }))
                            .unwrap_or_else(|_| {
                                parser.reset();
                                Err(anyhow::anyhow!("Go reference dependency worker panicked"))
                            })
                        });
                        let _ = job.done.send(DependencyResult {
                            index: job.index,
                            source,
                            probe,
                        });
                    }
                })
                .context("starting Go reference dependency worker")?;
        }
        Ok(Self(queue))
    }
}

impl ReferenceDependencies<'_> {
    pub(in crate::semantic) fn load(
        &self,
        paths: Vec<(PathBuf, Option<NavigationText>)>,
        spelling: &str,
        probe: Option<&NavigationProbe>,
    ) -> Result<Vec<Result<DependencySource>>> {
        let count = paths.len();
        let (done, results) = bounded(count);
        let spelling = Arc::<str>::from(spelling);
        let context = telemetry::context();
        for (index, (path, text)) in paths.into_iter().enumerate() {
            let mut job = DependencyJob {
                path,
                text,
                index,
                cancelled: self.cancelled.clone(),
                spelling: spelling.clone(),
                done: done.clone(),
                trace: probe.is_some(),
                context,
            };
            loop {
                check_cancelled(self.cancelled)?;
                match self.pool.0.send_timeout(job, CANCEL_POLL) {
                    Ok(()) => break,
                    Err(crossbeam_channel::SendTimeoutError::Timeout(pending)) => job = pending,
                    Err(crossbeam_channel::SendTimeoutError::Disconnected(_)) => {
                        anyhow::bail!("Go reference dependency workers exited")
                    }
                }
            }
        }
        drop(done);
        let mut sources = Vec::with_capacity(count);
        while sources.len() < count {
            check_cancelled(self.cancelled)?;
            match results.recv_timeout(CANCEL_POLL) {
                Ok(result) => {
                    let counts = result.probe.into_counts();
                    record(probe, |total| total.add_reference_verification(counts));
                    sources.push((result.index, result.source));
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("Go reference dependency worker dropped a result")
                }
            }
        }
        sources.sort_unstable_by_key(|(index, _)| *index);
        Ok(sources.into_iter().map(|(_, source)| source).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_worker_panics_send_errors_and_preserve_pool_capacity() {
        let mut worker = ReferenceWorker::new(Arc::default());
        let queue = worker.queue().unwrap().clone();

        for panic in [true, false] {
            let (started, received) = bounded(POOL_SIZE);
            let (release, released) = bounded::<()>(0);
            let mut results = Vec::new();
            for _ in 0..POOL_SIZE {
                let (done, result) = bounded(1);
                let started = started.clone();
                let released = released.clone();
                queue
                    .send(Job {
                        work: Work::Test(Box::new(move || {
                            started.send(()).unwrap();
                            released.recv_timeout(Duration::from_secs(5)).ok();
                            assert!(!panic, "injected reference worker panic");
                            anyhow::bail!("reference worker ran again");
                        })),
                        cancelled: Arc::default(),
                        context: None,
                        done: JobDone {
                            result: Some(done),
                            signal: Some(worker.signal.clone()),
                        },
                    })
                    .ok()
                    .expect("available worker queue");
                results.push(result);
            }
            for _ in 0..POOL_SIZE {
                received.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            while worker.wake.try_recv().is_ok() {}
            drop(release);
            for result in results {
                let error = result
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .err()
                    .expect("terminal worker result");
                assert_eq!(
                    error.to_string(),
                    if panic {
                        "Go reference worker panicked"
                    } else {
                        "reference worker ran again"
                    }
                );
            }
            worker.wake.recv_timeout(Duration::from_secs(5)).unwrap();
        }
    }

    #[test]
    fn disconnected_reference_results_are_ready_to_poll() {
        let mut worker = ReferenceWorker::new(Arc::default());
        let (done, result) = bounded(1);
        drop(done);
        let mut pending = PendingWork::new([]);
        pending.results.push_back(result);
        assert!(pending.ready(&worker));
        let error = pending
            .poll(&mut worker)
            .err()
            .expect("disconnected result");
        assert_eq!(error.to_string(), "Go reference worker dropped a result");
    }
}
