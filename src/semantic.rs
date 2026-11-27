use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyItem, CallHierarchyOutgoingCall, CodeActionKind,
    CodeActionOrCommand, DocumentHighlight, FileChangeType, FoldingRange, InlayHint, Position,
    Range, SemanticTokens, TextDocumentContentChangeEvent, TextEdit, TypeHierarchyItem, Uri,
};
use tree_sitter::{Node, Parser};

use crate::symbols::{
    Container, FunctionSignature, Symbol, SymbolKind, TypeDetails, TypeRef, TypeRefKind,
};
use crate::workspace::{Package, Workspace, WorkspaceLoad, path_key};
use completion::prefilter::{FileNames, Stamp};

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
mod presentation;
mod query;
mod references;
pub(crate) mod rename;
mod resolver;
mod signature_help;
pub(crate) mod snapshot;
mod source;
mod workspace_symbols;

pub(crate) use definition::DefinitionKind;
pub(crate) use editor::{EditorFoldingOptions, legend as semantic_tokens_legend};
use index::{DiskIndex, IndexedSource, NavigationText, text_fingerprint};
pub(crate) use query::{QueryProgress, SemanticTask};
pub(crate) use resolver::TypeIdentity;
// Preserve the shared imports used by the existing presentation and feature submodules.
use index::IndexedSource as Source;
use resolver::{
    MAX_TYPE_DEPTH, ResolvedSymbol, Resolver, Substitutions, TypeKey, TypeKind, is_exported,
    is_predeclared, is_predeclared_type, materialize_type, untyped_default,
};
use snapshot::DocumentSnapshot;
use source::{ReferenceQuery, Source as SyntaxSource, is_test_source, node_text, read_source};

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
    include_tests: bool,
}

struct EngineState {
    roots: Vec<PathBuf>,
    workspace: Option<Rc<Workspace>>,
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
}

impl Engine {
    pub(crate) fn semantic_tokens_full(&mut self, path: &Path) -> Result<SemanticTokens> {
        Ok(self
            .snapshot()
            .document(path)?
            .as_ref()
            .map_or_else(SemanticTokens::default, editor::semantic_tokens_full))
    }

    pub(crate) fn document_highlight(
        &mut self,
        path: &Path,
        position: Position,
    ) -> Result<Vec<DocumentHighlight>> {
        Ok(self
            .snapshot()
            .document(path)?
            .as_ref()
            .map_or_else(Vec::new, |document| {
                editor::document_highlight(document, position)
            }))
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

    #[cfg_attr(not(test), allow(dead_code, reason = "single-folder constructor"))]
    pub fn new(root: PathBuf) -> Self {
        Self::with_roots(vec![root])
    }

    pub(crate) fn with_roots(roots: Vec<PathBuf>) -> Self {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
            .expect("Go grammar must match the tree-sitter runtime");
        Self {
            track_rename_inputs: false,
            state: EngineState {
                roots,
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
            },
            diagnostics_cache: diagnostics::DiagnosticsCache::default(),
            implementation_cache: implementation::ImplementationCache::default(),
            implementation_sources: implementation::InterfaceSources::default(),
            workspace_symbol_index: workspace_symbols::WorkspaceSymbolIndex::default(),
            completion_imports: completion::ImportCache::default(),
            include_tests: false,
        }
    }

    pub fn open(&mut self, path: PathBuf, text: String, version: i32) -> Result<()> {
        if path.extension().is_some_and(|extension| extension == "go") {
            let key = path_key(&path);
            let new_file = !path.is_file();
            if !self.implementation_cache.is_empty() {
                let unchanged = match self.state.documents.get(&key) {
                    Some(previous) => previous.text == text,
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
            self.file_changed(path, FileChangeType::CREATED);
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
        self.invalidate(None);
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

    pub(crate) fn prewarm_completion_sdk(&self, refresh: bool) {
        self.completion_imports
            .prewarm(self.root(), refresh && self.state.workspace.is_some());
    }

    pub(crate) fn completion_wake(&self) -> &crossbeam_channel::Receiver<()> {
        self.completion_imports.wake()
    }

    pub fn install_workspace(&mut self, load: WorkspaceLoad, workspace: Workspace) -> bool {
        if load.generation != self.state.generation {
            return false;
        }
        if let Some(root) = self.completion_imports.sdk_root() {
            workspace.share_sdk_root(&root);
        }
        if self.state.workspace.is_some() && !self.state.completion_names.is_empty() {
            let selected: HashSet<_> = workspace
                .navigation_packages()
                .flat_map(|package| package.files.iter().map(|file| path_key(file)))
                .collect();
            let sdk = workspace.sdk_root().map(path_key);
            self.state.completion_names.retain(|path, names| {
                if sdk.as_ref().is_some_and(|sdk| path.starts_with(sdk)) {
                    return true;
                }
                let current =
                    fs::metadata(path).is_ok_and(|metadata| names.stamp == Stamp::from(metadata));
                if !current {
                    self.state.disk_index.remove(path);
                    self.completion_imports.source_changed(path);
                }
                selected.contains(path) && current
            });
        }
        if !self.state.navigation_texts.is_empty() {
            let selected: HashSet<_> = workspace
                .navigation_packages()
                .flat_map(|package| package.files.iter().map(|file| path_key(file)))
                .collect();
            self.state.navigation_texts.retain(|path, text| {
                if !selected.contains(path) || !text.is_current(path) {
                    self.state.disk_index.remove(path);
                    self.implementation_sources.invalidate(path);
                    return false;
                }
                true
            });
        }
        self.implementation_sources.retain_workspace(&workspace);
        self.state.workspace = Some(Rc::new(workspace));
        self.state.completion_graph_generation += 1;
        self.state.completion_sources_changed(None);
        self.state.graph_changes = GraphChanges::Clean;
        self.diagnostics_cache.clear();
        self.implementation_cache.clear();
        self.workspace_symbol_index.graph_changed();
        true
    }

    fn invalidate(&mut self, affected: Option<HashSet<String>>) {
        self.implementation_cache.clear();
        self.state.generation += 1;
        self.state.completion_graph_generation += 1;
        self.state.completion_sources_changed(None);
        self.state.graph_changes.include(affected);
    }

    pub fn file_changed(&mut self, path: &Path, change: FileChangeType) {
        self.diagnostics_cache.clear();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| matches!(name, "go.mod" | "go.sum" | "go.work" | "go.work.sum"))
        {
            self.invalidate(None);
            return;
        }
        if path.extension().is_none_or(|extension| extension != "go") {
            return;
        }

        self.implementation_cache.clear();
        self.state.completion_sources_changed(Some(path));
        let key = path_key(path);
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
        if change == FileChangeType::CHANGED
            && let Some(previous) = previous
            && let Ok(disk) = read_source(&mut self.state.parser, path)
        {
            let updated = DiskIndex::new(&disk);
            if previous.declarations.package_signature == updated.declarations.package_signature {
                self.state.disk_index.insert(key, updated);
                return;
            }
        }
        // A new, removed, or structurally changed source can change the set
        // of Go files selected by the current build or add an import.
        let affected = self
            .state
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.affected_packages(path));
        self.invalidate(affected);
    }
}

impl Engine {
    pub(crate) fn diagnostics_for(
        &self,
        document: &DocumentSnapshot<'_>,
    ) -> Option<&diagnostics::DiagnosticReport> {
        self.diagnostics_cache.get(
            document,
            self.state.generation,
            self.state
                .workspace
                .as_deref()
                .is_some_and(Workspace::includes_tests),
        )
    }

    pub(crate) fn diagnostics_report(
        &mut self,
        path: &Path,
    ) -> Result<(&diagnostics::DiagnosticReport, Option<WorkspaceLoad>)> {
        self.include_tests |= is_test_source(path);
        let load = self.workspace_load();
        let mut snapshot = self.state.snapshot(self.include_tests);
        let document = snapshot.document(path)?.context("expected a Go document")?;
        let report =
            diagnostics::get_or_compute(&mut self.diagnostics_cache, &mut snapshot, &document)?;
        Ok((report, report.is_partial.then_some(load)))
    }

    pub(crate) fn format_document(&mut self, path: &Path) -> Result<Option<Vec<TextEdit>>> {
        self.snapshot()
            .document(path)?
            .map(|document| {
                actions::format_document(&document, path.parent().filter(|dir| dir.is_dir()))
            })
            .transpose()
    }

    pub(crate) fn code_actions(
        &mut self,
        path: &Path,
        uri: &Uri,
        range: Range,
        only: Option<&[CodeActionKind]>,
    ) -> Result<QueryProgress<Vec<CodeActionOrCommand>>> {
        let require_tests = is_test_source(path);
        self.include_tests |= require_tests;
        if let QueryProgress::Pending(load) = self.snapshot().active_graph(require_tests) {
            return Ok(QueryProgress::Pending(load));
        }
        if only == Some(&[CodeActionKind::QUICKFIX])
            && let Some(source) = self.state.documents.get(&path_key(path))
            && let Some(report) = self.diagnostics_for(&DocumentSnapshot::borrowed(path, source))
            && report.items_overlapping(range).next().is_none()
        {
            return Ok(QueryProgress::Ready(Vec::new()));
        }
        self.local_code_actions(path, uri, range, only)
            .map(QueryProgress::Ready)
    }

    pub(crate) fn local_code_actions(
        &mut self,
        path: &Path,
        uri: &Uri,
        range: Range,
        only: Option<&[CodeActionKind]>,
    ) -> Result<Vec<CodeActionOrCommand>> {
        let mut snapshot = self.state.snapshot(self.include_tests);
        let Some(document) = snapshot.document(path)? else {
            return Ok(Vec::new());
        };
        let report =
            diagnostics::get_or_compute(&mut self.diagnostics_cache, &mut snapshot, &document)?;
        actions::code_actions(&mut snapshot, &document, uri, range, only, Some(report))
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
