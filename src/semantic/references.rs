use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use lsp_types::{Position, Range};
use tree_sitter::Node;

use super::Engine;
use super::index::{
    CachedNavigationText, IndexReader, NavigationText, active_package, draft_package,
};
use super::navigation_trace::{
    NavigationKind, NavigationProbe, active, file_sample, record, sample, verification,
};
use super::query::{QueryProgress, SemanticTask};
use super::resolver::{Resolver, is_exported};
use super::shared_disk::SharedDiskCache;
use super::source::{ReferenceQuery, Source, is_declaration_name, node_text};
use crate::scope::reference_binding;
use crate::workspace::{Package, path_key};

mod candidates;
mod large_file;
#[cfg(test)]
mod tests;
mod worker;

use candidates::{CandidateInput, CandidateList, OverlayTopology, ReferenceScope};
pub(super) use large_file::contains_spelling;
use large_file::{AnchorWork, CandidateDiscovery, CandidateWork, ReferenceWorkerSnapshot};
use worker::{BATCH_SIZE, PendingWork, Work, WorkOutput};
pub(super) use worker::{DependencySource, ReferenceDependencies, ReferenceWorker};

const PREFILTER_CHUNK_BYTES: usize = 32 * 1024;
const LARGE_FILE_BYTES: usize = 128 * 1024;

pub(crate) struct ReferenceSearch {
    origin: ReferenceAnchor,
    anchor: Option<ReferenceAnchor>,
    plan: Option<ReferencePlan>,
    target: Option<Arc<ReferenceTarget>>,
    files: Vec<Arc<ReferenceCandidate>>,
    next_file: usize,
    deferred: VecDeque<Arc<ReferenceCandidate>>,
    processing_deferred: bool,
    matches: Vec<(PathBuf, Range)>,
    verified: BTreeMap<PathBuf, (PathBuf, Vec<Range>)>,
    verified_count: usize,
    work: Option<PendingWork>,
    source_revision: u64,
    observations: Arc<ReferenceObservations>,
    snapshot: Option<Arc<ReferenceWorkerSnapshot>>,
    disk_retries: usize,
}

#[derive(Default)]
pub(super) struct ReferenceObservations(Mutex<ObservedFiles>);

#[derive(Default)]
struct ObservedFiles {
    files: HashMap<PathBuf, Option<CachedNavigationText>>,
    changed: bool,
}

impl ReferenceObservations {
    pub(super) fn remember(&self, key: &Path, text: Option<&NavigationText>) {
        let mut observed = self.0.lock().unwrap();
        if let Some(previous) = observed.files.get(key) {
            let matches = match (previous, text) {
                (Some(previous), Some(text)) => previous.same_revision(text),
                (None, None) => true,
                _ => false,
            };
            observed.changed |= !matches;
        } else {
            observed
                .files
                .insert(key.to_owned(), text.map(NavigationText::downgrade));
        }
    }

    pub(super) fn cached_text(
        &self,
        cache: &SharedDiskCache,
        path: &Path,
        key: &Path,
    ) -> Option<NavigationText> {
        if let Some(text) = self
            .0
            .lock()
            .unwrap()
            .files
            .get(key)
            .and_then(Option::as_ref)
            .and_then(CachedNavigationText::upgrade)
        {
            return Some(text);
        }
        let text = cache.cached_read(path, key)?;
        self.remember(key, Some(&text));
        Some(text)
    }

    fn changed(&self) -> bool {
        self.0.lock().unwrap().changed
    }

    fn mark_changed(&self) {
        self.0.lock().unwrap().changed = true;
    }

    fn current(&self) -> bool {
        let observed = self.0.lock().unwrap();
        !observed.changed
            && observed.files.iter().all(|(key, text)| {
                text.as_ref().map_or_else(
                    || {
                        std::fs::metadata(key)
                            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                    },
                    |text| text.matches_disk(key),
                )
            })
    }
}

#[derive(Clone)]
struct ReferenceAnchor {
    key: PathBuf,
    path: PathBuf,
    position: Position,
    include_declaration: bool,
}

#[derive(Clone)]
struct ReferenceTarget {
    spelling: String,
    path: PathBuf,
    range: Range,
    include_declaration: bool,
    direct_import: Option<String>,
    container: Option<String>,
}

struct ReferencePlan {
    target: ReferenceTarget,
    scope: ReferenceScope,
    declaration: PathBuf,
}

#[derive(Default)]
pub(super) struct ReferenceCandidates {
    revisions: Option<(u64, u64)>,
    overlays: Arc<OverlayTopology>,
    scopes: HashMap<ReferenceScope, CandidateList>,
    total_files: usize,
}

impl ReferenceCandidates {
    fn refresh(
        &mut self,
        graph_revision: u64,
        declaration_revision: u64,
        indexes: &IndexReader<'_>,
        new_documents: &HashSet<PathBuf>,
    ) {
        let revisions = (graph_revision, declaration_revision);
        if self.revisions == Some(revisions) {
            return;
        }
        let next = OverlayTopology::capture(indexes.workspace, indexes.documents, new_documents);
        if self
            .revisions
            .is_some_and(|(graph, _)| graph == graph_revision)
        {
            self.scopes
                .retain(|_, list| list.same_scope_inputs(&self.overlays, &next));
        } else {
            self.scopes.clear();
        }
        self.total_files = self.scopes.values().map(|list| list.files.len()).sum();
        self.overlays = Arc::new(next);
        self.revisions = Some(revisions);
    }

    fn get(&self, scope: &ReferenceScope) -> Option<CandidateList> {
        self.scopes.get(scope).cloned()
    }

    fn insert(&mut self, scope: &ReferenceScope, candidates: CandidateList) {
        const MAX_CACHED_FILES: usize = 8192;
        const MAX_CACHED_SCOPES: usize = 16;
        if candidates.files.len() > MAX_CACHED_FILES || self.scopes.contains_key(scope) {
            return;
        }
        if self.total_files + candidates.files.len() > MAX_CACHED_FILES
            || self.scopes.len() >= MAX_CACHED_SCOPES
        {
            self.scopes.clear();
            self.total_files = 0;
        }
        self.total_files += candidates.files.len();
        self.scopes.insert(scope.clone(), candidates);
    }
}

#[derive(Clone)]
struct ReferenceCandidate {
    key: PathBuf,
    path: PathBuf,
    package_id: String,
    priority: u8,
}

impl Engine {
    pub(crate) fn references(
        &mut self,
        path: &Path,
        position: Position,
        include_declaration: bool,
    ) -> Result<QueryProgress<Vec<(PathBuf, Range)>, ReferenceSearch>> {
        let probe = active(NavigationKind::References);
        let probe = probe.as_deref();
        let _exec = sample(probe, |counts| &mut counts.exec);
        let key = path_key(path);
        let source_revision = self.state.completion_source_generation;
        let mut snapshot = self.state.snapshot(self.include_tests);
        let load = match snapshot.active_graph(true) {
            QueryProgress::Ready(_) => None,
            QueryProgress::Pending(load) => Some(load),
            QueryProgress::Searching(task) => match task {},
        };
        if let Some(source) = snapshot.docs().get(&key) {
            if source.text.len() < LARGE_FILE_BYTES || load.is_some() {
                let Some(reference) = source.reference_at(position) else {
                    return Ok(QueryProgress::Ready(Vec::new()));
                };
                if let Some(locations) =
                    local_references(source, reference, include_declaration, probe, None)
                {
                    return Ok(QueryProgress::Ready(locations));
                }
            }
        } else {
            if path.extension().is_none_or(|extension| extension != "go") {
                return Ok(QueryProgress::Ready(Vec::new()));
            }
            // A small local search can succeed even before a graph is installed.
            if load.is_some()
                && std::fs::metadata(path)
                    .is_ok_and(|metadata| metadata.len() < LARGE_FILE_BYTES as u64)
                && let Some(document) = snapshot.navigation_document(path, probe)?
            {
                let source = document.indexed();
                let Some(reference) = source.reference_at(position) else {
                    return Ok(QueryProgress::Ready(Vec::new()));
                };
                if let Some(locations) =
                    local_references(source, reference, include_declaration, probe, None)
                {
                    return Ok(QueryProgress::Ready(locations));
                }
            }
        }

        self.include_tests = true;
        if let Some(load) = load {
            record(probe, |counts| counts.workspace_wait());
            return Ok(QueryProgress::Pending(load));
        }
        Ok(QueryProgress::Searching(ReferenceSearch::new(
            ReferenceAnchor {
                key,
                path: path.to_owned(),
                position,
                include_declaration,
            },
            source_revision,
            0,
        )))
    }
}

impl ReferenceSearch {
    fn new(origin: ReferenceAnchor, source_revision: u64, disk_retries: usize) -> Self {
        Self {
            anchor: Some(origin.clone()),
            origin,
            plan: None,
            target: None,
            files: Vec::new(),
            next_file: 0,
            deferred: VecDeque::new(),
            processing_deferred: false,
            matches: Vec::new(),
            verified: BTreeMap::new(),
            verified_count: 0,
            work: None,
            source_revision,
            observations: Arc::default(),
            snapshot: None,
            disk_retries,
        }
    }
}

impl SemanticTask<Vec<(PathBuf, Range)>> for ReferenceSearch {
    fn ready(&self, engine: &Engine) -> bool {
        self.source_revision != engine.state.completion_source_generation
            || self
                .work
                .as_ref()
                .is_none_or(|work| work.ready(&engine.reference_worker))
    }

    fn advance(&mut self, engine: &mut Engine) -> Result<bool> {
        let probe = active(NavigationKind::References);
        let probe = probe.as_deref();
        let _exec = sample(probe, |counts| &mut counts.exec);
        let started = Instant::now();
        record(probe, |counts| counts.batches += 1);
        let result = self.step(engine, probe).and_then(|done| {
            if !done || self.observations.current() {
                return Ok(done);
            }
            ensure!(
                self.disk_retries < 2,
                "disk sources kept changing during reference search"
            );
            *self = Self::new(
                self.origin.clone(),
                self.source_revision,
                self.disk_retries + 1,
            );
            Ok(false)
        });
        record(probe, |counts| {
            counts.matches_count = self.matches.len() + self.verified_count;
            counts.reference_max_step = counts.reference_max_step.max(started.elapsed());
        });
        result
    }

    fn finish(mut self) -> Vec<(PathBuf, Range)> {
        self.matches.reserve(self.verified_count);
        for (path, ranges) in self.verified.into_values() {
            self.matches
                .extend(ranges.into_iter().map(|range| (path.clone(), range)));
        }
        self.matches
    }
}

impl ReferenceSearch {
    fn step(&mut self, engine: &mut Engine, probe: Option<&NavigationProbe>) -> Result<bool> {
        ensure!(
            self.source_revision == engine.state.completion_source_generation,
            "sources changed during reference search"
        );
        let worker = &mut engine.reference_worker;
        let cache = &mut engine.reference_candidates;
        let graph_revision = engine.state.completion_graph_generation;
        let declaration_revision = engine.state.declaration_generation;
        let graph = engine
            .state
            .workspace
            .as_ref()
            .context("reference search needs a workspace")?
            .clone();
        let mut snapshot = engine.state.snapshot(engine.include_tests);
        let new_documents = snapshot.new_documents();
        let workspace = snapshot
            .workspace()
            .context("reference search needs a workspace")?;
        ensure!(
            matches!(snapshot.active_graph(false), QueryProgress::Ready(_)),
            "workspace changed during reference search"
        );
        let root = snapshot.root();
        let mut resolver = snapshot.resolver(workspace);
        resolver.indexes.navigation = probe;
        let worker_snapshot = self
            .snapshot
            .get_or_insert_with(|| {
                ReferenceWorkerSnapshot::capture(
                    graph.clone(),
                    &resolver.indexes,
                    new_documents,
                    root,
                    self.observations.clone(),
                )
            })
            .clone();
        if let Some(work) = &mut self.work {
            let Some(outputs) = work.poll(worker)? else {
                return Ok(false);
            };
            self.work = None;
            let verifies = self.install_work(&mut resolver, cache, outputs)?;
            if self.observations.changed() {
                return Ok(true);
            }
            if !verifies.is_empty() {
                self.work = Some(PendingWork::new(verifies));
                return Ok(false);
            }
        }
        if let Some(anchor) = self.anchor.clone() {
            self.work = Some(PendingWork::new([Work::Anchor(AnchorWork {
                snapshot: worker_snapshot,
                anchor,
            })]));
            return Ok(false);
        }
        if let Some(plan) = &self.plan {
            cache.refresh(
                graph_revision,
                declaration_revision,
                &resolver.indexes,
                new_documents,
            );
            if let Some(candidates) = cache.get(&plan.scope) {
                self.install_candidates(candidates, probe);
                record(probe, |counts| counts.reference_scope_cache_hit = true);
            } else {
                let input = CandidateInput::new(graph, cache.overlays.clone(), plan.scope.clone());
                self.work = Some(PendingWork::new([Work::Candidates(input)]));
                return Ok(false);
            }
        }
        let Some(target) = &self.target else {
            return Ok(true);
        };

        let batch = if self.next_file == self.files.len() {
            self.processing_deferred = true;
            self.deferred
                .drain(..self.deferred.len().min(BATCH_SIZE))
                .collect()
        } else {
            let end = (self.next_file + BATCH_SIZE).min(self.files.len());
            let batch = &self.files[self.next_file..end];
            let same_priority =
                batch.partition_point(|candidate| candidate.priority == batch[0].priority);
            self.next_file += same_priority;
            record(probe, |counts| counts.processed_files += same_priority);
            batch[..same_priority].to_vec()
        };
        if batch.is_empty() {
            return Ok(true);
        }
        let work = batch.into_iter().map(|candidate| {
            Work::Discover(CandidateDiscovery {
                snapshot: worker_snapshot.clone(),
                candidate,
                target: target.clone(),
            })
        });
        self.work = Some(PendingWork::new(work));
        Ok(false)
    }

    fn install_work(
        &mut self,
        resolver: &mut Resolver<'_, '_>,
        cache: &mut ReferenceCandidates,
        outputs: Vec<WorkOutput>,
    ) -> Result<Vec<Work>> {
        let probe = resolver.indexes.navigation;
        let mut discovered = Vec::new();
        for output in outputs {
            match output {
                WorkOutput::Candidates(candidates) => {
                    let plan = self.plan.as_ref().expect("prepared reference scope");
                    cache.insert(&plan.scope, candidates.clone());
                    self.install_candidates(candidates, probe);
                }
                WorkOutput::Discovered(scan) => {
                    let candidate = &scan.input.candidate;
                    if (scan.missing && candidate.path.is_file())
                        || scan.text.as_ref().is_some_and(|text| {
                            !resolver
                                .indexes
                                .shared_disk
                                .validates_read(&candidate.path, text)
                        })
                    {
                        self.observations.mark_changed();
                        continue;
                    }
                    if let Some(text) = &scan.text {
                        resolver
                            .indexes
                            .navigation_texts
                            .insert(candidate.key.clone(), text.downgrade());
                    }
                    record(probe, |counts| {
                        counts.add_reference_verification(scan.counts.clone())
                    });
                    if scan.should_verify {
                        if !self.processing_deferred
                            && candidate.priority == 2
                            && scan.input.target.container.is_some()
                            && !scan.has_container
                        {
                            // Retain only the file identity while later container
                            // hits run. The second pass checks the current ticket
                            // and reuses resident text and token postings.
                            self.deferred.push_back(candidate.clone());
                        } else {
                            discovered.push(scan);
                        }
                    }
                }
                WorkOutput::Verified(result) => {
                    record(probe, |counts| {
                        counts.add_reference_verification(result.counts)
                    });
                    let key = &result.candidate.key;
                    let prepared = result.source;
                    let source = prepared
                        .as_ref()
                        .map(|source| source.clone().into_indexed());
                    if result.changed
                        || (result.missing && result.candidate.path.is_file())
                        || prepared.is_some_and(|source| {
                            !resolver
                                .indexes
                                .install_reference_source(key.clone(), source)
                        })
                    {
                        self.observations.mark_changed();
                    } else if !result.matches.is_empty() {
                        let source = source
                            .as_ref()
                            .or_else(|| resolver.indexes.documents.get(key))
                            .context("missing verified reference source")?;
                        let mut ranges: Vec<_> = result
                            .matches
                            .into_iter()
                            .map(|range| source.byte_range(range))
                            .collect();
                        ranges.sort_unstable_by_key(|range| (range.start, range.end));
                        ranges.dedup();
                        self.verified_count += ranges.len();
                        self.verified
                            .insert(key.clone(), (source.path.clone(), ranges));
                    }
                }
                WorkOutput::Anchor(result) => {
                    record(probe, |counts| {
                        counts.reference_target = result.counts.reference_target;
                        counts.add_reference_verification(result.counts);
                    });
                    let anchor = self.anchor.as_ref().expect("reference anchor");
                    if (result.missing && anchor.path.is_file())
                        || result.source.is_some_and(|source| {
                            !resolver
                                .indexes
                                .install_reference_source(anchor.key.clone(), source)
                        })
                    {
                        self.observations.mark_changed();
                        continue;
                    }
                    self.install_anchor(result.anchor);
                }
            }
        }
        Ok(discovered
            .into_iter()
            .map(|discovery| Work::Verify(CandidateWork { discovery }))
            .collect())
    }

    fn install_anchor(&mut self, result: AnchorResult) {
        self.anchor = None;
        match result {
            AnchorResult::Local(locations) => self.matches = locations,
            AnchorResult::Plan(plan) => self.plan = Some(plan),
        }
    }

    fn install_candidates(&mut self, candidates: CandidateList, probe: Option<&NavigationProbe>) {
        let plan = self.plan.take().expect("prepared reference scope");
        record(probe, |counts| {
            counts.candidate_packages = candidates.package_count;
            counts.candidate_files = candidates.files.len();
        });
        if plan.target.include_declaration
            && candidates
                .files
                .binary_search_by(|candidate| candidate.key.cmp(&plan.target.path))
                .is_err()
        {
            self.matches
                .push((plan.declaration.clone(), plan.target.range));
        }
        let declaration_directory = plan.target.path.parent();
        self.files = candidates
            .files
            .iter()
            .map(|candidate| {
                let priority = if candidate.key == plan.target.path {
                    0
                } else if candidate.package_id == plan.scope.owner
                    || candidate.key.parent() == declaration_directory
                {
                    1
                } else {
                    2
                };
                Arc::new(ReferenceCandidate {
                    priority,
                    ..candidate.as_ref().clone()
                })
            })
            .collect();
        self.files.sort_by_key(|candidate| candidate.priority);
        self.target = Some(Arc::new(plan.target));
    }
}

enum AnchorResult {
    Local(Vec<(PathBuf, Range)>),
    Plan(ReferencePlan),
}

impl ReferenceAnchor {
    fn plan(
        &self,
        resolver: &mut Resolver<'_, '_>,
        source: &Source,
        root: &Path,
        new_documents: &HashSet<PathBuf>,
    ) -> Result<AnchorResult> {
        let Some(reference) = source.reference_at(self.position) else {
            return Ok(AnchorResult::Local(Vec::new()));
        };
        let probe = resolver.indexes.navigation;
        if let Some(locations) = local_references(
            source,
            reference,
            self.include_declaration,
            probe,
            resolver.indexes.cancelled,
        ) {
            return Ok(AnchorResult::Local(locations));
        }
        let Some(from) = active_package(resolver.workspace, source, new_documents) else {
            return Ok(AnchorResult::Local(Vec::new()));
        };
        Ok(
            match resolver.reference_search(
                from,
                source,
                reference,
                root,
                self.include_declaration,
            )? {
                Some(plan) => AnchorResult::Plan(plan),
                None => AnchorResult::Local(Vec::new()),
            },
        )
    }
}

fn local_references(
    source: &Source,
    reference: ReferenceQuery<'_>,
    include_declaration: bool,
    probe: Option<&NavigationProbe>,
    cancelled: Option<&AtomicBool>,
) -> Option<Vec<(PathBuf, Range)>> {
    let ReferenceQuery::Name(name) = reference else {
        return None;
    };
    let declaration = reference_binding(name, &source.text)?;
    let scope = local_reference_scope(declaration)?;
    let _file = file_sample(probe, &source.path);
    let _verify = verification(probe);
    let mut locations = Vec::new();
    let mut offsets_checked = 0;
    source.for_each_name_cancellable(scope, node_text(declaration, &source.text), cancelled, |candidate| {
        offsets_checked += 1;
        if (include_declaration || candidate != declaration)
            && matches!(source.reference_from_leaf(candidate, candidate.start_byte()), Some(ReferenceQuery::Name(node)) if node == candidate)
            && reference_binding(candidate, &source.text) == Some(declaration)
        {
            locations.push((source.path.clone(), source.range(candidate)));
        }
    });
    record(probe, |counts| {
        counts.reference_target = "local";
        counts.candidate_files = 1;
        counts.ident_offsets_checked += offsets_checked;
        counts.matches_count = locations.len();
    });
    Some(locations)
}

fn local_reference_scope(declaration: Node<'_>) -> Option<Node<'_>> {
    let type_parameter = declaration
        .parent()
        .is_some_and(|node| node.kind() == "type_parameter_declaration");
    let mut cursor = declaration.parent();
    while let Some(node) = cursor {
        match node.kind() {
            "function_declaration" | "method_declaration" | "func_literal" => return Some(node),
            "type_spec" | "type_alias" if type_parameter => return Some(node),
            _ => cursor = node.parent(),
        }
    }
    None
}

impl<'w> Resolver<'w, '_> {
    fn reference_search(
        &mut self,
        from: &'w Package,
        source: &Source,
        reference: ReferenceQuery<'_>,
        root: &Path,
        include_declaration: bool,
    ) -> Result<Option<ReferencePlan>> {
        let spelling = node_text(reference.name(), &source.text);
        // The file scan used to enumerate only identifier nodes with this
        // exact spelling. Resolve implicit names here as before: their
        // canonical declaration may be an import path with different text.
        if spelling == "_"
            || !spelling
                .chars()
                .next()
                .is_some_and(|ch| ch.is_alphabetic() || ch == '_')
        {
            return Ok(None);
        }
        let Some((target, target_range)) =
            self.reference_location(from, source, reference, root)?
        else {
            return Ok(None);
        };
        let probe = self.indexes.navigation;
        record(probe, |counts| {
            counts.reference_target = match counts.definition_route {
                "builtin" => "builtin",
                "member" => "member",
                _ if reference.name().parent().is_some_and(|parent| {
                    matches!(
                        parent.kind(),
                        "method_declaration" | "method_elem" | "field_declaration"
                    )
                }) && is_declaration_name(reference.name()) =>
                {
                    "member"
                }
                _ => "package_symbol",
            };
        });
        let target_key = path_key(&target);
        let Some(owner) = self.workspace.package_of_file(&target).or_else(|| {
            self.indexes
                .documents
                .get(&target_key)
                .and_then(|source| draft_package(self.workspace, source))
        }) else {
            return Ok(None);
        };
        let (member, top_level, container) = self.reference_owner(&target, target_range)?;
        let transitive = is_exported(spelling) && member;
        let direct_import =
            (is_exported(spelling) && top_level).then(|| owner.import_path().to_owned());
        record(probe, |counts| {
            if transitive {
                counts.reference_target = "member";
            }
        });
        Ok(Some(ReferencePlan {
            target: ReferenceTarget {
                spelling: spelling.to_owned(),
                path: target_key,
                range: target_range,
                include_declaration,
                direct_import,
                container: container
                    .or_else(|| (!owner.name.is_empty()).then(|| owner.name.clone())),
            },
            scope: ReferenceScope {
                owner: owner.id.clone(),
                exported: is_exported(spelling),
                transitive,
            },
            declaration: target,
        }))
    }

    fn reference_owner(
        &mut self,
        path: &Path,
        range: Range,
    ) -> Result<(bool, bool, Option<String>)> {
        let Some(source) = self.indexes.reference_source(path)? else {
            return Ok((false, false, None));
        };
        let source = source.as_ref();
        let Some(mut node) = source.reference_at(range.start).map(ReferenceQuery::name) else {
            return Ok((false, false, None));
        };
        let mut declaration = is_declaration_name(node);
        let mut member = false;
        while let Some(owner) = node.parent() {
            match owner.kind() {
                "method_declaration" => {
                    let container = owner
                        .child_by_field_name("receiver")
                        .and_then(|receiver| receiver.named_child(0))
                        .and_then(|parameter| parameter.child_by_field_name("type"))
                        .and_then(|receiver| {
                            let mut cursor = receiver;
                            loop {
                                match cursor.kind() {
                                    "type_identifier" => {
                                        return Some(node_text(cursor, &source.text).to_owned());
                                    }
                                    "pointer_type" | "generic_type" => {
                                        cursor = cursor.named_child(0)?
                                    }
                                    _ => return None,
                                }
                            }
                        });
                    return Ok((true, false, container));
                }
                "method_elem" | "field_declaration" => member = true,
                "function_declaration" | "var_spec" | "const_spec" => {
                    return Ok((member, declaration && !member, None));
                }
                "type_spec" | "type_alias" => {
                    let container = member
                        .then(|| owner.child_by_field_name("name"))
                        .flatten()
                        .map(|name| node_text(name, &source.text).to_owned());
                    return Ok((member, declaration && !member, container));
                }
                "function_type" | "func_literal" | "import_spec" => declaration = false,
                "source_file" => break,
                _ => {}
            }
            node = owner;
        }
        Ok((member, false, None))
    }
}
