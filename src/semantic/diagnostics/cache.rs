use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use super::model::DiagnosticDocumentRevision;
use super::{DiagnosticReport, DiagnosticSnapshotKey};

const CAPACITY: usize = 256;

#[derive(Default)]
pub(crate) struct DiagnosticsCache {
    reports: HashMap<PathBuf, [Option<DiagnosticReport>; 2]>,
    order: VecDeque<PathBuf>,
}

impl DiagnosticsCache {
    pub(crate) fn get(&self, key: &DiagnosticSnapshotKey) -> Option<&DiagnosticReport> {
        self.reports.get(&key.path_key)?[usize::from(key.partial)]
            .as_ref()
            .filter(|report| &report.snapshot == key)
    }

    pub(crate) fn insert(&mut self, report: DiagnosticReport) {
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
        self.reports.entry(path).or_default()[index] = Some(report);
    }

    pub(crate) fn invalidate_document(&mut self, path_key: &Path) {
        if self.reports.remove(path_key).is_some() {
            self.order.retain(|key| key != path_key);
        }
    }

    pub(crate) fn clear(&mut self) {
        self.reports.clear();
        self.order.clear();
    }
}

/// Reuses keys within an input epoch. The epoch only controls this memo;
/// report equality depends on the selected file and semantic inputs.
#[derive(Default)]
pub(crate) struct DiagnosticKeys {
    entries: HashMap<PathBuf, ((u64, u64), DiagnosticSnapshotKey)>,
}

impl DiagnosticKeys {
    pub(super) fn get(
        &self,
        path: &Path,
        revision: DiagnosticDocumentRevision,
        epoch: (u64, u64),
    ) -> Option<&DiagnosticSnapshotKey> {
        let (cached_epoch, key) = self.entries.get(path)?;
        (*cached_epoch == epoch && key.revision == revision).then_some(key)
    }

    pub(super) fn insert(&mut self, epoch: (u64, u64), key: DiagnosticSnapshotKey) {
        if self.entries.len() == CAPACITY && !self.entries.contains_key(&key.path_key) {
            self.entries.clear();
        }
        self.entries.insert(key.path_key.clone(), (epoch, key));
    }
}
