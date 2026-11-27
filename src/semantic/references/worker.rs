use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use tree_sitter::Parser;

use super::candidates::{CandidateInput, CandidateList};
use super::large_file::{
    LargeAnchorResult, LargeFileResult, LargeFileVerification, LargeReferenceAnchor,
};
use crate::semantic::index::{IndexedSourceRevision, NavigationText, PreparedReferenceSource};
use crate::semantic::source::check_cancelled;

pub(super) enum Work {
    Candidates(CandidateInput),
    Read(Vec<(PathBuf, PathBuf)>),
    PrepareDisk {
        key: PathBuf,
        path: PathBuf,
        text: Option<Arc<String>>,
        preparation: Preparation,
    },
    PrepareOpen(IndexedSourceRevision),
    VerifyLarge(Box<LargeFileVerification>),
    PlanLarge(Box<LargeReferenceAnchor>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Preparation {
    Anchor,
    Candidate,
    Dependency,
}

pub(super) enum WorkOutput {
    Candidates(CandidateList),
    Texts(Vec<(PathBuf, Result<NavigationText>)>),
    Disk {
        key: PathBuf,
        text: Option<NavigationText>,
        source: Result<PreparedReferenceSource>,
        preparation: Preparation,
    },
    Open(Result<()>),
    Large(LargeFileResult),
    Anchor(LargeAnchorResult),
}

#[derive(Default)]
pub(super) struct WorkStats {
    pub(super) reads: usize,
    pub(super) read: Duration,
    pub(super) parsed: bool,
    pub(super) parse: Duration,
}

pub(super) struct WorkResult {
    pub(super) output: WorkOutput,
    pub(super) stats: WorkStats,
}

impl Work {
    fn run(self, parser: &mut Parser, cancelled: &AtomicBool) -> Result<WorkResult> {
        let mut stats = WorkStats::default();
        let mut read = |path: &std::path::Path| {
            let start = Instant::now();
            let result = NavigationText::read_cancellable(path, cancelled);
            stats.reads += 1;
            stats.read += start.elapsed();
            result
        };
        check_cancelled(cancelled)?;
        let output = match self {
            Self::Candidates(input) => WorkOutput::Candidates(input.run(cancelled)?),
            Self::Read(files) => {
                const READ_AHEAD_BYTES: usize = 4 * 1024 * 1024;
                let mut texts = Vec::with_capacity(files.len());
                let mut bytes = 0;
                for (key, path) in files {
                    check_cancelled(cancelled)?;
                    let text = read(&path);
                    if let Ok(text) = &text {
                        bytes += text.text.len();
                    }
                    texts.push((key, text));
                    if bytes >= READ_AHEAD_BYTES {
                        break;
                    }
                }
                WorkOutput::Texts(texts)
            }
            Self::PrepareDisk {
                key,
                path,
                text,
                preparation,
            } => {
                let mut observation = None;
                let text = match text {
                    Some(text) => Ok(text),
                    None => read(&path).map(|observed| {
                        let text = Arc::clone(&observed.text);
                        observation = Some(observed);
                        text
                    }),
                };
                let source = text.and_then(|text| {
                    let start = Instant::now();
                    stats.parsed = true;
                    let result = PreparedReferenceSource::parse(parser, &path, text, cancelled);
                    stats.parse = start.elapsed();
                    result
                });
                WorkOutput::Disk {
                    key,
                    text: observation,
                    source,
                    preparation,
                }
            }
            Self::PrepareOpen(revision) => WorkOutput::Open(revision.build_declarations(cancelled)),
            Self::VerifyLarge(input) => WorkOutput::Large(input.run(parser, cancelled)?),
            Self::PlanLarge(input) => WorkOutput::Anchor(input.run(parser, cancelled)?),
        };
        check_cancelled(cancelled)?;
        Ok(WorkResult { output, stats })
    }
}

struct Job {
    work: Work,
    cancelled: Arc<AtomicBool>,
    done: Sender<Result<WorkResult>>,
}

pub(super) struct PendingWork {
    job: Option<Job>,
    result: Receiver<Result<WorkResult>>,
    cancelled: Arc<AtomicBool>,
}

impl PendingWork {
    pub(super) fn new(work: Work) -> Self {
        let (done, result) = bounded(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        Self {
            job: Some(Job {
                work,
                cancelled: cancelled.clone(),
                done,
            }),
            result,
            cancelled,
        }
    }

    pub(super) fn ready(&self, worker: &ReferenceWorker) -> bool {
        if self.job.is_some() {
            return worker.queue.as_ref().is_none_or(|queue| !queue.is_full());
        }
        !self.result.is_empty()
    }

    pub(super) fn poll(&mut self, worker: &mut ReferenceWorker) -> Result<Option<WorkResult>> {
        if let Some(job) = self.job.take() {
            match worker.queue()?.try_send(job) {
                Ok(()) => {}
                Err(TrySendError::Full(job)) => self.job = Some(job),
                Err(TrySendError::Disconnected(_)) => anyhow::bail!("Go reference worker exited"),
            }
            return Ok(None);
        }
        match self.result.try_recv() {
            Ok(result) => result.map(Some),
            Err(crossbeam_channel::TryRecvError::Empty) => Ok(None),
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                anyhow::bail!("Go reference worker dropped the result")
            }
        }
    }
}

impl Drop for PendingWork {
    fn drop(&mut self) {
        self.cancelled.store(true, Relaxed);
    }
}

pub(in crate::semantic) struct ReferenceWorker {
    queue: Option<Sender<Job>>,
    signal: Sender<()>,
    wake: Receiver<()>,
}

impl Default for ReferenceWorker {
    fn default() -> Self {
        let (signal, wake) = bounded(1);
        Self {
            queue: None,
            signal,
            wake,
        }
    }
}

impl ReferenceWorker {
    pub(in crate::semantic) fn wake(&self) -> &Receiver<()> {
        &self.wake
    }

    fn queue(&mut self) -> Result<&Sender<Job>> {
        if self.queue.is_none() {
            let mut parser = Parser::new();
            parser.set_language(&tree_sitter_go::LANGUAGE.into())?;
            let (queue, jobs) = bounded::<Job>(8);
            let signal = self.signal.clone();
            thread::Builder::new()
                .name("go-references".into())
                .spawn(move || {
                    while let Ok(job) = jobs.recv() {
                        let _ = signal.try_send(());
                        if !job.cancelled.load(Relaxed) {
                            let result = job.work.run(&mut parser, &job.cancelled);
                            let _ = job.done.send(result);
                        }
                        let _ = signal.try_send(());
                    }
                })
                .context("starting Go reference worker")?;
            self.queue = Some(queue);
        }
        Ok(self.queue.as_ref().unwrap())
    }
}
