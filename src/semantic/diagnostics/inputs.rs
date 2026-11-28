use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::path::Path;

use super::cache::SemanticInputs;
use super::imports::import_error_evidence;
use super::model::{DiagnosticDocumentRevision, DiagnosticSnapshotKey};
use super::package_errors::PackageErrorContext;
use crate::semantic::completion::render::package_name;
use crate::semantic::index::{IndexedSource, draft_files};
use crate::semantic::snapshot::{DocumentSnapshot, EngineSnapshot};
use crate::workspace::{Package, Workspace, path_key};

pub(super) struct DiagnosticView<'w> {
    pub(super) graph: &'w Workspace,
    pub(super) package: Option<&'w Package>,
    pub(super) group_id: Option<usize>,
}

impl<'w> DiagnosticView<'w> {
    fn for_path(
        snapshot: &EngineSnapshot<'w>,
        path: &Path,
        source: Option<&IndexedSource>,
    ) -> Option<Self> {
        let source = source.map(|source| &**source);
        let graph = snapshot.diagnostic_workspace(path, source)?;
        let package = snapshot.diagnostic_package(graph, path, source);
        let group_id = graph
            .group_for_path(path)
            .or_else(|| package.map(Package::group_id));
        Some(Self {
            graph,
            package,
            group_id,
        })
    }

    fn package_group(&self, package: &Package) -> usize {
        self.group_id
            .filter(|group| self.graph.package_in_group(package, *group))
            .unwrap_or_else(|| package.group_id())
    }
}

pub(super) fn diagnostics_view<'w>(
    snapshot: &EngineSnapshot<'w>,
    document: &DocumentSnapshot<'_>,
) -> Option<DiagnosticView<'w>> {
    let source = document.version().is_some().then(|| document.indexed());
    DiagnosticView::for_path(snapshot, document.path(), source)
}

pub(crate) fn snapshot_key_for_path(
    snapshot: &EngineSnapshot<'_>,
    path: &Path,
) -> io::Result<DiagnosticSnapshotKey> {
    let path_key = path_key(path);
    let source = snapshot
        .docs()
        .get(&path_key)
        .filter(|source| source.document_version.is_some());
    let revision = if let Some(source) = source {
        DiagnosticDocumentRevision::Open {
            version: source
                .document_version
                .expect("an open document has a version"),
            open_generation: source.open_generation,
            fingerprint: source.fingerprint(),
            content_revision: source.content_revision,
        }
    } else {
        DiagnosticDocumentRevision::Disk {
            stamp: fs::metadata(path)?.into(),
            epoch: snapshot
                .diagnostic_file_change(&path_key)
                .map_or(0, |change| change.epoch),
        }
    };
    let inputs = DiagnosticView::for_path(snapshot, path, source).map(|view| {
        if let Some(fingerprint) = snapshot.diagnostic_keys().borrow().get(&path_key) {
            return fingerprint;
        }
        let inputs = semantic_inputs(snapshot, path, source, &view);
        let fingerprint = inputs.fingerprint;
        snapshot
            .diagnostic_keys()
            .borrow_mut()
            .insert(path_key.clone(), inputs);
        fingerprint
    });
    Ok(DiagnosticSnapshotKey::new(path_key, revision, inputs))
}

fn semantic_inputs(
    snapshot: &EngineSnapshot<'_>,
    path: &Path,
    source: Option<&IndexedSource>,
    view: &DiagnosticView<'_>,
) -> SemanticInputs {
    let graph = view.graph;
    let mut dependencies = SemanticInputs::default();
    let mut hasher = DefaultHasher::new();
    snapshot.environment().variables().hash(&mut hasher);
    snapshot
        .environment()
        .allows_module_auto()
        .hash(&mut hasher);
    view.group_id.hash(&mut hasher);
    view.group_id
        .and_then(|group| graph.diagnostic_group_inputs(group))
        .hash(&mut hasher);
    graph.group_scope_for_path(path).hash(&mut hasher);
    view.package
        .map(|package| package.id.as_str())
        .hash(&mut hasher);

    let direct_imports: BTreeSet<_> = match (source, view.package) {
        (Some(source), _) => diagnostic_import_paths(source).collect(),
        (None, Some(package)) => package.diagnostic_import_paths().collect(),
        (None, None) => BTreeSet::new(),
    };
    let mut packages: Vec<_> = view.package.into_iter().collect();
    let mut unresolved = Vec::new();
    if let Some(package) = view.package
        && !direct_imports.is_empty()
    {
        let group = view.package_group(package);
        let cycles = cycle_candidates(graph, package, group);
        for import in direct_imports {
            let target = graph.resolve_import_in_group(package, group, import);
            let cycle = target.is_some_and(|target| cycles.contains(target.id.as_str()));
            (import, target.map(|target| target.id.as_str()), cycle).hash(&mut hasher);
            packages.extend(target);
            if target.is_none() && source.is_some() {
                unresolved.push(import);
            }
        }
    }
    let errors = PackageErrorContext::new(graph, path, view.package, view.group_id);
    for error in graph.package_errors() {
        if errors
            .position(graph, error, |name| {
                source.map_or_else(
                    || view.package.is_none_or(|package| package.name == name),
                    |source| package_name(source) == Some(name),
                )
            })
            .is_some()
            || view.package.is_some_and(|package| {
                error.group_id == view.package_group(package)
                    && unresolved
                        .iter()
                        .any(|import| import_error_evidence(error, package, import))
            })
        {
            error.hash(&mut hasher);
        }
    }

    let drafts = draft_files(graph, snapshot.docs(), snapshot.new_documents());
    let mut views = BTreeMap::new();
    while let Some(package) = packages.pop() {
        if views.contains_key(package.id.as_str()) {
            continue;
        }
        let mut inputs = DefaultHasher::new();
        let (directory, name) = package.directory_and_name();
        (directory, name).hash(&mut inputs);
        dependencies
            .draft_groups
            .entry(directory.to_owned())
            .or_default()
            .insert(name.to_owned());
        package.private_name_owner().hash(&mut inputs);
        package.test_of().hash(&mut inputs);
        package.load_error.hash(&mut inputs);

        let mut imports: BTreeSet<_> = package.diagnostic_import_paths().collect();
        for file in package.files.iter().chain(
            drafts
                .get(&package.directory_and_name())
                .into_iter()
                .flatten()
                .copied(),
        ) {
            let key = path_key(file);
            key.hash(&mut inputs);
            if let Some(overlay) = snapshot.docs().get(&key) {
                overlay.declaration_revision.hash(&mut inputs);
                imports.extend(diagnostic_import_paths(overlay));
            } else {
                graph
                    .diagnostic_disk_input(&key, snapshot.diagnostic_file_change(&key))
                    .hash(&mut inputs);
            }
            dependencies.files.insert(key);
        }
        for import in imports {
            let target = graph.resolve_import(package, import);
            (import, target.map(|target| target.id.as_str())).hash(&mut inputs);
            packages.extend(target);
        }
        views.insert(package.id.as_str(), inputs.finish());
    }
    views.hash(&mut hasher);
    dependencies.fingerprint = hasher.finish();
    dependencies
}

fn diagnostic_import_paths(source: &IndexedSource) -> impl Iterator<Item = &str> {
    source
        .diagnostic_imports
        .iter()
        .filter_map(|import| import.path.as_deref().or(import.decoded_path.as_deref()))
}

// Import diagnostics use reverse edges because overlays can add an edge not
// present in go list. Hash only the reachability of the actual import targets.
fn cycle_candidates<'w>(graph: &'w Workspace, package: &Package, group: usize) -> HashSet<&'w str> {
    let mut seen = HashSet::new();
    let mut stack: Vec<_> = graph.direct_importers(package).collect();
    while let Some(parent) = stack.pop() {
        if graph.package_in_group(parent, group) && seen.insert(parent.id.as_str()) {
            stack.extend(graph.direct_importers(parent));
        }
    }
    seen
}
