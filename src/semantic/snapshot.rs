use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use lsp_types::{Position, Range};
use tree_sitter::{Node, Parser};

use super::completion::{InferredMemo, prefilter::FileNames};
use super::diagnostics::DiagnosticKeys;
use super::index::{
    CachedNavigationText, CompletionRead, DiskIndex, IndexReader, IndexedSource, active_package,
    draft_files,
};
use super::navigation_trace::{NavigationProbe, record};
use super::open_overlay::PendingOpen;
use super::query::QueryProgress;
use super::resolver::{Resolver, TypeIdentity};
use super::shared_disk::SharedDiskCache;
use super::source::{EditorNames, Import, Source, is_test_source, node_text};
use super::{Engine, EngineState, GraphChanges};
use crate::go_environment::GoEnvironment;
use crate::scope::{Binding, lexical_binding};
use crate::symbols::{FileOutline, FileSymbols};
use crate::workspace::{DiagnosticFileObservation, Package, Workspace, WorkspaceLoad, path_key};

/// A query's view of the open documents and the installed package graph.
/// The parser and disk indexes are borrowed for lazy reads.
pub(crate) struct EngineSnapshot<'a> {
    roots: &'a [PathBuf],
    environment: &'a GoEnvironment,
    workspace: Option<&'a Workspace>,
    graph_changes: &'a GraphChanges,
    documents: &'a HashMap<PathBuf, IndexedSource>,
    pending_documents: &'a HashMap<PathBuf, PendingOpen>,
    new_documents: &'a HashSet<PathBuf>,
    disk_index: &'a mut HashMap<PathBuf, DiskIndex>,
    shared_disk: &'a Arc<SharedDiskCache>,
    navigation_texts: &'a mut HashMap<PathBuf, CachedNavigationText>,
    disk_names: &'a mut HashMap<PathBuf, FileNames>,
    inferred_weights: &'a mut HashMap<InferredMemo, i32>,
    parser: &'a mut Parser,
    load: WorkspaceLoad,
    completion_source_generation: u64,
    diagnostic_file_changes: &'a HashMap<PathBuf, DiagnosticFileObservation>,
    diagnostic_keys: &'a RefCell<DiagnosticKeys>,
}

pub(crate) struct DocumentSnapshot<'a> {
    path: &'a Path,
    source: DocumentSource<'a>,
}

enum DocumentSource<'a> {
    Borrowed(&'a IndexedSource),
    Disk(IndexedSource),
}

impl Engine {
    pub(crate) fn snapshot(&mut self) -> EngineSnapshot<'_> {
        self.state.snapshot(self.include_tests)
    }
}

impl EngineState {
    // Query wrappers can borrow the source state while updating request
    // preferences or a feature cache on the engine.
    pub(super) fn snapshot(&mut self, include_tests: bool) -> EngineSnapshot<'_> {
        EngineSnapshot {
            roots: &self.roots,
            environment: &self.environment,
            workspace: self.workspace.as_deref(),
            graph_changes: &self.graph_changes,
            documents: &self.documents,
            pending_documents: &self.pending_documents,
            new_documents: &self.new_documents,
            disk_index: &mut self.disk_index,
            shared_disk: &self.shared_disk,
            navigation_texts: &mut self.navigation_texts,
            disk_names: &mut self.completion_names,
            inferred_weights: &mut self.completion_weights,
            parser: &mut self.parser,
            load: WorkspaceLoad {
                generation: self.generation,
                include_tests,
            },
            completion_source_generation: self.completion_source_generation,
            diagnostic_file_changes: &self.diagnostic_file_changes,
            diagnostic_keys: &self.diagnostic_keys,
        }
    }
}

impl<'a> EngineSnapshot<'a> {
    pub(crate) fn environment(&self) -> &'a GoEnvironment {
        self.environment
    }

    pub(crate) fn document<'d>(&mut self, path: &'d Path) -> Result<Option<DocumentSnapshot<'d>>>
    where
        'a: 'd,
    {
        self.navigation_document(path, None)
    }

    pub(super) fn navigation_document<'d>(
        &mut self,
        path: &'d Path,
        probe: Option<&NavigationProbe>,
    ) -> Result<Option<DocumentSnapshot<'d>>>
    where
        'a: 'd,
    {
        let key = path_key(path);
        if let Some(pending) = self.pending_documents.get(&key) {
            return Err(pending.ready.pending().into());
        }
        if let Some(source) = self.docs().get(&key) {
            record(probe, |counts| {
                counts.scanned_files += 1;
                counts.ast_access(true);
            });
            return Ok(Some(DocumentSnapshot::borrowed(path, source)));
        }
        if path.extension().is_none_or(|extension| extension != "go") {
            return Ok(None);
        }
        record(probe, |counts| {
            counts.scanned_files += 1;
            counts.ast_access(false);
        });
        let source = self.shared_disk.load(self.parser, path, None, probe)?;
        Ok(Some(DocumentSnapshot {
            path,
            source: DocumentSource::Disk(source.into_indexed()),
        }))
    }

    pub(crate) fn active_graph(&self, require_tests: bool) -> QueryProgress<&'a Workspace> {
        match self.diagnostics_graph(require_tests) {
            QueryProgress::Ready(graph) if graph.total_load_failure().is_some() => {
                QueryProgress::Pending(self.load_for(require_tests))
            }
            graph => graph,
        }
    }

    pub(super) fn diagnostics_graph(&self, require_tests: bool) -> QueryProgress<&'a Workspace> {
        match self.workspace {
            Some(workspace)
                if matches!(self.graph_changes, GraphChanges::Clean)
                    && (!require_tests || workspace.includes_tests()) =>
            {
                QueryProgress::Ready(workspace)
            }
            _ => QueryProgress::Pending(self.load_for(require_tests)),
        }
    }

    pub(super) fn diagnostic_workspace(
        &self,
        path: &Path,
        source: Option<&Source>,
    ) -> Option<&'a Workspace> {
        let workspace = self.workspace?;
        if is_test_source(path) && !workspace.includes_tests() {
            return None;
        }
        let package = self.diagnostic_package(workspace, path, source);
        let stale = match self.graph_changes {
            GraphChanges::Clean => false,
            GraphChanges::Global => true,
            GraphChanges::Packages(_) => source.map_or_else(
                || package.is_none_or(|package| self.graph_changes.affects(package)),
                |source| {
                    self.graph_changes
                        .affects_source(workspace, package, source)
                },
            ),
        };
        (!stale).then_some(workspace)
    }

    pub(super) fn diagnostic_package(
        &self,
        workspace: &'a Workspace,
        path: &Path,
        source: Option<&Source>,
    ) -> Option<&'a Package> {
        if workspace.total_load_failure().is_some() {
            return None;
        }
        match source {
            Some(source) => active_package(workspace, source, self.new_documents),
            None => workspace.package_of_file(path),
        }
    }

    pub(crate) fn package_for(
        &self,
        document: &DocumentSnapshot<'_>,
        require_tests: bool,
    ) -> QueryProgress<Option<&'a Package>> {
        let Some(workspace) = self.workspace else {
            return QueryProgress::Pending(self.load_for(require_tests));
        };
        if workspace.total_load_failure().is_some() || require_tests && !workspace.includes_tests()
        {
            return QueryProgress::Pending(self.load_for(require_tests));
        }
        let source = document.indexed();
        let package = active_package(workspace, source, self.new_documents);
        if self
            .graph_changes
            .affects_source(workspace, package, source)
        {
            return QueryProgress::Pending(self.load_for(require_tests));
        }
        QueryProgress::Ready(package)
    }

    // Feature modules with an existing resolver reuse it for several related
    // inferences. This entry point serves callers that start from a document.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn infer_type(
        &mut self,
        document: &DocumentSnapshot<'_>,
        expression: Node<'_>,
    ) -> Result<QueryProgress<Option<TypeIdentity<'a>>>> {
        match self.package_for(document, is_test_source(document.path())) {
            QueryProgress::Ready(Some(package)) => {
                let workspace = self.workspace.expect("a package belongs to a workspace");
                self.resolver(workspace)
                    .infer_type(package, document.indexed(), expression, 0, 0)
                    .map(QueryProgress::Ready)
            }
            QueryProgress::Ready(None) => Ok(QueryProgress::Ready(None)),
            QueryProgress::Pending(load) => Ok(QueryProgress::Pending(load)),
            QueryProgress::Searching(task) => match task {},
        }
    }

    pub(super) fn load_for(&self, require_tests: bool) -> WorkspaceLoad {
        WorkspaceLoad {
            include_tests: self.load.include_tests || require_tests,
            ..self.load
        }
    }

    pub(super) fn note_test_requirement(&mut self, require_tests: bool) {
        self.load.include_tests |= require_tests;
    }

    pub(super) fn root(&self) -> &'a Path {
        self.roots.first().map_or(Path::new("."), PathBuf::as_path)
    }

    pub(super) fn roots(&self) -> &'a [PathBuf] {
        self.roots
    }

    pub(super) fn completion_source_generation(&self) -> u64 {
        self.completion_source_generation
    }

    pub(super) fn diagnostic_file_change(&self, path: &Path) -> Option<&DiagnosticFileObservation> {
        self.diagnostic_file_changes.get(path)
    }

    pub(super) fn diagnostic_cached_package_names(
        &self,
        graph: &Workspace,
        key: &Path,
        package: &str,
    ) -> Option<Arc<EditorNames>> {
        let stamp = graph.diagnostic_disk_stamp(key, self.diagnostic_file_change(key));
        self.shared_disk.cached_package_names(key, stamp?, package)
    }

    pub(super) fn diagnostic_keys(&self) -> &RefCell<DiagnosticKeys> {
        self.diagnostic_keys
    }

    pub(super) fn workspace(&self) -> Option<&'a Workspace> {
        self.workspace
    }

    pub(super) fn docs(&self) -> &'a HashMap<PathBuf, IndexedSource> {
        self.documents
    }

    pub(super) fn new_documents(&self) -> &'a HashSet<PathBuf> {
        self.new_documents
    }

    pub(super) fn parser(&mut self) -> &mut Parser {
        self.parser
    }

    pub(super) fn resolver(&mut self, workspace: &'a Workspace) -> Resolver<'a, '_> {
        self.resolver_with_completion(workspace, None)
    }

    pub(super) fn resolver_with_completion<'s>(
        &'s mut self,
        workspace: &'a Workspace,
        completion: Option<&'s mut CompletionRead>,
    ) -> Resolver<'a, 's> {
        self.resolver_for(workspace, completion, None, false)
    }

    pub(super) fn diagnostic_resolver<'s>(
        &'s mut self,
        workspace: &'a Workspace,
        cancelled: Option<&'s AtomicBool>,
    ) -> Resolver<'a, 's> {
        self.resolver_for(workspace, None, cancelled, true)
    }

    fn resolver_for<'s>(
        &'s mut self,
        workspace: &'a Workspace,
        completion: Option<&'s mut CompletionRead>,
        cancelled: Option<&'s AtomicBool>,
        diagnostics: bool,
    ) -> Resolver<'a, 's> {
        let indexes = IndexReader {
            workspace,
            parser: self.parser,
            shared_disk: self.shared_disk,
            documents: self.documents,
            disk_index: self.disk_index,
            navigation_texts: self.navigation_texts,
            owned_reference: None,
            navigation: None,
            reference_dependencies: None,
            implementation_parse_time: None,
            drafts: draft_files(workspace, self.documents, self.new_documents),
            disk_names: self.disk_names,
            completion,
            inferred_weights: self.inferred_weights,
            cancelled,
            diagnostic_types: diagnostics.then(HashMap::new),
        };
        Resolver::new(workspace, indexes, diagnostics)
    }

    pub(super) fn active_package(
        &self,
        workspace: &'a Workspace,
        source: &Source,
    ) -> Option<&'a Package> {
        active_package(workspace, source, self.new_documents)
    }
}

impl<'a> DocumentSnapshot<'a> {
    pub(crate) fn path(&self) -> &Path {
        self.path
    }

    pub(crate) fn version(&self) -> Option<i32> {
        self.indexed().document_version
    }

    pub(crate) fn text(&self) -> &str {
        &self.indexed().text
    }

    pub(crate) fn root_node(&self) -> Node<'_> {
        self.indexed().tree.root_node()
    }

    pub(crate) fn offset(&self, position: Position) -> Option<usize> {
        self.indexed().lines.offset(self.text(), position)
    }

    pub(crate) fn byte_range(&self, bytes: std::ops::Range<usize>) -> Range {
        self.indexed().byte_range(bytes)
    }

    pub(crate) fn lexical_binding<'tree>(&self, name: Node<'tree>) -> Option<Binding<'tree>> {
        lexical_binding(name, node_text(name, self.text()), self.text())
    }

    pub(crate) fn declarations(&self) -> &FileSymbols {
        &self.indexed().declarations().symbols
    }

    /// The syntactic import list in source order, including dot and blank imports.
    #[allow(dead_code, reason = "consumed by completion in Wave 2")]
    pub(super) fn raw_imports(&self) -> impl Iterator<Item = Import> + '_ {
        self.indexed().editor_names.raw_imports.iter().cloned()
    }

    pub(crate) fn outline(&self) -> &FileOutline {
        self.indexed().outline()
    }

    pub(super) fn borrowed(path: &'a Path, source: &'a IndexedSource) -> Self {
        Self {
            path,
            source: DocumentSource::Borrowed(source),
        }
    }

    pub(super) fn indexed(&self) -> &IndexedSource {
        match &self.source {
            DocumentSource::Borrowed(source) => source,
            DocumentSource::Disk(source) => source,
        }
    }

    pub(super) fn into_disk(self) -> Option<IndexedSource> {
        match self.source {
            DocumentSource::Disk(source) => Some(source),
            DocumentSource::Borrowed(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic::resolver::TypeKind;
    use crate::test_support::TestProject;

    #[test]
    fn reads_raw_imports_without_adding_blank_bindings_to_the_index() {
        let project = TestProject::new();
        let path = project.root.join("sample.go");
        let text = concat!(
            "package sample\n",
            "import /* 😀 */ _ \"\\x66mt\"\n",
            "import . \"example.test/dot\"\n",
            "import \"io\"\n",
        );
        let mut engine = Engine::new(project.root.clone());
        engine.open(path.clone(), text.into(), 1).unwrap();
        let mut snapshot = engine.snapshot();
        let document = snapshot.document(&path).unwrap().unwrap();
        let imports: Vec<_> = document.raw_imports().collect();
        assert_eq!(
            imports
                .iter()
                .map(|import| (import.path.as_str(), import.alias.as_deref()))
                .collect::<Vec<_>>(),
            [
                ("fmt", Some("_")),
                ("example.test/dot", Some(".")),
                ("io", None)
            ]
        );
        let blank = imports[0].binding;
        assert_eq!(
            blank,
            Range::new(Position::new(1, 16), Position::new(1, 17))
        );
        assert_eq!(document.indexed().imports.len(), 2);
    }

    #[test]
    fn resolves_open_document_names_and_types_against_the_same_graph() {
        let project = TestProject::new();
        let path = project.write("sample.go", "package sample\nvar Disk int\n");
        let text = "package sample\ntype Box struct{}\nvar Needle Box\nvar _ = Needle\n";
        let mut engine = Engine::new(project.root.clone());
        engine.open(path.clone(), text.into(), 7).unwrap();
        assert!(engine.install_workspace(
            engine.workspace_load(),
            project.workspace(&["sample.go"], false)
        ));
        let mut snapshot = engine.snapshot();
        let document = snapshot.document(&path).unwrap().unwrap();
        let position = Position::new(3, 8);
        let offset = document.offset(position).unwrap();
        let name = document
            .root_node()
            .named_descendant_for_byte_range(offset, offset)
            .unwrap();
        assert_eq!(document.path(), path);
        assert_eq!(document.version(), Some(7));
        assert_eq!(node_text(name, document.text()), "Needle");
        assert!(document.declarations().declaration("Disk").is_none());
        let QueryProgress::Ready(graph) = snapshot.active_graph(false) else {
            panic!("expected a regular graph");
        };
        assert!(
            matches!(snapshot.active_graph(true), QueryProgress::Pending(load) if load.include_tests)
        );
        let QueryProgress::Ready(Some(package)) = snapshot.package_for(&document, false) else {
            panic!("expected an active package");
        };
        assert!(std::ptr::eq(package, graph.package_of_file(&path).unwrap()));
        let QueryProgress::Ready(Some((file, declaration))) =
            snapshot.resolve_name(&document, position).unwrap()
        else {
            panic!("expected a declaration from the overlay");
        };
        assert_eq!(file, path);
        assert_eq!(declaration.start, Position::new(2, 4));
        let QueryProgress::Ready(Some((file, definition))) = snapshot
            .resolve_type_definition(&document, position)
            .unwrap()
        else {
            panic!("expected the type definition from the overlay");
        };
        assert_eq!(file, path);
        assert_eq!(definition.start, Position::new(1, 5));
        let QueryProgress::Ready(Some(ty)) = snapshot.infer_type(&document, name).unwrap() else {
            panic!("expected the inferred type from the overlay");
        };
        assert!(matches!(ty.kind, TypeKind::Named { name, .. } if name == "Box"));
        assert!(std::ptr::eq(ty.package, package));
    }
}
