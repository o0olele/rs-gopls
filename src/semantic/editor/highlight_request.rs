use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Result;
use lsp_types::{DocumentHighlight, Position};
use tree_sitter::Parser;

use super::Editor;
use super::document_highlight::{self, HighlightKey};
use crate::semantic::Engine;
use crate::semantic::index::{IndexedSource, IndexedSourceRevision};
use crate::semantic::snapshot::DocumentSnapshot;
use crate::semantic::source::Source;
use crate::{telemetry, workspace::path_key};

const INLINE_RESULTS: usize = 256;
const CACHED_BINDINGS: usize = 128;
const CACHED_POSITIONS: usize = 512;
const CACHED_RANGES: usize = 65_536;

#[derive(Clone)]
struct Highlights {
    results: Arc<[DocumentHighlight]>,
    candidates: usize,
    path: &'static str,
}

#[derive(Default)]
pub(in crate::semantic) struct HighlightCache(Mutex<HighlightEntries>);

#[derive(Default)]
struct HighlightEntries {
    bindings: HashMap<HighlightKey, Highlights>,
    positions: HashMap<Position, HighlightKey>,
    ranges: usize,
}

impl HighlightCache {
    fn at(&self, position: Position) -> Option<Highlights> {
        let cache = self.0.lock().unwrap();
        cache.bindings.get(cache.positions.get(&position)?).cloned()
    }

    fn binding(&self, key: &HighlightKey) -> Option<Highlights> {
        self.0.lock().unwrap().bindings.get(key).cloned()
    }

    fn insert(&self, position: Position, key: HighlightKey, highlights: Highlights) {
        let mut cache = self.0.lock().unwrap();
        if !cache.bindings.contains_key(&key) {
            if cache.bindings.len() >= CACHED_BINDINGS
                || cache.ranges.saturating_add(highlights.results.len()) > CACHED_RANGES
            {
                *cache = HighlightEntries::default();
            }
            cache.ranges += highlights.results.len();
            cache.bindings.insert(key.clone(), highlights);
        }
        if cache.positions.len() >= CACHED_POSITIONS {
            cache.positions.clear();
        }
        cache.positions.insert(position, key);
    }
}

pub(crate) enum HighlightPreparation {
    Ready(Vec<DocumentHighlight>),
    Background(HighlightInput),
}

pub(crate) struct HighlightInput {
    path: PathBuf,
    position: Position,
    source: HighlightSource,
}

enum HighlightSource {
    Open(IndexedSourceRevision),
    Cached(Arc<HighlightCache>, Highlights),
    Disk(PathBuf),
}

pub(crate) struct HighlightOutput {
    path: PathBuf,
    position: Position,
    revision: Option<Arc<HighlightCache>>,
    target: Option<HighlightKey>,
    highlights: Highlights,
}

impl HighlightOutput {
    pub(crate) fn results(&self) -> &[DocumentHighlight] {
        &self.highlights.results
    }
}

impl Engine {
    pub(crate) fn prepare_highlight(
        &self,
        path: &Path,
        position: Position,
    ) -> Result<HighlightPreparation> {
        let start = telemetry::now();
        let key = path_key(path);
        let source = if let Some(source) = self.state.documents.get(&key) {
            if let Some(highlights) = source.highlights.at(position) {
                if highlights.results.len() <= INLINE_RESULTS {
                    trace(start, &highlights, true);
                    return Ok(HighlightPreparation::Ready(highlights.results.to_vec()));
                }
                HighlightSource::Cached(source.highlights.clone(), highlights)
            } else {
                HighlightSource::Open(source.revision())
            }
        } else if path.extension().is_some_and(|extension| extension == "go") {
            HighlightSource::Disk(path.to_owned())
        } else {
            return Ok(HighlightPreparation::Ready(Vec::new()));
        };
        Ok(HighlightPreparation::Background(HighlightInput {
            path: key,
            position,
            source,
        }))
    }

    pub(crate) fn finish_highlight(&self, output: HighlightOutput) -> bool {
        let is_current = match (&output.revision, self.state.documents.get(&output.path)) {
            (Some(revision), Some(source)) => Arc::ptr_eq(revision, &source.highlights),
            (None, None) => true,
            _ => false,
        };
        if !is_current {
            return false;
        }
        if let (Some(revision), Some(target)) = (output.revision, output.target) {
            revision.insert(output.position, target, output.highlights);
        }
        true
    }

    #[cfg(test)]
    pub(crate) fn document_highlight(
        &mut self,
        path: &Path,
        position: Position,
    ) -> Result<Vec<DocumentHighlight>> {
        Ok(match self.prepare_highlight(path, position)? {
            HighlightPreparation::Ready(result) => result,
            HighlightPreparation::Background(input) => {
                let output = input.analyze(&AtomicBool::new(false))?.unwrap();
                let results = output.results().to_vec();
                assert!(self.finish_highlight(output));
                results
            }
        })
    }
}

impl HighlightInput {
    pub(crate) fn analyze(self, cancelled: &AtomicBool) -> Result<Option<HighlightOutput>> {
        let result = self.compute(cancelled);
        if cancelled.load(Relaxed) {
            return Ok(None);
        }
        result
    }

    fn compute(self, cancelled: &AtomicBool) -> Result<Option<HighlightOutput>> {
        if cancelled.load(Relaxed) {
            return Ok(None);
        }
        let start = telemetry::now();
        let Self {
            path,
            position,
            source,
        } = self;
        let (source, open) = match source {
            HighlightSource::Cached(revision, highlights) => {
                trace(start, &highlights, true);
                return Ok(Some(HighlightOutput {
                    path,
                    position,
                    revision: Some(revision),
                    target: None,
                    highlights,
                }));
            }
            HighlightSource::Open(revision) => (revision.into_indexed(), true),
            HighlightSource::Disk(disk_path) => {
                let mut parser = Parser::new();
                parser.set_language(&tree_sitter_go::LANGUAGE.into())?;
                (
                    IndexedSource::new(
                        Source::read_cancellable(&mut parser, &disk_path, cancelled)?,
                        None,
                    ),
                    false,
                )
            }
        };
        let document = DocumentSnapshot::borrowed(&path, &source);
        let editor = Editor::new(&document);
        let (target, highlights, hit) =
            if let Some(target) = document_highlight::prepare(&editor, position) {
                let key = target.key();
                if let Some(highlights) = source.highlights.binding(&key) {
                    (Some(key), highlights, true)
                } else {
                    let path = target.path();
                    let Some((results, candidates)) =
                        document_highlight::compute(&editor, target, Some(cancelled))
                    else {
                        return Ok(None);
                    };
                    (
                        Some(key),
                        Highlights {
                            results: results.into(),
                            candidates,
                            path,
                        },
                        false,
                    )
                }
            } else {
                (
                    None,
                    Highlights {
                        results: Arc::default(),
                        candidates: 0,
                        path: "empty",
                    },
                    false,
                )
            };
        trace(start, &highlights, hit);
        Ok(Some(HighlightOutput {
            path,
            position,
            revision: open.then(|| source.highlights.clone()),
            target,
            highlights,
        }))
    }
}

fn trace(start: Option<Instant>, highlights: &Highlights, cache_hit: bool) {
    if let Some(start) = start {
        telemetry::emit(
            "document_highlight",
            serde_json::json!({
                "cache_hit": cache_hit,
                "path": highlights.path,
                "candidates": highlights.candidates,
                "elapsed_ms": telemetry::elapsed(start),
            }),
        );
    }
}
