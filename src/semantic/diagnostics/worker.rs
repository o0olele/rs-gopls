use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Result, ensure};

use super::{DiagnosticReport, DiagnosticSnapshotKey, analyze, snapshot_key_for_path};
use crate::go_environment::GoEnvironment;
use crate::semantic::Engine;
use crate::semantic::index::{IndexedSource, IndexedSourceRevision};
use crate::semantic::snapshot::DocumentSnapshot;
use crate::semantic::source::{Source, check_cancelled, is_test_source};
use crate::workspace::{Workspace, WorkspaceLoad};

pub(crate) enum DiagnosticsPreparation<'a> {
    Pending(WorkspaceLoad),
    Ready(&'a DiagnosticReport),
    Cold(DiagnosticSnapshotKey),
}

pub(crate) struct DiagnosticInput {
    key: DiagnosticSnapshotKey,
    path: PathBuf,
    roots: Vec<PathBuf>,
    environment: GoEnvironment,
    workspace: Option<Arc<Workspace>>,
    overlays: HashMap<PathBuf, IndexedSourceRevision>,
    new_documents: HashSet<PathBuf>,
}

impl Engine {
    pub(crate) fn diagnostic_key(&mut self, path: &Path) -> Result<DiagnosticSnapshotKey> {
        ensure!(
            path.extension().is_some_and(|extension| extension == "go"),
            "expected a Go document"
        );
        Ok(snapshot_key_for_path(&self.snapshot(), path)?)
    }

    pub(crate) fn prepare_diagnostics(
        &mut self,
        path: &Path,
        allow_partial: bool,
    ) -> Result<DiagnosticsPreparation<'_>> {
        self.include_tests |= is_test_source(path);
        let key = self.diagnostic_key(path)?;
        if key.partial && !allow_partial {
            return Ok(DiagnosticsPreparation::Pending(self.workspace_load()));
        }
        if let Some(report) = self.diagnostics_cache.get(&key) {
            return Ok(DiagnosticsPreparation::Ready(report));
        }
        Ok(DiagnosticsPreparation::Cold(key))
    }

    pub(crate) fn capture_diagnostics(
        &self,
        path: &Path,
        key: DiagnosticSnapshotKey,
    ) -> DiagnosticInput {
        let (overlays, new_documents) = if key.partial {
            let overlays = self
                .state
                .documents
                .get_key_value(&key.path_key)
                .into_iter()
                .map(|(path, source)| (path.clone(), source.revision()))
                .collect();
            (overlays, HashSet::new())
        } else {
            let overlays = self
                .state
                .documents
                .iter()
                .map(|(path, source)| (path.clone(), source.revision()))
                .collect();
            (overlays, self.state.new_documents.clone())
        };
        DiagnosticInput {
            workspace: if key.partial {
                None
            } else {
                self.state.workspace.clone()
            },
            key,
            path: path.to_owned(),
            roots: self.state.roots.clone(),
            environment: self.state.environment.clone(),
            overlays,
            new_documents,
        }
    }

    pub(crate) fn install_diagnostic_report(&mut self, report: DiagnosticReport) {
        self.diagnostics_cache.insert(report);
    }

    pub(crate) fn diagnostic_report_for_key(
        &self,
        key: &DiagnosticSnapshotKey,
    ) -> Option<&DiagnosticReport> {
        self.diagnostics_cache.get(key)
    }
}

impl DiagnosticInput {
    pub(crate) fn analyze(self, cancelled: &AtomicBool) -> Result<Option<DiagnosticReport>> {
        let result = self.compute(cancelled);
        if check_cancelled(cancelled).is_err() {
            return Ok(None);
        }
        result.map(Some)
    }

    fn compute(self, cancelled: &AtomicBool) -> Result<DiagnosticReport> {
        check_cancelled(cancelled)?;
        let mut engine = Engine::with_environment(self.roots, self.environment);
        engine.state.workspace = self.workspace;
        engine.state.new_documents = self.new_documents;
        engine.state.documents = self
            .overlays
            .into_iter()
            .map(|(path, revision)| (path, revision.into_indexed()))
            .collect();
        if !engine.state.documents.contains_key(&self.key.path_key) {
            let source = Source::read_cancellable(&mut engine.state.parser, &self.path, cancelled)?;
            engine
                .state
                .documents
                .insert(self.key.path_key.clone(), IndexedSource::new(source, None));
        }
        let mut snapshot = engine.snapshot();
        let document = DocumentSnapshot::borrowed(&self.path, &snapshot.docs()[&self.key.path_key]);
        analyze(&mut snapshot, &document, self.key, Some(cancelled))
    }
}
