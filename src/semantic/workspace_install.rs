use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use anyhow::Result;
use tree_sitter::Parser;

use crate::telemetry;
use crate::workspace::{CancelToken, Workspace, WorkspaceLoad, path_key};

use super::completion::prefilter::{FileNames, Stamp};
use super::disk_selection::DiskPackageSelection;
use super::index::{CachedNavigationText, DiskIndex, FileIndex};
use super::shared_disk::FileObservation;
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
    selection_inputs: HashMap<PathBuf, Option<FileObservation>>,
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

    fn text_current(&self, text: &CachedNavigationText) -> bool {
        text.stamp.is_some()
            && self.current_stamp == text.stamp
            && self.text.as_ref().is_some_and(|(stamp, previous)| {
                *stamp == text.stamp && Weak::ptr_eq(previous, &text.text)
            })
    }

    fn disk_current(&self, disk: &DiskIndex) -> bool {
        self.disk.as_ref().is_some_and(|(stamp, previous)| {
            *stamp == disk.stamp && Weak::ptr_eq(previous, &disk.declarations)
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
    package_selections: HashMap<PathBuf, DiskPackageSelection>,
}

impl WorkspaceCacheSnapshot {
    #[cfg(test)]
    pub(crate) fn validate(mut self, workspace: Workspace) -> LoadedWorkspace {
        for (path, observation) in &mut self.selection_inputs {
            *observation = FileObservation::read(path).ok();
        }
        self.validate_inner(workspace, None)
            .expect("uncancellable workspace validation")
    }

    pub(crate) fn observe_package_selections(&mut self, cancel: &CancelToken) -> Result<()> {
        for (path, observation) in &mut self.selection_inputs {
            cancel.check()?;
            *observation = FileObservation::read(path).ok();
        }
        Ok(())
    }

    pub(crate) fn validate_cancellable(
        self,
        workspace: Workspace,
        cancel: &CancelToken,
    ) -> Result<LoadedWorkspace> {
        self.validate_inner(workspace, Some(cancel))
    }

    fn validate_inner(
        mut self,
        workspace: Workspace,
        cancel: Option<&CancelToken>,
    ) -> Result<LoadedWorkspace> {
        let started = telemetry::now();
        let sdk = workspace.sdk_root().map(path_key).or(self.sdk_root.take());
        let mut graph_stamp_paths = 0;
        let mut metadata_paths = 0;
        for (path, observation) in &mut self.files {
            if let Some(cancel) = cancel {
                cancel.check()?;
            }
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
        let mut package_selections = HashMap::new();
        if !self.selection_inputs.is_empty() {
            let mut parser = Parser::new();
            parser
                .set_language(&tree_sitter_go::LANGUAGE.into())
                .expect("Go grammar must match the tree-sitter runtime");
            for (path, before) in self.selection_inputs.drain() {
                if let Some(cancel) = cancel {
                    cancel.check()?;
                }
                if let Some(before) = before
                    && workspace.selected_file_stamp(&path) == Some(Some(before.stamp))
                    && let Some(selection) =
                        DiskPackageSelection::read_observed(&mut parser, &path, before)
                {
                    package_selections.insert(path, selection);
                }
            }
        }
        if let Some(cancel) = cancel {
            cancel.check()?;
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
        Ok(LoadedWorkspace {
            workspace,
            validation: self,
            package_selections,
        })
    }
}

impl Engine {
    pub(crate) fn can_install_background_test_view(&self, workspace: &Workspace) -> bool {
        self.state
            .workspace
            .as_ref()
            .is_some_and(|production| workspace.covers_production_groups(production))
    }

    pub(crate) fn workspace_cache_snapshot(&self, load: WorkspaceLoad) -> WorkspaceCacheSnapshot {
        let mut files = HashMap::<_, CacheObservation>::new();
        for (path, names) in &self.state.completion_names {
            files.entry(path.clone()).or_default().names = Some((names.stamp, names.fingerprint));
        }
        for (path, text) in &self.state.navigation_texts {
            files.entry(path.clone()).or_default().text = Some((text.stamp, text.text.clone()));
        }
        for (path, disk) in &self.state.disk_index {
            files.entry(path.clone()).or_default().disk =
                Some((disk.stamp, disk.declarations.clone()));
        }
        WorkspaceCacheSnapshot {
            load,
            graph_generation: self.state.completion_graph_generation,
            sdk_root: self.completion_imports.sdk_root().as_deref().map(path_key),
            files,
            selection_inputs: self
                .state
                .documents
                .keys()
                .chain(self.state.pending_documents.keys())
                .chain(self.state.diagnostic_file_changes.keys())
                .map(|path| (path.clone(), None))
                .collect(),
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
        let installed = self.install_validated_workspace(load, loaded);
        if installed {
            self.include_tests |= load.include_tests;
        }
        installed
    }

    fn install_validated_workspace(
        &mut self,
        load: WorkspaceLoad,
        LoadedWorkspace {
            workspace,
            validation,
            package_selections,
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
        for (path, selection) in package_selections {
            if !changed.contains(&path)
                || self
                    .state
                    .diagnostic_file_changes
                    .get(&path)
                    .is_none_or(|change| change.stamp == Some(selection.stamp))
            {
                self.state.disk_package_selections.insert(path, selection);
            }
        }
        let cache_validation_ms = cache_started.map(telemetry::elapsed);
        self.implementation_sources.retain_workspace(&workspace);
        self.state.workspace = Some(Arc::new(workspace));
        self.state.completion_graph_generation += 1;
        self.state.completion_sources_changed(None);
        self.state.diagnostic_keys.get_mut().clear();
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
    use crate::semantic::index::NavigationText;
    use crate::semantic::source::Source;

    fn cache(engine: &mut Engine, path: &Path) -> Arc<FileIndex> {
        let text = NavigationText::read_cancellable(path, &AtomicBool::new(false)).unwrap();
        let source = Source::parse(
            &mut engine.state.parser,
            path.to_owned(),
            text.text.as_ref().clone(),
            None,
        )
        .unwrap();
        let (mut index, declarations) = DiskIndex::test_observation(&source);
        index.stamp = text.stamp;
        let key = path_key(path);
        engine
            .state
            .completion_names
            .insert(key.clone(), FileNames::new(text.stamp.unwrap(), &text.text));
        engine.state.disk_index.insert(key.clone(), index);
        engine.state.navigation_texts.insert(key, text.downgrade());
        declarations
    }

    #[test]
    fn adding_a_gap_after_legacy_build_tag_invalidates_selected_files() {
        assert_legacy_build_gap_changes_selection(
            "// +build rs_gopls_unset_build_tag\npackage sample\n",
            "// +build rs_gopls_unset_build_tag\n\npackage sample\n",
            true,
        );
    }

    #[test]
    fn removing_a_gap_after_legacy_build_tag_invalidates_excluded_files() {
        assert_legacy_build_gap_changes_selection(
            "// +build rs_gopls_unset_build_tag\n\npackage sample\n",
            "// +build rs_gopls_unset_build_tag\npackage sample\n",
            false,
        );
    }

    fn assert_legacy_build_gap_changes_selection(before: &str, after: &str, selected: bool) {
        let project = TestProject::new();
        project.write("stable.go", "package sample\n");
        let path = project.write("conditional.go", before);
        let selected: &[&str] = if selected {
            &["stable.go", "conditional.go"]
        } else {
            &["stable.go"]
        };
        let mut engine = Engine::new(project.root.clone());
        let load = engine.workspace_load();
        assert!(engine.install_workspace(load, project.workspace(selected, false)));
        let _declarations = cache(&mut engine, &path);
        project.write("conditional.go", after);
        engine.file_changed(&path, FileChangeType::CHANGED);
        assert_eq!(engine.workspace_load().generation, load.generation + 1);
    }

    #[test]
    fn workspace_install_backfills_open_disk_selection_independently_of_the_overlay() {
        let project = TestProject::new();
        project.write("go.mod", "module example.test/sample\n\ngo 1.22\n");
        let path = project.write("api.go", "package sample\nfunc Value() int { return 1 }\n");
        let mut engine = Engine::new(project.root.clone());
        let load = engine.workspace_load();
        let overlay = "package overlay\nimport \"bytes\"\nvar _ bytes.Buffer\n";
        engine.open(path.clone(), overlay.into(), 1).unwrap();
        let initial = Workspace::load(&project.root, false).unwrap();
        let expected = package_selection_view(&initial);
        assert!(engine.install_workspace(load, initial));
        let key = path_key(&path);
        assert!(engine.state.disk_package_selections.contains_key(&key));
        assert!(!engine.state.disk_index.contains_key(&key));
        for value in [22, 333, 4444] {
            project.write(
                "api.go",
                &format!("package sample\nfunc Value() int {{ return {value} }}\n"),
            );
            assert_eq!(
                engine.observe_file_change(&path, FileChangeType::CHANGED, true),
                None
            );
            assert_eq!(engine.state.documents[&key].text.as_str(), overlay);
        }
        engine.close(&path);
        project.write(
            "api.go",
            "package sample\nfunc Value() int { return 55555 }\n",
        );
        assert_eq!(
            engine.observe_file_change(&path, FileChangeType::CHANGED, true),
            None
        );
        assert_eq!(engine.workspace_load(), load);
        let rebuilt = Workspace::load(&project.root, false).unwrap();
        assert_eq!(package_selection_view(&rebuilt), expected);
        assert_eq!(
            engine.state.workspace.as_ref().unwrap().package_errors(),
            rebuilt.package_errors()
        );
    }

    fn package_selection_view(workspace: &Workspace) -> Vec<(String, String, Vec<PathBuf>, bool)> {
        workspace
            .navigation_packages()
            .map(|package| {
                (
                    package.id.clone(),
                    package.name.clone(),
                    package.files.clone(),
                    package.load_error,
                )
            })
            .collect()
    }

    #[test]
    fn workspace_first_cold_save_backfills_even_without_an_overlay_disk_index() {
        let project = TestProject::new();
        let path = project.write("api.go", "package sample\nfunc Value() int { return 1 }\n");
        let mut engine = Engine::new(project.root.clone());
        let load = engine.workspace_load();
        // The document opened after this worker captured the observed paths.
        let snapshot = engine.workspace_cache_snapshot(load);
        engine
            .open(path.clone(), "package overlay\n".into(), 1)
            .unwrap();
        let loaded = snapshot.validate(project.workspace(&["api.go"], false));
        assert!(engine.install_loaded_workspace(load, loaded));
        assert!(engine.state.disk_package_selections.is_empty());

        project.write("api.go", "package sample\nfunc Value() int { return 22 }\n");
        let first = engine.observe_file_change(&path, FileChangeType::CHANGED, true);
        assert_eq!(first.unwrap().1, "cold_file");
        project.write(
            "api.go",
            "package sample\nfunc Value() int { return 333 }\n",
        );
        assert_eq!(
            engine.observe_file_change(&path, FileChangeType::CHANGED, true),
            None
        );
        assert!(!engine.state.disk_index.contains_key(&path_key(&path)));
    }

    #[test]
    fn workspace_disk_selection_changes_are_not_hidden_by_an_open_overlay() {
        let cases = [
            ("package sample\n", "package renamed\n"),
            ("package sample\n", "package sample\nimport _ \"fmt\"\n"),
            (
                "package sample\n",
                "//go:build rs_gopls_unset_build_tag\n\npackage sample\n",
            ),
            (
                "package sample\n/*\n#cgo CFLAGS: -DFIRST\n*/\nimport \"C\"\n",
                "package sample\n/*\n#cgo CFLAGS: -DSECOND\n*/\nimport \"C\"\n",
            ),
            (
                "package sample\n// #cgo CFLAGS: -DFIRST\nimport \"C\"\n",
                "package sample\n// #cgo CFLAGS: -DFIRST\n\nimport \"C\"\n",
            ),
            (
                "package sample\nimport _ \"embed\"\n\n//go:embed first.txt\nvar data string\n",
                "package sample\nimport _ \"embed\"\n\n//go:embed second.txt\nvar data string\n",
            ),
            (
                "package sample\nimport _ \"\\x65mbed\"\nvar data = \x60\n//go:embed first.txt\n\x60\n",
                "package sample\nimport _ \"\\x65mbed\"\n//go:embed first.txt\nvar data string\n",
            ),
        ];
        for (before, after) in cases {
            let project = TestProject::new();
            let path = project.write("api.go", before);
            let mut engine = Engine::new(project.root.clone());
            let load = engine.workspace_load();
            assert!(engine.install_workspace(load, project.workspace(&["api.go"], false)));
            engine.open(path.clone(), before.into(), 1).unwrap();
            project.write("api.go", after);
            assert!(!engine.watched_package_selection_is_unchanged(&path, FileChangeType::CHANGED));
            assert_eq!(
                engine
                    .observe_file_change(&path, FileChangeType::CHANGED, true)
                    .unwrap()
                    .1,
                "signature_changed",
                "{after}",
            );
        }
    }

    #[test]
    fn workspace_lifecycle_events_reload_even_when_package_selection_spelling_matches() {
        let project = TestProject::new();
        let original = project.write("api.go", "package sample\n");
        let renamed = project.root.join("renamed.go");
        let mut engine = Engine::new(project.root.clone());
        let load = engine.workspace_load();
        assert!(engine.install_workspace(load, project.workspace(&["api.go"], false)));
        engine
            .open(original.clone(), "package sample\n".into(), 1)
            .unwrap();
        assert!(
            engine
                .observe_file_change(&original, FileChangeType::CREATED, true)
                .is_some()
        );
        fs::rename(&original, &renamed).unwrap();
        engine.watched_files_changed(&[
            (original.clone(), FileChangeType::DELETED),
            (renamed.clone(), FileChangeType::CREATED),
        ]);
        let next = engine.workspace_load();
        assert_eq!(next.generation, load.generation + 1);
        assert!(
            !engine
                .state
                .disk_package_selections
                .contains_key(&path_key(&original))
        );
        assert!(engine.install_workspace(next, project.workspace(&["renamed.go"], false)));
        let graph = engine.state.workspace.as_ref().unwrap();
        assert!(graph.package_of_file(&original).is_none());
        assert!(graph.package_of_file(&renamed).is_some());

        let unknown = project.write("unobserved.go", "package sample\n");
        engine.file_changed(&unknown, FileChangeType::CHANGED);
        assert_eq!(engine.workspace_load().generation, next.generation + 1);
    }

    #[test]
    fn workspace_body_watch_keeps_declarations_and_graph_but_reads_new_disk_text() {
        let project = TestProject::new();
        let dependency = project.write("dep.go", "package sample\nfunc Value() int { return 1 }\n");
        let text = "package sample\nvar _ = Value()\n";
        let caller = project.write("api.go", text);
        let mut engine = Engine::new(project.root.clone());
        let load = engine.workspace_load();
        let files = ["api.go", "dep.go"];
        assert!(engine.install_workspace(load, project.workspace(&files, false)));
        engine.open(caller.clone(), text.into(), 1).unwrap();
        let initial = engine.diagnostics_report(&caller).unwrap().0.clone();
        assert!(initial.items.is_empty());
        engine.state.shared_disk.clear_full_cache();
        assert_eq!(
            engine.state.shared_disk.projection_counts(&dependency),
            (0, 1)
        );

        let updated = "package sample\nfunc Value() int { return 1234 }\n";
        project.write("dep.go", updated);
        engine.watched_files_changed(&[(dependency.clone(), FileChangeType::CHANGED)]);
        assert_eq!(engine.workspace_load(), load);
        assert_eq!(
            engine.state.shared_disk.projection_counts(&dependency),
            (0, 2)
        );
        let disk = engine
            .state
            .shared_disk
            .read(&dependency, None, None)
            .unwrap();
        assert_eq!(disk.text.as_str(), updated);
        assert_eq!(
            engine.diagnostics_report(&caller).unwrap().0.result_id,
            initial.result_id,
        );

        let upgrade = WorkspaceLoad {
            include_tests: true,
            ..load
        };
        assert!(engine.install_workspace(upgrade, project.workspace(&files, true)));
        assert_eq!(
            engine.diagnostics_report(&caller).unwrap().0.result_id,
            initial.result_id,
        );
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
        let _initial_declarations =
            [&stable, &edit, &watch, &stale, &replace].map(|path| cache(&mut engine, path));
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
        let _updated_declarations = [&watch, &new, &replace].map(|path| cache(&mut engine, path));
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
