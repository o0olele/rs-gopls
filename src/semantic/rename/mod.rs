mod collisions;
mod field_safety;
mod methods;
mod scan;
mod target;

use super::index::{IndexedSource, text_fingerprint};
use super::source::Source;
use super::{Engine, QueryProgress, SemanticTask};
use crate::protocol::path_uri;
use crate::workspace::path_key;
use anyhow::{Context, Result, ensure};
use lsp_types::{
    DocumentChanges, OneOf, OptionalVersionedTextDocumentIdentifier, Position,
    PrepareRenameResponse, TextDocumentEdit, WorkspaceEdit,
};
use methods::MethodContracts;
use scan::FileScan;
use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use target::Target;

#[derive(Debug)]
pub(crate) enum RenameError {
    Rejected(String),
    ContentModified,
}

pub(crate) type RenameResult = std::result::Result<WorkspaceEdit, RenameError>;
type Location = (PathBuf, lsp_types::Range);

fn workspace_edit(edits: Vec<TextDocumentEdit>) -> WorkspaceEdit {
    WorkspaceEdit {
        document_changes: Some(DocumentChanges::Edits(edits)),
        ..Default::default()
    }
}

impl Engine {
    fn rename_load(&mut self) -> Result<Option<crate::workspace::WorkspaceLoad>> {
        self.include_tests = true;
        self.track_rename_inputs = true;
        let snapshot = self.snapshot();
        match snapshot.active_graph(true) {
            QueryProgress::Ready(graph)
                if graph.rename_inputs_current(snapshot.environment())? =>
            {
                Ok(None)
            }
            QueryProgress::Ready(_) => {
                self.invalidate(None);
                Ok(Some(self.workspace_load()))
            }
            QueryProgress::Pending(load) => Ok(Some(load)),
            QueryProgress::Searching(task) => match task {},
        }
    }

    pub(crate) fn prepare_rename(
        &mut self,
        path: &Path,
        pos: Position,
    ) -> Result<QueryProgress<Option<PrepareRenameResponse>>> {
        if let Some(load) = self.rename_load()? {
            return Ok(QueryProgress::Pending(load));
        }
        let target = target::target(self, path, pos)?;
        Ok(QueryProgress::Ready(target.map(|target| {
            PrepareRenameResponse::RangeWithPlaceholder {
                range: target.range,
                placeholder: target.spelling,
            }
        })))
    }

    pub(crate) fn rename(
        &mut self,
        path: &Path,
        pos: Position,
        new_name: &str,
    ) -> Result<QueryProgress<RenameResult, RenameTask>> {
        if !valid_name(new_name) {
            return Ok(QueryProgress::Ready(Err(RenameError::Rejected(format!(
                "{new_name:?} is not a usable Go identifier"
            )))));
        }
        if let Some(load) = self.rename_load()? {
            return Ok(QueryProgress::Pending(load));
        }
        let Some(target) = target::target(self, path, pos)? else {
            return Ok(QueryProgress::Ready(Err(RenameError::Rejected(
                "No editable Go declaration at this position".into(),
            ))));
        };
        if target.spelling == new_name {
            return Ok(QueryProgress::Ready(Ok(workspace_edit(Vec::new()))));
        }
        let mut task = RenameTask::new(self, target, path, pos, new_name)?;
        // Pin both ends of the initial resolution before returning to the event loop.
        task.capture(self, path)?;
        let declaration = task.target.location.0.clone();
        if path_key(&declaration) != path_key(path) {
            task.capture(self, &declaration)?;
        }
        Ok(QueryProgress::Searching(task))
    }
}

fn valid_name(name: &str) -> bool {
    static IDENTIFIER: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\A[\p{L}_][\p{L}\p{Nd}_]*\z").unwrap());
    !matches!(
        name,
        "_" | "break"
            | "default"
            | "func"
            | "interface"
            | "select"
            | "case"
            | "defer"
            | "go"
            | "map"
            | "struct"
            | "chan"
            | "else"
            | "goto"
            | "package"
            | "switch"
            | "const"
            | "fallthrough"
            | "if"
            | "range"
            | "type"
            | "continue"
            | "for"
            | "import"
            | "return"
            | "var"
    ) && IDENTIFIER.is_match(name)
}

struct Input {
    path: PathBuf,
    version: Option<i32>,
    fingerprint: u64,
}

enum Phase {
    Capture,
    Family,
    Scan,
    FieldUses,
    Apply,
    Validate,
    Contracts,
    Publish,
}

pub(crate) struct RenameTask {
    target: Target,
    request: (PathBuf, Position),
    new_name: String,
    generation: u64,
    // The private engine shares the immutable graph. Its documents, lazy
    // indexes and query caches belong only to this hypothetical edit.
    preview: Engine,
    files: Vec<PathBuf>,
    queue: VecDeque<PathBuf>,
    inputs: BTreeMap<PathBuf, Input>,
    scans: BTreeMap<PathBuf, FileScan>,
    contracts: Option<MethodContracts>,
    destinations: Option<methods::MethodDestinations>,
    fields: Vec<(Location, methods::FieldDestination)>,
    embedded: Vec<Location>,
    phase: Phase,
    result: Option<RenameResult>,
}

impl RenameTask {
    fn new(
        engine: &mut Engine,
        target: Target,
        path: &Path,
        pos: Position,
        new_name: &str,
    ) -> Result<Self> {
        let graph = engine
            .state
            .workspace
            .as_ref()
            .context("missing package graph")?;
        let mut files: Vec<_> = if target.file_local {
            vec![target.location.0.clone()]
        } else {
            let mut active = graph.project_symbol_files();
            for key in &engine.state.new_documents {
                if let Some(doc) = engine.state.documents.get(key)
                    && graph.contains_project_path(key)
                    && super::index::draft_package(graph, doc).is_some()
                {
                    active.insert(key.clone(), doc.path.clone());
                }
            }
            active.into_values().collect()
        };
        files.sort_unstable();
        let mut preview =
            Engine::with_environment(engine.roots().to_vec(), engine.environment().clone());
        preview.state.workspace = Some(graph.clone());
        preview.include_tests = true;
        preview.state.new_documents = engine.state.new_documents.clone();
        let mut queue: VecDeque<_> = files.iter().cloned().collect();
        // Open dependencies can supply types; they are inputs but not edit destinations.
        queue.extend(
            engine
                .state
                .documents
                .values()
                .filter(|s| target.file_local || !graph.contains_project_path(&path_key(&s.path)))
                .map(|s| s.path.clone()),
        );
        Ok(Self {
            preview,
            files,
            queue,
            target,
            request: (path.into(), pos),
            new_name: new_name.into(),
            generation: engine.workspace_load().generation,
            inputs: BTreeMap::new(),
            scans: BTreeMap::new(),
            contracts: None,
            destinations: None,
            fields: Vec::new(),
            embedded: Vec::new(),
            phase: Phase::Capture,
            result: None,
        })
    }

    fn capture(&mut self, engine: &mut Engine, path: &Path) -> Result<()> {
        let key = path_key(path);
        if self.inputs.contains_key(&key) {
            return Ok(());
        }
        let mut snap = engine.snapshot();
        let doc = snap.document(path)?.context("Go file disappeared")?;
        let input = Input {
            path: path.into(),
            version: doc.version(),
            fingerprint: text_fingerprint(doc.text()),
        };
        let source = Source::parse(
            &mut self.preview.state.parser,
            path.into(),
            doc.text().into(),
            None,
        )?;
        self.preview
            .state
            .documents
            .insert(key.clone(), IndexedSource::new(source, doc.version()));
        self.inputs.insert(key, input);
        Ok(())
    }

    fn next_phase(&mut self, phase: Phase) {
        self.phase = phase;
        self.queue = self.files.iter().cloned().collect();
    }

    fn step(&mut self, engine: &mut Engine) -> Result<()> {
        match self.phase {
            Phase::Capture => {
                if let Some(path) = self.queue.pop_front() {
                    self.capture(engine, &path)?;
                } else {
                    self.phase = Phase::Family;
                }
            }
            Phase::Family => {
                let current = target::target(&mut self.preview, &self.request.0, self.request.1)?;
                if !current.is_some_and(|t| {
                    t.location == self.target.location && t.spelling == self.target.spelling
                }) {
                    self.result = Some(Err(RenameError::ContentModified));
                    return Ok(());
                }
                let contracts = methods::method_contracts(
                    &mut self.preview,
                    &self.target.location,
                    &self.target.spelling,
                )?;
                collisions::check_collisions(&mut self.preview, &contracts.family, &self.new_name)?;
                if contracts.is_method {
                    self.destinations = Some(methods::method_destinations(
                        &mut self.preview,
                        &contracts.family,
                        &self.new_name,
                    )?);
                }
                if let Some(field) = methods::field_destination(
                    &mut self.preview,
                    &self.target.location,
                    &self.new_name,
                )? {
                    field_safety::check(
                        &mut self.preview,
                        &self.target.location,
                        &self.new_name,
                        &[],
                        &field.disk_inputs,
                    )?;
                    self.fields.push((self.target.location.clone(), field));
                }
                self.contracts = Some(contracts);
                self.next_phase(Phase::Scan);
            }
            Phase::Scan => {
                if let Some(path) = self.queue.pop_front() {
                    let scan = scan::collect(
                        &mut self.preview,
                        &path,
                        &self.target,
                        &self.new_name,
                        &self.contracts.as_ref().unwrap().family,
                    )?;
                    self.embedded.extend(scan.embedded_fields.iter().cloned());
                    self.scans.insert(path_key(&path), scan);
                } else {
                    for (path, declaration) in &self.contracts.as_ref().unwrap().family {
                        ensure!(
                            self.scans
                                .get(&path_key(path))
                                .is_some_and(|s| s.edits.iter().any(|e| e.range == *declaration
                                    || (self.target.implicit_import
                                        && e.range.start == declaration.start
                                        && e.range.start == e.range.end))),
                            "could not verify an edit for the declaration at {}",
                            path.display()
                        );
                    }
                    if self.embedded.is_empty() {
                        self.next_phase(Phase::Apply);
                    } else {
                        collisions::check_collisions(
                            &mut self.preview,
                            &self.embedded,
                            &self.new_name,
                        )?;
                        for location in &self.embedded {
                            let field = methods::field_destination(
                                &mut self.preview,
                                location,
                                &self.new_name,
                            )?
                            .context("cannot verify implicit field method sets")?;
                            field_safety::check(
                                &mut self.preview,
                                location,
                                &self.new_name,
                                &self.embedded,
                                &field.disk_inputs,
                            )?;
                            self.fields.push((location.clone(), field));
                        }
                        self.next_phase(Phase::FieldUses);
                    }
                }
            }
            Phase::FieldUses => {
                if let Some(path) = self.queue.pop_front() {
                    self.scans
                        .get_mut(&path_key(&path))
                        .unwrap()
                        .add_field_uses(&mut self.preview, &path, &self.embedded, &self.new_name)?;
                } else {
                    self.next_phase(Phase::Apply);
                }
            }
            Phase::Apply => {
                if let Some(path) = self.queue.pop_front() {
                    let key = path_key(&path);
                    self.scans
                        .get_mut(&key)
                        .unwrap()
                        .apply(&mut self.preview, &path)?;
                } else {
                    self.next_phase(Phase::Validate);
                }
            }
            Phase::Validate => {
                if let Some(path) = self.queue.pop_front() {
                    self.scans[&path_key(&path)].validate(&mut self.preview, &path, &self.scans)?;
                } else {
                    self.phase = Phase::Contracts;
                }
            }
            Phase::Contracts => {
                let before = self.contracts.as_ref().unwrap();
                let target = scan::map_location(&self.scans, &self.target.location);
                for (location, field) in &self.fields {
                    let mapped = scan::map_location(&self.scans, location);
                    let after =
                        methods::field_destination(&mut self.preview, &mapped, &self.new_name)?
                            .context("cannot verify the renamed field")?;
                    if field.disk_inputs != after.disk_inputs {
                        self.result = Some(Err(RenameError::ContentModified));
                        return Ok(());
                    }
                    ensure!(
                        field.receivers.is_subset(&after.receivers),
                        "field {} would shadow a promoted method",
                        self.new_name
                    );
                }
                if before.is_method {
                    let after =
                        methods::method_contracts(&mut self.preview, &target, &self.new_name)?;
                    let destinations = methods::method_destinations(
                        &mut self.preview,
                        &before.family,
                        &self.new_name,
                    )?;
                    self.destinations
                        .as_ref()
                        .context("missing original method selections")?
                        .verify(
                            &destinations,
                            |location| scan::map_location(&self.scans, location),
                            &self.new_name,
                        )?;
                    let remapped: std::collections::HashSet<_> = before
                        .family
                        .iter()
                        .map(|loc| scan::map_location(&self.scans, loc))
                        .collect();
                    ensure!(
                        before.relations == after.relations
                            && remapped
                                == after
                                    .family
                                    .into_iter()
                                    .map(|(p, r)| (path_key(&p), r))
                                    .collect(),
                        "renaming {} would change interface implementation relationships",
                        self.target.spelling
                    );
                    if after.disk_inputs != before.disk_inputs {
                        self.result = Some(Err(RenameError::ContentModified));
                        return Ok(());
                    }
                }
                self.phase = Phase::Publish;
            }
            Phase::Publish => {
                self.result = Some(self.publish(engine)?);
            }
        }
        Ok(())
    }

    fn inputs_current(&self, engine: &Engine) -> bool {
        if !engine.state.workspace.as_ref().is_some_and(|w| {
            w.rename_inputs_current(engine.environment())
                .unwrap_or(false)
        }) {
            return false;
        }
        for (key, input) in &self.inputs {
            let current = match engine.state.documents.get(key) {
                Some(doc) => {
                    doc.document_version == input.version
                        && text_fingerprint(&doc.text) == input.fingerprint
                }
                None => {
                    input.version.is_none()
                        && fs::read_to_string(&input.path)
                            .is_ok_and(|text| text_fingerprint(&text) == input.fingerprint)
                }
            };
            if !current {
                return false;
            }
        }
        let disk_current = |key: &Path, hash: u64| {
            fs::read_to_string(key).is_ok_and(|s| text_fingerprint(&s) == hash)
        };
        if let Some(contracts) = &self.contracts
            && contracts
                .disk_inputs
                .iter()
                .any(|(key, hash)| !disk_current(key, *hash))
        {
            return false;
        }
        if self.fields.iter().any(|(_, field)| {
            field
                .disk_inputs
                .iter()
                .any(|(key, hash)| !disk_current(key, *hash))
        }) {
            return false;
        }
        self.preview.state.disk_index.iter().all(|(key, index)| {
            self.contracts
                .as_ref()
                .is_some_and(|c| c.disk_inputs.contains_key(key))
                || disk_current(key, index.fingerprint)
        })
    }

    fn publish(&self, engine: &Engine) -> Result<RenameResult> {
        if !self.inputs_current(engine) {
            return Ok(Err(RenameError::ContentModified));
        }
        let mut documents = Vec::new();
        for (key, scan) in &self.scans {
            if scan.edits.is_empty() {
                continue;
            }
            let input = &self.inputs[key];
            ensure!(
                target::writable(
                    self.preview.state.workspace.as_deref().unwrap(),
                    &input.path,
                    input.version.is_some()
                ),
                "file cannot be edited: {}",
                input.path.display()
            );
            documents.push(TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier {
                    uri: path_uri(&input.path)?,
                    version: input.version,
                },
                edits: scan.edits.iter().cloned().map(OneOf::Left).collect(),
            });
        }
        Ok(Ok(workspace_edit(documents)))
    }
}

impl SemanticTask<RenameResult> for RenameTask {
    fn advance(&mut self, engine: &mut Engine) -> Result<bool> {
        if self.generation != engine.workspace_load().generation
            || !engine
                .state
                .workspace
                .as_ref()
                .zip(self.preview.state.workspace.as_ref())
                .is_some_and(|(live, captured)| std::sync::Arc::ptr_eq(live, captured))
        {
            self.result = Some(Err(RenameError::ContentModified));
        } else if let Err(error) = self.step(engine) {
            self.result = Some(Err(if self.inputs_current(engine) {
                RenameError::Rejected(format!("{error:#}"))
            } else {
                RenameError::ContentModified
            }));
        }
        Ok(self.result.is_some())
    }

    fn finish(self) -> RenameResult {
        self.result
            .expect("only completed rename tasks are published")
    }
}

#[cfg(test)]
mod tests;
