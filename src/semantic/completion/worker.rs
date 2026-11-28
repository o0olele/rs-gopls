use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use lsp_types::{CompletionResponse, Position};
use serde_json::Value;

use crate::go_environment::GoEnvironment;
use crate::semantic::index::{IndexedSourceRevision, overlay_revision};
use crate::semantic::open_overlay::PendingOpen;
use crate::semantic::shared_disk::SharedDiskCache;
use crate::semantic::source::check_cancelled;
use crate::semantic::{Engine, EngineState, GraphChanges, QueryProgress, SharedLoadPending};
use crate::workspace::{CancelToken, Workspace, WorkspaceLoad, path_key};

use super::imports::{ImportCache, ImportPrewarm};
use super::{CompletionClientCapabilities, StaleCompletion, Trace, completion_document};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct CompletionStamp {
    workspace: u64,
    graph: u64,
    sources: u64,
    disk: u64,
    origin: Option<(u64, u64, i32)>,
}

pub(crate) struct CompletionInput {
    path: PathBuf,
    position: Position,
    caps: CompletionClientCapabilities,
    trace: Option<Trace>,
    cancel: CancelToken,
    local_only: bool,
    stamp: CompletionStamp,
    include_tests: bool,
    roots: Vec<PathBuf>,
    environment: GoEnvironment,
    workspace: Option<Arc<Workspace>>,
    graph_changes: GraphChanges,
    shared_disk: Arc<SharedDiskCache>,
    overlays: HashMap<PathBuf, IndexedSourceRevision>,
    pending: HashMap<PathBuf, PendingOpen>,
    drafts: HashSet<PathBuf>,
    prewarm: ImportPrewarm,
}

pub(crate) enum CompletionOutcome {
    Ready(Value),
    NeedWorkspace(WorkspaceLoad),
    NeedOpenParse(SharedLoadPending),
    Cancelled,
    Failed(anyhow::Error),
}

/// Owned by the completion thread. Parser, indexes, SDK exports, and inference
/// weights survive between requests. Open source trees belong to one input.
#[derive(Default)]
pub(crate) struct CompletionWorker {
    caches: Option<WorkerCaches>,
}

struct WorkerCaches {
    state: EngineState,
    imports: ImportCache,
    overlay_versions: HashMap<PathBuf, (u64, u64)>,
    disk_generation: u64,
    graph_includes_tests: bool,
}

impl Engine {
    pub(crate) fn completion_stamp(&self, path: &Path) -> CompletionStamp {
        let key = path_key(path);
        let state = &self.state;
        let origin = if let Some(open) = state.pending_documents.get(&key) {
            Some((open.open_generation, open.content_revision, open.version))
        } else {
            state.documents.get(&key).and_then(|source| {
                Some((
                    source.open_generation,
                    source.content_revision,
                    source.document_version?,
                ))
            })
        };
        CompletionStamp {
            workspace: state.generation,
            graph: state.completion_graph_generation,
            sources: state.completion_source_generation,
            disk: state.shared_disk.generation(),
            origin,
        }
    }

    pub(crate) fn note_completion_load(&mut self, load: WorkspaceLoad) {
        self.include_tests |= load.include_tests;
    }

    pub(crate) fn capture_completion(
        &self,
        path: &Path,
        position: Position,
        caps: CompletionClientCapabilities,
        trace: Option<Trace>,
        cancel: CancelToken,
        local_only: bool,
    ) -> CompletionInput {
        #[cfg(test)]
        let trace = if let Some(hook) = &self.completion_hook {
            Some(Trace::with_hook(trace, path.to_owned(), hook.clone()))
        } else {
            trace
        };
        let _phase = trace.as_ref().map(|trace| trace.phase("snapshot_capture"));
        CompletionInput {
            path: path.to_owned(),
            position,
            caps,
            trace,
            cancel,
            local_only,
            stamp: self.completion_stamp(path),
            include_tests: self.include_tests,
            roots: self.state.roots.clone(),
            environment: self.state.environment.clone(),
            workspace: self.state.workspace.clone(),
            graph_changes: self.state.graph_changes.clone(),
            shared_disk: self.state.shared_disk.clone(),
            overlays: self
                .state
                .documents
                .iter()
                .map(|(key, source)| (key.clone(), source.revision()))
                .collect(),
            pending: self
                .state
                .pending_documents
                .iter()
                .map(|(key, open)| (key.clone(), open.read_snapshot()))
                .collect(),
            drafts: self.state.new_documents.clone(),
            prewarm: self.completion_imports.prewarm_snapshot(),
        }
    }

    #[cfg(test)]
    pub(crate) fn set_completion_hook(&mut self, hook: Option<super::CompletionHook>) {
        self.completion_hook = hook;
    }

    #[cfg(test)]
    pub(crate) fn use_empty_completion_sdk(&mut self) {
        self.completion_imports.use_empty_sdk(
            self.state.completion_graph_generation,
            self.state.workspace.as_deref().unwrap(),
        );
    }
}

impl CompletionWorker {
    pub(crate) fn run(&mut self, mut input: CompletionInput) -> CompletionOutcome {
        if input.cancel.is_cancelled() {
            return CompletionOutcome::Cancelled;
        }
        let caches = self.caches.get_or_insert_with(|| WorkerCaches {
            state: EngineState::new(
                input.roots.clone(),
                input.environment.clone(),
                input.shared_disk.clone(),
            ),
            imports: input.prewarm.clone().cache(),
            overlay_versions: HashMap::new(),
            disk_generation: input.stamp.disk,
            graph_includes_tests: false,
        });
        let install = input
            .trace
            .as_ref()
            .map(|trace| trace.phase("snapshot_install"));
        caches.install(&mut input);
        drop(install);
        let result = caches.complete(&input);
        caches.state.documents.clear();
        caches.state.pending_documents.clear();
        caches.state.workspace = None;
        if input.cancel.is_cancelled() {
            return CompletionOutcome::Cancelled;
        }
        match result {
            Ok(outcome) => outcome,
            Err(error) => match error.downcast::<SharedLoadPending>() {
                Ok(pending) => CompletionOutcome::NeedOpenParse(pending),
                Err(error)
                    if error
                        .downcast_ref::<io::Error>()
                        .is_some_and(|error| error.kind() == io::ErrorKind::Interrupted) =>
                {
                    CompletionOutcome::Failed(StaleCompletion.into())
                }
                Err(error) => CompletionOutcome::Failed(error),
            },
        }
    }
}

impl WorkerCaches {
    fn install(&mut self, input: &mut CompletionInput) {
        let state = &mut self.state;
        let graph_includes_tests = input
            .workspace
            .as_ref()
            .is_some_and(|graph| graph.includes_tests());
        let prewarm_changed = self.imports.update_prewarm(input.prewarm.clone());
        if prewarm_changed || !Arc::ptr_eq(&state.shared_disk, &input.shared_disk) {
            state.disk_index.clear();
            state.navigation_texts.clear();
            state.completion_names.clear();
            state.completion_weights.clear();
            self.imports.clear_sdk_sources();
        } else if state.completion_graph_generation != input.stamp.graph {
            let sdk = input
                .workspace
                .as_ref()
                .and_then(|graph| graph.sdk_root())
                .map(path_key)
                .or_else(|| self.imports.sdk_root().as_deref().map(path_key));
            // File artifacts are keyed by source revision and checked on
            // use. Keep files still selected by the new graph, including the
            // common transition from production to the test view.
            let graph = input.workspace.as_deref();
            let test_view_promotion = !self.graph_includes_tests
                && graph_includes_tests
                && state.generation == input.stamp.workspace
                && self.imports.has_sdk_catalog(sdk.as_deref());
            let is_sdk = |key: &Path| sdk.as_ref().is_some_and(|root| key.starts_with(root));
            let selected = |key: &Path| {
                is_sdk(key) || graph.is_some_and(|graph| graph.selected_file_stamp(key).is_some())
            };
            state.disk_index.retain(|key, _| selected(key));
            state.navigation_texts.retain(|key, _| selected(key));
            state.completion_names.retain(|key, names| {
                if test_view_promotion && names.sdk_generation.is_some() {
                    // The installed test view doesn't invalidate the SDK
                    // catalog. Its own disk generation is checked on use.
                    return true;
                }
                let stamp = graph.and_then(|graph| graph.selected_file_stamp(key));
                let keep = stamp.map_or_else(|| is_sdk(key), |stamp| stamp == Some(names.stamp));
                if !keep {
                    self.imports.source_changed(key);
                }
                keep
            });
            state.completion_weights.clear();
        }
        self.graph_includes_tests = graph_includes_tests;

        let mut changed = HashSet::new();
        if self.disk_generation != input.stamp.disk {
            match input.shared_disk.invalidated_since(self.disk_generation) {
                Some(paths) => {
                    for path in paths {
                        state.disk_index.remove(&path);
                        state.navigation_texts.remove(&path);
                        state.completion_names.remove(&path);
                        self.imports.source_changed(&path);
                        changed.insert(path);
                    }
                }
                None => {
                    state.disk_index.clear();
                    state.navigation_texts.clear();
                    state.completion_names.clear();
                    state.completion_weights.clear();
                    self.imports.clear_sdk_sources();
                }
            }
            self.disk_generation = input.stamp.disk;
        }

        state.roots = std::mem::take(&mut input.roots);
        state.environment = input.environment.clone();
        state.workspace = input.workspace.take();
        state.shared_disk = input.shared_disk.clone();
        state.graph_changes = std::mem::take(&mut input.graph_changes);
        state.generation = input.stamp.workspace;
        state.completion_graph_generation = input.stamp.graph;
        state.completion_source_generation = input.stamp.sources;
        state.new_documents = std::mem::take(&mut input.drafts);
        state.pending_documents = std::mem::take(&mut input.pending);
        state.documents.extend(
            input
                .overlays
                .drain()
                .map(|(path, revision)| (path, revision.into_indexed())),
        );

        let revision =
            |key: &Path| overlay_revision(key, &state.documents, Some(&state.pending_documents));
        self.overlay_versions.retain(|key, previous| {
            let current = revision(key);
            if current != Some(*previous) {
                changed.insert(key.clone());
            }
            if let Some(current) = current {
                *previous = current;
            }
            current.is_some()
        });
        for key in state.documents.keys().chain(state.pending_documents.keys()) {
            if !self.overlay_versions.contains_key(key) {
                self.overlay_versions
                    .insert(key.clone(), revision(key).expect("captured overlay"));
                changed.insert(key.clone());
            }
        }
        if !state.completion_weights.is_empty() && !changed.is_empty() {
            let mut affected = HashSet::new();
            for path in changed {
                let Some(packages) = state
                    .workspace
                    .as_ref()
                    .and_then(|graph| graph.affected_packages(&path))
                else {
                    state.completion_weights.clear();
                    return;
                };
                affected.extend(packages);
            }
            state
                .completion_weights
                .retain(|key, _| !affected.contains(key.package_id()));
        }
    }

    fn complete(&mut self, input: &CompletionInput) -> Result<CompletionOutcome> {
        let cancelled = input.cancel.flag();
        check_cancelled(cancelled)?;
        if let Some(pending) = self.state.pending_open_dependency(&input.path) {
            return Ok(CompletionOutcome::NeedOpenParse(pending));
        }
        let trace = input.trace.as_ref();
        let mut snapshot = self
            .state
            .snapshot(input.include_tests)
            .with_completion_cancelled(cancelled);
        let read = trace.map(|trace| trace.phase("document"));
        let document = completion_document(&mut snapshot, &input.path, trace)?;
        drop(read);
        let Some(document) = document else {
            return Ok(CompletionOutcome::Ready(Value::Array(Vec::new())));
        };
        let disk_ticket = document.read_ticket().cloned();
        let generation = (!input.local_only).then_some(input.stamp.graph);
        let response = match snapshot.complete_document(
            document,
            input.position,
            &input.caps,
            generation,
            trace,
        )? {
            QueryProgress::Ready(response) => response,
            QueryProgress::Pending(load) => return Ok(CompletionOutcome::NeedWorkspace(load)),
            QueryProgress::Searching(mut task) => {
                loop {
                    check_cancelled(cancelled)?;
                    if !task.search.is_ready(&self.imports) {
                        let _phase = trace.map(|trace| trace.phase("import_wait"));
                        let _ = self.imports.wake().recv_timeout(Duration::from_millis(5));
                        continue;
                    }
                    if task.advance_snapshot(&mut snapshot, &mut self.imports)? {
                        break;
                    }
                }
                task.into_response()
            }
        };

        let encode = trace.map(|trace| trace.phase("encode"));
        check_cancelled(cancelled)?;
        let value = match response {
            CompletionResponse::Array(items) => {
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    check_cancelled(cancelled)?;
                    values.push(serde_json::to_value(item)?);
                }
                Value::Array(values)
            }
            response => serde_json::to_value(response)?,
        };
        drop(encode);
        check_cancelled(cancelled)?;
        let _validate = trace.map(|trace| trace.phase("validate_disk"));
        if disk_ticket.is_some_and(|ticket| !ticket.matches_disk(&input.path)) {
            return Err(StaleCompletion.into());
        }
        Ok(CompletionOutcome::Ready(value))
    }
}
