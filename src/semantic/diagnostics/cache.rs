use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::retention::{DeliveredReport, DiagnosticSource};
use super::{DiagnosticReport, DiagnosticSnapshotKey, DiagnosticStage};
use crate::semantic::Engine;

const CAPACITY: usize = 256;

#[derive(Default)]
pub(crate) struct DiagnosticsCache {
    reports: HashMap<PathBuf, [Option<Arc<DiagnosticReport>>; 2]>,
    order: VecDeque<PathBuf>,
    delivered: HashMap<PathBuf, DeliveredReport>,
    // At most one replacement per open document, until the client consumes it
    // or its snapshot expires. Unrelated reports only compete in the FIFO.
    replacements: HashMap<PathBuf, Arc<DiagnosticReport>>,
}

impl DiagnosticsCache {
    pub(crate) fn get(&self, key: &DiagnosticSnapshotKey) -> Option<&DiagnosticReport> {
        self.get_shared(key).map(Arc::as_ref)
    }

    pub(crate) fn get_complete(&self, key: &DiagnosticSnapshotKey) -> Option<&DiagnosticReport> {
        self.get(key)
            .filter(|report| report.diagnostic_stage == DiagnosticStage::Stage2)
    }

    fn get_shared(&self, key: &DiagnosticSnapshotKey) -> Option<&Arc<DiagnosticReport>> {
        self.replacements
            .get(&key.path_key)
            .filter(|report| &report.snapshot == key)
            .or_else(|| {
                self.reports.get(&key.path_key)?[usize::from(key.partial)]
                    .as_ref()
                    .filter(|report| &report.snapshot == key)
            })
    }

    pub(crate) fn insert(&mut self, mut report: DiagnosticReport) {
        if let Some(previous) = self.get(&report.snapshot) {
            if previous.diagnostic_stage >= report.diagnostic_stage {
                return;
            }
            if previous.same_visible_content(&report) {
                report.result_id.clone_from(&previous.result_id);
            }
        }
        let path = report.snapshot.path_key.clone();
        if !self.reports.contains_key(&path) {
            if self.order.len() == CAPACITY
                && let Some(oldest) = self.order.pop_front()
            {
                self.reports.remove(&oldest);
            }
            self.order.push_back(path.clone());
        }
        let index = usize::from(report.snapshot.partial);
        self.reports.entry(path).or_default()[index] = Some(Arc::new(report));
    }

    pub(crate) fn invalidate_document(&mut self, path_key: &Path) {
        self.invalidate_reports(path_key);
        self.delivered.remove(path_key);
    }

    pub(in crate::semantic) fn invalidate_reports(&mut self, path_key: &Path) {
        self.replacements.remove(path_key);
        if self.reports.remove(path_key).is_some() {
            self.order.retain(|key| key != path_key);
        }
    }

    pub(crate) fn clear(&mut self) {
        self.reports.clear();
        self.order.clear();
        self.replacements.clear();
    }

    pub(super) fn delivered(&self, path_key: &Path) -> Option<&DeliveredReport> {
        self.delivered.get(path_key)
    }

    pub(super) fn mark_delivered(
        &mut self,
        key: &DiagnosticSnapshotKey,
        source: Option<DiagnosticSource>,
    ) {
        if let Some(report) = self.get_shared(key).cloned() {
            if report.items.is_empty() && source.is_none() {
                self.delivered.remove(&key.path_key);
            } else {
                self.delivered
                    .insert(key.path_key.clone(), DeliveredReport::new(report, source));
            }
        }
    }
}

impl Engine {
    pub(crate) fn shared_diagnostic_report(
        &self,
        key: &DiagnosticSnapshotKey,
    ) -> Option<Arc<DiagnosticReport>> {
        self.diagnostics_cache.get_shared(key).cloned()
    }

    pub(crate) fn retain_diagnostic_replacement(&mut self, report: Arc<DiagnosticReport>) {
        self.diagnostics_cache
            .replacements
            .insert(report.snapshot.path_key.clone(), report);
    }

    pub(crate) fn release_diagnostic_replacement(&mut self, path: &Path) {
        self.diagnostics_cache.replacements.remove(path);
    }
}

/// Reuses the semantic part of a key across edits to document bodies.
#[derive(Default)]
pub(crate) struct DiagnosticKeys {
    entries: HashMap<PathBuf, SemanticInputs>,
    #[cfg(test)]
    computations: usize,
}

#[derive(Default)]
pub(super) struct SemanticInputs {
    pub(super) fingerprint: u64,
    pub(super) files: HashSet<PathBuf>,
    pub(super) draft_groups: HashMap<PathBuf, HashSet<String>>,
}

impl DiagnosticKeys {
    pub(super) fn get(&self, path: &Path) -> Option<u64> {
        self.entries.get(path).map(|inputs| inputs.fingerprint)
    }

    pub(super) fn insert(&mut self, path: PathBuf, inputs: SemanticInputs) {
        if self.entries.len() == CAPACITY && !self.entries.contains_key(&path) {
            self.entries.clear();
        }
        self.entries.insert(path, inputs);
        #[cfg(test)]
        {
            self.computations += 1;
        }
    }

    pub(in crate::semantic) fn invalidate_file<'a>(
        &mut self,
        path: &Path,
        drafts: impl IntoIterator<Item = (&'a Path, &'a str)> + Clone,
    ) {
        self.entries.retain(|document, inputs| {
            document != path
                && !inputs.files.contains(path)
                && !drafts.clone().into_iter().any(|(directory, name)| {
                    inputs
                        .draft_groups
                        .get(directory)
                        .is_some_and(|names| names.contains(name))
                })
        });
    }

    pub(in crate::semantic) fn clear(&mut self) {
        self.entries.clear();
    }

    #[cfg(test)]
    pub(super) fn computations(&self) -> usize {
        self.computations
    }
}
