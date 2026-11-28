use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use tree_sitter::Parser;

use super::index::{IndexedSource, IndexedSourceRevision};
use super::shared_disk::SharedLoadPending;
use super::source::OpenText;

pub(super) struct PendingOpen {
    pub(super) path: PathBuf,
    pub(super) text: OpenText,
    pub(super) version: i32,
    pub(super) open_generation: u64,
    pub(super) content_revision: u64,
    pub(super) ready: OpenReadiness,
}

pub(super) struct OpenReadiness {
    finished: Arc<AtomicBool>,
    signal_on_drop: bool,
}

impl Default for OpenReadiness {
    fn default() -> Self {
        Self {
            finished: Arc::default(),
            signal_on_drop: true,
        }
    }
}

impl PendingOpen {
    pub(super) fn read_snapshot(&self) -> Self {
        Self {
            path: self.path.clone(),
            text: self.text.clone(),
            version: self.version,
            open_generation: self.open_generation,
            content_revision: self.content_revision,
            ready: OpenReadiness {
                finished: self.ready.finished.clone(),
                signal_on_drop: false,
            },
        }
    }
}

impl OpenReadiness {
    pub(super) fn pending(&self) -> SharedLoadPending {
        SharedLoadPending::new(self.finished.clone())
    }
}

impl Drop for OpenReadiness {
    fn drop(&mut self) {
        if self.signal_on_drop {
            self.finished.store(true, Ordering::Release);
        }
    }
}

pub(crate) struct OpenParseResult {
    key: PathBuf,
    revision: u64,
    result: Result<IndexedSourceRevision>,
}

struct OpenParseWork {
    key: PathBuf,
    path: PathBuf,
    text: OpenText,
    version: i32,
    revision: u64,
    cancelled: Arc<AtomicBool>,
}

pub(super) struct OpenParser {
    work: Sender<OpenParseWork>,
    pub(super) completed: Receiver<OpenParseResult>,
    running: Option<RunningParse>,
}

struct RunningParse {
    key: PathBuf,
    revision: u64,
    cancelled: Arc<AtomicBool>,
}

impl OpenParser {
    pub(super) fn new() -> Result<Self> {
        let (work, jobs) = bounded::<OpenParseWork>(1);
        let (results, completed) = unbounded();
        thread::Builder::new()
            .name("go-open-parser".to_owned())
            .spawn(move || {
                let mut parser = Parser::new();
                parser
                    .set_language(&tree_sitter_go::LANGUAGE.into())
                    .expect("Go grammar must match the tree-sitter runtime");
                while let Ok(job) = jobs.recv() {
                    let result = job
                        .text
                        .parse(&mut parser, job.path, &job.cancelled)
                        .and_then(|source| {
                            let revision = IndexedSource::new(source, Some(job.version)).revision();
                            revision.clone().build_declarations(&job.cancelled)?;
                            Ok(revision)
                        });
                    if results
                        .send(OpenParseResult {
                            key: job.key,
                            revision: job.revision,
                            result,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .context("starting Go open document parser")?;
        Ok(Self {
            work,
            completed,
            running: None,
        })
    }

    pub(super) fn schedule(&mut self, pending: &HashMap<PathBuf, PendingOpen>) -> Result<()> {
        if let Some(running) = &self.running {
            if pending
                .get(&running.key)
                .is_none_or(|open| open.content_revision != running.revision)
            {
                running.cancelled.store(true, Ordering::Relaxed);
            }
            return Ok(());
        }
        let Some((key, open)) = pending.iter().min_by_key(|(_, open)| open.content_revision) else {
            return Ok(());
        };
        let job = OpenParseWork {
            key: key.clone(),
            path: open.path.clone(),
            text: open.text.clone(),
            version: open.version,
            revision: open.content_revision,
            cancelled: Arc::default(),
        };
        let running = RunningParse {
            key: job.key.clone(),
            revision: job.revision,
            cancelled: job.cancelled.clone(),
        };
        self.work
            .try_send(job)
            .context("queuing Go open document parse")?;
        self.running = Some(running);
        Ok(())
    }

    pub(super) fn complete(
        &mut self,
        result: OpenParseResult,
        pending: &mut HashMap<PathBuf, PendingOpen>,
    ) -> Result<Option<(PathBuf, PendingOpen, IndexedSource)>> {
        self.running = None;
        let current = pending
            .get(&result.key)
            .is_some_and(|open| open.content_revision == result.revision);
        let completed = if current {
            let open = pending
                .remove(&result.key)
                .expect("checked the pending revision");
            result
                .result
                .map(|revision| Some((result.key, open, revision.into_indexed())))
        } else {
            Ok(None)
        };
        self.schedule(pending)?;
        completed
    }
}

impl Drop for OpenParser {
    fn drop(&mut self) {
        if let Some(running) = &self.running {
            running.cancelled.store(true, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use lsp_types::{Position, Range, TextDocumentContentChangeEvent};

    use crate::semantic::{Engine, source::LARGE_OPEN_BYTES};
    use crate::test_support::TestProject;
    use crate::workspace::path_key;

    #[test]
    fn large_open_keeps_edits_and_drops_the_old_parse_without_blocking_other_packages() {
        let project = TestProject::new();
        let large = project.root.join("large.go");
        let sibling = project.root.join("sibling.go");
        let small = project.root.join("other").join("small.go");
        let text = format!(
            "package sample\nvar Value = \"😀old\"\n/* {} */\n",
            " ".repeat(LARGE_OPEN_BYTES)
        );
        let mut engine = Engine::new(project.root.clone());
        engine.open(large.clone(), text, 1).unwrap();
        let (open_generation, _) = engine.open_document_identity(&large).unwrap();
        assert!(!engine.state.documents.contains_key(&path_key(&large)));
        assert!(engine.pending_document_affects(&sibling));
        assert!(!engine.pending_document_affects(&small));
        engine
            .open(
                small.clone(),
                "package other\nvar Value = 1\n".to_owned(),
                1,
            )
            .unwrap();
        assert!(engine.snapshot().document(&small).unwrap().is_some());

        engine
            .edit(
                &large,
                &[TextDocumentContentChangeEvent {
                    range: Some(Range::new(Position::new(1, 15), Position::new(1, 18))),
                    range_length: None,
                    text: "new".to_owned(),
                }],
                2,
            )
            .unwrap();
        assert_eq!(
            engine.open_document_identity(&large),
            Some((open_generation, 2))
        );
        let completed = engine
            .open_parse_wake()
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        assert_eq!(engine.complete_open_parse(completed).unwrap(), None);
        assert!(engine.document_parse_pending(&large));
        let completed = engine
            .open_parse_wake()
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        assert_eq!(
            engine.complete_open_parse(completed).unwrap(),
            Some(large.clone())
        );
        let document = &engine.state.documents[&path_key(&large)];
        assert_eq!(document.open_generation, open_generation);
        assert_eq!(document.document_version, Some(2));
        assert!(document.text.contains("\"😀new\""));
        assert!(!document.tree.root_node().has_error());
        assert!(
            document
                .declarations()
                .symbols
                .declaration("Value")
                .is_some()
        );
    }
}
