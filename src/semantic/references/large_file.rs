use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result};
use lsp_types::Range;
use tree_sitter::Parser;

use super::{AnchorResult, FileScan, ReferenceAnchor, ReferenceTarget};
use crate::semantic::index::{
    DiskIndex, FileIndex, IndexReader, IndexedSource, IndexedSourceRevision, NavigationText,
    draft_files,
};
use crate::semantic::navigation_trace::{NavigationCounts, NavigationProbe, record, verification};
use crate::semantic::resolver::Resolver;
use crate::semantic::source::{ReferenceQuery, Source, check_cancelled};
use crate::telemetry;
use crate::workspace::{Package, Workspace, path_key};

const SEED_FILES_PER_PACKAGE: usize = 32;

pub(super) struct LargeFileVerification {
    snapshot: ReferenceWorkerSnapshot,
    target: ReferenceTarget,
    scan: FileScan,
    source: IndexedSourceRevision,
}

pub(super) struct LargeReferenceAnchor {
    snapshot: ReferenceWorkerSnapshot,
    anchor: ReferenceAnchor,
    source: IndexedSourceRevision,
}

struct ReferenceWorkerSnapshot {
    workspace: Arc<Workspace>,
    overlays: HashMap<PathBuf, IndexedSourceRevision>,
    new_documents: HashSet<PathBuf>,
    declarations: HashMap<PathBuf, (Arc<FileIndex>, u64)>,
    texts: HashMap<PathBuf, NavigationText>,
    root: PathBuf,
}

pub(super) struct LargeFileResult {
    pub(super) matches: Vec<(PathBuf, Range)>,
    pub(super) counts: Option<NavigationCounts>,
}

pub(super) struct LargeAnchorResult {
    pub(super) anchor: AnchorResult,
    pub(super) counts: Option<NavigationCounts>,
}

impl LargeFileVerification {
    pub(super) fn capture(
        workspace: Arc<Workspace>,
        indexes: &IndexReader<'_>,
        new_documents: &HashSet<PathBuf>,
        root: &Path,
        target: ReferenceTarget,
        scan: FileScan,
        source: &IndexedSource,
    ) -> Self {
        // Seed the declaration and candidate packages without copying a
        // workspace-sized index. Other sources stay lazy in the worker.
        let keys = [&target.path, &scan.candidate.key]
            .into_iter()
            .cloned()
            .chain(
                workspace
                    .package_of_file(&target.path)
                    .into_iter()
                    .chain(workspace.package(&scan.candidate.package_id))
                    .flat_map(|package| package.files.iter().take(SEED_FILES_PER_PACKAGE))
                    .map(|path| path_key(path)),
            );
        let snapshot =
            ReferenceWorkerSnapshot::capture(workspace.clone(), indexes, new_documents, root, keys);
        Self {
            snapshot,
            target,
            scan,
            source: source.revision(),
        }
    }

    pub(super) fn run(
        self,
        parser: &mut Parser,
        cancelled: &AtomicBool,
    ) -> Result<LargeFileResult> {
        let Self {
            snapshot,
            target,
            mut scan,
            source,
        } = self;
        let key = scan.candidate.key.clone();
        let (matches, counts) = snapshot.resolve(
            parser,
            cancelled,
            &key,
            source,
            |resolver, source, root, _| {
                scan.verify_large(resolver, source, root, &target, cancelled)
            },
        )?;
        Ok(LargeFileResult { matches, counts })
    }
}

impl FileScan {
    fn verify_large(
        &mut self,
        resolver: &mut Resolver<'_, '_>,
        source: &Source,
        root: &Path,
        target: &ReferenceTarget,
        cancelled: &AtomicBool,
    ) -> Result<Vec<(PathBuf, Range)>> {
        let probe = resolver.indexes.navigation;
        let package = resolver
            .workspace
            .package(&self.candidate.package_id)
            .context("reference candidate package changed")?;
        let key = self.candidate.key.clone();
        let mut matches = Vec::new();
        let mut verify = |offset, cached_offset| -> Result<()> {
            check_cancelled(cancelled)?;
            let _verify = verification(probe);
            record(probe, |counts| counts.ident_offsets_checked += 1);
            let candidate = if cached_offset {
                source.reference_at_offset(offset)
            } else {
                source.reference_at_spelling(offset, &target.spelling)
            };
            if let Some(candidate) = candidate
                && let Some(range) =
                    verify_reference(resolver, package, source, &key, candidate, target, root)?
                && matches.last().is_none_or(|(_, last)| *last != range)
            {
                matches.push((source.path.clone(), range));
            }
            Ok(())
        };
        if self.text.is_none()
            && let Some(offsets) = source.ready_name_offsets(&target.spelling)
        {
            if !self.cached_offsets {
                record(probe, |counts| counts.cached_offset_files += 1);
            }
            for &offset in &offsets[self.hot_index..] {
                verify(offset, true)?;
            }
        } else {
            self.text.get_or_insert_with(|| source.text.clone());
            loop {
                check_cancelled(cancelled)?;
                if let Some(offset) = self.offsets.pop_front() {
                    verify(offset, false)?;
                } else if self.cursor < source.text.len() {
                    self.scan_chunk(&target.spelling, probe);
                } else {
                    break;
                }
            }
            if !self.had_spelling {
                record(probe, |counts| counts.prefilter_skipped += 1);
            }
        }
        self.record_completion(probe);
        Ok(matches)
    }
}

impl LargeReferenceAnchor {
    pub(super) fn capture(
        workspace: Arc<Workspace>,
        indexes: &IndexReader<'_>,
        new_documents: &HashSet<PathBuf>,
        root: &Path,
        anchor: ReferenceAnchor,
        source: &IndexedSource,
    ) -> Self {
        let keys = std::iter::once(anchor.key.clone()).chain(
            workspace
                .package_of_file(&anchor.key)
                .into_iter()
                .flat_map(|package| package.files.iter().take(SEED_FILES_PER_PACKAGE))
                .map(|path| path_key(path)),
        );
        let snapshot =
            ReferenceWorkerSnapshot::capture(workspace.clone(), indexes, new_documents, root, keys);
        Self {
            snapshot,
            anchor,
            source: source.revision(),
        }
    }

    pub(super) fn run(
        self,
        parser: &mut Parser,
        cancelled: &AtomicBool,
    ) -> Result<LargeAnchorResult> {
        let (anchor, counts) = self.snapshot.resolve(
            parser,
            cancelled,
            &self.anchor.key,
            self.source,
            |resolver, source, root, new_documents| {
                self.anchor.plan(resolver, source, root, new_documents)
            },
        )?;
        Ok(LargeAnchorResult { anchor, counts })
    }
}

impl ReferenceWorkerSnapshot {
    fn capture(
        workspace: Arc<Workspace>,
        indexes: &IndexReader<'_>,
        new_documents: &HashSet<PathBuf>,
        root: &Path,
        keys: impl Iterator<Item = PathBuf>,
    ) -> Self {
        let mut declarations = HashMap::new();
        let mut texts = HashMap::new();
        for key in keys {
            if let Some(index) = indexes.disk_index.get(&key) {
                declarations.insert(key.clone(), index.reference_metadata());
            }
            if let Some(text) = indexes.navigation_texts.get(&key) {
                texts.insert(key, text.clone());
            }
        }
        Self {
            workspace,
            overlays: indexes
                .documents
                .iter()
                .map(|(key, source)| (key.clone(), source.revision()))
                .collect(),
            new_documents: new_documents.clone(),
            declarations,
            texts,
            root: root.to_owned(),
        }
    }

    fn resolve<T>(
        self,
        parser: &mut Parser,
        cancelled: &AtomicBool,
        key: &Path,
        source: IndexedSourceRevision,
        operation: impl FnOnce(
            &mut Resolver<'_, '_>,
            &IndexedSource,
            &Path,
            &HashSet<PathBuf>,
        ) -> Result<T>,
    ) -> Result<(T, Option<NavigationCounts>)> {
        check_cancelled(cancelled)?;
        let Self {
            workspace,
            overlays,
            new_documents,
            declarations,
            mut texts,
            root,
        } = self;
        let source = Rc::new(source.into_indexed());
        let documents = overlays
            .into_iter()
            .map(|(key, revision)| (key, revision.into_indexed()))
            .collect::<HashMap<_, _>>();
        let mut disk_index = declarations
            .into_iter()
            .map(|(key, metadata)| (key, DiskIndex::from_reference_metadata(metadata)))
            .collect::<HashMap<_, _>>();
        if !documents.contains_key(key) {
            disk_index.insert(
                key.to_owned(),
                DiskIndex::with_reference_revision(source.clone(), cancelled)?,
            );
        }
        let probe = telemetry::enabled().then(NavigationProbe::default);
        let indexes = IndexReader {
            workspace: &workspace,
            parser,
            drafts: draft_files(&workspace, &documents, &new_documents),
            documents: &documents,
            disk_index: &mut disk_index,
            navigation_texts: &mut texts,
            navigation: probe.as_ref(),
            deferred_reference: None,
            implementation_parse_time: None,
            disk_names: &mut HashMap::new(),
            completion: None,
            inferred_weights: &mut HashMap::new(),
            cancelled: Some(cancelled),
            diagnostic_types: None,
        };
        let mut resolver = Resolver::new(&workspace, indexes, false);
        let result = operation(&mut resolver, &source, &root, &new_documents)?;
        check_cancelled(cancelled)?;
        Ok((result, probe.map(NavigationProbe::into_counts)))
    }
}

pub(super) fn verify_reference<'w>(
    resolver: &mut Resolver<'w, '_>,
    package: &'w Package,
    source: &Source,
    key: &Path,
    candidate: ReferenceQuery<'_>,
    target: &ReferenceTarget,
    root: &Path,
) -> Result<Option<Range>> {
    let range = source.range(candidate.name());
    if !target.include_declaration && key == target.path && range == target.range {
        return Ok(None);
    }
    let declaration = resolver.reference_location(package, source, candidate, root)?;
    Ok(declaration
        .filter(|(path, range)| *range == target.range && path_key(path) == target.path)
        .map(|_| range))
}
