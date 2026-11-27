use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result};

use super::ReferenceCandidate;
use crate::semantic::index::{
    DraftFiles, IndexReader, IndexedSource, IndexedSourceRevision, draft_files, draft_package,
};
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
    overlays: HashMap<PathBuf, IndexedSourceRevision>,
    new_documents: HashSet<PathBuf>,
    scope: ReferenceScope,
}

#[derive(Clone)]
pub(super) struct CandidateList {
    pub(super) files: Arc<[Arc<ReferenceCandidate>]>,
    pub(super) package_count: usize,
}

impl CandidateInput {
    pub(super) fn capture(
        workspace: Arc<Workspace>,
        indexes: &IndexReader<'_>,
        new_documents: &HashSet<PathBuf>,
        scope: ReferenceScope,
    ) -> Self {
        Self {
            workspace,
            overlays: indexes
                .documents
                .iter()
                .map(|(key, source)| (key.clone(), source.revision()))
                .collect(),
            new_documents: new_documents.clone(),
            scope,
        }
    }

    pub(super) fn run(self, cancelled: &AtomicBool) -> Result<CandidateList> {
        check_cancelled(cancelled)?;
        let workspace = &self.workspace;
        let mut documents = HashMap::with_capacity(self.overlays.len());
        for (key, revision) in self.overlays {
            check_cancelled(cancelled)?;
            documents.insert(key, revision.into_indexed());
        }
        let drafts = draft_files(workspace, &documents, &self.new_documents);
        let packages = self
            .scope
            .packages(workspace, &documents, &drafts, cancelled)?;
        let package_count = packages.len();
        let mut candidates = BTreeMap::new();
        for view in packages {
            let extra = drafts.get(&view.directory_and_name()).into_iter().flatten();
            for path in view.files.iter().chain(extra.copied()) {
                check_cancelled(cancelled)?;
                candidates
                    .entry(path_key(path))
                    .or_insert_with(|| (path, workspace.package_of_file(path).unwrap_or(view)));
            }
        }
        let mut files = Vec::with_capacity(candidates.len());
        for (key, (path, package)) in candidates {
            check_cancelled(cancelled)?;
            files.push(Arc::new(ReferenceCandidate {
                key,
                path: path.clone(),
                package_id: package.id.clone(),
            }));
        }
        Ok(CandidateList {
            files: files.into(),
            package_count,
        })
    }
}

impl ReferenceScope {
    fn packages<'w>(
        &self,
        workspace: &'w Workspace,
        documents: &HashMap<PathBuf, IndexedSource>,
        drafts: &DraftFiles<'_>,
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
            let overlay_importers = overlay_importers(workspace, documents, drafts, cancelled)?;
            let mut seen: HashSet<_> = queue.iter().map(|package| package.id.as_str()).collect();
            let seeds = queue.len();
            let mut next = 0;
            while next < if self.transitive { queue.len() } else { seeds } {
                let package = queue[next];
                next += 1;
                for importer in workspace.direct_importers(package).chain(
                    overlay_importers
                        .get(package.id.as_str())
                        .into_iter()
                        .flatten()
                        .copied(),
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

fn overlay_importers<'w>(
    workspace: &'w Workspace,
    documents: &HashMap<PathBuf, IndexedSource>,
    drafts: &DraftFiles<'_>,
    cancelled: &AtomicBool,
) -> Result<HashMap<&'w str, Vec<&'w Package>>> {
    let mut importers = HashMap::<_, Vec<_>>::new();
    for source in documents.values() {
        check_cancelled(cancelled)?;
        let from = workspace.package_of_file(&source.path).or_else(|| {
            let package = draft_package(workspace, source)?;
            drafts
                .get(&package.directory_and_name())?
                .contains(&&source.path)
                .then_some(package)
        });
        let Some(from) = from else {
            continue;
        };
        if !workspace.is_project_package(from) {
            continue;
        }
        for import in source.imports.iter() {
            check_cancelled(cancelled)?;
            if let Some(target) = workspace.resolve_import(from, &import.path) {
                importers.entry(target.id.as_str()).or_default().push(from);
            }
        }
    }
    Ok(importers)
}
