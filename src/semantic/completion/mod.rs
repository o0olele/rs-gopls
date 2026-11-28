mod candidates;
mod cursor;
pub(in crate::semantic) mod disk_observation;
mod expected;
mod imports;
pub(super) mod prefilter;
mod profile;
mod ranking;
pub(super) mod render;
mod sdk;
pub(super) mod sdk_names;
#[cfg(test)]
mod tests;
mod worker;

use std::io;
use std::path::{Path, PathBuf};

use anyhow::Result;
use lsp_types::{
    CompletionItem, CompletionResponse, Documentation, InitializeParams, MarkupKind, Position,
};

use crate::protocol::file_path;
use crate::rpc_telemetry;
use crate::workspace::{Package, Workspace, path_key};

#[cfg(test)]
use super::SemanticTask;
use super::{
    Engine, QueryProgress, QueryReadMemo,
    index::CompletionRead,
    shared_disk::NeedsFullDeclarations,
    snapshot::{DocumentSnapshot, EngineSnapshot},
    source::{check_optional_cancelled, is_test_source},
};
use cursor::{Edit, Probe, Site};
pub(super) use expected::InferredMemo;
pub(super) use imports::ImportCache;
use imports::{Search, SearchKind};
#[cfg(test)]
pub(crate) use profile::CompletionHook;
pub(crate) use profile::Trace;
use render::{
    ItemData, SourceRev, Target, build_import_edits, declaration_markup, markup, nonoverlapping,
    package_name, response,
};
pub(crate) use worker::{CompletionInput, CompletionOutcome, CompletionStamp, CompletionWorker};

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CompletionClientCapabilities {
    pub(crate) snippet_support: bool,
    pub(crate) label_details_support: bool,
    pub(crate) insert_replace_support: bool,
    pub(crate) deprecated_support: bool,
    pub(crate) markdown_docs: bool,
    pub(crate) resolve_documentation: bool,
    pub(crate) resolve_detail: bool,
    pub(crate) resolve_additional_text_edits: bool,
}

impl CompletionClientCapabilities {
    pub(crate) fn from_initialize(params: &InitializeParams) -> Self {
        let Some(item) = params
            .capabilities
            .text_document
            .as_ref()
            .and_then(|document| document.completion.as_ref())
            .and_then(|completion| completion.completion_item.as_ref())
        else {
            return Self::default();
        };
        let can_resolve = |property| {
            item.resolve_support
                .as_ref()
                .is_some_and(|support| support.properties.iter().any(|name| name == property))
        };
        Self {
            snippet_support: item.snippet_support == Some(true),
            label_details_support: item.label_details_support == Some(true),
            insert_replace_support: item.insert_replace_support == Some(true),
            deprecated_support: item.deprecated_support == Some(true)
                || item.tag_support.as_ref().is_some_and(|tags| {
                    tags.value_set
                        .contains(&lsp_types::CompletionItemTag::DEPRECATED)
                }),
            markdown_docs: item
                .documentation_format
                .as_ref()
                .is_some_and(|formats| formats.first() == Some(&MarkupKind::Markdown)),
            resolve_documentation: can_resolve("documentation"),
            resolve_detail: can_resolve("detail"),
            resolve_additional_text_edits: can_resolve("additionalTextEdits"),
        }
    }
}

pub(crate) enum ResolveCompletionResult {
    Item(CompletionItem),
    ContentModified,
    InvalidData,
}

pub(crate) struct CompletionResolve {
    item: CompletionItem,
    data: ItemData,
    path: PathBuf,
}

impl CompletionResolve {
    pub(crate) fn new(item: CompletionItem) -> Option<Self> {
        let data: ItemData = serde_json::from_value(item.data.as_ref()?.clone()).ok()?;
        let path = file_path(&data.uri).ok()?;
        Some(Self { item, data, path })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Debug)]
pub(crate) struct StaleCompletion;

impl std::fmt::Display for StaleCompletion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the completion document, target, or package graph changed")
    }
}

impl std::error::Error for StaleCompletion {}

impl Engine {
    #[cfg(test)]
    pub(crate) fn complete(
        &mut self,
        path: &Path,
        position: Position,
        _context: Option<lsp_types::CompletionContext>,
        caps: &CompletionClientCapabilities,
    ) -> Result<QueryProgress<CompletionResponse, CompletionTask>> {
        let generation = self.state.completion_graph_generation;
        let mut snapshot = self.snapshot();
        let Some(document) = completion_document(&mut snapshot, path, None)? else {
            return Ok(empty());
        };
        let progress =
            snapshot.complete_document(document, position, caps, Some(generation), None)?;
        if let QueryProgress::Pending(load) = &progress {
            self.include_tests |= load.include_tests;
        }
        Ok(progress)
    }

    pub(crate) fn resolve_completion_item(
        &mut self,
        request: &CompletionResolve,
        caps: &CompletionClientCapabilities,
        memo: &QueryReadMemo,
    ) -> Result<ResolveCompletionResult> {
        let rev_check = rpc_telemetry::phase("rev_check");
        let generation = self.state.completion_graph_generation;
        let target_opening = request.data.target.as_ref().is_some_and(|target| {
            self.track_query_overlay(memo, &target.file);
            self.document_parse_pending(&target.file)
        });
        drop(rev_check);
        self.snapshot()
            .resolve_completion_item(request, caps, generation, memo, target_opening)
    }
}

impl EngineSnapshot<'_> {
    fn complete_document(
        &mut self,
        document: DocumentSnapshot<'_>,
        position: Position,
        caps: &CompletionClientCapabilities,
        generation: Option<u64>,
        trace: Option<&Trace>,
    ) -> Result<QueryProgress<CompletionResponse, CompletionTask>> {
        let mut request = CompletionRead::new(trace.cloned());
        let cancelled = self.completion_cancelled();
        let phase = trace.map(|trace| trace.phase("cursor"));
        let Some(probe) = Probe::new(&document, self.parser(), position, trace, cancelled)? else {
            return Ok(empty());
        };
        let source = probe.source(&document);
        let anchor = probe.anchor(source);
        let site = probe.site(source, document.text(), cancelled)?;
        let edit = &probe.edit;
        drop(phase);
        if let Some(candidates) = candidates::syntax_candidates(
            &site,
            source,
            anchor,
            edit.prefix(document.text()),
            self.docs(),
            self.shared_disk(),
            cancelled,
        )? {
            return Ok(QueryProgress::Ready(response(
                self,
                &document,
                edit,
                candidates,
                caps,
                None,
                &mut request,
            )?));
        }

        let Some(generation) = generation else {
            let local_candidates = trace.map(|trace| trace.phase("local_candidates"));
            let candidates = candidates::collect(
                &document, source, anchor, site, &probe, None, caps, cancelled,
            )?
            .candidates;
            drop(local_candidates);
            return Ok(QueryProgress::Ready(response(
                self,
                &document,
                edit,
                candidates,
                caps,
                None,
                &mut request,
            )?));
        };
        let test = is_test_source(document.path());
        let workspace = match self.active_graph(test) {
            QueryProgress::Ready(workspace) => workspace,
            QueryProgress::Pending(load) => return Ok(QueryProgress::Pending(load)),
            QueryProgress::Searching(task) => match task {},
        };
        let package = match self.package_for(&document, test) {
            QueryProgress::Ready(package) => package,
            QueryProgress::Pending(load) => return Ok(QueryProgress::Pending(load)),
            QueryProgress::Searching(task) => match task {},
        };
        let package = completion_package(workspace, &document, package);
        let (items, search) = if let Site::ImportPath { close } = site {
            (Vec::new(), Some(SearchKind::Path { close }))
        } else {
            let _phase = trace.map(|trace| trace.phase("local_candidates"));
            #[cfg(test)]
            if let Some(trace) = trace {
                trace.checkpoint("local_candidates", cancelled);
            }
            check_optional_cancelled(cancelled)?;
            let mut resolver = self.resolver_with_completion(workspace, Some(&mut request));
            let graph = package.map(|package| (&mut resolver, package));
            let collection = candidates::collect(
                &document, source, anchor, site, &probe, graph, caps, cancelled,
            )?;
            (
                collection.candidates,
                collection.search.map(SearchKind::Symbols),
            )
        };
        if let Some(search) = search {
            let path = document.path().to_owned();
            let prefix = edit.prefix(document.text()).to_owned();
            let edit = edit.clone();
            if let Some((source, ticket)) = document.into_disk() {
                request.pin_document(source, ticket);
            }
            return Ok(QueryProgress::Searching(CompletionTask {
                path,
                generation,
                #[cfg(test)]
                source_generation: self.completion_source_generation(),
                prefix,
                edit,
                caps: *caps,
                search: Search::new(search, package),
                request,
                candidates: items,
                result: None,
            }));
        }
        Ok(QueryProgress::Ready(response(
            self,
            &document,
            edit,
            items,
            caps,
            Some(generation),
            &mut request,
        )?))
    }

    fn resolve_completion_item(
        &mut self,
        request: &CompletionResolve,
        caps: &CompletionClientCapabilities,
        generation: u64,
        memo: &QueryReadMemo,
        target_opening: bool,
    ) -> Result<ResolveCompletionResult> {
        let rev_check = rpc_telemetry::phase("rev_check");
        let CompletionResolve { item, data, path } = request;
        let Some(source) = self.docs().get(&path_key(path)) else {
            return Ok(ResolveCompletionResult::ContentModified);
        };
        let document = DocumentSnapshot::borrowed(path, source);
        if data.document_version.is_none()
            || document.version() != data.document_version
            || format!("{:016x}", source.fingerprint()) != data.document_fingerprint
            || data
                .document_revision
                .as_ref()
                .is_some_and(|revision| SourceRev::overlay(source).as_ref() != Some(revision))
            || data
                .graph_generation
                .is_some_and(|graph| graph != generation)
        {
            return Ok(ResolveCompletionResult::ContentModified);
        }
        drop(rev_check);
        let docs = if let Some(target) = &data.target {
            self.resolve_target(
                target,
                item.documentation.is_none(),
                target_opening,
                caps,
                memo,
            )?
        } else {
            None
        };
        let mut item = {
            let _detail_doc = rpc_telemetry::phase("detail_doc");
            let mut item = item.clone();
            item.detail = item.detail.or_else(|| data.detail.clone());
            item.documentation = item.documentation.or(docs);
            item
        };
        if caps.resolve_additional_text_edits
            && let Some(import) = &data.pending_import
        {
            let _import_edit = rpc_telemetry::phase("import_edit");
            let Some(edits) = build_import_edits(&document, import) else {
                return Ok(ResolveCompletionResult::ContentModified);
            };
            if item
                .text_edit
                .as_ref()
                .is_none_or(|primary| !nonoverlapping(primary, &edits))
            {
                return Ok(ResolveCompletionResult::InvalidData);
            }
            if !edits.is_empty() {
                item.additional_text_edits = Some(edits);
            }
        }
        Ok(ResolveCompletionResult::Item(item))
    }

    fn resolve_target(
        &self,
        target: &Target,
        want_docs: bool,
        opening: bool,
        caps: &CompletionClientCapabilities,
        memo: &QueryReadMemo,
    ) -> Result<Option<Documentation>> {
        let rev_check = rpc_telemetry::phase("rev_check");
        let Some(expected) = &target.revision else {
            return Ok(None);
        };
        let source = self.docs().get(&path_key(&target.file));
        if let Some(source) = source {
            if SourceRev::overlay(source).as_ref() != Some(expected) {
                return Err(StaleCompletion.into());
            }
            drop(rev_check);
            let _detail_doc = rpc_telemetry::phase("detail_doc");
            return Ok(want_docs
                .then(|| markup(source, target.range, caps))
                .flatten());
        }
        let SourceRev::Disk(expected) = expected else {
            return Err(StaleCompletion.into());
        };
        if opening {
            return Err(StaleCompletion.into());
        }
        let disk = self.shared_disk();
        let Some(text) = optional_target(disk.read_foreground_verified(&target.file, memo, None))?
        else {
            return Ok(None);
        };
        if text
            .read_ticket
            .as_ref()
            .is_none_or(|ticket| ticket.source_revision() != *expected)
        {
            return Err(StaleCompletion.into());
        }
        drop(rev_check);
        if !want_docs {
            return Ok(None);
        }
        let _detail_doc = rpc_telemetry::phase("detail_doc");
        let source = optional_target(disk.load_documentation_foreground(&target.file, memo, None))?;
        Ok(source.and_then(|source| declaration_markup(&source.declarations, target.range, caps)))
    }
}

fn optional_target<T>(result: Result<T>) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.is::<NeedsFullDeclarations>() => Ok(None),
        Err(error) => match error.downcast_ref::<io::Error>().map(io::Error::kind) {
            Some(io::ErrorKind::NotFound | io::ErrorKind::Interrupted) => {
                Err(StaleCompletion.into())
            }
            Some(_) => Ok(None),
            None => Err(error),
        },
    }
}

fn completion_document<'w, 'd>(
    snapshot: &mut EngineSnapshot<'w>,
    path: &'d Path,
    trace: Option<&Trace>,
) -> Result<Option<DocumentSnapshot<'d>>>
where
    'w: 'd,
{
    if let Some(trace) = trace
        && path.extension().is_some_and(|extension| extension == "go")
    {
        let key = path_key(path);
        if !snapshot.docs().contains_key(&key) {
            trace.read(&key, false, false, false);
        }
    }
    match snapshot.document(path) {
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Ok(Some(document)) => {
            if document.version().is_none()
                && let Some(trace) = trace
            {
                trace.parse();
            }
            Ok(Some(document))
        }
        document => document,
    }
}

fn completion_package<'w>(
    workspace: &'w Workspace,
    document: &DocumentSnapshot<'_>,
    active: Option<&'w Package>,
) -> Option<&'w Package> {
    let name = package_name(document.indexed())?;
    if let Some(active) = active.filter(|package| package.name == name) {
        return Some(active);
    }
    let directory = path_key(document.path().parent()?);
    let test = is_test_source(document.path());
    workspace
        .navigation_packages()
        .filter(|package| {
            package.directory_and_name() == (directory.as_path(), name)
                && (test || package.is_production())
        })
        .min_by_key(|package| test == package.is_production())
}

fn empty() -> QueryProgress<CompletionResponse, CompletionTask> {
    QueryProgress::Ready(CompletionResponse::Array(Vec::new()))
}

pub(crate) struct CompletionTask {
    path: PathBuf,
    generation: u64,
    #[cfg(test)]
    source_generation: u64,
    prefix: String,
    edit: Edit,
    caps: CompletionClientCapabilities,
    search: Search,
    request: CompletionRead,
    candidates: Vec<ranking::Candidate>,
    result: Option<CompletionResponse>,
}

#[cfg(test)]
impl SemanticTask<CompletionResponse> for CompletionTask {
    fn ready(&self, engine: &Engine) -> bool {
        engine.state.completion_graph_generation != self.generation
            || engine.state.completion_source_generation != self.source_generation
            || self.search.is_ready(&engine.completion_imports)
    }

    fn discard(&mut self, code: lsp_server::ErrorCode) {
        if let Some(trace) = &self.request.trace {
            trace.error(code);
        }
    }

    fn advance(&mut self, engine: &mut Engine) -> Result<bool> {
        if !self.ready(engine) {
            // Synchronous callers also drive the same asynchronous batches.
            std::thread::yield_now();
            return Ok(false);
        }
        if engine.state.completion_graph_generation != self.generation
            || engine.state.completion_source_generation != self.source_generation
            || self
                .request
                .document
                .as_ref()
                .is_some_and(|(_, ticket)| !ticket.matches_disk(&self.path))
        {
            self.discard(lsp_server::ErrorCode::ContentModified);
            return Err(StaleCompletion.into());
        }
        let mut snapshot = engine.state.snapshot(engine.include_tests);
        self.advance_snapshot(&mut snapshot, &mut engine.completion_imports)
    }

    fn finish(self) -> CompletionResponse {
        self.into_response()
    }
}

impl CompletionTask {
    fn advance_snapshot(
        &mut self,
        snapshot: &mut EngineSnapshot<'_>,
        imports: &mut ImportCache,
    ) -> Result<bool> {
        check_optional_cancelled(snapshot.completion_cancelled())?;
        let retained;
        let document = if let Some((source, ticket)) = &self.request.document {
            retained = std::rc::Rc::clone(source);
            DocumentSnapshot::pinned_disk(&self.path, &retained, ticket.clone())
        } else {
            completion_document(snapshot, &self.path, self.request.trace.as_ref())?
                .ok_or(StaleCompletion)?
        };
        if !self.search.advance(
            imports,
            snapshot,
            &document,
            self.generation,
            &self.prefix,
            &mut self.candidates,
            &self.caps,
            &mut self.request,
        )? {
            return Ok(false);
        }
        self.result = Some(response(
            snapshot,
            &document,
            &self.edit,
            std::mem::take(&mut self.candidates),
            &self.caps,
            Some(self.generation),
            &mut self.request,
        )?);
        if !self.request.completion_sdk_is_current() {
            return Err(StaleCompletion.into());
        }
        Ok(true)
    }

    fn into_response(self) -> CompletionResponse {
        self.result
            .expect("completion task must finish its last batch")
    }
}
