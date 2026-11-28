use std::collections::{HashMap, HashSet};
use std::ops::Range as ByteRange;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use anyhow::{Context, Result};
use tree_sitter::Parser;

use super::worker::{ReferenceDependencies, ReferenceDependencyPool};
use super::{
    AnchorResult, PREFILTER_CHUNK_BYTES, ReferenceAnchor, ReferenceCandidate,
    ReferenceObservations, ReferenceTarget,
};
use crate::semantic::index::{
    DiskIndex, IndexReader, IndexedSource, IndexedSourceRevision, NavigationText,
    PreparedReferenceSource, ReferenceIdentifiers, draft_files, missing_source,
};
use crate::semantic::navigation_trace::{NavigationCounts, NavigationProbe, record, verification};
use crate::semantic::resolver::Resolver;
use crate::semantic::shared_disk::SharedDiskCache;
use crate::semantic::source::{ReferenceQuery, Source, check_cancelled};
use crate::workspace::{Package, Workspace, path_key};

pub(super) struct AnchorWork {
    pub(super) snapshot: Arc<ReferenceWorkerSnapshot>,
    pub(super) anchor: ReferenceAnchor,
}

pub(super) struct CandidateDiscovery {
    pub(super) snapshot: Arc<ReferenceWorkerSnapshot>,
    pub(super) candidate: Arc<ReferenceCandidate>,
    pub(super) target: Arc<ReferenceTarget>,
}

pub(super) struct DiscoveredCandidate {
    pub(super) input: CandidateDiscovery,
    pub(super) text: Option<NavigationText>,
    pub(super) should_verify: bool,
    pub(super) has_container: bool,
    identifiers: Option<Arc<ReferenceIdentifiers>>,
    pub(super) missing: bool,
    pub(super) counts: NavigationCounts,
}

pub(super) struct CandidateWork {
    pub(super) discovery: DiscoveredCandidate,
}

pub(super) struct CandidateResult {
    pub(super) candidate: Arc<ReferenceCandidate>,
    pub(super) source: Option<PreparedReferenceSource>,
    pub(super) changed: bool,
    pub(super) matches: Vec<ByteRange<usize>>,
    pub(super) counts: NavigationCounts,
    pub(super) missing: bool,
}

pub(super) struct ReferenceWorkerSnapshot {
    workspace: Arc<Workspace>,
    shared_disk: Arc<SharedDiskCache>,
    overlays: HashMap<PathBuf, IndexedSourceRevision>,
    new_documents: HashSet<PathBuf>,
    observations: Arc<ReferenceObservations>,
    root: PathBuf,
    trace: bool,
}

pub(super) struct ReferenceAnchorResult {
    pub(super) anchor: AnchorResult,
    pub(super) source: Option<PreparedReferenceSource>,
    pub(super) missing: bool,
    pub(super) counts: NavigationCounts,
}

impl ReferenceWorkerSnapshot {
    pub(super) fn capture(
        workspace: Arc<Workspace>,
        indexes: &IndexReader<'_>,
        new_documents: &HashSet<PathBuf>,
        root: &Path,
        observations: Arc<ReferenceObservations>,
    ) -> Arc<Self> {
        Arc::new(Self {
            workspace,
            shared_disk: indexes.shared_disk.clone(),
            overlays: indexes
                .documents
                .iter()
                .map(|(key, source)| (key.clone(), source.revision()))
                .collect(),
            new_documents: new_documents.clone(),
            observations,
            root: root.to_owned(),
            trace: indexes.navigation.is_some(),
        })
    }

    fn text(
        &self,
        key: &Path,
        path: &Path,
        cancelled: &AtomicBool,
        probe: Option<&NavigationProbe>,
    ) -> Result<NavigationText> {
        if let Some(text) = self.observations.cached_text(&self.shared_disk, path, key) {
            record(probe, |counts| counts.text_hits += 1);
            return Ok(text);
        }
        let text = self.shared_disk.read(path, Some(cancelled), probe);
        self.observe(key, &text, |text| text);
        text
    }

    fn observe<T>(&self, key: &Path, result: &Result<T>, text: impl FnOnce(&T) -> &NavigationText) {
        match result {
            Ok(value) => self.observations.remember(key, Some(text(value))),
            Err(error) if missing_source(error) => self.observations.remember(key, None),
            _ => {}
        }
    }

    fn prepare(
        &self,
        key: &Path,
        path: &Path,
        parser: &mut Parser,
        cancelled: &AtomicBool,
        text: Option<NavigationText>,
        probe: Option<&NavigationProbe>,
    ) -> Result<Option<PreparedReferenceSource>> {
        if self.overlays.contains_key(key) {
            return Ok(None);
        }
        let text = text.or_else(|| self.observations.cached_text(&self.shared_disk, path, key));
        if let Some(source) = text
            .as_ref()
            .and_then(|text| self.shared_disk.cached_parse(path, text))
        {
            record(probe, |counts| counts.text_hits += 1);
            return Ok(Some(source));
        }
        let prepared = self
            .shared_disk
            .load_prefetched(parser, path, text, Some(cancelled), probe);
        self.observe(key, &prepared, |prepared| &prepared.text);
        prepared.map(Some)
    }

    fn resolve<T>(
        &self,
        parser: &mut Parser,
        dependencies: &ReferenceDependencyPool,
        cancelled: &Arc<AtomicBool>,
        key: &Path,
        prepared: Option<&PreparedReferenceSource>,
        probe: Option<&NavigationProbe>,
        operation: impl FnOnce(
            &mut Resolver<'_, '_>,
            &IndexedSource,
            &Path,
            &HashSet<PathBuf>,
        ) -> Result<T>,
    ) -> Result<T> {
        check_cancelled(cancelled)?;
        let documents: HashMap<_, _> = self
            .overlays
            .iter()
            .map(|(key, revision)| (key.clone(), revision.clone().into_indexed()))
            .collect();
        let borrowed = documents.get(key);
        let owned = prepared.map(|prepared| Rc::new(prepared.clone().into_indexed()));
        let source = borrowed
            .or(owned.as_deref())
            .context("missing reference source")?;
        let mut disk_index = HashMap::new();
        if borrowed.is_none() {
            disk_index.insert(
                key.to_owned(),
                DiskIndex::from_prepared(prepared.expect("disk source")),
            );
        }
        let reference_dependencies = ReferenceDependencies {
            pool: dependencies,
            cancelled,
            observations: &self.observations,
        };
        let indexes = IndexReader {
            workspace: &self.workspace,
            parser,
            shared_disk: &self.shared_disk,
            foreground: None,
            drafts: draft_files(&self.workspace, &documents, &self.new_documents),
            documents: &documents,
            pending_documents: None,
            disk_index: &mut disk_index,
            request_declarations: Default::default(),
            use_captured_disk: false,
            navigation_texts: &mut HashMap::new(),
            owned_reference: owned.as_ref().map(|source| (key, source.clone())),
            navigation: probe,
            reference_dependencies: Some(&reference_dependencies),
            implementation_parse_time: None,
            disk_names: &mut HashMap::new(),
            completion: None,
            inferred_weights: &mut HashMap::new(),
            cancelled: Some(cancelled),
            diagnostic_types: None,
            diagnostic_observations: None,
            package_indexes: Default::default(),
        };
        let mut resolver = Resolver::new(&self.workspace, indexes, true);
        let result = operation(&mut resolver, source, &self.root, &self.new_documents)?;
        check_cancelled(cancelled)?;
        Ok(result)
    }
}

impl AnchorWork {
    pub(super) fn run(
        self,
        parser: &mut Parser,
        dependencies: &ReferenceDependencyPool,
        cancelled: &Arc<AtomicBool>,
    ) -> Result<ReferenceAnchorResult> {
        let Self { snapshot, anchor } = self;
        let probe = NavigationProbe::default();
        let trace = snapshot.trace.then_some(&probe);
        let prepared = snapshot.prepare(&anchor.key, &anchor.path, parser, cancelled, None, trace);
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) if missing_source(&error) => {
                return Ok(ReferenceAnchorResult {
                    anchor: AnchorResult::Local(Vec::new()),
                    source: None,
                    missing: true,
                    counts: probe.into_counts(),
                });
            }
            Err(error) => return Err(error),
        };
        let anchor_parses = probe.parse_count();
        let result = snapshot.resolve(
            parser,
            dependencies,
            cancelled,
            &anchor.key,
            prepared.as_ref(),
            trace,
            |resolver, source, root, new_documents| {
                anchor.plan(resolver, source, root, new_documents)
            },
        )?;
        let mut counts = probe.into_counts();
        counts.reference_anchor_parses = anchor_parses;
        counts.reference_dependency_parses = counts.ast_parses - anchor_parses;
        Ok(ReferenceAnchorResult {
            anchor: result,
            source: prepared,
            counts,
            missing: false,
        })
    }
}

impl CandidateDiscovery {
    pub(super) fn run(self, cancelled: &AtomicBool) -> Result<DiscoveredCandidate> {
        let probe = NavigationProbe::default();
        let trace = self.snapshot.trace.then_some(&probe);
        let key = &self.candidate.key;
        let overlay = self
            .snapshot
            .overlays
            .get(key)
            .cloned()
            .map(IndexedSourceRevision::into_indexed);
        let text = if overlay.is_some() {
            None
        } else {
            let result = self
                .snapshot
                .text(key, &self.candidate.path, cancelled, trace);
            match result {
                Ok(text) => Some(text),
                Err(error) if missing_source(&error) => {
                    return Ok(DiscoveredCandidate {
                        input: self,
                        text: None,
                        should_verify: false,
                        has_container: false,
                        identifiers: None,
                        missing: true,
                        counts: probe.into_counts(),
                    });
                }
                Err(error) => return Err(error),
            }
        };
        let bytes = overlay
            .as_ref()
            .map(|source| source.text.as_str())
            .or_else(|| text.as_ref().map(|text| text.text.as_str()))
            .expect("candidate has text");
        let (should_verify, has_container, identifiers) =
            if contains_spelling(bytes, &self.target.spelling, cancelled)? {
                let identifiers = if let Some(overlay) = &overlay {
                    overlay.reference_identifiers(cancelled)?
                } else {
                    self.snapshot
                        .shared_disk
                        .reference_identifiers(text.as_ref().unwrap(), cancelled)?
                };
                let imports_target =
                    match (self.target.direct_import.as_deref(), identifiers.imports()) {
                        (Some(owner), Some(imports)) if self.allows_import_filter(owner) => imports
                            .iter()
                            .any(|import| self.accepts_import(import, owner)),
                        _ => true,
                    };
                let should_verify = imports_target
                    && identifiers
                        .offsets(&self.target.spelling)
                        .is_none_or(|offsets| !offsets.is_empty());
                let has_container = self.target.container.as_deref().is_some_and(|name| {
                    identifiers
                        .offsets(name)
                        .map_or_else(|| bytes.contains(name), |offsets| !offsets.is_empty())
                });
                (should_verify, has_container, Some(identifiers))
            } else {
                (false, false, None)
            };
        let mut counts = probe.into_counts();
        counts.prefilter_reads = counts.disk_reads;
        counts.prefilter_bytes = bytes.len();
        counts.prefilter_skipped = usize::from(!should_verify);
        Ok(DiscoveredCandidate {
            input: self,
            text,
            should_verify,
            has_container,
            identifiers,
            counts,
            missing: false,
        })
    }

    fn allows_import_filter(&self, owner: &str) -> bool {
        let Some(package) = self.snapshot.workspace.package(&self.candidate.package_id) else {
            return false;
        };
        self.candidate.priority >= 2 && !package.load_error && package.import_path() != owner
    }

    fn accepts_import(&self, import: &str, owner: &str) -> bool {
        let Some(package) = self.snapshot.workspace.package(&self.candidate.package_id) else {
            return true;
        };
        import.starts_with('.')
            || import == owner
            || self
                .snapshot
                .workspace
                .resolve_import(package, import)
                .is_some_and(|target| target.import_path() == owner)
    }
}

impl CandidateWork {
    pub(super) fn run(
        self,
        parser: &mut Parser,
        dependencies: &ReferenceDependencyPool,
        cancelled: &Arc<AtomicBool>,
    ) -> Result<CandidateResult> {
        let DiscoveredCandidate {
            input,
            text,
            identifiers,
            ..
        } = self.discovery;
        let CandidateDiscovery {
            snapshot,
            candidate,
            target,
        } = input;
        let probe = NavigationProbe::default();
        let trace = snapshot.trace.then_some(&probe);
        let started = Instant::now();
        let prepared = match snapshot.prepare(
            &candidate.key,
            &candidate.path,
            parser,
            cancelled,
            text.clone(),
            trace,
        ) {
            Ok(prepared) => prepared,
            Err(error) if missing_source(&error) => {
                return Ok(CandidateResult {
                    candidate,
                    source: None,
                    changed: false,
                    matches: Vec::new(),
                    counts: probe.into_counts(),
                    missing: true,
                });
            }
            Err(error) => return Err(error),
        };
        if let Some((before, prepared)) = text.as_ref().zip(prepared.as_ref())
            && !Arc::ptr_eq(&before.text, &prepared.text.text)
            && before.text != prepared.text.text
        {
            return Ok(CandidateResult {
                candidate,
                source: None,
                changed: true,
                matches: Vec::new(),
                counts: probe.into_counts(),
                missing: false,
            });
        }
        let candidate_parses = probe.parse_count();
        let matches = snapshot.resolve(
            parser,
            dependencies,
            cancelled,
            &candidate.key,
            prepared.as_ref(),
            trace,
            |resolver, source, root, _| {
                let package = resolver
                    .workspace
                    .package(&candidate.package_id)
                    .context("reference candidate package changed")?;
                verify_candidates(
                    resolver,
                    package,
                    source,
                    &candidate.key,
                    &target,
                    root,
                    identifiers.as_deref(),
                    cancelled,
                )
            },
        )?;
        let mut counts = probe.into_counts();
        counts.reference_candidate_parses = candidate_parses;
        counts.reference_dependency_parses = counts.ast_parses - candidate_parses;
        if snapshot.trace {
            counts.file_elapsed(&candidate.path, started.elapsed());
        }
        Ok(CandidateResult {
            candidate,
            source: prepared,
            changed: false,
            matches,
            counts,
            missing: false,
        })
    }
}

fn verify_candidates<'w>(
    resolver: &mut Resolver<'w, '_>,
    package: &'w Package,
    source: &IndexedSource,
    key: &Path,
    target: &ReferenceTarget,
    root: &Path,
    identifiers: Option<&ReferenceIdentifiers>,
    cancelled: &AtomicBool,
) -> Result<Vec<ByteRange<usize>>> {
    let probe = resolver.indexes.navigation;
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
            && verify_reference(resolver, package, source, key, candidate, target, root)?
        {
            let range = candidate.name().byte_range();
            if matches.last() != Some(&range) {
                matches.push(range);
            }
        }
        Ok(())
    };

    if let Some(offsets) = source.ready_name_offsets(&target.spelling) {
        record(probe, |counts| counts.cached_offset_files += 1);
        for &offset in offsets {
            verify(offset, true)?;
        }
    } else if !source.tree.root_node().has_error()
        && let Some(offsets) = identifiers.and_then(|names| names.offsets(&target.spelling))
    {
        for &offset in offsets {
            verify(offset, false)?;
        }
    } else {
        // Recovery nodes can split malformed tokens into real references.
        // The old byte scan preserves those bindings.
        let text = source.text.as_str();
        let mut cursor = 0;
        while cursor < text.len() {
            check_cancelled(cancelled)?;
            let end = text.ceil_char_boundary((cursor + PREFILTER_CHUNK_BYTES).min(text.len()));
            let overlap =
                text.ceil_char_boundary((end + target.spelling.len() - 1).min(text.len()));
            while let Some(found) = text[cursor..overlap].find(&target.spelling) {
                let offset = cursor + found;
                if offset >= end {
                    break;
                }
                verify(offset, false)?;
                cursor = text.ceil_char_boundary(offset + 1);
            }
            cursor = end;
        }
    }
    Ok(matches)
}

pub(in crate::semantic) fn contains_spelling(
    text: &str,
    spelling: &str,
    cancelled: &AtomicBool,
) -> Result<bool> {
    let mut start = 0;
    while start < text.len() {
        check_cancelled(cancelled)?;
        let end = text.ceil_char_boundary((start + PREFILTER_CHUNK_BYTES).min(text.len()));
        let overlap = text.ceil_char_boundary((end + spelling.len() - 1).min(text.len()));
        if text[start..overlap].contains(spelling) {
            return Ok(true);
        }
        start = end;
    }
    Ok(false)
}

fn verify_reference<'w>(
    resolver: &mut Resolver<'w, '_>,
    package: &'w Package,
    source: &Source,
    key: &Path,
    candidate: ReferenceQuery<'_>,
    target: &ReferenceTarget,
    root: &Path,
) -> Result<bool> {
    if !target.include_declaration
        && key == target.path
        && source.range(candidate.name()) == target.range
    {
        return Ok(false);
    }
    let declaration = resolver.reference_location(package, source, candidate, root)?;
    Ok(declaration
        .is_some_and(|(path, range)| range == target.range && path_key(&path) == target.path))
}
