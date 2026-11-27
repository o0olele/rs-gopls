use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use lsp_types::{Position, Range};
use tree_sitter::Node;

use super::Engine;
use super::index::{
    DeferredReferenceRead, IndexReader, ReferenceReadRequest, active_package, draft_package,
    missing_source,
};
use super::navigation_trace::{
    NavigationKind, NavigationProbe, active, file_sample, record, sample, verification,
};
use super::query::{QueryProgress, SemanticTask};
use super::resolver::{Resolver, is_exported};
use super::source::{ReferenceQuery, Source, is_declaration_name, node_text};
use crate::scope::reference_binding;
use crate::workspace::{Package, path_key};

mod candidates;
mod large_file;
#[cfg(test)]
mod tests;
mod worker;

use candidates::{CandidateInput, CandidateList, ReferenceScope};
use large_file::{LargeFileVerification, LargeReferenceAnchor, verify_reference};
pub(super) use worker::ReferenceWorker;
use worker::{PendingWork, Preparation, Work, WorkOutput, WorkResult};

const SEARCH_BUDGET: Duration = Duration::from_millis(2);
const PREFILTER_CHUNK_BYTES: usize = 32 * 1024;
const PREFILTER_CHUNK_OFFSETS: usize = 256;
const READ_AHEAD_FILES: usize = 32;
const LARGE_FILE_BYTES: usize = 128 * 1024;

pub(crate) struct ReferenceSearch {
    anchor: Option<ReferenceAnchor>,
    plan: Option<ReferencePlan>,
    target: Option<ReferenceTarget>,
    files: Arc<[Arc<ReferenceCandidate>]>,
    next_file: usize,
    matches: Vec<(PathBuf, Range)>,
    current: Option<FileScan>,
    unreadable: HashMap<PathBuf, anyhow::Error>,
    dependency: DeferredReferenceRead,
    work: Option<PendingWork>,
    revision: u64,
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
}

struct ReferencePlan {
    target: ReferenceTarget,
    scope: ReferenceScope,
    declaration: PathBuf,
}

#[derive(Default)]
pub(super) struct ReferenceCandidates {
    revision: u64,
    scopes: HashMap<ReferenceScope, CandidateList>,
    total_files: usize,
}

impl ReferenceCandidates {
    fn get(&self, revision: u64, scope: &ReferenceScope) -> Option<CandidateList> {
        if self.revision != revision {
            return None;
        }
        self.scopes.get(scope).cloned()
    }

    fn insert(&mut self, revision: u64, scope: &ReferenceScope, candidates: CandidateList) {
        const MAX_CACHED_FILES: usize = 8192;
        const MAX_CACHED_SCOPES: usize = 16;
        if candidates.files.len() > MAX_CACHED_FILES || self.get(revision, scope).is_some() {
            return;
        }
        if self.revision != revision
            || self.total_files + candidates.files.len() > MAX_CACHED_FILES
            || self.scopes.len() >= MAX_CACHED_SCOPES
        {
            self.scopes.clear();
            self.total_files = 0;
            self.revision = revision;
        }
        self.total_files += candidates.files.len();
        self.scopes.insert(scope.clone(), candidates);
    }
}

struct ReferenceCandidate {
    key: PathBuf,
    path: PathBuf,
    package_id: String,
}

struct FileScan {
    candidate: Arc<ReferenceCandidate>,
    started: Option<Instant>,
    text: Option<Arc<String>>,
    cursor: usize,
    offsets: VecDeque<usize>,
    hot_index: usize,
    had_spelling: bool,
    cached_offsets: bool,
}

enum FileProgress {
    Continue,
    Done,
    NeedText,
    Work(Work),
    VerifyLarge,
}

impl FileScan {
    fn new(candidate: Arc<ReferenceCandidate>, probe: Option<&NavigationProbe>) -> Self {
        Self {
            candidate,
            started: probe.map(|_| Instant::now()),
            text: None,
            cursor: 0,
            offsets: VecDeque::new(),
            hot_index: 0,
            had_spelling: false,
            cached_offsets: false,
        }
    }

    fn record_completion(&self, probe: Option<&NavigationProbe>) {
        if let Some(started) = self.started {
            record(probe, |counts| {
                counts.file_elapsed(&self.candidate.path, started.elapsed())
            });
        }
    }

    fn scan_chunk(&mut self, spelling: &str, probe: Option<&NavigationProbe>) {
        let text = self.text.as_ref().unwrap().as_str();
        let start = self.cursor;
        let mut end = (start + PREFILTER_CHUNK_BYTES).min(text.len());
        // A chunk owns match starts before its end, and can read a full
        // spelling across that boundary without enqueuing the next start.
        let overlap = text.ceil_char_boundary(end.saturating_add(spelling.len() - 1));
        let mut next = text.ceil_char_boundary(start);
        while let Some(found) = text[next..overlap].find(spelling) {
            let offset = next + found;
            if offset >= end {
                break;
            }
            self.offsets.push_back(offset);
            next = text.ceil_char_boundary(offset + 1);
            if self.offsets.len() == PREFILTER_CHUNK_OFFSETS {
                end = end.min(next);
                break;
            }
        }
        self.had_spelling |= !self.offsets.is_empty();
        self.cursor = end;
        record(probe, |counts| counts.prefilter_bytes += end - start);
    }

    fn step(
        &mut self,
        resolver: &mut Resolver<'_, '_>,
        target: &ReferenceTarget,
        root: &Path,
        matches: &mut Vec<(PathBuf, Range)>,
        unreadable: &HashMap<PathBuf, anyhow::Error>,
    ) -> Result<FileProgress> {
        let key = &self.candidate.key;
        let path = &self.candidate.path;
        let Some(package) = resolver.workspace.package(&self.candidate.package_id) else {
            return Ok(FileProgress::Done);
        };
        let probe = resolver.indexes.navigation;
        let parsed = resolver.indexes.cached_reference_source(key);
        let source = parsed.as_ref().map(AsRef::as_ref);
        let large = source.is_some_and(|source| source.text.len() >= LARGE_FILE_BYTES);
        // Keep a scan on one route even if an intervening query builds a table.
        let hot = source.and_then(|source| {
            self.text
                .is_none()
                .then(|| source.ready_name_offsets(&target.spelling))
                .flatten()
        });
        let (source, candidate) = if let Some(offsets) = hot {
            if !self.cached_offsets {
                self.cached_offsets = true;
                record(probe, |counts| counts.cached_offset_files += 1);
            }
            let Some(&offset) = offsets.get(self.hot_index) else {
                return Ok(FileProgress::Done);
            };
            if large {
                return Ok(FileProgress::VerifyLarge);
            }
            let source = source.unwrap();
            (source, source.reference_at_offset(offset))
        } else {
            if self.text.is_none() {
                self.text = resolver.indexes.cached_reference_text(key);
                if self.text.is_none() {
                    return match unreadable.get(key) {
                        Some(error) if missing_source(error) => Ok(FileProgress::Done),
                        Some(error) => anyhow::bail!("{error:#}"),
                        None => Ok(FileProgress::NeedText),
                    };
                }
                record(probe, |counts| counts.text_hits += 1);
            }
            let Some(&offset) = self.offsets.front() else {
                if self.cursor < self.text.as_ref().unwrap().len() {
                    self.scan_chunk(&target.spelling, probe);
                    return Ok(FileProgress::Continue);
                }
                if !self.had_spelling {
                    record(probe, |counts| counts.prefilter_skipped += 1);
                }
                return Ok(FileProgress::Done);
            };
            let Some(source) = source else {
                return Ok(FileProgress::Work(Work::PrepareDisk {
                    key: key.clone(),
                    path: path.clone(),
                    text: self.text.clone(),
                    preparation: Preparation::Candidate,
                }));
            };
            if large {
                return Ok(FileProgress::VerifyLarge);
            }
            (
                source,
                source.reference_at_spelling(offset, &target.spelling),
            )
        };
        let _verify = verification(probe);
        if let Some(candidate) = candidate {
            let resolved =
                verify_reference(resolver, package, source, key, candidate, target, root);
            if let Some(request) = resolver.indexes.deferred_reference.unwrap().take_pending() {
                return Ok(FileProgress::Work(dependency_work(
                    request,
                    &resolver.indexes,
                )));
            }
            if let Some(range) = resolved?
                && matches
                    .last()
                    .is_none_or(|(file, found)| file != &source.path || *found != range)
            {
                matches.push((source.path.clone(), range));
            }
        }
        record(probe, |counts| counts.ident_offsets_checked += 1);
        if hot.is_some() {
            self.hot_index += 1;
        } else {
            self.offsets.pop_front();
        }
        Ok(FileProgress::Continue)
    }
}

fn dependency_work(request: ReferenceReadRequest, indexes: &IndexReader<'_>) -> Work {
    match request {
        ReferenceReadRequest::Disk { key, path } => Work::PrepareDisk {
            text: indexes.cached_reference_text(&key),
            key,
            path,
            preparation: Preparation::Dependency,
        },
        ReferenceReadRequest::OpenDeclarations { revision } => Work::PrepareOpen(revision),
    }
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
        let revision = self.state.completion_source_generation;
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
        Ok(QueryProgress::Searching(ReferenceSearch {
            anchor: Some(ReferenceAnchor {
                key,
                path: path.to_owned(),
                position,
                include_declaration,
            }),
            plan: None,
            target: None,
            files: Arc::default(),
            next_file: 0,
            matches: Vec::new(),
            current: None,
            unreadable: HashMap::new(),
            dependency: DeferredReferenceRead::default(),
            work: None,
            revision,
        }))
    }
}

impl SemanticTask<Vec<(PathBuf, Range)>> for ReferenceSearch {
    fn ready(&self, engine: &Engine) -> bool {
        self.revision != engine.state.completion_source_generation
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
        let result = self.step(engine, probe, started);
        record(probe, |counts| {
            counts.matches_count = self.matches.len();
            counts.reference_max_step = counts.reference_max_step.max(started.elapsed());
        });
        result
    }

    fn finish(self) -> Vec<(PathBuf, Range)> {
        self.matches
    }
}

impl ReferenceSearch {
    fn step(
        &mut self,
        engine: &mut Engine,
        probe: Option<&NavigationProbe>,
        started: Instant,
    ) -> Result<bool> {
        ensure!(
            self.revision == engine.state.completion_source_generation,
            "sources changed during reference search"
        );
        let worker = &mut engine.reference_worker;
        let cache = &mut engine.reference_candidates;
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
        if let Some(work) = &mut self.work {
            let Some(result) = work.poll(worker)? else {
                return Ok(false);
            };
            self.work = None;
            self.install_work(&mut resolver, cache, result)?;
        }
        if let Some(anchor) = &self.anchor {
            let work = match resolver.indexes.cached_reference_source(&anchor.key) {
                None => Work::PrepareDisk {
                    key: anchor.key.clone(),
                    path: anchor.path.clone(),
                    text: resolver.indexes.cached_reference_text(&anchor.key),
                    preparation: Preparation::Anchor,
                },
                Some(source) if source.as_ref().text.len() >= LARGE_FILE_BYTES => {
                    let input = LargeReferenceAnchor::capture(
                        graph.clone(),
                        &resolver.indexes,
                        new_documents,
                        root,
                        anchor.clone(),
                        source.as_ref(),
                    );
                    Work::PlanLarge(Box::new(input))
                }
                Some(source) => {
                    resolver.indexes.deferred_reference = Some(&self.dependency);
                    let result = anchor.plan(&mut resolver, source.as_ref(), root, new_documents);
                    if let Some(request) = self.dependency.take_pending() {
                        dependency_work(request, &resolver.indexes)
                    } else {
                        self.install_anchor(result?);
                        return Ok(self.plan.is_none());
                    }
                }
            };
            let work = self.work.insert(PendingWork::new(work));
            work.poll(worker)?;
            return Ok(false);
        }
        if let Some(plan) = &self.plan {
            if let Some(candidates) = cache.get(self.revision, &plan.scope) {
                self.install_candidates(candidates, probe);
                record(probe, |counts| counts.reference_scope_cache_hit = true);
            } else {
                let input = CandidateInput::capture(
                    graph.clone(),
                    &resolver.indexes,
                    new_documents,
                    plan.scope.clone(),
                );
                let work = self.work.insert(PendingWork::new(Work::Candidates(input)));
                work.poll(worker)?;
                return Ok(false);
            }
        }
        let Some(target) = &self.target else {
            return Ok(true);
        };
        resolver.indexes.deferred_reference = Some(&self.dependency);
        let mut completed = 0;
        while completed < 8 && started.elapsed() < SEARCH_BUDGET {
            if self.current.is_none() {
                let Some(candidate) = self.files.get(self.next_file).cloned() else {
                    return Ok(true);
                };
                self.next_file += 1;
                self.current = Some(FileScan::new(candidate, probe));
                record(probe, |counts| counts.processed_files += 1);
            }
            let scan = self.current.as_mut().unwrap();
            match scan.step(
                &mut resolver,
                target,
                root,
                &mut self.matches,
                &self.unreadable,
            )? {
                FileProgress::Continue => {}
                FileProgress::Done => {
                    scan.record_completion(probe);
                    self.current = None;
                    completed += 1;
                }
                FileProgress::NeedText => {
                    let files = std::iter::once(&scan.candidate)
                        .chain(
                            self.files[self.next_file..]
                                .iter()
                                .take(READ_AHEAD_FILES - 1),
                        )
                        .filter(|file| {
                            !self.unreadable.contains_key(&file.key)
                                && resolver.indexes.cached_reference_text(&file.key).is_none()
                        })
                        .map(|file| (file.key.clone(), file.path.clone()))
                        .collect();
                    self.work = Some(PendingWork::new(Work::Read(files)));
                }
                FileProgress::Work(work) => self.work = Some(PendingWork::new(work)),
                FileProgress::VerifyLarge => {
                    let scan = self.current.take().unwrap();
                    let source = resolver
                        .indexes
                        .cached_reference_source(&scan.candidate.key)
                        .unwrap();
                    let verification = LargeFileVerification::capture(
                        graph.clone(),
                        &resolver.indexes,
                        new_documents,
                        root,
                        target.clone(),
                        scan,
                        source.as_ref(),
                    );
                    self.work = Some(PendingWork::new(Work::VerifyLarge(Box::new(verification))));
                }
            }
            if let Some(work) = &mut self.work {
                work.poll(worker)?;
                return Ok(false);
            }
        }
        Ok(self.current.is_none() && self.next_file == self.files.len())
    }

    fn install_work(
        &mut self,
        resolver: &mut Resolver<'_, '_>,
        cache: &mut ReferenceCandidates,
        result: WorkResult,
    ) -> Result<()> {
        let probe = resolver.indexes.navigation;
        let WorkResult { output, stats } = result;
        record(probe, |counts| {
            counts.disk_reads += stats.reads;
            counts.text_misses += stats.reads;
            counts.read += stats.read;
            counts.parse += stats.parse;
            counts.ast_parses += usize::from(stats.parsed);
        });
        match output {
            WorkOutput::Candidates(candidates) => {
                let plan = self.plan.as_ref().expect("prepared reference scope");
                cache.insert(self.revision, &plan.scope, candidates.clone());
                self.install_candidates(candidates, probe);
            }
            WorkOutput::Texts(texts) => {
                record(probe, |counts| counts.prefilter_reads += stats.reads);
                for (key, text) in texts {
                    match text {
                        Ok(text) => {
                            resolver.indexes.navigation_texts.entry(key).or_insert(text);
                        }
                        Err(error) => {
                            self.unreadable.insert(key, error);
                        }
                    }
                }
            }
            WorkOutput::Disk {
                key,
                text,
                source,
                preparation,
            } => {
                record(probe, |counts| {
                    counts.ast_misses += 1;
                    match preparation {
                        Preparation::Dependency => {
                            counts.reference_dependency_parses += usize::from(stats.parsed)
                        }
                        Preparation::Candidate => {
                            counts.reference_candidate_parses += usize::from(stats.parsed)
                        }
                        Preparation::Anchor => {
                            counts.reference_anchor_parses += usize::from(stats.parsed)
                        }
                    }
                });
                if let Some(text) = text {
                    resolver
                        .indexes
                        .navigation_texts
                        .entry(key.clone())
                        .or_insert(text);
                }
                match source {
                    Ok(prepared) => resolver.indexes.install_reference_source(key, prepared),
                    Err(error) if preparation == Preparation::Dependency => {
                        self.dependency.fail(key, &error)
                    }
                    Err(error) => return Err(error),
                }
            }
            WorkOutput::Open(result) => result?,
            WorkOutput::Large(mut result) => {
                self.matches.append(&mut result.matches);
                if let Some(work) = result.counts {
                    record(probe, |counts| counts.add_reference_verification(work));
                }
            }
            WorkOutput::Anchor(result) => {
                self.install_anchor(result.anchor);
                if let Some(work) = result.counts {
                    record(probe, |counts| {
                        counts.reference_target = work.reference_target;
                        counts.candidate_packages = work.candidate_packages;
                        counts.candidate_files = work.candidate_files;
                        counts.processed_files += work.processed_files;
                        counts.add_reference_verification(work);
                    });
                }
            }
        }
        Ok(())
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
            self.matches.push((plan.declaration, plan.target.range));
        }
        self.target = Some(plan.target);
        self.files = candidates.files;
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
        let transitive = if is_exported(spelling) {
            self.is_member_declaration(&target, target_range)?
        } else {
            false
        };
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
            },
            scope: ReferenceScope {
                owner: owner.id.clone(),
                exported: is_exported(spelling),
                transitive,
            },
            declaration: target,
        }))
    }

    fn is_member_declaration(&mut self, path: &Path, range: Range) -> Result<bool> {
        let Some(source) = self.indexes.reference_source(path)? else {
            return Ok(false);
        };
        let source = source.as_ref();
        let Some(mut node) = source.reference_at(range.start).map(ReferenceQuery::name) else {
            return Ok(false);
        };
        while let Some(owner) = node.parent() {
            match owner.kind() {
                "method_declaration" | "method_elem" | "field_declaration" => return Ok(true),
                "function_declaration"
                | "type_spec"
                | "type_alias"
                | "var_spec"
                | "const_spec"
                | "source_file" => return Ok(false),
                _ => node = owner,
            }
        }
        Ok(false)
    }
}
