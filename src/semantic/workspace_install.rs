use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use crate::telemetry;
use crate::workspace::{Workspace, WorkspaceLoad, path_key};

use super::completion::prefilter::{FileNames, Stamp};
use super::index::{DiskIndex, FileIndex, NavigationText};
use super::{Engine, GraphChanges};

pub(super) struct WorkspaceCacheChanges {
    load: WorkspaceLoad,
    pub(super) paths: HashSet<PathBuf>,
}

pub(crate) struct WorkspaceCacheSnapshot {
    load: WorkspaceLoad,
    graph_generation: u64,
    sdk_root: Option<PathBuf>,
    files: HashMap<PathBuf, CacheObservation>,
}

#[derive(Default)]
struct CacheObservation {
    names: Option<(Stamp, u64)>,
    text: Option<(Option<Stamp>, Weak<String>)>,
    disk: Option<(Option<Stamp>, Weak<FileIndex>)>,
    current_stamp: Option<Stamp>,
    selected: bool,
}

impl CacheObservation {
    fn names_current(&self, names: &FileNames) -> bool {
        self.names == Some((names.stamp, names.fingerprint))
            && self.current_stamp == Some(names.stamp)
    }

    fn text_current(&self, text: &NavigationText) -> bool {
        text.stamp.is_some()
            && self.current_stamp == text.stamp
            && self.text.as_ref().is_some_and(|(stamp, previous)| {
                *stamp == text.stamp && previous.as_ptr() == Arc::as_ptr(&text.text)
            })
    }

    fn disk_current(&self, disk: &DiskIndex) -> bool {
        self.disk.as_ref().is_some_and(|(stamp, previous)| {
            *stamp == disk.stamp && previous.as_ptr() == Arc::as_ptr(&disk.declarations)
        }) && self.current_stamp.is_some()
            && (self.current_stamp == disk.stamp
                || self.names.is_some_and(|(stamp, fingerprint)| {
                    Some(stamp) == self.current_stamp && disk.fingerprint == fingerprint
                })
                || self.text.as_ref().is_some_and(|(stamp, previous)| {
                    *stamp == self.current_stamp && disk.references_text(previous.as_ptr())
                }))
    }
}

pub(crate) struct LoadedWorkspace {
    pub(crate) workspace: Workspace,
    validation: WorkspaceCacheSnapshot,
}

impl WorkspaceCacheSnapshot {
    pub(crate) fn validate(mut self, workspace: Workspace) -> LoadedWorkspace {
        let started = telemetry::now();
        let sdk = workspace.sdk_root().map(path_key).or(self.sdk_root.take());
        let mut graph_stamp_paths = 0;
        let mut metadata_paths = 0;
        for (path, observation) in &mut self.files {
            if let Some(stamp) = workspace.selected_file_stamp(path) {
                // The graph loader already sampled selected paths for diagnostic identities.
                observation.current_stamp = stamp;
                observation.selected = true;
                graph_stamp_paths += 1;
            } else if !sdk.as_ref().is_some_and(|root| path.starts_with(root))
                && (observation.names.is_some() || observation.disk.is_some())
            {
                // Unselected navigation text is always dropped. An unselected disk index
                // can still be reused, and names may invalidate its underlying disk cache.
                metadata_paths += 1;
                observation.current_stamp = fs::metadata(path).ok().map(Stamp::from);
            }
        }
        if let Some(started) = started {
            telemetry::event!("workspace_cache_validation", {
                "target_generation": self.load.generation,
                "include_tests": self.load.include_tests,
                "validation_ms": telemetry::elapsed(started),
                "cache_paths": self.files.len(),
                "metadata_paths": metadata_paths,
                "graph_stamp_paths": graph_stamp_paths,
            });
        }
        LoadedWorkspace {
            workspace,
            validation: self,
        }
    }
}

impl Engine {
    pub(crate) fn workspace_cache_snapshot(&self, load: WorkspaceLoad) -> WorkspaceCacheSnapshot {
        let mut files = HashMap::<_, CacheObservation>::new();
        for (path, names) in &self.state.completion_names {
            files.entry(path.clone()).or_default().names = Some((names.stamp, names.fingerprint));
        }
        for (path, text) in &self.state.navigation_texts {
            files.entry(path.clone()).or_default().text =
                Some((text.stamp, Arc::downgrade(&text.text)));
        }
        for (path, disk) in &self.state.disk_index {
            files.entry(path.clone()).or_default().disk =
                Some((disk.stamp, Arc::downgrade(&disk.declarations)));
        }
        WorkspaceCacheSnapshot {
            load,
            graph_generation: self.state.completion_graph_generation,
            sdk_root: self.completion_imports.sdk_root().as_deref().map(path_key),
            files,
        }
    }

    pub(crate) fn begin_workspace_cache_validation(
        &mut self,
        load: WorkspaceLoad,
    ) -> WorkspaceCacheSnapshot {
        let snapshot = self.workspace_cache_snapshot(load);
        self.state.workspace_cache_changes = Some(WorkspaceCacheChanges {
            load,
            paths: HashSet::new(),
        });
        snapshot
    }

    pub(crate) fn discard_workspace_cache_validation(&mut self, load: WorkspaceLoad) {
        self.take_workspace_cache_changes(load);
    }

    fn take_workspace_cache_changes(&mut self, load: WorkspaceLoad) -> HashSet<PathBuf> {
        if self
            .state
            .workspace_cache_changes
            .as_ref()
            .is_some_and(|changes| changes.load == load)
        {
            return self.state.workspace_cache_changes.take().unwrap().paths;
        }
        HashSet::new()
    }

    #[cfg(test)]
    pub fn install_workspace(&mut self, load: WorkspaceLoad, workspace: Workspace) -> bool {
        let loaded = self.workspace_cache_snapshot(load).validate(workspace);
        self.install_validated_workspace(load, loaded)
    }

    pub(crate) fn install_loaded_workspace(
        &mut self,
        load: WorkspaceLoad,
        loaded: LoadedWorkspace,
    ) -> bool {
        if load.include_tests != loaded.workspace.includes_tests() {
            self.discard_workspace_cache_validation(load);
            return false;
        }
        self.install_validated_workspace(load, loaded)
    }

    fn install_validated_workspace(
        &mut self,
        load: WorkspaceLoad,
        LoadedWorkspace {
            workspace,
            validation,
        }: LoadedWorkspace,
    ) -> bool {
        let changed = self.take_workspace_cache_changes(load);
        if validation.load != load
            || self.state.generation != load.generation
            || self.state.completion_graph_generation != validation.graph_generation
        {
            return false;
        }
        let _loop_op = telemetry::loop_start("workspace_install", 20.0);
        let started = telemetry::now();
        telemetry::event!("workspace_install_start", {
            "target_generation": load.generation,
            "include_tests": load.include_tests,
        });
        if let Some(root) = self.completion_imports.sdk_root() {
            workspace.share_sdk_root(&root);
        }
        let sdk = workspace.sdk_root().map(path_key);
        let cache_started = telemetry::now();
        let observed = |path: &Path| {
            validation
                .files
                .get(path)
                .filter(|_| !changed.contains(path))
        };
        let is_sdk = |path: &Path| sdk.as_ref().is_some_and(|root| path.starts_with(root));
        self.state.completion_names.retain(|path, names| {
            if is_sdk(path) {
                return true;
            }
            if let Some(current) = observed(path).filter(|entry| entry.names_current(names)) {
                return current.selected;
            }
            self.state.disk_index.remove(path);
            self.completion_imports.source_changed(path);
            false
        });
        self.state.navigation_texts.retain(|path, text| {
            if observed(path).is_some_and(|entry| entry.selected && entry.text_current(text)) {
                return true;
            }
            self.state.disk_index.remove(path);
            self.implementation_sources.invalidate(path);
            false
        });
        self.state.disk_index.retain(|path, disk| {
            if is_sdk(path) || observed(path).is_some_and(|entry| entry.disk_current(disk)) {
                return true;
            }
            self.completion_imports.source_changed(path);
            self.implementation_sources.invalidate(path);
            false
        });
        let cache_validation_ms = cache_started.map(telemetry::elapsed);
        self.implementation_sources.retain_workspace(&workspace);
        self.state.workspace = Some(Arc::new(workspace));
        self.state.completion_graph_generation += 1;
        self.state.completion_sources_changed(None);
        self.state.graph_changes = GraphChanges::Clean;
        self.implementation_cache.clear();
        self.workspace_symbol_index.graph_changed();
        if let Some(started) = started {
            telemetry::event!("workspace_install", {
                "target_generation": load.generation,
                "include_tests": load.include_tests,
                "install_ms": telemetry::elapsed(started),
                "cache_validation_ms": cache_validation_ms,
                "metadata_ms": 0.0,
            });
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use lsp_types::{FileChangeType, TextDocumentContentChangeEvent};

    use crate::test_support::TestProject;

    use super::*;
    use crate::semantic::source::Source;

    fn cache(engine: &mut Engine, path: &Path) {
        let text = NavigationText::read_cancellable(path, &AtomicBool::new(false)).unwrap();
        let source = Source::parse(
            &mut engine.state.parser,
            path.to_owned(),
            text.text.as_ref().clone(),
            None,
        )
        .unwrap();
        let mut index = DiskIndex::new(&source);
        index.stamp = text.stamp;
        let key = path_key(path);
        engine.state.completion_names.insert(
            key.clone(),
            FileNames::new(text.stamp.unwrap(), &text.text, false),
        );
        engine.state.disk_index.insert(key.clone(), index);
        engine.state.navigation_texts.insert(key, text);
    }

    #[test]
    fn validation_retains_only_caches_from_unchanged_observations() {
        let project = TestProject::new();
        let files = [
            "stable.go",
            "edit.go",
            "watch.go",
            "new.go",
            "stale.go",
            "replace.go",
        ];
        let text = "package sample\nvar Value int\n";
        let [stable, edit, watch, new, stale, replace] =
            files.map(|file| project.write(file, text));
        let mut engine = Engine::new(project.root.clone());
        assert!(
            engine.install_workspace(engine.workspace_load(), project.workspace(&files, false))
        );
        engine.open(edit.clone(), text.into(), 1).unwrap();
        for path in [&stable, &edit, &watch, &stale, &replace] {
            cache(&mut engine, path);
        }
        let load = WorkspaceLoad {
            include_tests: true,
            ..engine.workspace_load()
        };
        let snapshot = engine.begin_workspace_cache_validation(load);
        assert_eq!(snapshot.files.len(), 5);
        project.write("stale.go", "package sample\nvar ChangedOnDisk string\n");
        let graph = project.workspace(&files, true);
        let loaded = std::thread::spawn(move || snapshot.validate(graph))
            .join()
            .unwrap();

        engine
            .edit(
                &edit,
                &[TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: "package sample\nvar Overlay int\n".into(),
                }],
                2,
            )
            .unwrap();
        engine.file_changed(&watch, FileChangeType::CHANGED);
        assert_eq!(engine.workspace_load().generation, load.generation);
        for path in [&watch, &new, &replace] {
            cache(&mut engine, path);
        }
        assert!(engine.install_loaded_workspace(load, loaded));

        let stable = path_key(&stable);
        assert!(engine.state.completion_names.contains_key(&stable));
        assert!(engine.state.navigation_texts.contains_key(&stable));
        assert!(engine.state.disk_index.contains_key(&stable));
        for path in [&edit, &watch, &new, &stale, &replace] {
            let key = path_key(path);
            // Recomputed names are equivalent when both their stamp and fingerprint match.
            assert_eq!(
                engine.state.completion_names.contains_key(&key),
                path == &replace
            );
            assert!(!engine.state.navigation_texts.contains_key(&key));
            assert!(!engine.state.disk_index.contains_key(&key));
        }
        assert!(engine.is_open(&edit));
        assert!(
            engine.state.documents[&path_key(&edit)]
                .text
                .contains("Overlay")
        );
        assert!(engine.state.workspace_cache_changes.is_none());
    }

    #[test]
    fn validation_cannot_replace_a_new_graph_or_install_a_mismatched_view() {
        let project = TestProject::new();
        let file = "sample.go";
        project.write(file, "package sample\n");
        let mut engine = Engine::new(project.root.clone());
        let production = engine.workspace_load();
        let tests = WorkspaceLoad {
            include_tests: true,
            ..production
        };
        let old = engine
            .workspace_cache_snapshot(production)
            .validate(project.workspace(&[file], false));
        let wrong_view = engine
            .begin_workspace_cache_validation(tests)
            .validate(project.workspace(&[file], false));
        assert!(!engine.install_loaded_workspace(tests, wrong_view));
        let new = engine
            .begin_workspace_cache_validation(tests)
            .validate(project.workspace(&[file], true));
        assert!(engine.install_loaded_workspace(tests, new));
        assert!(!engine.install_loaded_workspace(production, old));
        assert!(engine.state.workspace.as_ref().unwrap().includes_tests());

        let snapshot = engine.begin_workspace_cache_validation(tests);
        let created = project.write("new.go", "package sample\n");
        engine.file_changed(&created, FileChangeType::CREATED);
        let current = WorkspaceLoad {
            include_tests: true,
            ..engine.workspace_load()
        };
        let next = engine.begin_workspace_cache_validation(current);
        assert!(
            !engine.install_loaded_workspace(
                tests,
                snapshot.validate(project.workspace(&[file], true))
            )
        );
        assert_eq!(
            engine.state.workspace_cache_changes.as_ref().unwrap().load,
            current
        );
        assert!(engine.install_loaded_workspace(
            current,
            next.validate(project.workspace(&[file, "new.go"], true))
        ));
    }
}
