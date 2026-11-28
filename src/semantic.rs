use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyItem, CallHierarchyOutgoingCall, CodeActionContext,
    CodeActionKind, CodeActionOrCommand, FileChangeType, FoldingRange, InlayHint, Position, Range,
    SemanticTokens, TextDocumentContentChangeEvent, TextEdit, TypeHierarchyItem, Uri,
};
use tree_sitter::{Node, Parser};

use crate::go_environment::{EnvironmentOverrides, GoEnvironment};
use crate::symbols::{
    Container, FunctionSignature, Symbol, SymbolKind, TypeDetails, TypeRef, TypeRefKind,
};
use crate::telemetry;
use crate::workspace::{
    DiagnosticFileObservation, Package, Workspace, WorkspaceLoad, is_go_manifest, path_key,
};
use completion::prefilter::FileNames;

pub(crate) mod actions;
pub(crate) mod completion;
mod constants;
mod definition;
pub(crate) mod diagnostics;
mod disk_selection;
mod document_symbols;
mod editor;
pub(crate) mod hierarchy;
mod hover;
mod implementation;
mod index;
mod inference_memo;
pub(crate) mod navigation_trace;
mod open_overlay;
mod package_index;
mod presentation;
mod query;
mod references;
pub(crate) mod rename;
mod resolver;
mod shared_disk;
mod signature_help;
pub(crate) mod snapshot;
mod source;
mod workspace_install;
mod workspace_symbols;

pub(crate) use definition::DefinitionKind;
use disk_selection::DiskPackageSelection;
pub(crate) use editor::{EditorFoldingOptions, legend as semantic_tokens_legend};
pub(crate) use editor::{HighlightInput, HighlightOutput, HighlightPreparation};
use index::{CachedNavigationText, DiskIndex, IndexedSource, text_fingerprint};
pub(crate) use navigation_trace::NavigationTrace;
pub(crate) use open_overlay::OpenParseResult;
use open_overlay::{OpenParser, PendingOpen};
pub(crate) use query::{QueryProgress, SemanticTask};
pub(crate) use resolver::TypeIdentity;
#[cfg(test)]
pub(crate) use shared_disk::CacheEvent;
pub(crate) use shared_disk::{DiagnosticIo, QueryReadMemo, SharedLoadPending};
pub(crate) use workspace_install::LoadedWorkspace;
use workspace_install::WorkspaceCacheChanges;
// Preserve the shared imports used by the existing presentation and feature submodules.
use index::IndexedSource as Source;
use resolver::{
    MAX_TYPE_DEPTH, ResolvedSymbol, Resolver, Substitutions, TypeKey, TypeKind, is_exported,
    is_predeclared, is_predeclared_type, materialize_type, untyped_default,
};
use shared_disk::SharedDiskCache;
use source::{OpenText, ReferenceQuery, Source as SyntaxSource, node_text};

#[derive(Clone, Default)]
enum GraphChanges {
    #[default]
    Clean,
    Packages(Arc<HashSet<String>>),
    Global,
}

impl GraphChanges {
    fn affects(&self, package: &Package) -> bool {
        match self {
            Self::Clean => false,
            Self::Packages(packages) => packages.contains(&package.id),
            Self::Global => true,
        }
    }

    fn affects_source(
        &self,
        workspace: &Workspace,
        from: Option<&Package>,
        source: &SyntaxSource,
    ) -> bool {
        !matches!(self, Self::Clean)
            && from.is_none_or(|package| {
                self.affects(package)
                    || source.imports.iter().any(|import| {
                        workspace
                            .resolve_import(package, &import.path)
                            .is_none_or(|dependency| self.affects(dependency))
                    })
            })
    }

    fn include(&mut self, affected: Option<HashSet<String>>) {
        match (&mut *self, affected) {
            (changes, None) => *changes = Self::Global,
            (Self::Global, _) => {}
            (Self::Packages(packages), Some(affected)) => Arc::make_mut(packages).extend(affected),
            (changes, Some(affected)) => *changes = Self::Packages(Arc::new(affected)),
        }
    }
}

pub struct Engine {
    pub(crate) track_rename_inputs: bool,
    state: EngineState,
    diagnostics_cache: diagnostics::DiagnosticsCache,
    implementation_cache: implementation::ImplementationCache,
    implementation_sources: implementation::InterfaceSources,
    workspace_symbol_index: workspace_symbols::WorkspaceSymbolIndex,
    completion_imports: completion::ImportCache,
    #[cfg(test)]
    completion_hook: Option<completion::CompletionHook>,
    formatting_cache: actions::GofmtCache,
    reference_worker: references::ReferenceWorker,
    reference_candidates: references::ReferenceCandidates,
    open_parser: Option<OpenParser>,
    open_parse_idle: crossbeam_channel::Receiver<OpenParseResult>,
    include_tests: bool,
}

struct EngineState {
    roots: Vec<PathBuf>,
    environment: GoEnvironment,
    workspace: Option<Arc<Workspace>>,
    graph_changes: GraphChanges,
    generation: u64,
    completion_graph_generation: u64,
    completion_source_generation: u64,
    declaration_generation: u64,
    parser: Parser,
    documents: HashMap<PathBuf, IndexedSource>,
    pending_documents: HashMap<PathBuf, PendingOpen>,
    new_documents: HashSet<PathBuf>,
    disk_package_selections: HashMap<PathBuf, DiskPackageSelection>,
    disk_index: HashMap<PathBuf, DiskIndex>,
    shared_disk: Arc<SharedDiskCache>,
    hover_docs: RefCell<hover::HoverDocCache>,
    navigation_texts: HashMap<PathBuf, CachedNavigationText>,
    completion_names: HashMap<PathBuf, FileNames>,
    completion_weights: HashMap<completion::InferredMemo, i32>,
    diagnostic_file_changes: Arc<HashMap<PathBuf, DiagnosticFileObservation>>,
    diagnostic_keys: RefCell<diagnostics::DiagnosticKeys>,
    workspace_cache_changes: Option<WorkspaceCacheChanges>,
}

impl QueryReadMemo {
    pub(crate) fn touched_overlay(&self, key: &Path) -> bool {
        self.overlays.borrow().contains_key(key)
    }
}

impl Engine {
    pub(crate) fn query_generations(&self) -> (u64, u64) {
        (
            self.state.completion_graph_generation,
            self.state.shared_disk.generation(),
        )
    }

    pub(crate) fn track_query_overlay(&self, memo: &QueryReadMemo, path: &Path) {
        let key = path_key(path);
        index::track_overlay(
            memo,
            &key,
            &self.state.documents,
            Some(&self.state.pending_documents),
        );
    }

    pub(crate) fn query_overlays_current(&self, memo: &QueryReadMemo) -> bool {
        memo.overlays.borrow().iter().all(|(key, revision)| {
            *revision
                == index::overlay_revision(
                    key,
                    &self.state.documents,
                    Some(&self.state.pending_documents),
                )
        })
    }

    pub(crate) fn shared_disk_wake(&self) -> &crossbeam_channel::Receiver<()> {
        self.state.shared_disk.wake()
    }

    #[cfg(test)]
    pub(crate) fn set_shared_disk_hook(
        &self,
        hook: Option<Arc<dyn Fn(&Path, CacheEvent) + Send + Sync>>,
    ) {
        self.state.shared_disk.set_hook(hook);
    }

    pub(crate) fn semantic_tokens_full(&mut self, path: &Path) -> Result<SemanticTokens> {
        Ok(self
            .snapshot()
            .document(path)?
            .as_ref()
            .map_or_else(SemanticTokens::default, editor::semantic_tokens_full))
    }

    pub(crate) fn inlay_hints(&mut self, path: &Path, range: Range) -> Result<Vec<InlayHint>> {
        Ok(self
            .snapshot()
            .document(path)?
            .as_ref()
            .map_or_else(Vec::new, |document| editor::inlay_hints(document, range)))
    }

    pub(crate) fn folding_ranges(
        &mut self,
        path: &Path,
        options: EditorFoldingOptions,
    ) -> Result<Vec<FoldingRange>> {
        Ok(self
            .snapshot()
            .document(path)?
            .as_ref()
            .map_or_else(Vec::new, |document| {
                editor::folding_ranges(document, options)
            }))
    }

    #[cfg(test)]
    pub fn new(root: PathBuf) -> Self {
        Self::with_roots(vec![root])
    }

    #[cfg(test)]
    pub(crate) fn with_roots(roots: Vec<PathBuf>) -> Self {
        Self::with_environment(roots, GoEnvironment::capture())
    }

    pub(crate) fn with_environment(roots: Vec<PathBuf>, environment: GoEnvironment) -> Self {
        Self::with_shared_disk(roots, environment, Arc::default())
    }

    fn with_shared_disk(
        roots: Vec<PathBuf>,
        environment: GoEnvironment,
        shared_disk: Arc<SharedDiskCache>,
    ) -> Self {
        Self {
            track_rename_inputs: false,
            state: EngineState::new(roots, environment, shared_disk.clone()),
            diagnostics_cache: diagnostics::DiagnosticsCache::default(),
            implementation_cache: implementation::ImplementationCache::default(),
            implementation_sources: implementation::InterfaceSources::default(),
            workspace_symbol_index: workspace_symbols::WorkspaceSymbolIndex::default(),
            completion_imports: completion::ImportCache::default(),
            #[cfg(test)]
            completion_hook: None,
            formatting_cache: actions::GofmtCache::default(),
            reference_worker: references::ReferenceWorker::new(shared_disk),
            reference_candidates: references::ReferenceCandidates::default(),
            open_parser: None,
            open_parse_idle: crossbeam_channel::never(),
            include_tests: false,
        }
    }

    pub fn open(&mut self, path: PathBuf, text: String, version: i32) -> Result<()> {
        if path.extension().is_some_and(|extension| extension == "go") {
            let key = path_key(&path);
            let new_file = !path.is_file();
            if !new_file
                && !self.state.disk_package_selections.contains_key(&key)
                && let Some(stamp) = self
                    .state
                    .workspace
                    .as_ref()
                    .and_then(|workspace| workspace.selection_baseline_stamp(&key))
                && let Some(selection) =
                    DiskPackageSelection::read(&mut self.state.parser, &path, Some(stamp))
            {
                self.state
                    .disk_package_selections
                    .insert(key.clone(), selection);
            }
            if !self.implementation_cache.is_empty() {
                let unchanged = match self.state.documents.get(&key) {
                    Some(previous) => previous.text.as_ref() == &text,
                    None => {
                        !new_file
                            && self
                                .state
                                .disk_index
                                .get(&key)
                                .is_some_and(|disk| disk.fingerprint == text_fingerprint(&text))
                    }
                };
                if !unchanged {
                    self.implementation_cache.clear();
                }
            }
            if text.len() >= source::LARGE_OPEN_BYTES {
                if self.open_parser.is_none() {
                    self.open_parser = Some(OpenParser::new()?);
                }
                let previous_draft = self.state.diagnostic_draft(&key);
                self.state.documents.remove(&key);
                self.diagnostics_cache.invalidate_document(&key);
                self.state.completion_sources_changed(Some(&path));
                let revision = self.state.completion_source_generation;
                if new_file {
                    self.state.new_documents.insert(key.clone());
                } else {
                    self.state.new_documents.remove(&key);
                }
                self.workspace_symbol_index.document_opened(
                    self.state.workspace.as_deref(),
                    &self.state.roots,
                    &key,
                    &path,
                    new_file,
                );
                self.implementation_sources.invalidate(&key);
                self.state.declarations_changed(&key, previous_draft);
                self.state.pending_documents.insert(
                    key,
                    PendingOpen {
                        path,
                        text: OpenText::new(text),
                        version,
                        open_generation: revision,
                        content_revision: revision,
                        ready: Default::default(),
                    },
                );
                return self.schedule_open_parse();
            }
            let source = SyntaxSource::parse(&mut self.state.parser, path, text, None)?;
            let mut source = IndexedSource::new(source, Some(version));
            let previous_draft = self.state.diagnostic_draft(&key);
            let same_declarations = self.state.new_documents.contains(&key) == new_file
                && self.state.documents.get(&key).is_some_and(|previous| {
                    previous
                        .declaration_summary()
                        .is_some_and(|summary| source.declaration_summary() == Some(summary))
                });
            if same_declarations {
                source.declaration_revision = self.state.documents[&key].declaration_revision;
            }
            self.diagnostics_cache.invalidate_document(&key);
            self.state.completion_sources_changed(Some(&source.path));
            source.content_revision = self.state.completion_source_generation;
            source.open_generation = source.content_revision;
            if new_file {
                self.state.new_documents.insert(key.clone());
            } else {
                self.state.new_documents.remove(&key);
            }
            self.workspace_symbol_index.document_opened(
                self.state.workspace.as_deref(),
                &self.state.roots,
                &key,
                &source.path,
                new_file,
            );
            self.implementation_sources.invalidate(&key);
            self.state.pending_documents.remove(&key);
            self.state.documents.insert(key.clone(), source);
            if !same_declarations {
                self.state.declarations_changed(&key, previous_draft);
            }
            self.schedule_open_parse()?;
        }
        Ok(())
    }

    pub fn edit(
        &mut self,
        path: &Path,
        edits: &[TextDocumentContentChangeEvent],
        version: i32,
    ) -> Result<()> {
        let key = path_key(path);
        if let Some(pending) = self.state.pending_documents.get_mut(&key)
            && !edits.is_empty()
        {
            pending.text.edit(edits)?;
            pending.version = version;
            pending.content_revision = self.state.completion_source_generation + 1;
            self.diagnostics_cache.invalidate_reports(&key);
            self.state.completion_sources_changed(Some(path));
            self.state.declarations_changed(&key, None);
            self.workspace_symbol_index.document_edited(&key);
            self.implementation_sources.invalidate(&key);
            self.implementation_cache.clear();
            return self.schedule_open_parse();
        }
        let previous_draft = self.state.diagnostic_draft(&key);
        if let Some(source) = self.state.documents.get_mut(&key)
            && !edits.is_empty()
        {
            let mut edit = implementation::ObservedEdit::new();
            let mut observe = |delta: source::TextEditDelta<'_>| edit.observe(delta);
            let previous = source.declaration_summary().cloned();
            source.edit(
                &mut self.state.parser,
                edits,
                version,
                (!self.implementation_cache.is_empty()).then_some(&mut observe as _),
            )?;
            source.content_revision = self.state.completion_source_generation + 1;
            let same_declarations = previous
                .as_ref()
                .is_some_and(|summary| source.declaration_summary() == Some(summary));
            // The declaration summary covers full signatures and import
            // bindings. Postings also depend on lexical and local type spellings.
            let same_spellings = same_declarations
                && self
                    .implementation_sources
                    .same_overlay_spellings(&key, source);
            self.diagnostics_cache.invalidate_reports(&key);
            self.state.completion_sources_changed(Some(path));
            if !same_declarations {
                self.state.declarations_changed(&key, previous_draft);
            }
            self.workspace_symbol_index.document_edited(&key);
            if !same_spellings {
                self.implementation_sources.invalidate(&key);
            }
            edit.apply(&mut self.implementation_cache, &key);
        }
        Ok(())
    }

    pub fn close(&mut self, path: &Path) {
        self.implementation_cache.clear();
        let key = path_key(path);
        self.diagnostics_cache.invalidate_document(&key);
        self.implementation_sources.invalidate(&key);
        let previous_draft = self.state.diagnostic_draft(&key);
        let was_pending = self.state.pending_documents.remove(&key).is_some();
        if self.state.documents.remove(&key).is_some() || was_pending {
            self.state.shared_disk.invalidate(&key);
            self.state.completion_sources_changed(Some(path));
            self.state.declarations_changed(&key, previous_draft);
            self.workspace_symbol_index.document_closed(&key);
        }
        if was_pending && let Err(error) = self.schedule_open_parse() {
            eprintln!("rs-gopls: updating Go open parser after close: {error:#}");
        }
        if self.state.new_documents.remove(&key) && path.is_file() {
            // A new buffer can be saved by a client that does not send a
            // watched-file notification. Refresh the build's file selection.
            self.file_changed_from(path, FileChangeType::CREATED, "didClose", 1);
        }
        if self.state.navigation_texts.contains_key(&key)
            || self.state.disk_index.contains_key(&key)
            || self.state.completion_names.contains_key(&key)
        {
            let text = fs::read_to_string(path).ok();
            if self
                .state
                .navigation_texts
                .get(&key)
                .is_some_and(|cached| !cached.matches(text.as_deref()))
            {
                self.state.navigation_texts.remove(&key);
            }
            let fingerprint = text.as_deref().map(text_fingerprint);
            if self
                .state
                .disk_index
                .get(&key)
                .is_some_and(|index| Some(index.fingerprint) != fingerprint)
            {
                self.state.disk_index.remove(&key);
                self.completion_imports.source_changed(&key);
            }
            if self
                .state
                .completion_names
                .get(&key)
                .is_some_and(|names| Some(names.fingerprint) != fingerprint)
            {
                self.state.completion_names.remove(&key);
                self.completion_imports.source_changed(&key);
            }
        }
    }

    pub fn root(&self) -> &Path {
        self.roots()
            .first()
            .map_or(Path::new("."), PathBuf::as_path)
    }

    pub(crate) fn roots(&self) -> &[PathBuf] {
        &self.state.roots
    }

    pub(crate) fn environment(&self) -> &GoEnvironment {
        &self.state.environment
    }

    pub(crate) fn update_environment(&mut self, overrides: EnvironmentOverrides) -> bool {
        let environment = self.environment().with_overrides(overrides);
        if environment == *self.environment() {
            return false;
        }
        self.state.environment = environment;
        self.state.workspace = None;
        self.state.completion_names.clear();
        self.diagnostics_cache.clear();
        self.completion_imports = completion::ImportCache::default();
        self.formatting_cache.clear();
        self.implementation_sources = implementation::InterfaceSources::default();
        self.workspace_symbol_index.graph_changed();
        self.invalidate_observed(None, "didChangeConfiguration", "env_changed", 0, None);
        true
    }

    pub(crate) fn update_roots(&mut self, roots: Vec<PathBuf>) -> bool {
        if self
            .roots()
            .iter()
            .map(|root| path_key(root))
            .eq(roots.iter().map(|root| path_key(root)))
        {
            return false;
        }
        if self.roots().first().map(|p| path_key(p)) != roots.first().map(|p| path_key(p)) {
            self.completion_imports = completion::ImportCache::default();
        }
        self.state.roots = roots;
        self.diagnostics_cache.clear();
        self.invalidate_observed(
            None,
            "didChangeWorkspaceFolders",
            "folders_changed",
            0,
            None,
        );
        true
    }

    pub fn is_open(&self, path: &Path) -> bool {
        self.open_document_identity(path).is_some()
    }

    pub(crate) fn open_document_paths(&self) -> Vec<PathBuf> {
        self.state
            .documents
            .values()
            .map(|doc| doc.path.clone())
            .chain(
                self.state
                    .pending_documents
                    .values()
                    .map(|open| open.path.clone()),
            )
            .collect()
    }

    pub(crate) fn open_document_identity(&self, path: &Path) -> Option<(u64, i32)> {
        let key = path_key(path);
        if let Some(pending) = self.state.pending_documents.get(&key) {
            return Some((pending.open_generation, pending.version));
        }
        let source = self.state.documents.get(&key)?;
        Some((source.open_generation, source.document_version?))
    }

    pub(crate) fn has_pending_open_parses(&self) -> bool {
        !self.state.pending_documents.is_empty()
    }

    pub(crate) fn document_parse_pending(&self, path: &Path) -> bool {
        self.state.pending_documents.contains_key(&path_key(path))
    }

    pub(crate) fn pending_document_affects(&self, path: &Path) -> bool {
        self.pending_open_dependency(path).is_some()
    }

    pub(crate) fn pending_reference_document_affects(&self, path: &Path) -> bool {
        self.pending_document_affects(path)
            || (self.has_pending_open_parses()
                && self.state.workspace.as_ref().is_some_and(|graph| {
                    graph.affected_packages(path).is_some_and(|affected| {
                        graph.navigation_packages().any(|package| {
                            affected.contains(&package.id)
                                && self.state.pending_documents.keys().any(|pending| {
                                    pending.parent() == Some(package.directory_and_name().0)
                                })
                        })
                    })
                }))
    }

    pub(crate) fn pending_open_dependency(&self, path: &Path) -> Option<SharedLoadPending> {
        self.state.pending_open_dependency(path)
    }

    pub(crate) fn open_parse_wake(&self) -> &crossbeam_channel::Receiver<OpenParseResult> {
        self.open_parser
            .as_ref()
            .map_or(&self.open_parse_idle, |parser| &parser.completed)
    }

    pub(crate) fn complete_open_parse(
        &mut self,
        result: OpenParseResult,
    ) -> Result<Option<PathBuf>> {
        let Some(parser) = &mut self.open_parser else {
            return Ok(None);
        };
        let Some((key, open, mut source)) =
            parser.complete(result, &mut self.state.pending_documents)?
        else {
            return Ok(None);
        };
        source.open_generation = open.open_generation;
        source.content_revision = open.content_revision;
        self.state.documents.insert(key.clone(), source);
        self.implementation_sources.invalidate(&key);
        self.state.declarations_changed(&key, None);
        Ok(Some(open.path))
    }

    fn schedule_open_parse(&mut self) -> Result<()> {
        if let Some(parser) = &mut self.open_parser {
            parser.schedule(&self.state.pending_documents)?;
        }
        Ok(())
    }

    pub fn workspace_load(&self) -> WorkspaceLoad {
        WorkspaceLoad {
            generation: self.state.generation,
            include_tests: self.include_tests,
        }
    }

    pub(crate) fn workspace_wait_reason(&self, load: WorkspaceLoad) -> &'static str {
        match self.state.workspace.as_deref() {
            None => "no_graph",
            Some(graph) if graph.total_load_failure().is_some() => "graph_load_failed",
            Some(graph) if load.include_tests && !graph.includes_tests() => "missing_test_view",
            Some(_) if !matches!(self.state.graph_changes, GraphChanges::Clean) => "dirty_graph",
            Some(_) => "workspace_required",
        }
    }

    pub(crate) fn test_view_prewarm(&self) -> Option<WorkspaceLoad> {
        let graph = self.state.workspace.as_deref()?;
        (!graph.includes_tests()
            && graph.total_load_failure().is_none()
            && matches!(self.state.graph_changes, GraphChanges::Clean))
        .then_some(WorkspaceLoad {
            include_tests: true,
            ..self.workspace_load()
        })
    }

    pub(crate) fn prewarm_completion_sdk(&self, refresh: bool) {
        self.completion_imports.prewarm(
            self.root(),
            self.environment(),
            refresh && self.state.workspace.is_some(),
        );
    }

    #[cfg(test)]
    pub(crate) fn completion_wake(&self) -> &crossbeam_channel::Receiver<()> {
        self.completion_imports.wake()
    }

    pub(crate) fn reference_wake(&self) -> &crossbeam_channel::Receiver<()> {
        self.reference_worker.wake()
    }

    fn invalidate(&mut self, affected: Option<HashSet<String>>) {
        self.implementation_cache.clear();
        self.implementation_sources.cancel_build();
        self.state.generation += 1;
        self.state.completion_graph_generation += 1;
        self.state.completion_sources_changed(None);
        if affected.is_none() {
            self.state.diagnostic_keys.get_mut().clear();
        }
        self.state.graph_changes.include(affected);
    }

    fn invalidate_observed(
        &mut self,
        affected: Option<HashSet<String>>,
        trigger: &'static str,
        reason: &'static str,
        changed_file_count: usize,
        path: Option<&Path>,
    ) {
        let old_generation = self.state.generation;
        let scope = if affected.is_none() {
            "global"
        } else {
            "package_subset"
        };
        self.invalidate(affected);
        telemetry::event!("workspace_dirty", {
            "trigger": trigger,
            "reason": reason,
            "scope": scope,
            "changed_file_count": changed_file_count,
            "old_generation": old_generation,
            "new_generation": self.state.generation,
            "document": path.map(telemetry::path),
        });
    }

    #[cfg(test)]
    pub fn file_changed(&mut self, path: &Path, change: FileChangeType) {
        self.file_changed_from(path, change, "watched_files", 1);
    }

    pub(crate) fn file_changed_from(
        &mut self,
        path: &Path,
        change: FileChangeType,
        trigger: &'static str,
        changed_file_count: usize,
    ) {
        if let Some((affected, reason)) =
            self.observe_file_change(path, change, trigger == "watched_files")
        {
            self.invalidate_observed(affected, trigger, reason, changed_file_count, Some(path));
        }
    }

    pub(crate) fn watched_files_changed(&mut self, changes: &[(PathBuf, FileChangeType)]) {
        let mut dirty = GraphChanges::Clean;
        let mut reason = None;
        let mut document = None;
        let mut dirty_count = 0;
        for (path, change) in changes {
            if let Some((affected, change_reason)) = self.observe_file_change(path, *change, true) {
                dirty.include(affected);
                reason = Some(change_reason);
                document = Some(path.as_path());
                dirty_count += 1;
            }
        }
        let affected = match dirty {
            GraphChanges::Clean => return,
            GraphChanges::Packages(packages) => Some(Arc::unwrap_or_clone(packages)),
            GraphChanges::Global => None,
        };
        self.invalidate_observed(
            affected,
            "watched_files",
            if dirty_count == 1 {
                reason.unwrap()
            } else {
                "file_batch"
            },
            changes.len(),
            document.filter(|_| dirty_count == 1),
        );
    }

    pub(crate) fn watched_package_selection_is_unchanged(
        &mut self,
        path: &Path,
        change: FileChangeType,
    ) -> bool {
        if change != FileChangeType::CHANGED
            || path.extension().is_none_or(|extension| extension != "go")
        {
            return false;
        }
        let key = path_key(path);
        let Some(graph) = self.state.workspace.as_deref() else {
            return false;
        };
        if self.state.new_documents.contains(&key)
            || graph
                .package_of_file(&key)
                .is_none_or(|package| package.load_error)
        {
            return false;
        }
        let stamp = graph.diagnostic_disk_stamp(&key, self.state.diagnostic_file_changes.get(&key));
        let Some(previous) = self
            .state
            .disk_package_selections
            .get(&key)
            .filter(|previous| Some(previous.stamp) == stamp)
        else {
            return false;
        };
        DiskPackageSelection::read(&mut self.state.parser, path, None)
            .is_some_and(|disk| disk.selection == previous.selection)
    }

    fn observe_file_change(
        &mut self,
        path: &Path,
        change: FileChangeType,
        watcher: bool,
    ) -> Option<(Option<HashSet<String>>, &'static str)> {
        if is_go_manifest(path) {
            return Some((None, "mod_or_work"));
        }
        if path.extension().is_none_or(|extension| extension != "go") {
            return None;
        }

        self.implementation_cache.clear();
        self.state.completion_sources_changed(Some(path));
        let key = path_key(path);
        let is_open = self.is_open(path);
        let graph = self.state.workspace.as_deref();
        let old_observation = self.state.diagnostic_file_changes.get(&key);
        let old_stamp = graph.and_then(|graph| graph.diagnostic_disk_stamp(&key, old_observation));
        let semantic_input = graph.map(|graph| graph.diagnostic_disk_input(&key, old_observation));
        let selected = graph.and_then(|graph| graph.package_of_file(&key));
        let known_complete = selected.is_some_and(|package| !package.load_error);
        let unselected = selected.is_none();
        let previous = self.state.disk_index.remove(&key);
        let previous_selection = self.state.disk_package_selections.remove(&key);
        let shared_read_in_progress = self.state.shared_disk.read_pending(&key);
        let previous_declarations =
            old_stamp
                .filter(|_| !shared_read_in_progress)
                .and_then(|stamp| {
                    self.state
                        .shared_disk
                        .cached_declaration_revision(&key, stamp)
                });
        let previous_revision = old_stamp
            .filter(|_| !shared_read_in_progress && previous_declarations.is_none())
            .and_then(|stamp| self.state.shared_disk.cached_revision(&key, stamp))
            .map(|revision| revision.as_ref().clone().into_indexed());
        self.state.shared_disk.invalidate_disk_change(&key, watcher);
        let metadata = fs::metadata(path);
        let file_exists = metadata.as_ref().is_ok_and(|metadata| metadata.is_file());
        let can_parse = metadata.as_ref().is_ok_and(|metadata| {
            metadata.is_file() && metadata.len() < source::LARGE_OPEN_BYTES as u64
        });
        let mut observation = DiagnosticFileObservation::new(
            metadata
                .ok()
                .filter(std::fs::Metadata::is_file)
                .map(Into::into),
            old_observation.map_or(1, |change| change.epoch + 1),
        );
        self.state.completion_names.remove(&key);
        self.state.navigation_texts.remove(&key);
        self.completion_imports.source_changed(&key);
        self.workspace_symbol_index.disk_changed(&key, is_open);
        let previous_draft = self.state.diagnostic_draft(&key);
        let saved_draft = file_exists && self.state.new_documents.remove(&key);
        let graph_verified_previous = old_stamp.is_some()
            && graph.and_then(|graph| graph.selection_baseline_stamp(&key)) == old_stamp;
        let previous_index = previous
            .as_ref()
            .filter(|index| index.stamp == old_stamp)
            .and_then(|index| index.declarations.upgrade());
        let previous_signature = previous_selection
            .as_ref()
            .filter(|selection| Some(selection.stamp) == old_stamp)
            .map(|selection| &selection.selection)
            .or_else(|| {
                if !graph_verified_previous {
                    return None;
                }
                previous_declarations
                    .as_ref()
                    .and_then(|source| source.index.package_selection.as_ref())
                    .or_else(|| {
                        previous_revision
                            .as_ref()
                            .and_then(|source| source.declarations().package_selection.as_ref())
                    })
                    .or_else(|| {
                        previous_index
                            .as_ref()
                            .and_then(|index| index.package_selection.as_ref())
                    })
            });
        let observed_selection = observation.stamp.and_then(|stamp| {
            DiskPackageSelection::read(&mut self.state.parser, path, Some(stamp))
        });
        let signatures = previous_signature.zip(
            observed_selection
                .as_ref()
                .map(|selection| &selection.selection),
        );
        let signature_changed = signatures.is_some_and(|(previous, updated)| previous != updated);
        let same_selection = known_complete
            && change == FileChangeType::CHANGED
            && !saved_draft
            && signatures.is_some()
            && !signature_changed;
        if let Some(selection) = observed_selection {
            self.state
                .disk_package_selections
                .insert(key.clone(), selection);
        }
        if !same_selection || !self.state.documents.contains_key(&key) {
            self.implementation_sources.invalidate(&key);
        }

        let mut same_declarations = false;
        let previous_summary = previous_declarations
            .as_ref()
            .and_then(|source| source.summary.as_ref())
            .or_else(|| {
                previous_revision
                    .as_ref()
                    .and_then(IndexedSource::declaration_summary)
            });
        if same_selection
            && !is_open
            && !shared_read_in_progress
            && can_parse
            && let Some(previous) = previous_summary
            && let Ok(disk) = self.state.shared_disk.load_declarations(
                &mut self.state.parser,
                path,
                None,
                None,
                None,
            )
            && disk.text.stamp == observation.stamp
        {
            same_declarations = disk.declarations.summary.as_ref() == Some(previous);
            if same_declarations && let Some(input) = semantic_input {
                observation.retain_semantics(input);
            }
            self.state
                .disk_index
                .insert(key.clone(), DiskIndex::from_declarations(&disk));
        }
        if saved_draft || !is_open && !(same_selection && same_declarations) {
            self.state.declarations_changed(&key, previous_draft);
        }
        Arc::make_mut(&mut self.state.diagnostic_file_changes).insert(key, observation);

        if same_selection
            || unselected
                && path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(['.', '_']))
        {
            return None;
        }

        // A new, removed, or structurally changed source can change the set
        // of Go files selected by the current build or add an import.
        let affected = self
            .state
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.affected_packages(path));
        let reason = if !file_exists || saved_draft || change != FileChangeType::CHANGED {
            "created_or_deleted"
        } else if self.state.workspace.is_some() && affected.is_none() {
            "unknown_pkg"
        } else if signature_changed {
            "signature_changed"
        } else {
            "cold_file"
        };
        Some((affected, reason))
    }
}

impl Engine {
    #[cfg(test)]
    pub(crate) fn diagnostics_report(
        &mut self,
        path: &Path,
    ) -> Result<(&diagnostics::DiagnosticReport, Option<WorkspaceLoad>)> {
        self.include_tests |= source::is_test_source(path);
        let load = self.workspace_load();
        let mut snapshot = self.state.snapshot(self.include_tests);
        let document = snapshot.document(path)?.context("expected a Go document")?;
        let report =
            diagnostics::get_or_compute(&mut self.diagnostics_cache, &mut snapshot, &document)?;
        Ok((report, report.snapshot.partial.then_some(load)))
    }

    pub(crate) fn format_document(&mut self, path: &Path) -> Result<Option<Vec<TextEdit>>> {
        let mut snapshot = self.state.snapshot(self.include_tests);
        snapshot
            .document(path)?
            .map(|document| {
                actions::format_document(
                    &document,
                    path.parent().filter(|dir| dir.is_dir()),
                    snapshot.environment(),
                    &mut self.formatting_cache,
                )
            })
            .transpose()
    }

    /// `None` lets an explicit QuickFix request discover fixes from a complete report.
    pub(crate) fn local_code_actions(
        &mut self,
        path: &Path,
        uri: &Uri,
        range: Range,
        context: &CodeActionContext,
    ) -> Result<Option<Vec<CodeActionOrCommand>>> {
        let only = context.only.as_deref();
        let quickfix = actions::wants_kind(only, &CodeActionKind::QUICKFIX);
        let organize = actions::wants_kind(only, &CodeActionKind::SOURCE_ORGANIZE_IMPORTS);
        let use_report = quickfix
            && self.diagnostics_cache.has_reports_for(&path_key(path))
            && !self.pending_document_affects(path);
        let mut snapshot = self.state.snapshot(self.include_tests);
        let key = use_report
            .then(|| diagnostics::snapshot_key_for_path(&snapshot, path).ok())
            .flatten();
        let Some(document) = snapshot.document(path)? else {
            return Ok(Some(Vec::new()));
        };
        if actions::exact_range(&document, range).is_none() {
            return Ok(Some(Vec::new()));
        }
        let report = key
            .as_ref()
            .filter(|key| {
                document.version().is_some()
                    || diagnostics::snapshot_key(&snapshot, &document).is_ok_and(|now| &now == *key)
            })
            .and_then(|key| self.diagnostics_cache.for_code_actions(key));
        let edits = actions::code_actions(
            &mut snapshot,
            &document,
            uri,
            range,
            only,
            report,
            &context.diagnostics,
        )?;
        let discover = edits.is_empty()
            && quickfix
            && !organize
            && context.diagnostics.is_empty()
            && report.is_none_or(|report| {
                report.diagnostic_stage != diagnostics::DiagnosticStage::Stage2
            });
        Ok((!discover).then_some(edits))
    }

    pub(crate) fn code_actions_for_diagnostic_key(
        &mut self,
        path: &Path,
        uri: &Uri,
        range: Range,
        only: Option<&[CodeActionKind]>,
        key: &diagnostics::DiagnosticSnapshotKey,
    ) -> Result<Vec<CodeActionOrCommand>> {
        let Some(report) = self.diagnostics_cache.get_complete(key) else {
            return Ok(Vec::new());
        };
        if report.items_overlapping(range).next().is_none() {
            return Ok(Vec::new());
        }
        let mut snapshot = self.state.snapshot(self.include_tests);
        let Some(document) = snapshot.document(path)? else {
            return Ok(Vec::new());
        };
        if !diagnostics::snapshot_key(&snapshot, &document).is_ok_and(|now| &now == key) {
            return Ok(Vec::new());
        }
        actions::code_actions(
            &mut snapshot,
            &document,
            uri,
            range,
            only,
            Some(report),
            &[],
        )
    }

    pub(crate) fn prepare_call_hierarchy(
        &mut self,
        path: &Path,
        position: Position,
    ) -> Result<QueryProgress<Option<Vec<CallHierarchyItem>>>> {
        hierarchy::call::prepare_call_hierarchy(self, path, position)
    }

    pub(crate) fn incoming_calls(
        &mut self,
        item: &CallHierarchyItem,
    ) -> Result<
        QueryProgress<Option<Vec<CallHierarchyIncomingCall>>, hierarchy::call::IncomingCallsTask>,
    > {
        hierarchy::call::incoming_calls(self, item)
    }

    pub(crate) fn outgoing_calls(
        &mut self,
        item: &CallHierarchyItem,
    ) -> Result<QueryProgress<Option<Vec<CallHierarchyOutgoingCall>>>> {
        hierarchy::call::outgoing_calls(self, item)
    }

    pub(crate) fn prepare_type_hierarchy(
        &mut self,
        path: &Path,
        position: Position,
    ) -> Result<QueryProgress<Option<Vec<TypeHierarchyItem>>>> {
        hierarchy::type_hierarchy::prepare_type_hierarchy(self, path, position)
    }

    pub(crate) fn supertypes(
        &mut self,
        item: &TypeHierarchyItem,
    ) -> Result<
        QueryProgress<Option<Vec<TypeHierarchyItem>>, hierarchy::type_hierarchy::TypeHierarchyTask>,
    > {
        hierarchy::type_hierarchy::supertypes(self, item)
    }

    pub(crate) fn subtypes(
        &mut self,
        item: &TypeHierarchyItem,
    ) -> Result<
        QueryProgress<Option<Vec<TypeHierarchyItem>>, hierarchy::type_hierarchy::TypeHierarchyTask>,
    > {
        hierarchy::type_hierarchy::subtypes(self, item)
    }
}

impl EngineState {
    fn pending_open_dependency(&self, path: &Path) -> Option<SharedLoadPending> {
        if self.pending_documents.is_empty() {
            return None;
        }
        let key = path_key(path);
        let workspace = self.workspace.as_deref();
        let source = self.documents.get(&key);
        let package = workspace.and_then(|graph| {
            source.map_or_else(
                || graph.package_of_file(path),
                |source| index::active_package(graph, source, &self.new_documents),
            )
        });
        self.pending_documents.iter().find_map(|(pending, open)| {
            if pending == &key || pending.parent() == key.parent() {
                return Some(open.ready.pending());
            }
            let (graph, package) = workspace.zip(package)?;
            let affected = graph.affected_packages(pending)?;
            let depends = affected.contains(&package.id)
                || source.is_some_and(|source| {
                    source.diagnostic_imports.iter().any(|import| {
                        import
                            .path
                            .as_deref()
                            .or(import.decoded_path.as_deref())
                            .and_then(|path| graph.resolve_import(package, path))
                            .is_some_and(|dependency| affected.contains(&dependency.id))
                    })
                });
            depends.then(|| open.ready.pending())
        })
    }

    fn new(
        roots: Vec<PathBuf>,
        environment: GoEnvironment,
        shared_disk: Arc<SharedDiskCache>,
    ) -> Self {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
            .expect("Go grammar must match the tree-sitter runtime");
        Self {
            roots,
            environment,
            workspace: None,
            graph_changes: GraphChanges::default(),
            generation: 0,
            completion_graph_generation: 0,
            completion_source_generation: 0,
            declaration_generation: 0,
            parser,
            documents: HashMap::new(),
            pending_documents: HashMap::new(),
            new_documents: HashSet::new(),
            disk_package_selections: HashMap::new(),
            disk_index: HashMap::new(),
            shared_disk,
            hover_docs: RefCell::default(),
            navigation_texts: HashMap::new(),
            completion_names: HashMap::new(),
            completion_weights: HashMap::new(),
            diagnostic_file_changes: Arc::default(),
            diagnostic_keys: RefCell::default(),
            workspace_cache_changes: None,
        }
    }

    fn diagnostic_draft(&self, key: &Path) -> Option<(PathBuf, String)> {
        if !self.new_documents.contains(key) {
            return None;
        }
        let workspace = self.workspace.as_deref()?;
        let source = self.documents.get(key)?;
        if workspace.package_of_file(&source.path).is_some() {
            return None;
        }
        let (directory, name) = index::draft_package(workspace, source)?.directory_and_name();
        Some((directory.to_owned(), name.to_owned()))
    }

    fn declarations_changed(&mut self, key: &Path, previous_draft: Option<(PathBuf, String)>) {
        self.declaration_generation += 1;
        if let Some(source) = self.documents.get_mut(key) {
            source.declaration_revision = self.declaration_generation;
        }
        let draft = self.diagnostic_draft(key);
        self.diagnostic_keys.get_mut().invalidate_file(
            key,
            [previous_draft.as_ref(), draft.as_ref()]
                .into_iter()
                .flatten()
                .map(|(directory, name)| (directory.as_path(), name.as_str())),
        );
    }

    fn completion_sources_changed(&mut self, path: Option<&Path>) {
        self.completion_source_generation += 1;
        if let Some((changes, path)) = self.workspace_cache_changes.as_mut().zip(path) {
            changes.paths.insert(path_key(path));
        }
        if self.completion_weights.is_empty() {
            return;
        }
        if let Some(affected) = path
            .zip(self.workspace.as_ref())
            .and_then(|(path, graph)| graph.affected_packages(path))
        {
            self.completion_weights
                .retain(|key, _| !affected.contains(key.package_id()));
        } else {
            self.completion_weights.clear();
        }
    }
}
