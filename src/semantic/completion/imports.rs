use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::Result;
use lsp_types::{CompletionItemKind as Kind, Range};

use crate::go_environment::GoEnvironment;
use crate::semantic::{
    SharedLoadPending,
    index::{CachedNavigationText, CompletionRead, FileIndex, IndexReader},
    resolver::{ResolvedSymbol, Resolver, is_exported},
    snapshot::{DocumentSnapshot, EngineSnapshot},
    source::{check_optional_cancelled, is_test_source},
};
use crate::symbols::{Container, Symbol, SymbolKind, TypeRef, TypeRefKind};
use crate::workspace::{Package, Workspace, path_key};

use super::candidates::resolved_candidate;
use super::cursor::{identifier_char, identifier_start};
use super::expected::{Expected, named_key};
use super::prefilter::{Batch, Decision, Input, SOURCE_LIMIT, Scanner};
use super::profile::Phase;
use super::ranking::{
    AUTO_IMPORT, Candidate, Insert, Match, SYMBOL, Weight, match_member_hump, match_name,
    match_start, package_priority,
};
use super::render::{Description, PendingImport, SourceRev, Target, describe, package_name};
use super::sdk::{Catalog, Inventory};
use super::{CompletionClientCapabilities, Trace};

const BATCH_SIZE: usize = SOURCE_LIMIT;

pub(super) struct FileImport {
    pub(super) path: String,
    pub(super) qualifier: String,
    pub(super) in_graph: bool,
}

pub(super) enum Selection {
    Name { auto_import: bool },
    Qualifier { name: String, start: usize },
}

pub(super) struct SymbolSearch {
    pub(super) selection: Selection,
    pub(super) types_only: bool,
    pub(super) address_of: bool,
    pub(super) literal: bool,
    pub(super) declared: HashSet<String>,
    pub(super) skipped_package_names: HashSet<String>,
    pub(super) imports: Vec<FileImport>,
    pub(super) expected: Option<Expected>,
}

pub(super) enum SearchKind {
    Path { close: Option<char> },
    Symbols(SymbolSearch),
}

pub(in crate::semantic) struct ImportCache {
    generation: Option<u64>,
    graph: Rc<[Rc<Importable>]>,
    prewarm: ImportPrewarm,
    sdk: Option<(Arc<Catalog>, Rc<[Rc<Importable>]>)>,
    sdk_exports: HashMap<PathBuf, CachedSdkExports>,
    sdk_matches: HashMap<String, SdkMatches>,
    scanner: Scanner,
}

#[derive(Clone)]
pub(super) struct ImportPrewarm {
    inventory: Inventory,
    signal: crossbeam_channel::Sender<()>,
    wake: crossbeam_channel::Receiver<()>,
    #[cfg(test)]
    empty_sdk: Option<u64>,
}

struct Importable {
    path: String,
    name: String,
    directory: PathBuf,
    vendor_parent: Option<PathBuf>,
    prefilter_disk: bool,
    project: bool,
    package_id: Option<String>,
    files: Rc<[ImportFile]>,
}

#[derive(Clone)]
struct ImportFile {
    path: PathBuf,
    key: PathBuf,
}

struct SdkMatches {
    matcher: SymbolMatcher<'static>,
    generation: u64,
    source_files: Rc<[ImportFile]>,
    files: Rc<[ImportFile]>,
    names_validated: bool,
}

struct CachedSdkExports {
    text: CachedNavigationText,
    exports: Rc<[Export]>,
    sdk_generation: Option<u64>,
}

impl ImportFile {
    fn new(path: &Path) -> Self {
        Self {
            path: path.to_owned(),
            key: path_key(path),
        }
    }
}

impl Default for ImportCache {
    fn default() -> Self {
        let (signal, wake) = crossbeam_channel::bounded(1);
        ImportPrewarm {
            inventory: Inventory::new(signal.clone()),
            signal,
            wake,
            #[cfg(test)]
            empty_sdk: None,
        }
        .cache()
    }
}

impl ImportPrewarm {
    pub(super) fn cache(self) -> ImportCache {
        ImportCache {
            generation: None,
            graph: Rc::from([]),
            scanner: Scanner::new(self.signal.clone()),
            prewarm: self,
            sdk: None,
            sdk_exports: HashMap::new(),
            sdk_matches: HashMap::new(),
        }
    }
}

impl ImportCache {
    pub(super) fn prewarm_snapshot(&self) -> ImportPrewarm {
        self.prewarm.clone()
    }

    pub(super) fn update_prewarm(&mut self, prewarm: ImportPrewarm) -> bool {
        if !self.prewarm.signal.same_channel(&prewarm.signal) {
            *self = prewarm.cache();
            true
        } else {
            self.prewarm = prewarm;
            false
        }
    }

    pub(super) fn clear_sdk_sources(&mut self) {
        self.sdk = None;
        self.sdk_exports.clear();
        self.sdk_matches.clear();
    }

    pub(in crate::semantic) fn prewarm(
        &self,
        root: &Path,
        environment: &GoEnvironment,
        refresh: bool,
    ) {
        self.prewarm.inventory.start(root, environment, refresh);
    }

    pub(in crate::semantic) fn sdk_root(&self) -> Option<PathBuf> {
        self.prewarm.inventory.root()
    }

    pub(super) fn has_sdk_catalog(&self, root: Option<&Path>) -> bool {
        self.sdk
            .as_ref()
            .is_some_and(|(catalog, _)| catalog.watch.is_some() && catalog.root_key() == root)
    }

    pub(in crate::semantic) fn wake(&self) -> &crossbeam_channel::Receiver<()> {
        &self.prewarm.wake
    }

    fn prepare(
        &mut self,
        generation: u64,
        workspace: &Workspace,
        cancelled: Option<&AtomicBool>,
    ) -> Result<()> {
        if self.generation == Some(generation) {
            return Ok(());
        }
        let previous: HashMap<_, _> = self
            .graph
            .iter()
            .filter_map(|entry| Some((entry.package_id.as_deref()?, entry)))
            .collect();
        let mut graph = Vec::new();
        for package in workspace
            .navigation_packages()
            .filter(|package| package.is_production())
        {
            check_optional_cancelled(cancelled)?;
            if let Some(entry) = previous.get(package.id.as_str())
                && entry.matches_selected_files(workspace, package)
            {
                graph.push(Rc::clone(entry));
            } else if let Some(entry) = Importable::from_graph(workspace, package, cancelled)? {
                graph.push(Rc::new(entry));
            }
        }
        self.graph = graph.into();
        self.generation = Some(generation);
        Ok(())
    }

    fn sdk(
        &mut self,
        workspace: &Workspace,
        root: &Path,
        environment: &GoEnvironment,
        trace: Option<&Trace>,
        indexes: &mut IndexReader<'_>,
    ) -> Result<Option<Rc<[Rc<Importable>]>>> {
        let cancelled = indexes.cancelled;
        check_optional_cancelled(cancelled)?;
        #[cfg(test)]
        if self.prewarm.empty_sdk.is_some() && self.prewarm.empty_sdk == self.generation {
            return Ok(Some(Rc::from([])));
        }
        self.prewarm.inventory.start(root, environment, false);
        let Some(catalog) = self.prewarm.inventory.ready() else {
            if self.prewarm.inventory.is_loading() {
                if let Some(trace) = trace {
                    trace.sdk("loading", None);
                }
                return Ok(None);
            }
            return Ok(Some(Rc::from([])));
        };
        if let Some(root) = self.prewarm.inventory.root() {
            workspace.share_sdk_root(&root);
            if let Some(graph_root) = workspace.sdk_root()
                && path_key(&root) != path_key(graph_root)
                && root.canonicalize().ok().map(|root| path_key(&root))
                    != graph_root.canonicalize().ok().map(|root| path_key(&root))
            {
                if let Some(trace) = trace {
                    trace.sdk("different graph SDK", Some(catalog.stats.clone()));
                }
                return Ok(Some(Rc::from([])));
            }
        }
        let cached = self
            .sdk
            .as_ref()
            .filter(|(old, _)| Arc::ptr_eq(old, &catalog));
        if let Some(request) = &mut indexes.completion {
            request.observe_completion_sdk(catalog.watch.as_ref());
        }
        if let Some(trace) = trace {
            let mut stats = catalog.stats.clone();
            stats.reused = cached.is_some();
            trace.sdk(
                if stats.method == "unavailable" {
                    "unavailable"
                } else {
                    "ready"
                },
                Some(stats),
            );
        }
        if let Some((_, entries)) = cached {
            return Ok(Some(Rc::clone(entries)));
        }
        if let Some((previous, _)) = &self.sdk
            && let Some(root) = previous.root_key()
        {
            indexes.disk_names.retain(|path, _| !path.starts_with(root));
            if !previous.same_sdk(&catalog) {
                indexes.disk_index.retain(|path, _| !path.starts_with(root));
            }
        }
        for (path, names) in catalog.names.iter() {
            check_optional_cancelled(cancelled)?;
            indexes.disk_names.insert(path.clone(), names.clone());
        }
        self.sdk_exports.clear();
        self.sdk_matches.clear();
        let mut entries = Vec::with_capacity(catalog.packages.len());
        for package in catalog.packages.iter() {
            check_optional_cancelled(cancelled)?;
            let mut files = Vec::with_capacity(package.files.len());
            for file in package.files.iter() {
                check_optional_cancelled(cancelled)?;
                files.push(ImportFile::new(file));
            }
            entries.push(Rc::new(Importable {
                path: package.path.clone(),
                name: package.name.clone(),
                directory: path_key(&package.dir),
                vendor_parent: None,
                prefilter_disk: true,
                project: false,
                package_id: None,
                files: files.into(),
            }));
        }
        let entries: Rc<[_]> = entries.into();
        self.sdk = Some((catalog, Rc::clone(&entries)));
        Ok(Some(entries))
    }

    fn exports(
        &mut self,
        indexes: &mut IndexReader<'_>,
        file: &Path,
        key: &Path,
        entry: &Importable,
        sdk_generation: Option<u64>,
    ) -> Result<(Option<SourceRev>, Rc<[Export]>)> {
        let cancelled = indexes.cancelled;
        check_optional_cancelled(cancelled)?;
        if let Some(overlay) = indexes.documents.get(key) {
            return Ok((
                SourceRev::overlay(overlay),
                sdk_exports(
                    overlay.declarations(),
                    entry,
                    cancelled,
                    |name, symbol, kind| describe(overlay, symbol.range, name, kind, true),
                )?,
            ));
        }
        if !indexes.has_overlay(key)
            && let Some(cached) = self.sdk_exports.get_mut(key)
        {
            let observed = watched_sdk_export(indexes, key, &cached.text, sdk_generation);
            // A verified catalogue-origin export can reuse the same watched
            // contents as the name prefilter. The request polls the watch
            // again before handing any candidate back.
            let trusted = observed.is_some() && observed == cached.sdk_generation;
            if trusted || cached.text.is_current(file, indexes.shared_disk) {
                cached.sdk_generation = observed;
                let revision = cached
                    .text
                    .read_ticket()
                    .map(|ticket| SourceRev::Disk(ticket.source_revision()));
                return Ok((revision, Rc::clone(&cached.exports)));
            }
        }
        let source = match indexes.declaration_source(file) {
            Ok(Some(source)) => source,
            Err(error) if error.is::<SharedLoadPending>() => return Err(error),
            _ => {
                check_optional_cancelled(cancelled)?;
                return Ok((None, Rc::from([])));
            }
        };
        let exports = sdk_exports(&source.index, entry, cancelled, |_, symbol, kind| {
            source.presentation[&symbol.range].description(kind, true)
        })?;
        if let Some(text) = indexes.verified_declaration_text(key) {
            let sdk_generation = watched_sdk_export(indexes, key, &text, sdk_generation);
            self.sdk_exports.insert(
                key.to_owned(),
                CachedSdkExports {
                    text,
                    exports: Rc::clone(&exports),
                    sdk_generation,
                },
            );
        }
        Ok((indexes.completion_revision(file), exports))
    }

    pub(in crate::semantic) fn source_changed(&mut self, key: &Path) {
        self.sdk_exports.remove(key);
        if self
            .sdk
            .as_ref()
            .and_then(|(catalog, _)| catalog.root_key())
            .is_some_and(|root| key.starts_with(root))
        {
            self.sdk_matches.clear();
        }
    }

    fn matching_sdk_files(
        &mut self,
        entry: &Importable,
        indexes: &IndexReader<'_>,
        matcher: &SymbolMatcher<'_>,
        generation: u64,
    ) -> Result<(Rc<[ImportFile]>, bool)> {
        if let Some(cached) = self.sdk_matches.get(&entry.path)
            && cached.generation == generation
            && Rc::ptr_eq(&cached.source_files, &entry.files)
            && &cached.matcher == matcher
        {
            return Ok((Rc::clone(&cached.files), cached.names_validated));
        }
        let mut matching = Vec::new();
        let mut names_validated = true;
        for file in entry.files.iter() {
            check_optional_cancelled(indexes.cancelled)?;
            match indexes
                .disk_names
                .get(&file.key)
                .filter(|names| names.sdk_generation == Some(generation))
            {
                Some(names) if names.matches(matcher, indexes.cancelled) => {
                    matching.push(file.clone())
                }
                Some(_) => {}
                None => {
                    names_validated = false;
                    matching.push(file.clone());
                }
            }
        }
        let files = if matching.len() == entry.files.len() {
            Rc::clone(&entry.files)
        } else {
            Rc::from(matching)
        };
        self.sdk_matches.insert(
            entry.path.clone(),
            SdkMatches {
                matcher: matcher.owned(),
                generation,
                source_files: Rc::clone(&entry.files),
                files: Rc::clone(&files),
                names_validated,
            },
        );
        Ok((files, names_validated))
    }

    #[cfg(test)]
    pub(super) fn use_empty_sdk(&mut self, generation: u64, workspace: &Workspace) {
        self.prepare(generation, workspace, None).unwrap();
        self.prewarm.empty_sdk = Some(generation);
    }

    #[cfg(test)]
    pub(super) fn sdk_files(&self) -> impl Iterator<Item = &Path> {
        self.sdk_exports.keys().map(PathBuf::as_path)
    }
}

fn watched_sdk_export(
    indexes: &IndexReader<'_>,
    key: &Path,
    text: &CachedNavigationText,
    sdk_generation: Option<u64>,
) -> Option<u64> {
    let generation = sdk_generation?;
    let names = indexes.disk_names.get(key)?;
    let ticket = text.read_ticket()?;
    (names.sdk_generation == Some(generation)
        && names.fingerprint == ticket.fingerprint()
        && names.matches_observation(Some(ticket.observation())))
    .then_some(generation)
}

impl Importable {
    fn matches_selected_files(&self, workspace: &Workspace, package: &Package) -> bool {
        let (directory, name) = package.directory_and_name();
        self.name == name
            && self.directory == directory
            && self.prefilter_disk == !package.load_error
            && self.project
                == (workspace.is_project_package(package) || self.vendor_parent.is_some())
            && self
                .files
                .iter()
                .map(|file| &file.path)
                .eq(package.files.iter().filter(|file| !is_test_source(file)))
    }

    fn from_graph(
        workspace: &Workspace,
        package: &Package,
        cancelled: Option<&AtomicBool>,
    ) -> Result<Option<Self>> {
        let (directory, name) = package.directory_and_name();
        let vendor = directory
            .ancestors()
            .find(|part| part.file_name().is_some_and(|name| name == "vendor"));
        let path = package.import_path();
        let path = path
            .rsplit_once("/vendor/")
            .map_or(path, |(_, suffix)| suffix);
        let path = path.strip_prefix("vendor/").unwrap_or(path);
        if !valid_package(path, name) {
            return Ok(None);
        }
        let mut files = Vec::new();
        for file in &package.files {
            check_optional_cancelled(cancelled)?;
            if !is_test_source(file) {
                files.push(ImportFile::new(file));
            }
        }
        Ok(Some(Self {
            path: path.into(),
            name: name.into(),
            directory: directory.to_owned(),
            vendor_parent: vendor.and_then(Path::parent).map(Path::to_owned),
            prefilter_disk: !package.load_error,
            project: workspace.is_project_package(package) || vendor.is_some(),
            package_id: Some(package.id.clone()),
            files: files.into(),
        }))
    }

    fn weight(&self, workspace: &Workspace, caller: Option<&Package>, weight: &mut Weight) {
        if let Some(id) = &self.package_id {
            if let Some(package) = workspace.package(id) {
                package_weight(workspace, caller, package, weight);
            }
        } else {
            weight.vgo = 1;
        }
    }
}

pub(super) fn package_weight(
    workspace: &Workspace,
    caller: Option<&Package>,
    target: &Package,
    weight: &mut Weight,
) {
    weight.usage = workspace
        .direct_importers(target)
        .filter(|package| package.is_production() && workspace.is_project_package(package))
        .map(|package| &package.id)
        .collect::<HashSet<_>>()
        .len();
    weight.vgo = if target.module_path().is_some()
        && caller.and_then(Package::module_path) == target.module_path()
    {
        3
    } else if workspace.is_project_package(target) || target.is_direct_dependency() {
        2
    } else if target.is_standard() {
        1
    } else {
        0
    };
}

pub(super) fn valid_package(path: &str, name: &str) -> bool {
    !matches!(name, "" | "_" | "main" | "documentation")
        && path != "builtin"
        && path.split('/').all(|part| {
            !part.is_empty()
                && !matches!(part, "testdata" | "vendor")
                && !part.starts_with(['_', '.'])
        })
}

fn can_import(
    entry: &Importable,
    caller: Option<&Package>,
    directory: &Path,
    test_package: Option<&str>,
) -> bool {
    let external_test = entry.directory == directory
        && test_package.and_then(|name| name.strip_suffix("_test")) == Some(entry.name.as_str());
    if !external_test
        && (entry.directory == directory
            || caller.is_some_and(|package| package.import_path() == entry.path))
    {
        return false;
    }
    if entry
        .vendor_parent
        .as_ref()
        .is_some_and(|parent| !directory.starts_with(parent))
    {
        return false;
    }
    let Some(internal) = entry
        .path
        .split('/')
        .enumerate()
        .filter_map(|(index, part)| (part == "internal").then_some(index))
        .last()
    else {
        return true;
    };
    let parent = entry
        .path
        .split('/')
        .take(internal)
        .collect::<Vec<_>>()
        .join("/");
    if !parent.is_empty()
        && caller.is_some_and(|package| {
            let path = package.import_path();
            path == parent
                || path
                    .strip_prefix(&parent)
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    {
        return true;
    }
    entry
        .directory
        .ancestors()
        .find(|path| path.file_name().is_some_and(|name| name == "internal"))
        .and_then(Path::parent)
        .is_some_and(|parent| directory.starts_with(parent))
}

pub(super) fn assumed_package_name(path: &str) -> String {
    let mut parts = path.rsplit('/');
    let tail = parts.next().unwrap_or("");
    let tail = if tail.strip_prefix('v').is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    }) {
        parts.next().unwrap_or(tail)
    } else {
        tail
    };
    let name = tail.strip_prefix("go-").unwrap_or(tail);
    let mut clean: String = name
        .chars()
        .map(|ch| if identifier_char(ch) { ch } else { '_' })
        .collect();
    if clean.is_empty() || clean == "_" {
        return "pkg".into();
    }
    if clean.chars().next().is_some_and(|ch| !identifier_start(ch)) {
        clean.insert(0, '_');
    }
    clean
}

pub(super) struct Search {
    kind: SearchKind,
    caller: Option<String>,
    graph_prepared: bool,
    sdk_prepared: bool,
    sdk_match_generation: Option<u64>,
    seen_paths: HashSet<String>,
    queue: VecDeque<Rc<Importable>>,
    jobs: Vec<Job>,
    files: VecDeque<FileWork>,
    scans: Vec<Input>,
    pending: Option<Batch>,
    prefilter_time: Option<Phase>,
    waiting_sdk: bool,
    waiting_scanner: bool,
    checked_shadows: HashMap<String, bool>,
}

struct FileWork {
    job: usize,
    index: usize,
    skip: bool,
}

struct Job {
    entry: Rc<Importable>,
    files: Rc<[ImportFile]>,
    next_file: usize,
    names_validated: bool,
    names: HashSet<String>,
    binding: Binding,
    weight: Weight,
}

#[derive(PartialEq, Eq)]
pub(super) struct SymbolMatcher<'a> {
    prefix: Cow<'a, str>,
    qualified_prefix: Option<Cow<'a, str>>,
    allow_substring: bool,
    qualified_member: bool,
    pub(super) types_only: bool,
}

impl SymbolMatcher<'_> {
    #[cfg(test)]
    pub(super) fn for_test(prefix: &'static str) -> SymbolMatcher<'static> {
        SymbolMatcher {
            prefix: Cow::Borrowed(prefix),
            qualified_prefix: None,
            allow_substring: false,
            qualified_member: false,
            types_only: false,
        }
    }

    pub(super) fn match_symbol(&self, name: &str) -> Option<Match> {
        let prefix = self.prefix.as_ref();
        if self.allow_substring {
            return match_name(prefix, name);
        }
        if self.qualified_member {
            return match_member_hump(prefix, name);
        }
        let member = match_start(prefix, name);
        let qualified = self.qualified_prefix.as_deref().and_then(|qualifier| {
            if qualifier.starts_with(prefix) {
                Some(Match::Prefix)
            } else if prefix
                .as_bytes()
                .first()
                .zip(qualifier.as_bytes().first())
                .is_some_and(|(left, right)| {
                    left.is_ascii() && right.is_ascii() && !left.eq_ignore_ascii_case(right)
                })
            {
                // Prefix and anchored camel-hump matching both require the
                // same initial character. Most qualifiers can skip allocation.
                None
            } else {
                match_start(prefix, &format!("{qualifier}.{name}"))
            }
        });
        match (member, qualified) {
            (Some(a), Some(b)) => Some(a.min(b)),
            _ => member.or(qualified),
        }
    }

    fn owned(&self) -> SymbolMatcher<'static> {
        SymbolMatcher {
            prefix: Cow::Owned(self.prefix.to_string()),
            qualified_prefix: self
                .qualified_prefix
                .as_ref()
                .map(|qualifier| Cow::Owned(qualifier.to_string())),
            allow_substring: self.allow_substring,
            qualified_member: self.qualified_member,
            types_only: self.types_only,
        }
    }
}

enum Binding {
    Dot,
    Existing(String),
    New {
        qualifier: String,
        import: PendingImport,
    },
}

impl Binding {
    fn qualifier(&self) -> Option<&str> {
        match self {
            Self::Dot => None,
            Self::Existing(qualifier) | Self::New { qualifier, .. } => Some(qualifier),
        }
    }
}

impl Search {
    pub(super) fn new(kind: SearchKind, caller: Option<&Package>) -> Self {
        Self {
            kind,
            caller: caller.map(|package| package.id.clone()),
            graph_prepared: false,
            sdk_prepared: false,
            sdk_match_generation: None,
            seen_paths: HashSet::new(),
            queue: VecDeque::new(),
            jobs: Vec::new(),
            files: VecDeque::new(),
            scans: Vec::new(),
            pending: None,
            prefilter_time: None,
            waiting_sdk: false,
            waiting_scanner: false,
            checked_shadows: HashMap::new(),
        }
    }

    pub(super) fn is_ready(&self, cache: &ImportCache) -> bool {
        self.pending.as_ref().is_none_or(Batch::is_ready)
            && (!self.waiting_sdk || !cache.prewarm.inventory.is_loading())
            && (!self.waiting_scanner || !cache.scanner.busy())
    }

    pub(super) fn advance(
        &mut self,
        cache: &mut ImportCache,
        snapshot: &mut EngineSnapshot<'_>,
        document: &DocumentSnapshot<'_>,
        generation: u64,
        prefix: &str,
        candidates: &mut Vec<Candidate>,
        caps: &CompletionClientCapabilities,
        request: &mut CompletionRead,
    ) -> Result<bool> {
        let cancelled = snapshot.completion_cancelled();
        check_optional_cancelled(cancelled)?;
        let workspace = snapshot
            .workspace()
            .expect("import search requires an active graph");
        let caller = self.caller.as_deref().and_then(|id| workspace.package(id));
        let root = snapshot.root();
        let environment = snapshot.environment();
        let mut resolver = snapshot.resolver_with_completion(workspace, Some(request));
        {
            let trace = resolver.indexes.trace().cloned();
            let _phase = trace.as_ref().map(|trace| trace.phase("import_catalog"));
            cache.prepare(generation, workspace, cancelled)?;
            if !self.graph_prepared {
                self.enqueue(&cache.graph, workspace, caller, document, cancelled)?;
                self.graph_prepared = true;
            }
            if !self.sdk_prepared
                && let Some(sdk) = cache.sdk(
                    workspace,
                    root,
                    environment,
                    trace.as_ref(),
                    &mut resolver.indexes,
                )?
            {
                self.enqueue(&sdk, workspace, caller, document, cancelled)?;
                if matches!(self.kind, SearchKind::Symbols(_))
                    && let Some(root) = cache
                        .sdk
                        .as_ref()
                        .and_then(|(catalog, _)| catalog.root_key())
                    && !resolver
                        .indexes
                        .documents
                        .keys()
                        .chain(
                            resolver
                                .indexes
                                .pending_documents
                                .into_iter()
                                .flat_map(|docs| docs.keys()),
                        )
                        .any(|key| key.starts_with(root))
                {
                    self.sdk_match_generation = resolver
                        .indexes
                        .completion
                        .as_mut()
                        .and_then(|request| request.trust_completion_sdk_names());
                }
                self.sdk_prepared = true;
            }
        }
        if let SearchKind::Path { close } = self.kind {
            for _ in 0..BATCH_SIZE {
                check_optional_cancelled(cancelled)?;
                let Some(entry) = self.queue.pop_front() else {
                    break;
                };
                let mut weight = Weight::default();
                entry.weight(workspace, caller, &mut weight);
                candidates.extend(path_candidate(&entry, caller, prefix, close, weight));
            }
            self.waiting_sdk = self.queue.is_empty() && !self.sdk_prepared;
            return Ok(self.queue.is_empty() && self.sdk_prepared);
        }
        if self.files.is_empty() {
            debug_assert!(self.pending.is_none());
            let _phase = resolver
                .indexes
                .trace()
                .map(|trace| trace.phase("plan_batch"));
            self.plan_batch(
                cache,
                &mut resolver,
                caller,
                is_test_source(document.path()),
                prefix,
            )?;
            if self.files.is_empty() {
                self.waiting_sdk = !self.sdk_prepared;
                return Ok(self.sdk_prepared);
            }
        }
        self.waiting_sdk = false;
        if !self.scans.is_empty() {
            self.waiting_scanner = cache.scanner.busy();
            if self.waiting_scanner {
                return Ok(false);
            }
            self.prefilter_time = resolver
                .indexes
                .trace()
                .map(|trace| trace.phase("prefilter_batch"));
            self.pending = cache.scanner.submit(
                std::mem::take(&mut self.scans),
                Arc::clone(resolver.indexes.shared_disk),
            );
        }
        if let Some(batch) = &self.pending {
            if !batch.is_ready() {
                return Ok(false);
            }
            let batch = self.pending.take().unwrap();
            self.prefilter_time.take();
            for result in batch.results() {
                check_optional_cancelled(cancelled)?;
                if let Some(trace) = resolver.indexes.trace()
                    && result.read
                {
                    trace.read(&result.key, true, result.project, false);
                }
                if let Some(names) = result.names {
                    resolver
                        .indexes
                        .disk_names
                        .insert(result.key.clone(), names);
                }
                match result.decision {
                    Decision::Text(text) => {
                        resolver
                            .indexes
                            .completion
                            .as_mut()
                            .unwrap()
                            .prefetched
                            .insert(result.key, text);
                    }
                    Decision::Skip => {
                        for file in &mut self.files {
                            file.skip |= self.jobs[file.job].files[file.index].key == result.key;
                        }
                    }
                    Decision::Deferred => {}
                }
            }
        }
        while let Some(file) = self.files.pop_front() {
            check_optional_cancelled(cancelled)?;
            if !file.skip {
                self.consume_file(
                    cache,
                    &mut resolver,
                    document,
                    caller,
                    prefix,
                    candidates,
                    caps,
                    &file,
                )?;
            }
        }
        if self.files.is_empty() {
            resolver
                .indexes
                .completion
                .as_mut()
                .unwrap()
                .prefetched
                .clear();
            self.pending.take();
        }
        Ok(false)
    }

    fn plan_batch<'w>(
        &mut self,
        cache: &mut ImportCache,
        resolver: &mut Resolver<'w, '_>,
        caller: Option<&'w Package>,
        include_tests: bool,
        prefix: &str,
    ) -> Result<()> {
        let SearchKind::Symbols(query) = &self.kind else {
            unreachable!();
        };
        let cancelled = resolver.indexes.cancelled;
        let mut scanning = HashSet::new();
        while self.files.len() < BATCH_SIZE {
            resolver.check_cancelled()?;
            let Some(job) = self
                .jobs
                .last_mut()
                .filter(|job| job.next_file < job.files.len())
            else {
                let Some(entry) = self.queue.pop_front() else {
                    break;
                };
                let binding = query.binding(&entry, |name| {
                    resolver.check_cancelled()?;
                    if query.declared.contains(name) {
                        return Ok(true);
                    }
                    let Some(caller) =
                        caller.filter(|_| query.skipped_package_names.contains(name))
                    else {
                        return Ok(false);
                    };
                    if let Some(&shadowed) = self.checked_shadows.get(name) {
                        return Ok(shadowed);
                    }
                    // A skipped declaration only matters when its name
                    // would be used as an import qualifier. Verify it here.
                    let shadowed = resolver
                        .find_symbol(caller, name, None)?
                        .is_some_and(|decl| include_tests || !is_test_source(&decl.file));
                    self.checked_shadows.insert(name.to_owned(), shadowed);
                    Ok(shadowed)
                })?;
                let Some(binding) = binding else {
                    continue;
                };
                if entry.package_id.is_some() && caller.is_none() {
                    continue;
                }
                let drafts = entry
                    .package_id
                    .as_deref()
                    .and_then(|id| resolver.workspace.package(id))
                    .and_then(|package| resolver.indexes.drafts.get(&package.directory_and_name()));
                let (files, names_validated) = if let Some(generation) = self.sdk_match_generation
                    && entry.prefilter_disk
                    && drafts.is_none()
                    && (entry.package_id.is_none()
                        || cache
                            .sdk
                            .as_ref()
                            .and_then(|(catalog, _)| catalog.root_key())
                            .is_some_and(|root| entry.directory.starts_with(root)))
                {
                    let matcher = query.matcher(&binding, prefix);
                    cache.matching_sdk_files(&entry, &resolver.indexes, &matcher, generation)?
                } else {
                    (
                        drafts.map_or_else(
                            || Rc::clone(&entry.files),
                            |drafts| {
                                entry
                                    .files
                                    .iter()
                                    .cloned()
                                    .chain(
                                        drafts
                                            .iter()
                                            .filter(|file| !is_test_source(file))
                                            .map(|file| ImportFile::new(file)),
                                    )
                                    .collect()
                            },
                        ),
                        false,
                    )
                };
                if files.is_empty() {
                    continue;
                }
                let mut weight = Weight::default();
                entry.weight(resolver.workspace, caller, &mut weight);
                self.jobs.push(Job {
                    entry,
                    binding,
                    weight,
                    files,
                    next_file: 0,
                    names_validated,
                    names: HashSet::new(),
                });
                continue;
            };
            let index = job.next_file;
            job.next_file += 1;
            let ImportFile { path, key } = &job.files[index];
            if job.entry.prefilter_disk && !resolver.indexes.has_overlay(key) {
                let matcher = if job.names_validated {
                    None
                } else {
                    let matcher = query.matcher(&job.binding, prefix);
                    if let Some(names) = resolver.indexes.cached_completion_names(path, key) {
                        if !names.matches(&matcher, cancelled) {
                            continue;
                        }
                        None
                    } else {
                        Some(matcher)
                    }
                };
                if !resolver.indexes.disk_index.contains_key(key)
                    && !cache.sdk_exports.contains_key(key)
                    && scanning.insert(key.clone())
                {
                    self.scans.push(Input {
                        key: key.clone(),
                        path: path.clone(),
                        project: job.entry.project,
                        matcher: matcher.map(|matcher| matcher.owned()),
                    });
                }
            }
            self.files.push_back(FileWork {
                job: self.jobs.len() - 1,
                index,
                skip: false,
            });
        }
        Ok(())
    }

    fn consume_file<'w>(
        &mut self,
        cache: &mut ImportCache,
        resolver: &mut Resolver<'w, '_>,
        document: &DocumentSnapshot<'_>,
        caller: Option<&'w Package>,
        prefix: &str,
        candidates: &mut Vec<Candidate>,
        caps: &CompletionClientCapabilities,
        file: &FileWork,
    ) -> Result<()> {
        let cancelled = resolver.indexes.cancelled;
        let _phase = resolver
            .indexes
            .trace()
            .map(|trace| trace.phase("candidate_expansion"));
        #[cfg(test)]
        if let Some(trace) = resolver.indexes.trace() {
            trace.checkpoint("expand_file", cancelled);
        }
        check_optional_cancelled(cancelled)?;
        let SearchKind::Symbols(query) = &self.kind else {
            unreachable!();
        };
        let job = &mut self.jobs[file.job];
        let file = &job.files[file.index];
        let matcher = query.matcher(&job.binding, prefix);
        match &job.entry.package_id {
            Some(id) => {
                let Some((caller, target)) = caller.zip(resolver.workspace.package(id)) else {
                    return Ok(());
                };
                let mut symbols = Vec::new();
                let index = match resolver.indexes.file_index(&file.path) {
                    Ok(index) => Some(index),
                    Err(error) if error.is::<SharedLoadPending>() => return Err(error),
                    Err(_) => None,
                };
                if let Some(index) = index {
                    for (name, symbol) in index.symbols.declarations() {
                        check_optional_cancelled(cancelled)?;
                        if is_exported(name)
                            && allows_symbol(query, &symbol.kind)
                            && let Some(matched) = matcher.match_symbol(name)
                        {
                            symbols.push((name.to_owned(), symbol.clone(), matched));
                        }
                    }
                }
                check_optional_cancelled(cancelled)?;
                let completion_revision = resolver.indexes.completion_revision(&file.path);
                for (name, symbol, matched) in symbols {
                    check_optional_cancelled(cancelled)?;
                    if !job.names.insert(name.clone())
                        || matches!(job.binding, Binding::Dot) && query.shadows_dot_name(&name)
                    {
                        continue;
                    }
                    let resolved = ResolvedSymbol {
                        package: target,
                        file: file.path.clone(),
                        symbol,
                        receiver: None,
                        method_receiver: None,
                        completion_revision: completion_revision.clone(),
                    };
                    let candidate = resolved_candidate(
                        resolver,
                        caller,
                        document,
                        &name,
                        &resolved,
                        matched,
                        query.expected.as_ref(),
                        query.literal,
                        caps,
                    )?;
                    candidates.push(job.decorate(query, prefix, candidate));
                }
            }
            None => {
                let (revision, exports) = cache.exports(
                    &mut resolver.indexes,
                    &file.path,
                    &file.key,
                    &job.entry,
                    self.sdk_match_generation,
                )?;
                for export in exports.iter() {
                    check_optional_cancelled(cancelled)?;
                    if !export.allowed(query)
                        || matches!(job.binding, Binding::Dot)
                            && query.shadows_dot_name(&export.name)
                    {
                        continue;
                    }
                    let Some(matched) = matcher.match_symbol(&export.name) else {
                        continue;
                    };
                    if job.names.insert(export.name.clone()) {
                        candidates.push(job.decorate(
                            query,
                            prefix,
                            export.candidate(&file.path, revision.clone(), query, matched),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn enqueue(
        &mut self,
        entries: &[Rc<Importable>],
        workspace: &Workspace,
        caller: Option<&Package>,
        document: &DocumentSnapshot<'_>,
        cancelled: Option<&AtomicBool>,
    ) -> Result<()> {
        let mut visible = BTreeMap::<&str, &Rc<Importable>>::new();
        let directory = path_key(document.path().parent().unwrap_or_else(|| Path::new("")));
        let test = is_test_source(document.path())
            .then(|| package_name(document.indexed()))
            .flatten();
        for entry in entries {
            check_optional_cancelled(cancelled)?;
            if !can_import(entry, caller, &directory, test) {
                continue;
            }
            if let (Some(caller), Some(id)) = (caller, &entry.package_id)
                && !workspace.matches_import_variant(caller, &entry.path, id)
            {
                continue;
            }
            if matches!(&self.kind, SearchKind::Symbols(query) if !query.matches_package(entry)) {
                continue;
            }
            match visible.entry(entry.path.as_str()) {
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert(entry);
                }
                std::collections::btree_map::Entry::Occupied(mut slot) => {
                    let depth = |package: &Importable| {
                        package
                            .vendor_parent
                            .as_ref()
                            .map_or(0, |path| path.components().count())
                    };
                    if depth(entry) > depth(slot.get()) {
                        slot.insert(entry);
                    }
                }
            }
        }
        for entry in visible.into_values() {
            check_optional_cancelled(cancelled)?;
            if self.seen_paths.insert(entry.path.clone()) {
                self.queue.push_back(entry.clone());
            }
        }
        Ok(())
    }
}

impl SymbolSearch {
    fn matcher<'a>(&self, binding: &'a Binding, prefix: &'a str) -> SymbolMatcher<'a> {
        let qualified_prefix = binding.qualifier().filter(|qualifier| {
            matches!(self.selection, Selection::Name { .. })
                && prefix
                    .chars()
                    .next()
                    .zip(qualifier.chars().next())
                    .is_some_and(|(left, right)| left.to_lowercase().eq(right.to_lowercase()))
        });
        SymbolMatcher {
            prefix: Cow::Borrowed(prefix),
            qualified_prefix: qualified_prefix.map(Cow::Borrowed),
            allow_substring: matches!(binding, Binding::Dot)
                || matches!(
                    (&self.selection, binding),
                    (Selection::Qualifier { .. }, Binding::Existing(_))
                ),
            qualified_member: matches!(self.selection, Selection::Qualifier { .. }),
            types_only: self.types_only || self.address_of,
        }
    }

    fn shadows_dot_name(&self, name: &str) -> bool {
        self.declared.contains(name) || self.imports.iter().any(|import| import.qualifier == name)
    }

    fn matches_package(&self, entry: &Importable) -> bool {
        match &self.selection {
            Selection::Name { auto_import } => {
                *auto_import
                    || self.imports.iter().any(|import| {
                        import.path == entry.path && import.qualifier == "." && !import.in_graph
                    })
            }
            Selection::Qualifier { name, .. } => {
                let existing = self
                    .imports
                    .iter()
                    .find(|import| &import.qualifier == name && !self.declared.contains(name));
                existing.map_or_else(
                    || {
                        entry.name.starts_with(name)
                            || assumed_package_name(&entry.path).starts_with(name)
                    },
                    |import| entry.path == import.path,
                )
            }
        }
    }

    fn binding(
        &self,
        entry: &Importable,
        mut is_declared: impl FnMut(&str) -> Result<bool>,
    ) -> Result<Option<Binding>> {
        let imports = || {
            self.imports
                .iter()
                .filter(|import| import.path == entry.path)
        };
        if let Some(dot) = imports().find(|import| import.qualifier == ".") {
            return Ok(
                (matches!(self.selection, Selection::Name { .. }) && !dot.in_graph)
                    .then_some(Binding::Dot),
            );
        }
        if let Some(import) = imports().find(|import| import.qualifier != "_") {
            if is_declared(&import.qualifier)? {
                return Ok(None);
            }
            return Ok(Some(Binding::Existing(import.qualifier.clone())));
        }
        let mut clean = |qualifier: &str| -> Result<bool> {
            Ok(qualifier != "init"
                && self
                    .imports
                    .iter()
                    .all(|import| import.qualifier != qualifier)
                && !is_declared(qualifier)?)
        };
        let mut qualifier = entry.name.clone();
        let mut suffix = 2;
        while !clean(&qualifier)? {
            qualifier = format!("{}{suffix}", entry.name);
            suffix += 1;
        }
        let alias = (qualifier != entry.name || qualifier != assumed_package_name(&entry.path))
            .then(|| qualifier.clone());
        Ok(Some(Binding::New {
            qualifier,
            import: PendingImport {
                path: entry.path.clone(),
                alias,
            },
        }))
    }
}

impl Job {
    fn decorate(&self, query: &SymbolSearch, prefix: &str, mut candidate: Candidate) -> Candidate {
        candidate.import_path = Some(self.entry.path.clone());
        candidate.weight.usage = self.weight.usage;
        candidate.weight.vgo = self.weight.vgo;
        let qualifier = match &self.binding {
            Binding::Dot => return candidate,
            Binding::Existing(qualifier) => qualifier,
            Binding::New { qualifier, import } => {
                candidate.weight.priority += AUTO_IMPORT - SYMBOL;
                candidate.pending_import = Some(import.clone());
                qualifier
            }
        };
        candidate.qualified = true;
        match &query.selection {
            Selection::Name { .. } => {
                candidate.qualifier = Some(qualifier.clone());
                if match_start(prefix, &candidate.label).is_none() {
                    candidate.filter = Some(format!("{qualifier}.{}", candidate.label));
                }
            }
            Selection::Qualifier { name, start }
                if matches!(self.binding, Binding::New { .. }) || name != qualifier =>
            {
                // The typed qualifier is part of the edit and therefore
                // also part of the prefix the LSP client filters on.
                candidate.qualifier = Some(qualifier.clone());
                candidate.replace_start = Some(*start);
                candidate.filter = Some(format!("{name}.{}", candidate.label));
            }
            Selection::Qualifier { .. } => {}
        }
        candidate
    }
}

fn allows_symbol(query: &SymbolSearch, kind: &SymbolKind) -> bool {
    !(query.types_only || query.address_of) || matches!(kind, SymbolKind::Type)
}

fn path_candidate(
    entry: &Importable,
    caller: Option<&Package>,
    prefix: &str,
    close: Option<char>,
    mut weight: Weight,
) -> Option<Candidate> {
    if prefix.starts_with("./") || prefix.starts_with("../") {
        return None;
    }
    let (head, tail) = entry.path.rsplit_once('/').unwrap_or(("", &entry.path));
    let full = match_start(prefix, &entry.path);
    let last = match prefix.rsplit_once('/') {
        Some((directory, segment)) if directory == head => {
            match_start(segment, tail).map(|matched| (matched, entry.path.clone()))
        }
        Some((directory, segment)) if !segment.is_empty() && !directory.contains('/') => head
            .split('/')
            .any(|part| part.ends_with(directory))
            .then(|| match_start(segment, tail))
            .flatten()
            .map(|_| (Match::CamelHump, entry.path.clone())),
        Some(_) => None,
        None => match_start(prefix, tail).map(|matched| (matched, tail.to_owned())),
    };
    let (matched, filter) = match (full, last) {
        (Some(full), Some((segment, filter))) if segment < full => (segment, filter),
        (Some(full), _) => (full, entry.path.clone()),
        (None, Some(segment)) => segment,
        (None, None) => return None,
    };
    weight.priority = package_priority(caller.map_or("", Package::import_path), &entry.path);
    let mut candidate = Candidate::new(&entry.path, Kind::MODULE, matched, weight.priority);
    candidate.weight = weight;
    candidate.filter = Some(filter);
    candidate.import_path = Some(entry.path.clone());
    candidate.insert = Insert::ImportPath { close };
    Some(candidate)
}

struct Export {
    name: String,
    range: Range,
    description: Description,
    is_type: bool,
    braces: bool,
    parameters: Option<bool>,
    value: Option<String>,
}

impl Export {
    fn allowed(&self, query: &SymbolSearch) -> bool {
        !(query.types_only || query.address_of) || self.is_type
    }

    fn candidate(
        &self,
        file: &Path,
        revision: Option<SourceRev>,
        query: &SymbolSearch,
        matched: Match,
    ) -> Candidate {
        let mut candidate = Candidate::new(&self.name, self.description.kind, matched, SYMBOL);
        candidate.detail = self.description.detail.clone();
        candidate.label_suffix = self.description.label_suffix.clone();
        candidate.deprecated = self.description.deprecated;
        candidate.target = Some(Target {
            file: file.to_owned(),
            range: self.range,
            revision,
        });
        if let Some(has_parameters) = self.parameters {
            candidate.insert = Insert::Call { has_parameters };
        } else if self.braces && query.literal {
            candidate.insert = Insert::Braces;
        }
        if let Some((expected, value)) = query.expected.as_ref().zip(self.value.as_deref()) {
            candidate.weight.priority += expected.keys_weight(value, Some(value));
        }
        candidate
    }
}

fn sdk_exports(
    index: &FileIndex,
    entry: &Importable,
    cancelled: Option<&AtomicBool>,
    mut describe: impl FnMut(&str, &Symbol, Kind) -> Description,
) -> Result<Rc<[Export]>> {
    if index.package_name.as_deref() != Some(entry.name.as_str()) {
        return Ok(Rc::from([]));
    }
    let declarations = &index.symbols;
    let mut exports = Vec::new();
    for (name, symbol) in declarations.declarations() {
        check_optional_cancelled(cancelled)?;
        if !is_exported(name) {
            continue;
        }
        let mut braces = false;
        let mut value = None;
        let kind = match &symbol.kind {
            SymbolKind::Type => {
                value = Some(named_key(&entry.path, name));
                if let Some(details) = declarations.type_details(name) {
                    let base = details.alias.as_ref().or(details.underlying.as_ref());
                    braces = !details.interface && details.parameters.is_empty()
                        && base.is_none_or(|ty| ty.pointers == 0 && matches!(&ty.kind,
                            TypeRefKind::Container(container, _) if !matches!(container.as_ref(), Container::Channel { .. })
                        ));
                    if details.interface {
                        Kind::INTERFACE
                    } else if base.is_none() {
                        Kind::STRUCT
                    } else {
                        Kind::CLASS
                    }
                } else {
                    Kind::CLASS
                }
            }
            SymbolKind::Function(signature) => {
                if let [Some(ty)] = signature.results.as_ref() {
                    value = sdk_type_key(index, &entry.path, ty, cancelled);
                }
                Kind::FUNCTION
            }
            SymbolKind::Value(Some(ty)) => {
                value = sdk_type_key(index, &entry.path, ty, cancelled);
                Kind::VARIABLE
            }
            _ => Kind::VARIABLE,
        };
        check_optional_cancelled(cancelled)?;
        let description = describe(name, symbol, kind);
        let parameters =
            matches!(symbol.kind, SymbolKind::Function(_)).then_some(description.has_parameters);
        exports.push(Export {
            name: name.to_owned(),
            range: symbol.range,
            is_type: matches!(symbol.kind, SymbolKind::Type),
            description,
            parameters,
            braces,
            value,
        });
    }
    check_optional_cancelled(cancelled)?;
    Ok(exports.into())
}

fn sdk_type_key(
    index: &FileIndex,
    package: &str,
    ty: &TypeRef,
    cancelled: Option<&AtomicBool>,
) -> Option<String> {
    if cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
        return None;
    }
    let child = |ty| sdk_type_key(index, package, ty, cancelled);
    let key = match &ty.kind {
        TypeRefKind::Named(named) => {
            let owner = if let Some(qualifier) = &named.qualifier {
                &index
                    .imports
                    .iter()
                    .find(|import| {
                        import.alias.as_ref().map_or_else(
                            || assumed_package_name(&import.path) == *qualifier,
                            |alias| alias == qualifier,
                        )
                    })?
                    .path
            } else {
                package
            };
            let name = named_key(owner, &named.name);
            if named.arguments.is_empty() {
                name
            } else {
                let arguments = named
                    .arguments
                    .iter()
                    .map(|ty| ty.as_ref().and_then(child))
                    .collect::<Option<Vec<_>>>()?;
                format!("{name}[{}]", arguments.join(","))
            }
        }
        TypeRefKind::Container(container, _) => match container.as_ref() {
            Container::Slice(element) => format!("[]{}", child(element)?),
            Container::Array { element, length } => {
                format!(
                    "[{}]{}",
                    length.as_deref().unwrap_or("..."),
                    child(element)?
                )
            }
            Container::Map { key, value } => format!("map[{}]{}", child(key)?, child(value)?),
            Container::Channel {
                element,
                receive,
                send,
            } => format!("chan{receive}{send}:{}", child(element)?),
        },
        _ => return None,
    };
    Some(format!("{}{key}", "*".repeat(ty.pointers.into())))
}
