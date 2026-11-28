use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Result, ensure};

use super::model::DiagnosticDocumentRevision;
use super::retention::{DeliveredReport, DiagnosticSource};
use super::{
    DiagnosticReport, DiagnosticSnapshotKey, DiagnosticStage, Stages, analyze, analyze_stage1,
    snapshot_key_for_path,
};
use crate::go_environment::GoEnvironment;
use crate::semantic::Engine;
use crate::semantic::index::IndexedSourceRevision;
use crate::semantic::shared_disk::SharedDiskCache;
use crate::semantic::snapshot::DocumentSnapshot;
use crate::semantic::source::{check_cancelled, is_test_source};
use crate::workspace::{DiagnosticFileObservation, Workspace, WorkspaceLoad, path_key};

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
    shared_disk: Arc<SharedDiskCache>,
    overlays: HashMap<PathBuf, IndexedSourceRevision>,
    new_documents: HashSet<PathBuf>,
    file_changes: Arc<HashMap<PathBuf, DiagnosticFileObservation>>,
    delivered: Option<DeliveredReport>,
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
        if let Some(report) = self.diagnostics_cache.get_complete(&key) {
            return Ok(DiagnosticsPreparation::Ready(report));
        }
        Ok(DiagnosticsPreparation::Cold(key))
    }

    pub(crate) fn open_document_version(&self, path: &Path) -> Option<i32> {
        self.open_document_identity(path)
            .map(|(_, version)| version)
    }

    pub(crate) fn diagnostic_document_len(&self, path: &Path) -> usize {
        self.state
            .documents
            .get(&path_key(path))
            .map(|source| source.text.len())
            .or_else(|| std::fs::metadata(path).ok()?.len().try_into().ok())
            .unwrap_or(0)
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
        let delivered = self.diagnostics_cache.delivered(&key.path_key).cloned();
        DiagnosticInput {
            workspace: if key.partial {
                None
            } else {
                self.state.workspace.clone()
            },
            shared_disk: self.state.shared_disk.clone(),
            key,
            path: path.to_owned(),
            roots: self.state.roots.clone(),
            environment: self.state.environment.clone(),
            overlays,
            new_documents,
            file_changes: self.state.diagnostic_file_changes.clone(),
            delivered,
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

    pub(crate) fn mark_diagnostic_report_delivered(&mut self, key: &DiagnosticSnapshotKey) {
        if let DiagnosticDocumentRevision::Open {
            version,
            open_generation,
            fingerprint,
            content_revision,
        } = key.revision
            && let Some(source) = self.state.documents.get(&key.path_key)
            && source.open_generation == open_generation
            && source.document_version == Some(version)
            && source.fingerprint() == fingerprint
            && source.content_revision == content_revision
        {
            self.diagnostics_cache
                .mark_delivered(key, DiagnosticSource::capture(source));
        }
    }
}

impl DiagnosticInput {
    #[cfg(test)]
    pub(crate) fn analyze(self, cancelled: &AtomicBool) -> Result<Option<DiagnosticReport>> {
        self.analyze_at_stage(DiagnosticStage::Stage2, cancelled)
    }

    #[cfg(test)]
    pub(crate) fn analyze_at_stage(
        self,
        stage: DiagnosticStage,
        cancelled: &AtomicBool,
    ) -> Result<Option<DiagnosticReport>> {
        self.analyze_with_stages(stage, cancelled, &mut None)
    }

    pub(crate) fn analyze_with_stages(
        self,
        stage: DiagnosticStage,
        cancelled: &AtomicBool,
        stages: &mut Option<Stages>,
    ) -> Result<Option<DiagnosticReport>> {
        let result = self.compute(stage, cancelled, stages);
        if check_cancelled(cancelled).is_err() {
            return Ok(None);
        }
        result
    }

    fn compute(
        self,
        stage: DiagnosticStage,
        cancelled: &AtomicBool,
        stages: &mut Option<Stages>,
    ) -> Result<Option<DiagnosticReport>> {
        check_cancelled(cancelled)?;
        let mut engine = Engine::with_shared_disk(self.roots, self.environment, self.shared_disk);
        engine.state.workspace = self.workspace;
        engine.state.new_documents = self.new_documents;
        engine.state.diagnostic_file_changes = self.file_changes;
        engine.state.documents = self
            .overlays
            .into_iter()
            .map(|(path, revision)| (path, revision.into_indexed()))
            .collect();
        if !engine.state.documents.contains_key(&self.key.path_key) {
            let source = engine.state.shared_disk.load(
                &mut engine.state.parser,
                &self.path,
                Some(cancelled),
                None,
            )?;
            engine
                .state
                .documents
                .insert(self.key.path_key.clone(), source.into_indexed());
        }
        let mut snapshot = engine.snapshot();
        let document = DocumentSnapshot::borrowed(&self.path, &snapshot.docs()[&self.key.path_key]);
        if stage == DiagnosticStage::Stage2 {
            return analyze(&mut snapshot, &document, self.key, Some(cancelled), stages).map(Some);
        }

        let delivered = self.delivered.filter(|_| !self.key.partial);
        let retention = delivered
            .as_ref()
            .and_then(|previous| previous.retention(&self.key, document.indexed()));
        if delivered.is_some() && retention.is_none() {
            return Ok(None);
        }
        let dirty = retention
            .as_ref()
            .and_then(|previous| previous.dirty_range());
        let mut verify = retention
            .as_ref()
            .map(|previous| previous.unresolved_to_verify(cancelled))
            .transpose()?
            .unwrap_or_default();
        let report = analyze_stage1(
            &mut snapshot,
            &document,
            self.key,
            dirty,
            &mut verify,
            Some(cancelled),
            stages,
        )?;
        match retention {
            Some(previous) => previous.merge(report, verify, cancelled),
            None => Ok(Some(report)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering::Relaxed;
    use std::thread;
    use std::time::{Duration, Instant};

    use lsp_types::{CodeActionKind, Position, TextDocumentContentChangeEvent};

    use super::*;
    use crate::protocol::path_uri;
    use crate::semantic::diagnostics::{ContextCompleteness, DiagnosticCode, DiagnosticFixData};
    use crate::semantic::shared_disk::CacheEvent;
    use crate::test_support::TestProject;

    fn run(engine: &mut Engine, path: &Path, stage: DiagnosticStage) -> Option<DiagnosticReport> {
        let key = engine.diagnostic_key(path).unwrap();
        engine
            .capture_diagnostics(path, key)
            .analyze_at_stage(stage, &AtomicBool::new(false))
            .unwrap()
    }

    fn edit(engine: &mut Engine, path: &Path, version: i32, text: &str) {
        engine
            .edit(
                path,
                &[TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: text.into(),
                }],
                version,
            )
            .unwrap();
    }

    fn file(engine: &mut Engine, path: &Path, text: &str) {
        engine.open(path.into(), text.into(), 1).unwrap();
    }

    fn deliver(engine: &mut Engine, path: &Path) -> DiagnosticReport {
        let report = run(engine, path, DiagnosticStage::Stage2).unwrap();
        engine.install_diagnostic_report(report.clone());
        engine.mark_diagnostic_report_delivered(&report.snapshot);
        report
    }

    #[test]
    fn stages_keep_unchanged_findings_after_utf16_remapping() {
        let project = TestProject::new();
        let before = "package sample\r\nfunc edit(){ _ = 1 }\r\nfunc stable(){ _ = \"🙂\"; var n int = \"bad\"; _ = n }\r\n";
        let after = before.replace("_ = 1", "\r\n_ = unknown");
        let path = project.write("sample.go", before);
        let mut engine = Engine::new(project.root.clone());
        assert!(engine.install_workspace(
            engine.workspace_load(),
            project.workspace(&["sample.go"], false)
        ));
        file(&mut engine, &path, before);
        let previous = deliver(&mut engine, &path);
        let previous_type = previous
            .items
            .iter()
            .find(|item| item.code == DiagnosticCode::TypeMismatch)
            .unwrap();
        edit(&mut engine, &path, 2, &after);

        let first = run(&mut engine, &path, DiagnosticStage::Stage1).unwrap();
        assert!(
            first
                .items
                .iter()
                .any(|item| item.code == DiagnosticCode::NameUnresolved)
        );
        let moved = first
            .items
            .iter()
            .find(|item| item.code == DiagnosticCode::TypeMismatch)
            .unwrap();
        assert_eq!(
            moved.range.start,
            Position::new(
                previous_type.range.start.line + 1,
                previous_type.range.start.character
            )
        );
        let second = run(&mut engine, &path, DiagnosticStage::Stage2).unwrap();
        assert!(second.same_visible_content(&first));
    }

    #[test]
    fn locally_resolved_missing_name_is_cleared_in_the_fast_report() {
        let project = TestProject::new();
        let before = "package sample\nfunc f(){ _ = missing }\n";
        let after = "package sample\nfunc f(){ missing := 1; _ = missing }\n";
        let path = project.write("sample.go", before);
        let mut engine = Engine::new(project.root.clone());
        assert!(engine.install_workspace(
            engine.workspace_load(),
            project.workspace(&["sample.go"], false)
        ));
        file(&mut engine, &path, before);
        let previous = deliver(&mut engine, &path);
        assert_eq!(previous.items[0].code, DiagnosticCode::NameUnresolved);
        edit(&mut engine, &path, 2, after);

        let first = run(&mut engine, &path, DiagnosticStage::Stage1).unwrap();
        assert!(first.items.is_empty(), "{:?}", first.items);
    }

    #[test]
    fn stage_one_rechecks_positioned_package_errors_after_a_syntax_repair() {
        let project = TestProject::new();
        let before = "package sample\nfunc stable(){ _ = 1 }\nfunc edited(){ _ = ) }\n";
        let path = project.write("sample.go", before);
        let listing = serde_json::json!({
            "Dir": project.root, "ImportPath": "sample", "Name": "sample", "GoFiles": ["sample.go"],
            "Error": {"Err": "expected operand, found ')'", "Pos": "sample.go:2:20"}
        });
        let graph = Workspace::from_test_listing(
            std::slice::from_ref(&project.root),
            &listing.to_string(),
            false,
        );
        let mut engine = Engine::new(project.root.clone());
        assert!(engine.install_workspace(engine.workspace_load(), graph));
        file(&mut engine, &path, before);
        assert!(
            deliver(&mut engine, &path)
                .items
                .iter()
                .any(|item| item.code == DiagnosticCode::PackageLoad)
        );

        edit(&mut engine, &path, 2, &before.replace("= )", "= 2"));
        let first = run(&mut engine, &path, DiagnosticStage::Stage1).unwrap();
        assert!(first.items.is_empty(), "{:?}", first.items);

        edit(&mut engine, &path, 3, &before.replace("= )", "= @"));
        let first = run(&mut engine, &path, DiagnosticStage::Stage1).unwrap();
        assert!(
            first
                .items
                .iter()
                .any(|item| item.code == DiagnosticCode::SyntaxError)
        );
        assert!(
            first
                .items
                .iter()
                .all(|item| item.code != DiagnosticCode::PackageLoad),
            "{:?}",
            first.items
        );
    }

    #[test]
    fn stages_defer_if_the_edited_function_could_still_have_an_earlier_type_error() {
        let project = TestProject::new();
        let before = "package sample\nfunc f(){ var n int = \"bad\"; _ = n; _ = 1 }\n";
        let after = before.replace("_ = 1", "_ = unknown");
        let path = project.write("sample.go", before);
        let mut engine = Engine::new(project.root.clone());
        assert!(engine.install_workspace(
            engine.workspace_load(),
            project.workspace(&["sample.go"], false)
        ));
        file(&mut engine, &path, before);
        let previous = deliver(&mut engine, &path);
        assert!(
            previous
                .items
                .iter()
                .any(|item| item.code == DiagnosticCode::TypeMismatch)
        );
        edit(&mut engine, &path, 2, &after);

        assert!(run(&mut engine, &path, DiagnosticStage::Stage1).is_none());
        let second = run(&mut engine, &path, DiagnosticStage::Stage2).unwrap();
        assert!(
            second
                .items
                .iter()
                .any(|item| item.code == DiagnosticCode::TypeMismatch)
        );
        assert!(
            second
                .items
                .iter()
                .any(|item| item.code == DiagnosticCode::NameUnresolved)
        );
    }

    #[test]
    fn same_lsp_version_with_new_dependency_meaning_defers_unsafe_retention() {
        let project = TestProject::new();
        let text = "package sample\nvar value int\n";
        let dependency_text = "package sample\nvar value int\n";
        let path = project.write("sample.go", text);
        let dependency = project.write("dependency.go", dependency_text);
        let mut engine = Engine::new(project.root.clone());
        assert!(engine.install_workspace(
            engine.workspace_load(),
            project.workspace(&["sample.go", "dependency.go"], false)
        ));
        file(&mut engine, &path, text);
        file(&mut engine, &dependency, dependency_text);
        let previous = deliver(&mut engine, &path);
        assert!(
            previous
                .items
                .iter()
                .any(|item| item.code == DiagnosticCode::NameRedeclared)
        );
        edit(
            &mut engine,
            &dependency,
            2,
            "package sample\nvar other int\n",
        );

        let current = engine.diagnostic_key(&path).unwrap();
        assert!(previous.snapshot.same_document(&current));
        assert_ne!(previous.snapshot, current);
        assert!(run(&mut engine, &path, DiagnosticStage::Stage1).is_none());
        assert!(
            run(&mut engine, &path, DiagnosticStage::Stage2)
                .unwrap()
                .items
                .is_empty()
        );
    }

    #[test]
    fn code_actions_wait_for_complete_fix_data_and_equivalent_promotion_keeps_id() {
        let project = TestProject::new();
        let text = "package sample\nfunc f(){ unused := 1 }\n";
        let path = project.write("sample.go", text);
        let mut engine = Engine::new(project.root.clone());
        assert!(engine.install_workspace(
            engine.workspace_load(),
            project.workspace(&["sample.go"], false)
        ));
        file(&mut engine, &path, text);
        let second = run(&mut engine, &path, DiagnosticStage::Stage2).unwrap();
        let key = second.snapshot.clone();
        let item = second
            .items
            .iter()
            .find(|item| item.code == DiagnosticCode::VarUnused)
            .unwrap();
        let uri = path_uri(&path).unwrap();
        let range = item.range;
        let only = [CodeActionKind::QUICKFIX];
        let mut carried = item.clone();
        carried.fix_data = DiagnosticFixData::None;
        let first = DiagnosticReport::at_stage(
            key.clone(),
            vec![carried],
            DiagnosticStage::Stage1,
            ContextCompleteness::Complete,
        );
        let id = first.result_id.clone();
        engine.install_diagnostic_report(first.clone());
        assert!(
            engine
                .code_actions_for_diagnostic_key(&path, &uri, range, Some(&only), &key)
                .unwrap()
                .is_empty()
        );

        engine.install_diagnostic_report(second);
        engine.install_diagnostic_report(first);
        let promoted = engine.diagnostic_report_for_key(&key).unwrap();
        assert_eq!(promoted.result_id, id);
        assert_eq!(promoted.diagnostic_stage, DiagnosticStage::Stage2);
        assert_eq!(
            engine
                .code_actions_for_diagnostic_key(&path, &uri, range, Some(&only), &key)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn workspace_fallback_is_a_partial_stage_one_report_and_reopen_changes_the_key() {
        let project = TestProject::new();
        let text = "package sample\nfunc f(){ _ = ) }\n";
        let path = project.write("sample.go", text);
        let mut engine = Engine::new(project.root.clone());
        file(&mut engine, &path, text);
        let fallback = deliver(&mut engine, &path);
        assert_eq!(fallback.context_completeness, ContextCompleteness::Partial);
        assert_eq!(fallback.diagnostic_stage, DiagnosticStage::Stage1);
        assert!(
            fallback
                .items
                .iter()
                .any(|item| item.code == DiagnosticCode::SyntaxError)
        );
        engine.close(&path);
        file(&mut engine, &path, text);

        let reopened = engine.diagnostic_key(&path).unwrap();
        assert_eq!(reopened.version, fallback.snapshot.version);
        assert_ne!(
            reopened.open_generation(),
            fallback.snapshot.open_generation()
        );
        assert!(!reopened.same_document(&fallback.snapshot));
        assert!(
            engine
                .diagnostics_cache
                .delivered(&reopened.path_key)
                .is_none()
        );
    }

    #[test]
    fn cancelling_shared_read_and_parse_waits_releases_both_subscribers() {
        for (flight, waiting) in [
            (CacheEvent::Read, CacheEvent::WaitRead),
            (CacheEvent::Parse, CacheEvent::WaitParse),
        ] {
            let project = TestProject::new();
            let path = project.write("sample.go", "package sample\nvar v int\n");
            let mut engine = Engine::new(project.root.clone());
            let key = engine.diagnostic_key(&path).unwrap();
            let producer = engine.capture_diagnostics(&path, key.clone());
            let waiter = engine.capture_diagnostics(&path, key);
            let (started, start) = crossbeam_channel::bounded(1);
            let (subscribed, subscription) = crossbeam_channel::bounded(1);
            let (release, blocked) = crossbeam_channel::bounded(1);
            let (exited, exit) = crossbeam_channel::bounded(1);
            let (producer_done, producer_exit) = crossbeam_channel::bounded(1);
            engine
                .state
                .shared_disk
                .set_hook(Some(Arc::new(move |_, event| {
                    if event == flight {
                        started.send(()).unwrap();
                        blocked.recv().ok();
                    } else if event == waiting {
                        subscribed.send(()).unwrap();
                    }
                })));
            let producer_cancel = AtomicBool::new(false);
            let waiter_cancel = AtomicBool::new(false);
            let (start, subscription, exit, producer_exit) = thread::scope(|scope| {
                let producer_flag = &producer_cancel;
                scope.spawn(move || {
                    producer_done.send(producer.analyze(producer_flag)).unwrap();
                });
                let start = start.recv_timeout(Duration::from_secs(5));
                let waiter_flag = &waiter_cancel;
                scope.spawn(move || {
                    let result = waiter.analyze(waiter_flag);
                    exited.send((result, Instant::now())).unwrap();
                });
                let subscription = subscription.recv_timeout(Duration::from_secs(5));
                producer_cancel.store(true, Relaxed);
                let signaled_at = Instant::now();
                waiter_cancel.store(true, Relaxed);
                let exit = exit.recv_timeout(Duration::from_millis(250));
                let producer_exit = producer_exit.recv_timeout(Duration::from_millis(250));
                release.send(()).unwrap();
                (
                    start,
                    subscription,
                    exit.map(|(result, stopped_at)| {
                        (result, stopped_at.saturating_duration_since(signaled_at))
                    }),
                    producer_exit,
                )
            });
            start.unwrap();
            subscription.unwrap();
            let (result, delay) = exit.unwrap();
            assert!(result.unwrap().is_none());
            assert!(producer_exit.unwrap().unwrap().is_none());
            assert!(
                delay <= Duration::from_millis(50),
                "{flight:?} waiter stopped after {delay:?}"
            );
            engine.state.shared_disk.set_hook(None);
            let expected = if flight == CacheEvent::Read {
                (1, 0)
            } else {
                (1, 1)
            };
            assert_eq!(engine.state.shared_disk.counts(&path), expected);
        }
    }
}
