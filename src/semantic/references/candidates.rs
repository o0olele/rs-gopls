use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result};

use super::ReferenceCandidate;
use crate::semantic::index::{IndexedSource, draft_package};
use crate::semantic::source::check_cancelled;
use crate::workspace::{Package, Workspace, path_key};

#[derive(Clone, Hash, PartialEq, Eq)]
pub(super) struct ReferenceScope {
    pub(super) owner: String,
    pub(super) exported: bool,
    pub(super) transitive: bool,
}

pub(super) struct CandidateInput {
    workspace: Arc<Workspace>,
    overlays: Arc<OverlayTopology>,
    scope: ReferenceScope,
}

type DraftGroup = (PathBuf, String);

#[derive(Default)]
pub(super) struct OverlayTopology {
    importers: HashMap<String, Vec<String>>,
    drafts: HashMap<DraftGroup, Vec<PathBuf>>,
}

#[derive(Clone)]
pub(super) struct CandidateList {
    pub(super) files: Arc<[Arc<ReferenceCandidate>]>,
    pub(super) package_count: usize,
    dependencies: Arc<CandidateDependencies>,
}

#[derive(Default)]
struct CandidateDependencies {
    importer_targets: Vec<String>,
    draft_groups: HashSet<DraftGroup>,
}

impl OverlayTopology {
    pub(super) fn capture(
        workspace: &Workspace,
        documents: &HashMap<PathBuf, IndexedSource>,
        new_documents: &HashSet<PathBuf>,
    ) -> Self {
        let mut topology = Self::default();
        for (key, source) in documents {
            let from = if let Some(from) = workspace.package_of_file(&source.path) {
                from
            } else if new_documents.contains(key)
                && let Some(from) = draft_package(workspace, source)
            {
                topology
                    .drafts
                    .entry(draft_group(from))
                    .or_default()
                    .push(source.path.clone());
                from
            } else {
                continue;
            };
            if !workspace.is_project_package(from) {
                continue;
            }
            for import in source.imports.iter() {
                // Installed edges already take part in the candidate walk.
                if let Some(target) = workspace.resolve_import(from, &import.path)
                    && !workspace
                        .direct_importers(target)
                        .any(|package| package.id == from.id)
                {
                    topology
                        .importers
                        .entry(target.id.clone())
                        .or_default()
                        .push(from.id.clone());
                }
            }
        }
        for importers in topology.importers.values_mut() {
            importers.sort_unstable();
            importers.dedup();
        }
        for files in topology.drafts.values_mut() {
            files.sort_unstable();
        }
        topology
    }
}

impl CandidateList {
    pub(super) fn same_scope_inputs(
        &self,
        previous: &OverlayTopology,
        next: &OverlayTopology,
    ) -> bool {
        // An edge to a target outside the walk cannot grow its frontier.
        self.dependencies
            .importer_targets
            .iter()
            .all(|target| previous.importers.get(target) == next.importers.get(target))
            && self
                .dependencies
                .draft_groups
                .iter()
                .all(|group| previous.drafts.get(group) == next.drafts.get(group))
    }
}

fn draft_group(package: &Package) -> DraftGroup {
    let (directory, name) = package.directory_and_name();
    (directory.to_owned(), name.to_owned())
}

impl CandidateInput {
    pub(super) fn new(
        workspace: Arc<Workspace>,
        overlays: Arc<OverlayTopology>,
        scope: ReferenceScope,
    ) -> Self {
        Self {
            workspace,
            overlays,
            scope,
        }
    }

    pub(super) fn run(self, cancelled: &AtomicBool) -> Result<CandidateList> {
        check_cancelled(cancelled)?;
        let workspace = &self.workspace;
        let mut dependencies = CandidateDependencies::default();
        let packages =
            self.scope
                .packages(workspace, &self.overlays, &mut dependencies, cancelled)?;
        let package_count = packages.len();
        let mut candidates = BTreeMap::new();
        for view in packages {
            let group = draft_group(view);
            let extra = self.overlays.drafts.get(&group).into_iter().flatten();
            for path in view.files.iter().chain(extra) {
                check_cancelled(cancelled)?;
                candidates
                    .entry(path_key(path))
                    .or_insert_with(|| (path, workspace.package_of_file(path).unwrap_or(view)));
            }
            dependencies.draft_groups.insert(group);
        }
        let mut files = Vec::with_capacity(candidates.len());
        for (key, (path, package)) in candidates {
            check_cancelled(cancelled)?;
            files.push(Arc::new(ReferenceCandidate {
                key,
                path: path.clone(),
                package_id: package.id.clone(),
                priority: 2,
            }));
        }
        Ok(CandidateList {
            files: files.into(),
            package_count,
            dependencies: Arc::new(dependencies),
        })
    }
}

impl ReferenceScope {
    fn packages<'w>(
        &self,
        workspace: &'w Workspace,
        overlays: &OverlayTopology,
        dependencies: &mut CandidateDependencies,
        cancelled: &AtomicBool,
    ) -> Result<Vec<&'w Package>> {
        let owner = workspace
            .package(&self.owner)
            .context("reference owner package is unavailable")?;
        let mut queue: Vec<_> = workspace.same_package_variants(owner).collect();
        if queue.is_empty() {
            queue.push(owner);
        }
        if self.exported {
            let mut seen: HashSet<_> = queue.iter().map(|package| package.id.as_str()).collect();
            let seeds = queue.len();
            let mut next = 0;
            while next < if self.transitive { queue.len() } else { seeds } {
                check_cancelled(cancelled)?;
                let package = queue[next];
                next += 1;
                dependencies.importer_targets.push(package.id.clone());
                for importer in workspace.direct_importers(package).chain(
                    overlays
                        .importers
                        .get(&package.id)
                        .into_iter()
                        .flatten()
                        .filter_map(|id| workspace.package(id)),
                ) {
                    check_cancelled(cancelled)?;
                    if seen.insert(importer.id.as_str()) {
                        queue.push(importer);
                    }
                }
            }
        }
        queue.retain(|package| workspace.is_project_package(package));
        Ok(queue)
    }
}
