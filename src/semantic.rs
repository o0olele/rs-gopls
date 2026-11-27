use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyItem, CallHierarchyOutgoingCall, CodeActionKind,
    CodeActionOrCommand, FileChangeType, FoldingRange, InlayHint, Position, Range, SemanticTokens,
    TextDocumentContentChangeEvent, TextEdit, TypeHierarchyItem, Uri,
};
use tree_sitter::{Node, Parser};

use crate::go_environment::{EnvironmentOverrides, GoEnvironment};
use crate::symbols::{
    Container, FunctionSignature, Symbol, SymbolKind, TypeDetails, TypeRef, TypeRefKind,
};
use crate::telemetry;
use crate::workspace::{DiagnosticFileObservation, Package, Workspace, WorkspaceLoad, path_key};
use completion::prefilter::FileNames;

pub(crate) mod actions;
pub(crate) mod completion;
mod constants;
mod definition;
pub(crate) mod diagnostics;
mod document_symbols;
mod editor;
pub(crate) mod hierarchy;
mod hover;
mod implementation;
mod index;
pub(crate) mod navigation_trace;
mod presentation;
mod query;
mod references;
pub(crate) mod rename;
mod resolver;
mod signature_help;
pub(crate) mod snapshot;
mod source;
mod workspace_install;
mod workspace_symbols;

pub(crate) use definition::DefinitionKind;
pub(crate) use editor::{EditorFoldingOptions, legend as semantic_tokens_legend};
pub(crate) use editor::{HighlightInput, HighlightOutput, HighlightPreparation};
use index::{DiskIndex, IndexedSource, NavigationText, read_stamped_text, text_fingerprint};
pub(crate) use navigation_trace::NavigationTrace;
pub(crate) use query::{QueryProgress, SemanticTask};
pub(crate) use resolver::TypeIdentity;
pub(crate) use workspace_install::LoadedWorkspace;
use workspace_install::WorkspaceCacheChanges;
// Preserve the shared imports used by the existing presentation and feature submodules.
use index::IndexedSource as Source;
use resolver::{
    MAX_TYPE_DEPTH, ResolvedSymbol, Resolver, Substitutions, TypeKey, TypeKind, is_exported,
    is_predeclared, is_predeclared_type, materialize_type, untyped_default,
};
use source::{ReferenceQuery, Source as SyntaxSource, node_text};

#[derive(Default)]
enum GraphChanges {
    #[default]
    Clean,
    Packages(HashSet<String>),
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
            (Self::Packages(packages), Some(affected)) => packages.extend(affected),
            (changes, Some(affected)) => *changes = Self::Packages(affected),
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
    formatting_cache: actions::GofmtCache,
    reference_worker: references::ReferenceWorker,
    reference_candidates: references::ReferenceCandidates,
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
    parser: Parser,
    documents: HashMap<PathBuf, IndexedSource>,
    new_documents: HashSet<PathBuf>,
    disk_index: HashMap<PathBuf, DiskIndex>,
    navigation_texts: HashMap<PathBuf, NavigationText>,
    completion_names: HashMap<PathBuf, FileNames>,
    completion_weights: HashMap<completion::InferredMemo, i32>,
    diagnostic_file_changes: HashMap<PathBuf, DiagnosticFileObservation>,
    diagnostic_keys: RefCell<diagnostics::DiagnosticKeys>,
    workspace_cache_changes: Option<WorkspaceCacheChanges>,
}

impl Engine {
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
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
            .expect("Go grammar must match the tree-sitter runtime");
        Self {
            track_rename_inputs: false,
            state: EngineState {
                roots,
                environment,
                workspace: None,
                graph_changes: GraphChanges::default(),
                generation: 0,
                completion_graph_generation: 0,
                completion_source_generation: 0,
                parser,
                documents: HashMap::new(),
                new_documents: HashSet::new(),
                disk_index: HashMap::new(),
                navigation_texts: HashMap::new(),
                completion_names: HashMap::new(),
                completion_weights: HashMap::new(),
                diagnostic_file_changes: HashMap::new(),
                diagnostic_keys: RefCell::default(),
                workspace_cache_changes: None,
            },
            diagnostics_cache: diagnostics::DiagnosticsCache::default(),
            implementation_cache: implementation::ImplementationCache::default(),
            implementation_sources: implementation::InterfaceSources::default(),
            workspace_symbol_index: workspace_symbols::WorkspaceSymbolIndex::default(),
            completion_imports: completion::ImportCache::default(),
            formatting_cache: actions::GofmtCache::default(),
            reference_worker: references::ReferenceWorker::default(),
            reference_candidates: references::ReferenceCandidates::default(),
            include_tests: false,
        }
    }

    pub fn open(&mut self, path: PathBuf, text: String, version: i32) -> Result<()> {
        if path.extension().is_some_and(|extension| extension == "go") {
            let key = path_key(&path);
            let new_file = !path.is_file();
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
            let source = SyntaxSource::parse(&mut self.state.parser, path, text, None)?;
            self.diagnostics_cache.invalidate_document(&key);
            self.state.completion_sources_changed(Some(&source.path));
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
            self.state
                .documents
                .insert(key, IndexedSource::new(source, Some(version)));
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
        if let Some(source) = self.state.documents.get_mut(&key)
            && !edits.is_empty()
        {
            let mut edit = implementation::ObservedEdit::new();
            let mut observe = |delta: source::TextEditDelta<'_>| edit.observe(delta);
            source.edit(
                &mut self.state.parser,
                edits,
                version,
                (!self.implementation_cache.is_empty()).then_some(&mut observe as _),
            )?;
            self.diagnostics_cache.invalidate_document(&key);
            self.state.completion_sources_changed(Some(path));
            self.workspace_symbol_index.document_edited(&key);
            self.implementation_sources.invalidate(&key);
            edit.apply(&mut self.implementation_cache, &key);
        }
        Ok(())
    }

    pub fn close(&mut self, path: &Path) {
        self.implementation_cache.clear();
        let key = path_key(path);
        self.diagnostics_cache.invalidate_document(&key);
        self.implementation_sources.invalidate(&key);
        if self.state.documents.remove(&key).is_some() {
            self.state.completion_sources_changed(Some(path));
            self.workspace_symbol_index.document_closed(&key);
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
        self.state.documents.contains_key(&path_key(path))
    }

    pub(crate) fn open_document_paths(&self) -> Vec<PathBuf> {
        self.state
            .documents
            .values()
            .map(|doc| doc.path.clone())
            .collect()
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

    pub(crate) fn prewarm_completion_sdk(&self, refresh: bool) {
        self.completion_imports.prewarm(
            self.root(),
            self.environment(),
            refresh && self.state.workspace.is_some(),
        );
    }

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
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| matches!(name, "go.mod" | "go.sum" | "go.work" | "go.work.sum"))
        {
            self.invalidate_observed(None, trigger, "mod_or_work", changed_file_count, Some(path));
            return;
        }
        if path.extension().is_none_or(|extension| extension != "go") {
            return;
        }

        self.implementation_cache.clear();
        self.state.completion_sources_changed(Some(path));
        let key = path_key(path);
        let epoch = self
            .state
            .diagnostic_file_changes
            .get(&key)
            .map_or(1, |change| change.epoch + 1);
        self.state
            .diagnostic_file_changes
            .insert(key.clone(), DiagnosticFileObservation::new(path, epoch));
        self.state.completion_names.remove(&key);
        self.state.navigation_texts.remove(&key);
        self.completion_imports.source_changed(&key);
        self.workspace_symbol_index
            .disk_changed(&key, self.state.documents.contains_key(&key));
        self.implementation_sources.invalidate(&key);
        if change == FileChangeType::CREATED && path.is_file() {
            self.state.new_documents.remove(&key);
        }
        let previous = self.state.disk_index.remove(&key);
        let mut signature_changed = false;
        if change == FileChangeType::CHANGED
            && let Some(previous) = previous
            && let Ok((text, stamp)) = read_stamped_text(path)
            && let Ok(disk) =
                SyntaxSource::parse(&mut self.state.parser, path.to_owned(), text, None)
        {
            let mut updated = DiskIndex::new(&disk);
            updated.stamp = stamp;
            if previous.declarations.package_signature == updated.declarations.package_signature {
                self.state.disk_index.insert(key, updated);
                return;
            }
            signature_changed = true;
        }
        // A new, removed, or structurally changed source can change the set
        // of Go files selected by the current build or add an import.
        let affected = self
            .state
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.affected_packages(path));
        let reason = if change == FileChangeType::CREATED || change == FileChangeType::DELETED {
            "created_or_deleted"
        } else if self.state.workspace.is_some() && affected.is_none() {
            "unknown_pkg"
        } else if signature_changed {
            "signature_changed"
        } else {
            "cold_file"
        };
        self.invalidate_observed(affected, trigger, reason, changed_file_count, Some(path));
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
        Ok((report, report.is_partial.then_some(load)))
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

    pub(crate) fn code_actions_for_diagnostic_key(
        &mut self,
        path: &Path,
        uri: &Uri,
        range: Range,
        only: Option<&[CodeActionKind]>,
        key: &diagnostics::DiagnosticSnapshotKey,
    ) -> Result<Vec<CodeActionOrCommand>> {
        let report = self.diagnostics_cache.get(key);
        if only == Some(&[CodeActionKind::QUICKFIX])
            && report.is_none_or(|report| report.items_overlapping(range).next().is_none())
        {
            return Ok(Vec::new());
        }
        let mut snapshot = self.state.snapshot(self.include_tests);
        let Some(document) = snapshot.document(path)? else {
            return Ok(Vec::new());
        };
        actions::code_actions(&mut snapshot, &document, uri, range, only, report)
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
