use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, Read};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use lsp_types::{CompletionItemKind, Range, TextDocumentContentChangeEvent};
use tree_sitter::Parser;

use super::completion::{
    InferredMemo, Trace,
    disk_observation::{DirectoryObservations, DirectoryWatch},
    prefilter::{BYTE_LIMIT, FileNames, SOURCE_LIMIT, Stamp},
    render::package_name,
    sdk_names::scan_function_bodies_cancellable,
};
use super::disk_selection::PackageSelection;
use super::navigation_trace::{NavigationProbe, record};
use super::open_overlay::PendingOpen;
use super::package_index::{
    self, DeclarationIdentity, PackageDeclFile, PackageDeclIndex, PackageDeclKey,
    PackageFileRevision, PackageLookup, PackageLookupFiles,
};
use super::references::{DependencySource, ReferenceDependencies};
use super::shared_disk::{
    DiskSourceRev, FileObservation, FileReadTicket, PreparedDeclarations, QueryReadMemo,
    SharedDeclarations, SharedDiskCache,
};
use super::source::{
    Import, Source, SourceRevision, TextEditDelta, check_cancelled, check_optional_cancelled,
    node_text,
};
use crate::symbols::{self, FileSymbols, TypeDetails};
use crate::workspace::{Package, Workspace, path_key};

mod declaration_summary;
mod reference_identifiers;

pub(super) use declaration_summary::DeclarationSummary;
pub(super) use reference_identifiers::ReferenceIdentifiers;

static UNCANCELLED: AtomicBool = AtomicBool::new(false);

pub(super) struct FileIndex {
    pub(super) symbols: FileSymbols,
    pub(super) imports: Arc<[Import]>,
    pub(super) package_selection: Option<PackageSelection>,
    pub(super) package_name: Option<String>,
}

impl FileIndex {
    fn new(source: &Source) -> Self {
        let symbols = symbols::collect(
            &source.tree,
            &source.text,
            |name| source.range(name),
            |_, _, _| {},
        );
        Self::from_symbols(source, symbols)
    }

    fn cancellable(source: &Source, cancelled: &AtomicBool) -> Result<Self> {
        let symbols = symbols::collect_cancellable(
            &source.tree,
            &source.text,
            |name| source.range(name),
            cancelled,
        )?;
        Ok(Self::from_symbols(source, symbols))
    }

    fn from_symbols(source: &Source, symbols: FileSymbols) -> Self {
        Self {
            symbols,
            imports: source.imports.clone(),
            package_selection: PackageSelection::from_source(source),
            package_name: package_name(source).map(str::to_owned),
        }
    }
}

pub(super) struct IndexedSource {
    source: Source,
    declarations: Arc<OnceLock<Arc<FileIndex>>>,
    shared_declarations: Arc<OnceLock<Arc<SharedDeclarations>>>,
    declaration_summary: Arc<OnceLock<Option<Arc<DeclarationSummary>>>>,
    reference_identifiers: Arc<OnceLock<Arc<ReferenceIdentifiers>>>,
    pub(super) document_version: Option<i32>,
    pub(super) open_generation: u64,
    pub(super) content_revision: u64,
    pub(super) declaration_revision: u64,
}

#[derive(Clone)]
pub(super) struct IndexedSourceRevision {
    source: SourceRevision,
    declarations: Arc<OnceLock<Arc<FileIndex>>>,
    shared_declarations: Arc<OnceLock<Arc<SharedDeclarations>>>,
    declaration_summary: Arc<OnceLock<Option<Arc<DeclarationSummary>>>>,
    reference_identifiers: Arc<OnceLock<Arc<ReferenceIdentifiers>>>,
    document_version: Option<i32>,
    open_generation: u64,
    content_revision: u64,
    declaration_revision: u64,
}

impl IndexedSourceRevision {
    pub(super) fn into_indexed(self) -> IndexedSource {
        IndexedSource {
            source: self.source.into_source(),
            declarations: self.declarations,
            shared_declarations: self.shared_declarations,
            declaration_summary: self.declaration_summary,
            reference_identifiers: self.reference_identifiers,
            document_version: self.document_version,
            open_generation: self.open_generation,
            content_revision: self.content_revision,
            declaration_revision: self.declaration_revision,
        }
    }
}

impl IndexedSource {
    pub(super) fn new(source: Source, document_version: Option<i32>) -> Self {
        Self {
            source,
            declarations: Arc::default(),
            shared_declarations: Arc::default(),
            declaration_summary: Arc::default(),
            reference_identifiers: Arc::default(),
            document_version,
            open_generation: 0,
            content_revision: 0,
            declaration_revision: 0,
        }
    }

    pub(super) fn declaration_summary(&self) -> Option<&Arc<DeclarationSummary>> {
        self.declaration_summary
            .get_or_init(|| DeclarationSummary::collect(&self.source).map(Arc::new))
            .as_ref()
    }

    pub(super) fn reference_identifiers(
        &self,
        cancelled: &AtomicBool,
    ) -> Result<Arc<ReferenceIdentifiers>> {
        check_cancelled(cancelled)?;
        if let Some(identifiers) = self.reference_identifiers.get() {
            return Ok(Arc::clone(identifiers));
        }
        let identifiers = if self.tree.root_node().has_error() {
            ReferenceIdentifiers::default()
        } else {
            ReferenceIdentifiers::scan(&self.text, cancelled)?
        };
        let identifiers = self
            .reference_identifiers
            .get_or_init(|| Arc::new(identifiers));
        Ok(Arc::clone(identifiers))
    }

    pub(super) fn declarations(&self) -> &FileIndex {
        if let Some(declarations) = self.declarations.get() {
            return declarations;
        }
        // Build outside the lock so a foreground query can race a worker
        // without waiting for its parse or declaration collection.
        let declarations = Arc::new(FileIndex::new(&self.source));
        self.declarations.get_or_init(|| declarations)
    }

    fn cancellable_declarations(&self, cancelled: &AtomicBool) -> Result<&FileIndex> {
        self.declaration_index(cancelled)?;
        Ok(self.declarations.get().expect("declarations collected"))
    }

    pub(super) fn declaration_index(&self, cancelled: &AtomicBool) -> Result<Arc<FileIndex>> {
        check_cancelled(cancelled)?;
        if let Some(declarations) = self.declarations.get() {
            return Ok(Arc::clone(declarations));
        }
        let declarations = Arc::new(FileIndex::cancellable(&self.source, cancelled)?);
        check_cancelled(cancelled)?;
        Ok(Arc::clone(self.declarations.get_or_init(|| declarations)))
    }

    pub(super) fn cached_file_index(&self) -> Option<&Arc<FileIndex>> {
        self.declarations.get()
    }

    pub(super) fn remember_file_index(&self, index: Arc<FileIndex>) {
        let _ = self.declarations.set(index);
    }

    pub(super) fn shared_declarations(
        &self,
        cancelled: &AtomicBool,
    ) -> Result<Arc<SharedDeclarations>> {
        check_cancelled(cancelled)?;
        if let Some(declarations) = self.shared_declarations.get() {
            return Ok(Arc::clone(declarations));
        }
        let declarations = Arc::new(SharedDeclarations::collect(self, cancelled)?);
        check_cancelled(cancelled)?;
        Ok(Arc::clone(
            self.shared_declarations.get_or_init(|| declarations),
        ))
    }

    pub(super) fn cached_overlay_declarations(&self) -> Option<&Arc<SharedDeclarations>> {
        self.shared_declarations.get()
    }

    pub(super) fn revision(&self) -> IndexedSourceRevision {
        IndexedSourceRevision {
            source: self.source.revision(),
            declarations: self.declarations.clone(),
            shared_declarations: Arc::clone(&self.shared_declarations),
            declaration_summary: Arc::clone(&self.declaration_summary),
            reference_identifiers: Arc::clone(&self.reference_identifiers),
            document_version: self.document_version,
            open_generation: self.open_generation,
            content_revision: self.content_revision,
            declaration_revision: self.declaration_revision,
        }
    }

    pub(super) fn edit(
        &mut self,
        parser: &mut Parser,
        changes: &[TextDocumentContentChangeEvent],
        version: i32,
        observe: Option<&mut dyn FnMut(TextEditDelta<'_>) -> bool>,
    ) -> Result<()> {
        self.source.edit(parser, changes, observe)?;
        self.document_version = Some(version);
        self.declarations = Arc::default();
        self.shared_declarations = Arc::default();
        self.declaration_summary = Arc::default();
        self.reference_identifiers = Arc::default();
        Ok(())
    }
}

impl IndexedSourceRevision {
    pub(super) fn build_declarations(self, cancelled: &AtomicBool) -> Result<()> {
        self.into_indexed().cancellable_declarations(cancelled)?;
        Ok(())
    }
}

impl Deref for IndexedSource {
    type Target = Source;

    fn deref(&self) -> &Source {
        &self.source
    }
}

pub(super) struct DiskIndex {
    pub(super) declarations: Weak<FileIndex>,
    text: Option<CachedNavigationText>,
    pub(super) fingerprint: u64,
    pub(super) stamp: Option<Stamp>,
    reference_source: Option<CachedReferenceSource>,
}

struct CachedReferenceSource {
    revision: Weak<IndexedSourceRevision>,
    text: CachedNavigationText,
    path: PathBuf,
}

impl DiskIndex {
    pub(super) fn from_prepared(prepared: &PreparedReferenceSource) -> Self {
        Self {
            declarations: Arc::downgrade(
                prepared
                    .revision
                    .declarations
                    .get()
                    .expect("prepared source must have declarations"),
            ),
            text: Some(prepared.text.downgrade()),
            fingerprint: prepared.fingerprint,
            stamp: prepared.text.stamp,
            reference_source: None,
        }
    }

    pub(super) fn references_text(&self, text: *const String) -> bool {
        self.text
            .as_ref()
            .is_some_and(|cached| cached.text.as_ptr() == text)
    }

    #[cfg(test)]
    pub(super) fn test_observation(source: &Source) -> (Self, Arc<FileIndex>) {
        let declarations = Arc::new(FileIndex::new(source));
        (
            Self {
                declarations: Arc::downgrade(&declarations),
                text: None,
                fingerprint: source.fingerprint(),
                stamp: None,
                reference_source: None,
            },
            declarations,
        )
    }

    pub(super) fn from_declarations(prepared: &PreparedDeclarations) -> Self {
        Self {
            declarations: Arc::downgrade(&prepared.declarations.index),
            text: Some(prepared.text.downgrade()),
            fingerprint: prepared.fingerprint,
            stamp: prepared.text.stamp,
            reference_source: None,
        }
    }

    fn matches_revision(&self, text: &NavigationText) -> bool {
        self.text
            .as_ref()
            .is_some_and(|cached| cached.same_revision(text) && cached.matches(Some(&text.text)))
    }
}

pub(super) fn text_fingerprint(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

#[derive(Clone)]
pub(super) struct NavigationText {
    pub(super) text: Arc<String>,
    pub(super) stamp: Option<Stamp>,
    pub(super) read_ticket: Option<FileReadTicket>,
}

pub(super) struct CachedNavigationText {
    pub(super) text: Weak<String>,
    pub(super) stamp: Option<Stamp>,
    ticket: Option<FileReadTicket>,
}

impl CachedNavigationText {
    pub(super) fn read_ticket(&self) -> Option<&FileReadTicket> {
        self.ticket.as_ref()
    }

    pub(super) fn same_revision(&self, text: &NavigationText) -> bool {
        self.ticket
            .as_ref()
            .zip(text.read_ticket.as_ref())
            .is_some_and(|(previous, next)| previous.same_revision(next))
    }

    pub(super) fn matches_disk(&self, path: &Path) -> bool {
        self.ticket
            .as_ref()
            .is_some_and(|ticket| ticket.matches_disk(path))
    }

    pub(super) fn is_current(&self, path: &Path, cache: &SharedDiskCache) -> bool {
        self.ticket.as_ref().is_some_and(|ticket| {
            cache.validates_ticket(path, ticket, self.text.upgrade().as_ref())
        })
    }

    pub(super) fn upgrade(&self) -> Option<NavigationText> {
        Some(NavigationText {
            text: self.text.upgrade()?,
            stamp: self.stamp,
            read_ticket: self.ticket.clone(),
        })
    }

    pub(super) fn matches(&self, disk_text: Option<&str>) -> bool {
        self.text
            .upgrade()
            .is_some_and(|text| Some(text.as_str()) == disk_text)
    }
}

impl NavigationText {
    fn is_captured(&self) -> bool {
        self.read_ticket
            .as_ref()
            .is_none_or(FileReadTicket::is_captured)
    }

    #[cfg(test)]
    pub(super) fn read_cancellable(path: &Path, cancelled: &AtomicBool) -> Result<Self> {
        Self::read_chunks(path, None, cancelled)?
            .with_context(|| format!("reading {}", path.display()))
    }

    pub(super) fn read_observation(
        path: &Path,
        observation: FileObservation,
        cancelled: &AtomicBool,
    ) -> Result<Option<Self>> {
        Self::read_chunks(path, Some(observation), cancelled)
            .with_context(|| format!("reading {}", path.display()))
    }

    fn read_chunks(
        path: &Path,
        expected: Option<FileObservation>,
        cancelled: &AtomicBool,
    ) -> Result<Option<Self>> {
        check_cancelled(cancelled)?;
        let mut file = fs::File::open(path)?;
        let observation = file.metadata().ok().map(FileObservation::from_metadata);
        if expected.is_some() && expected != observation {
            return Ok(None);
        }
        let stamp = observation.map(|file| file.stamp);
        let length = stamp.map_or(0, |stamp| stamp.revision_parts().1);
        let mut text = Vec::with_capacity(usize::try_from(length).unwrap_or_default());
        let mut buffer = [0; 64 * 1024];
        let mut body = file.by_ref().take(expected.map_or(u64::MAX, |_| length));
        loop {
            check_cancelled(cancelled)?;
            match body.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => text.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            }
        }
        // When serving an observed revision, do not read a growing file past
        // the reservation made by a prefilter subscriber.
        if expected.is_some() && (text.len() as u64 != length || file.read(&mut buffer[..1])? != 0)
        {
            return Ok(None);
        }
        let text = String::from_utf8(text)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.utf8_error()))?;
        check_cancelled(cancelled)?;
        Ok(Some(Self {
            text: Arc::new(text),
            stamp,
            read_ticket: None,
        }))
    }

    pub(super) fn downgrade(&self) -> CachedNavigationText {
        CachedNavigationText {
            text: Arc::downgrade(&self.text),
            stamp: self.stamp,
            ticket: self.read_ticket.clone(),
        }
    }
}

pub(super) type DraftFiles<'a> = HashMap<(&'a Path, &'a str), Vec<&'a PathBuf>>;

pub(super) fn active_package<'w>(
    workspace: &'w Workspace,
    source: &Source,
    new_documents: &HashSet<PathBuf>,
) -> Option<&'w Package> {
    workspace.package_of_file(&source.path).or_else(|| {
        new_documents
            .contains(&path_key(&source.path))
            .then(|| draft_package(workspace, source))
            .flatten()
    })
}

pub(super) fn draft_files<'a>(
    workspace: &'a Workspace,
    documents: &'a HashMap<PathBuf, IndexedSource>,
    new_documents: &HashSet<PathBuf>,
) -> DraftFiles<'a> {
    let mut drafts = DraftFiles::new();
    for path in new_documents {
        if let Some(document) = documents.get(path)
            && workspace.package_of_file(&document.path).is_none()
            && let Some(package) = draft_package(workspace, document)
        {
            drafts
                .entry(package.directory_and_name())
                .or_default()
                .push(&document.path);
        }
    }
    for files in drafts.values_mut() {
        files.sort_unstable();
    }
    drafts
}

pub(super) fn draft_package<'w>(workspace: &'w Workspace, source: &Source) -> Option<&'w Package> {
    let name = source.path.file_stem()?.to_str()?;
    let parts = name.split_once('_').map(|(_, suffixes)| suffixes);
    if name.starts_with(['_', '.'])
        || name.ends_with("_test")
        || parts.is_some_and(|suffixes| {
            suffixes.rsplit('_').next().is_some_and(|part| {
                matches!(
                    part,
                    "aix"
                        | "android"
                        | "darwin"
                        | "dragonfly"
                        | "freebsd"
                        | "hurd"
                        | "illumos"
                        | "ios"
                        | "js"
                        | "linux"
                        | "netbsd"
                        | "openbsd"
                        | "plan9"
                        | "solaris"
                        | "wasip1"
                        | "windows"
                        | "zos"
                        | "386"
                        | "amd64"
                        | "amd64p32"
                        | "arm"
                        | "arm64"
                        | "loong64"
                        | "mips"
                        | "mipsle"
                        | "mips64"
                        | "mips64le"
                        | "ppc64"
                        | "ppc64le"
                        | "riscv64"
                        | "s390x"
                        | "sparc64"
                        | "wasm"
                )
            })
        })
        || source.imports.iter().any(|import| import.path == "C")
    {
        return None;
    }
    let mut cursor = source.tree.root_node().walk();
    for node in source.tree.root_node().named_children(&mut cursor) {
        if node.kind() == "package_clause" {
            let name = node.named_child(0)?;
            return workspace.package_of_draft(&source.path, node_text(name, &source.text));
        }
        if node.kind() == "comment"
            && node_text(node, &source.text)
                .strip_prefix("//")
                .is_some_and(|text| {
                    matches!(text.split_whitespace().next(), Some("go:build" | "+build"))
                })
        {
            return None;
        }
    }
    None
}

pub(super) struct IndexReader<'a> {
    pub(super) workspace: &'a Workspace,
    pub(super) parser: &'a mut Parser,
    pub(super) shared_disk: &'a Arc<SharedDiskCache>,
    pub(super) foreground: Option<&'a QueryReadMemo>,
    pub(super) documents: &'a HashMap<PathBuf, IndexedSource>,
    pub(super) pending_documents: Option<&'a HashMap<PathBuf, PendingOpen>>,
    pub(super) disk_index: &'a mut HashMap<PathBuf, DiskIndex>,
    pub(super) navigation_texts: &'a mut HashMap<PathBuf, CachedNavigationText>,
    pub(super) owned_reference: Option<(&'a Path, Rc<IndexedSource>)>,
    pub(super) navigation: Option<&'a NavigationProbe>,
    pub(super) reference_dependencies: Option<&'a ReferenceDependencies<'a>>,
    pub(super) implementation_parse_time: Option<Duration>,
    pub(super) drafts: DraftFiles<'a>,
    pub(super) disk_names: &'a mut HashMap<PathBuf, FileNames>,
    pub(super) completion: Option<&'a mut CompletionRead>,
    pub(super) inferred_weights: &'a mut HashMap<InferredMemo, i32>,
    pub(super) cancelled: Option<&'a AtomicBool>,
    pub(super) diagnostic_types: Option<HashMap<String, TypeDetailsMemo>>,
    pub(super) diagnostic_observations: Option<DiagnosticObservations>,
    pub(super) package_indexes: HashMap<String, Option<Arc<PackageDeclIndex>>>,
    pub(super) request_declarations: RequestDeclarations,
    pub(super) use_captured_disk: bool,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum SourceRev {
    Overlay {
        open_generation: u64,
        content_revision: u64,
    },
    Disk(DiskSourceRev),
    Missing,
}

impl SourceRev {
    pub(super) fn overlay(source: &IndexedSource) -> Option<Self> {
        source.document_version.map(|_| Self::Overlay {
            open_generation: source.open_generation,
            content_revision: source.content_revision,
        })
    }
}

pub(super) fn overlay_revision(
    key: &Path,
    documents: &HashMap<PathBuf, IndexedSource>,
    pending_documents: Option<&HashMap<PathBuf, PendingOpen>>,
) -> Option<(u64, u64)> {
    if let Some(pending) = pending_documents.and_then(|pending| pending.get(key)) {
        return Some((pending.open_generation, pending.content_revision));
    }
    let source = documents.get(key)?;
    Some((source.open_generation, source.content_revision))
}

pub(super) fn track_overlay(
    memo: &QueryReadMemo,
    key: &Path,
    documents: &HashMap<PathBuf, IndexedSource>,
    pending_documents: Option<&HashMap<PathBuf, PendingOpen>>,
) {
    let mut overlays = memo.overlays.borrow_mut();
    if !overlays.contains_key(key) {
        overlays.insert(
            key.to_owned(),
            overlay_revision(key, documents, pending_documents),
        );
    }
}

#[derive(Default)]
pub(super) struct RequestDeclarations {
    entries: HashMap<PathBuf, Rc<PreparedDeclarations>>,
    bytes: usize,
}

/// Cache the directory listing once a diagnostic actually scans siblings.
#[derive(Default)]
pub(super) struct DiagnosticObservations {
    generation: u64,
    visits: HashMap<PathBuf, usize>,
    directories: DirectoryObservations,
}

impl DiagnosticObservations {
    fn observe(
        &mut self,
        file: &Path,
        generation: u64,
        batch: bool,
    ) -> Option<(FileObservation, u64)> {
        if !cfg!(windows) {
            return None;
        }
        if self.generation != generation {
            *self = Self {
                generation,
                ..Self::default()
            };
        }
        const BATCH_MIN: usize = 4;
        let parent = file.parent()?;
        let visits = if let Some(visits) = self.visits.get_mut(parent) {
            visits
        } else {
            if self.visits.len() >= 32 {
                self.visits.clear();
            }
            self.visits.entry(parent.to_owned()).or_default()
        };
        if batch {
            *visits = BATCH_MIN;
        } else {
            *visits = (*visits + 1).min(BATCH_MIN);
        }
        if *visits < BATCH_MIN {
            return None;
        }
        self.directories
            .observe(file)
            .map(|observation| (observation, generation))
    }
}

impl RequestDeclarations {
    fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }

    fn insert(&mut self, key: PathBuf, source: Rc<PreparedDeclarations>) {
        const ENTRIES: usize = 256;
        const BYTES: usize = 16 * 1024 * 1024;
        let weight = declaration_weight(&source);
        if let Some(previous) = self.entries.remove(&key) {
            self.bytes -= declaration_weight(&previous);
        }
        if self.entries.len() >= ENTRIES || self.bytes.saturating_add(weight) > BYTES {
            self.clear();
        }
        self.bytes = self.bytes.saturating_add(weight);
        self.entries.insert(key, source);
    }
}

fn declaration_weight(source: &PreparedDeclarations) -> usize {
    source
        .declarations
        .cache_weight()
        .saturating_add(source.text.text.capacity())
}

enum ReferenceInput {
    Indexed,
    Absent,
    Cold(Option<NavigationText>),
}

type TypeDetailsMemo = HashMap<String, Option<(PathBuf, TypeDetails)>>;

pub(super) struct CompletionRead {
    pub(super) trace: Option<Trace>,
    pub(super) prefetched: HashMap<PathBuf, NavigationText>,
    sources: VecDeque<(PathBuf, CompletionSource)>,
    pub(super) document: Option<(Rc<IndexedSource>, FileReadTicket)>,
    bytes: usize,
    disk_observations: DirectoryObservations,
    sdk_watch: Option<SdkRead>,
    // The completion task owns this value for a single graph and set of
    // overlays. Edits and graph replacements discard the task and its keys.
    types: HashMap<String, HashMap<String, Option<(PathBuf, TypeDetails)>>>,
    pub(super) type_kinds: HashMap<(String, String), (Option<CompletionItemKind>, bool)>,
}

struct SdkRead {
    watch: Arc<DirectoryWatch>,
    generation: u64,
    trusted_names: bool,
}

#[derive(Clone)]
enum CompletionSource {
    Declarations(Rc<PreparedDeclarations>),
    Syntax(Rc<IndexedSource>),
}

impl CompletionSource {
    fn weight(&self) -> usize {
        match self {
            Self::Declarations(source) => declaration_weight(source),
            Self::Syntax(source) => source.text.len(),
        }
    }
}

impl CompletionRead {
    pub(super) fn new(trace: Option<Trace>) -> Self {
        Self {
            trace,
            prefetched: HashMap::new(),
            sources: VecDeque::new(),
            document: None,
            bytes: 0,
            disk_observations: DirectoryObservations::default(),
            sdk_watch: None,
            types: HashMap::new(),
            type_kinds: HashMap::new(),
        }
    }

    pub(super) fn observe_completion_sdk(&mut self, watch: Option<&Arc<DirectoryWatch>>) {
        self.sdk_watch = watch.and_then(|watch| {
            Some(SdkRead {
                watch: Arc::clone(watch),
                generation: watch.generation()?,
                trusted_names: false,
            })
        });
        if self.sdk_watch.is_some() {
            // Earlier lookups may have listed the directory before this SDK
            // generation was observed. Only fresh metadata can validate it.
            self.disk_observations = DirectoryObservations::default();
        }
    }

    pub(super) fn completion_sdk_is_current(&self) -> bool {
        self.sdk_watch
            .as_ref()
            .is_none_or(|sdk| !sdk.trusted_names || sdk.watch.generation() == Some(sdk.generation))
    }

    pub(super) fn trust_completion_sdk_names(&mut self) -> Option<u64> {
        let sdk = self.sdk_watch.as_mut()?;
        sdk.trusted_names = true;
        Some(sdk.generation)
    }

    fn get_syntax(&mut self, key: &Path, text: &str) -> Option<Rc<IndexedSource>> {
        let CompletionSource::Syntax(source) = self.take_recent(key, |source| {
            matches!(source, CompletionSource::Syntax(source) if source.text.as_str() == text)
        })? else {
            unreachable!("completion syntax entry")
        };
        Some(source)
    }

    pub(super) fn declarations(&mut self, key: &Path) -> Option<Rc<PreparedDeclarations>> {
        let CompletionSource::Declarations(source) = self.take_recent(key, |entry| {
            matches!(entry, CompletionSource::Declarations(_))
        })?
        else {
            unreachable!("completion declaration entry")
        };
        Some(source)
    }

    fn take_recent(
        &mut self,
        key: &Path,
        accepts: impl Fn(&CompletionSource) -> bool,
    ) -> Option<CompletionSource> {
        let position = self
            .sources
            .iter()
            .position(|(path, entry)| path == key && accepts(entry))?;
        let entry = self.sources.remove(position).unwrap();
        let source = entry.1.clone();
        self.sources.push_back(entry);
        Some(source)
    }

    fn store(&mut self, key: PathBuf, source: CompletionSource) {
        let len = source.weight();
        let doc_size = self.document.as_ref().map_or(0, |(doc, _)| doc.text.len());
        if len > BYTE_LIMIT - doc_size {
            return;
        }
        if let Some(position) = self.sources.iter().position(|(path, entry)| {
            path == &key && std::mem::discriminant(entry) == std::mem::discriminant(&source)
        }) {
            self.bytes -= self.sources.remove(position).unwrap().1.weight();
        }
        self.make_room(1, len);
        self.bytes += len;
        self.sources.push_back((key, source));
        self.report_size();
    }

    pub(super) fn pin_document(&mut self, source: IndexedSource, ticket: FileReadTicket) {
        let len = source.text.len();
        if len <= BYTE_LIMIT {
            self.make_room(1, len);
            self.bytes += len;
            self.document = Some((Rc::new(source), ticket));
            self.report_size();
        }
    }

    fn make_room(&mut self, count: usize, len: usize) {
        let pinned = usize::from(self.document.is_some());
        while self.sources.len() + pinned + count > SOURCE_LIMIT || self.bytes + len > BYTE_LIMIT {
            self.bytes -= self.sources.pop_front().unwrap().1.weight();
        }
    }

    fn report_size(&self) {
        if let Some(trace) = &self.trace {
            trace.cache_size(
                self.sources.len() + usize::from(self.document.is_some()),
                self.bytes,
            );
        }
    }

    #[cfg(test)]
    pub(super) fn retained_declarations(&self) -> Vec<std::rc::Weak<PreparedDeclarations>> {
        self.sources
            .iter()
            .filter_map(|(_, entry)| match entry {
                CompletionSource::Declarations(source) => Some(Rc::downgrade(source)),
                CompletionSource::Syntax(_) => None,
            })
            .collect()
    }
}

impl Drop for CompletionRead {
    fn drop(&mut self) {
        self.sources.clear();
        self.document.take();
        self.prefetched.clear();
        if let Some(trace) = &self.trace {
            trace.cache_released();
        }
    }
}

pub(super) enum ReferenceSource<'a> {
    Open(&'a IndexedSource),
    Disk(Rc<IndexedSource>),
}

#[derive(Clone)]
pub(super) struct PreparedReferenceSource {
    pub(super) revision: Arc<IndexedSourceRevision>,
    pub(super) text: NavigationText,
    fingerprint: u64,
    path: PathBuf,
}

impl PreparedReferenceSource {
    pub(super) fn new(
        revision: Arc<IndexedSourceRevision>,
        path: &Path,
        text: NavigationText,
        fingerprint: u64,
    ) -> Self {
        Self {
            revision,
            text,
            fingerprint,
            path: path.to_owned(),
        }
    }

    pub(super) fn into_indexed(self) -> IndexedSource {
        Self::indexed_at(self.revision, self.path)
    }

    fn indexed_at(revision: Arc<IndexedSourceRevision>, path: PathBuf) -> IndexedSource {
        let mut indexed = revision.as_ref().clone().into_indexed();
        indexed.source.rebind_path(path);
        indexed
    }
}

impl IndexedSourceRevision {
    pub(super) fn from_disk_source(
        source: Source,
        cancelled: &AtomicBool,
    ) -> Result<(Self, usize)> {
        let source = IndexedSource::new(source, None);
        source.cancellable_declarations(cancelled)?;
        let nodes = source.tree.root_node().descendant_count();
        Ok((source.revision(), nodes))
    }
}

impl AsRef<IndexedSource> for ReferenceSource<'_> {
    fn as_ref(&self) -> &IndexedSource {
        match self {
            Self::Open(source) => source,
            Self::Disk(source) => source,
        }
    }
}

impl<'a> IndexReader<'a> {
    pub(super) fn has_overlay(&self, key: &Path) -> bool {
        self.documents.contains_key(key) || self.pending_overlay(key).is_some()
    }

    fn pending_overlay(&self, key: &Path) -> Option<&PendingOpen> {
        self.pending_documents?.get(key)
    }

    fn check_pending_overlay(&self, key: &Path) -> Result<()> {
        check_optional_cancelled(self.cancelled)?;
        if let Some(memo) = self.foreground {
            track_overlay(memo, key, self.documents, self.pending_documents);
        }
        if let Some(pending) = self.pending_overlay(key) {
            return Err(pending.ready.pending().into());
        }
        Ok(())
    }

    pub(super) fn navigation_text(
        &mut self,
        path: &Path,
        key: &Path,
    ) -> Result<(Arc<String>, bool)> {
        self.check_pending_overlay(key)?;
        if let Some(memo) = self.foreground {
            let text = self
                .shared_disk
                .read_foreground(path, memo, self.navigation)?;
            return Ok((text.text, true));
        }
        if let Some(text) = self.cached_navigation_text(key) {
            return Ok((text.text, false));
        }
        let text = self
            .shared_disk
            .read(path, self.cancelled, self.navigation)?;
        self.navigation_texts
            .insert(key.to_owned(), text.downgrade());
        Ok((text.text, true))
    }

    pub(super) fn reference_source(&mut self, path: &Path) -> Result<Option<ReferenceSource<'a>>> {
        let key = path_key(path);
        self.check_pending_overlay(&key)?;
        let probe = self.navigation;
        record(probe, |counts| counts.scanned_files += 1);
        if let Some(source) = self.cached_reference_source(&key) {
            record(probe, |counts| counts.ast_access(true));
            return Ok(Some(source));
        }
        record(probe, |counts| counts.ast_access(false));
        let started = self.implementation_parse_time.map(|_| Instant::now());
        let prepared = match self.load_navigation_source(path, &key) {
            Ok(prepared) => prepared,
            Err(error) if missing_source(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        let source = self.store_disk_source(key, prepared);
        if let Some(started) = started {
            *self.implementation_parse_time.as_mut().unwrap() += started.elapsed();
        }
        Ok(Some(ReferenceSource::Disk(source)))
    }

    pub(super) fn cached_reference_source(&self, key: &Path) -> Option<ReferenceSource<'a>> {
        if let Some(memo) = self.foreground {
            track_overlay(memo, key, self.documents, self.pending_documents);
        }
        if let Some(source) = self.documents.get(key) {
            return Some(ReferenceSource::Open(source));
        }
        if let Some((origin, source)) = &self.owned_reference
            && *origin == key
        {
            return Some(ReferenceSource::Disk(Rc::clone(source)));
        }
        if self.foreground.is_some() {
            // The foreground shared loader handles verified disk cache hits.
            return None;
        }
        let cached = self.disk_index.get(key)?.reference_source.as_ref()?;
        let source = cached.revision.upgrade()?;
        let usable = match self.captured_implementation_text(key) {
            Some(text) => cached.text.matches(Some(&text.text)),
            None => cached.text.is_current(key, self.shared_disk),
        };
        if !usable {
            return None;
        }
        Some(ReferenceSource::Disk(Rc::new(
            PreparedReferenceSource::indexed_at(source, cached.path.clone()),
        )))
    }

    pub(super) fn cached_navigation_text(&self, key: &Path) -> Option<NavigationText> {
        let text = self.navigation_texts.get(key)?.upgrade()?;
        self.shared_disk.validates_read(key, &text).then_some(text)
    }

    pub(super) fn install_reference_source(
        &mut self,
        key: PathBuf,
        prepared: PreparedReferenceSource,
    ) -> bool {
        if self.has_overlay(&key) || !self.shared_disk.validates_read(&key, &prepared.text) {
            return false;
        }
        self.navigation_texts
            .insert(key.clone(), prepared.text.downgrade());
        self.store_reference_index(key, &prepared);
        true
    }

    pub(super) fn declaration_source(
        &mut self,
        path: &Path,
    ) -> Result<Option<Arc<SharedDeclarations>>> {
        let key = path_key(path);
        self.check_pending_overlay(&key)?;
        if let Some(source) = self.overlay_or_owned(&key) {
            return self.indexed_declarations(source).map(Some);
        }
        match self.ensure_declarations(path, &key) {
            Ok(()) => Ok(Some(Arc::clone(
                &self.request_declarations.entries[&key].declarations,
            ))),
            Err(error) if missing_source(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(super) fn indexed_declarations(
        &self,
        source: &IndexedSource,
    ) -> Result<Arc<SharedDeclarations>> {
        if let Some(memo) = self.foreground {
            return self
                .shared_disk
                .source_declarations_foreground(source, memo);
        }
        if source.document_version.is_some() {
            return source.shared_declarations(self.cancelled.unwrap_or(&UNCANCELLED));
        }
        // The Stage 2 worker holds a full disk snapshot even when the
        // shared declaration projection was built from elided syntax.
        self.shared_disk.source_declarations(source, self.cancelled)
    }

    pub(super) fn source_revision(&self, path: &Path) -> Result<SourceRev> {
        let key = path_key(path);
        self.check_pending_overlay(&key)?;
        if let Some(source) = self.documents.get(&key) {
            return Ok(SourceRev::overlay(source).expect("open document revision"));
        }
        let text = match self.foreground {
            Some(memo) => self
                .shared_disk
                .read_foreground(path, memo, self.navigation),
            None => self.shared_disk.read(path, self.cancelled, self.navigation),
        };
        match text {
            Ok(text) => Ok(SourceRev::Disk(
                text.read_ticket
                    .expect("disk read ticket")
                    .source_revision(),
            )),
            Err(error) if missing_source(&error) => Ok(SourceRev::Missing),
            Err(error) => Err(error),
        }
    }

    pub(super) fn builtin_path(&self, root: &Path) -> Result<PathBuf> {
        if self.foreground.is_none() && (self.completion.is_none() || self.cancelled.is_none()) {
            return self.workspace.builtin_source(root).map(Path::to_owned);
        }
        if let Some(path) = self.workspace.known_builtin_source() {
            return path;
        }
        let environment = self.workspace.environment();
        let path = if let Some(memo) = self.foreground {
            self.shared_disk
                .builtin_path_foreground(root, environment, memo)?
        } else {
            self.shared_disk.builtin_path_cancellable(
                root,
                environment,
                self.cancelled.expect("completion cancellation"),
            )?
        };
        self.workspace.remember_builtin_source(path.clone());
        Ok(path)
    }

    pub(super) fn verified_declaration_text(&self, key: &Path) -> Option<CachedNavigationText> {
        let prepared = self.request_declarations.entries.get(key)?;
        self.shared_disk
            .validates_read(key, &prepared.text)
            .then(|| prepared.text.downgrade())
    }

    pub(super) fn declaration_ticket(&self, key: &Path) -> Option<&FileReadTicket> {
        self.request_declarations
            .entries
            .get(key)?
            .text
            .read_ticket
            .as_ref()
    }

    pub(super) fn completion_revision(&self, path: &Path) -> Option<SourceRev> {
        self.completion.as_ref()?;
        let key = path_key(path);
        if let Some(source) = self.documents.get(&key) {
            return SourceRev::overlay(source);
        }
        self.declaration_ticket(&key)
            .map(|ticket| SourceRev::Disk(ticket.source_revision()))
    }

    pub(super) fn declaration_syntax(
        &mut self,
        path: &Path,
    ) -> Result<Option<ReferenceSource<'a>>> {
        if self.completion.is_none() {
            return self.reference_source(path);
        }
        let key = path_key(path);
        self.check_pending_overlay(&key)?;
        if let Some(source) = self.documents.get(&key) {
            return Ok(Some(ReferenceSource::Open(source)));
        }
        match self.ensure_declarations(path, &key) {
            Ok(()) => {}
            Err(error) if missing_source(&error) => return Ok(None),
            Err(error) => return Err(error),
        }
        let text = self.request_declarations.entries[&key].text.clone();
        if let Some(source) = self
            .completion
            .as_mut()
            .and_then(|request| request.get_syntax(&key, &text.text))
        {
            return Ok(Some(ReferenceSource::Disk(source)));
        }
        if let Some(source) = self.shared_disk.cached_parse(path, &text) {
            return Ok(Some(ReferenceSource::Disk(Rc::new(source.into_indexed()))));
        }
        let started = self.trace().map(|trace| (trace.clone(), Instant::now()));
        let elided = {
            let _parse = self.trace().map(|trace| trace.phase("tree_sitter"));
            match scan_function_bodies_cancellable(&text.text, self.cancelled)? {
                Some(bodies) if !bodies.is_empty() => Source::try_parse_dependency_declarations(
                    self.parser,
                    path,
                    text.text.clone(),
                    &bodies,
                    self.cancelled.unwrap_or(&UNCANCELLED),
                )?,
                _ => None,
            }
        };
        let declarations_only = elided.is_some();
        let source = Rc::new(match elided {
            Some(source) => IndexedSource::new(source, None),
            None => self
                .shared_disk
                .parse_snapshot_with(self.parser, path, text, self.cancelled, self.navigation)?
                .into_indexed(),
        });
        if let Some((trace, started)) = started {
            if declarations_only {
                trace.declaration_parse();
            } else {
                trace.parse();
            }
            trace.index_time(path, source.text.len(), declarations_only, started);
        }
        self.completion
            .as_mut()
            .unwrap()
            .store(key, CompletionSource::Syntax(Rc::clone(&source)));
        Ok(Some(ReferenceSource::Disk(source)))
    }

    fn load_navigation_source(
        &mut self,
        path: &Path,
        key: &Path,
    ) -> Result<PreparedReferenceSource> {
        if let Some(memo) = self.foreground {
            return self
                .shared_disk
                .load_foreground(path, memo, self.navigation);
        }
        if let Some(dependencies) = self.reference_dependencies {
            let prefetched = dependencies
                .observations
                .cached_text(self.shared_disk, path, key);
            if let Some(source) = prefetched
                .as_ref()
                .and_then(|text| self.shared_disk.cached_parse(path, text))
            {
                record(self.navigation, |counts| counts.text_hits += 1);
                return Ok(source);
            }
            let prepared = self.shared_disk.load_prefetched(
                self.parser,
                path,
                prefetched,
                self.cancelled,
                self.navigation,
            )?;
            dependencies
                .observations
                .remember(key, Some(&prepared.text));
            return Ok(prepared);
        }
        if let Some(text) = self.captured_implementation_text(key) {
            return self
                .shared_disk
                .parse_snapshot(self.parser, path, text, self.navigation);
        }
        let prefetched = self.cached_navigation_text(key);
        self.shared_disk.load_prefetched(
            self.parser,
            path,
            prefetched,
            self.cancelled,
            self.navigation,
        )
    }

    fn store_disk_source(
        &mut self,
        key: PathBuf,
        prepared: PreparedReferenceSource,
    ) -> Rc<IndexedSource> {
        if self.foreground.is_none() && self.shared_disk.validates_read(&key, &prepared.text) {
            self.store_reference_index(key, &prepared);
        }
        Rc::new(prepared.into_indexed())
    }

    fn store_reference_index(&mut self, key: PathBuf, prepared: &PreparedReferenceSource) {
        let hit = self
            .disk_index
            .get(&key)
            .is_some_and(|disk| disk.matches_revision(&prepared.text));
        record(self.navigation, |counts| counts.declaration_access(hit));
        if !hit {
            self.disk_index
                .insert(key.clone(), DiskIndex::from_prepared(prepared));
        }
        let index = self.disk_index.get_mut(&key).unwrap();
        index.reference_source = Some(CachedReferenceSource {
            revision: Arc::downgrade(&prepared.revision),
            text: prepared.text.downgrade(),
            path: prepared.path.clone(),
        });
    }

    pub(super) fn files<'p>(&self, package: &'p Package) -> Cow<'p, [PathBuf]> {
        match self.drafts.get(&package.directory_and_name()) {
            None => Cow::Borrowed(&package.files),
            Some(files) => Cow::Owned(
                package
                    .files
                    .iter()
                    .chain(files.iter().copied())
                    .cloned()
                    .collect(),
            ),
        }
    }

    pub(super) fn prepare_package_index(
        &mut self,
        package: &Package,
    ) -> Result<Option<Arc<PackageDeclIndex>>> {
        self.package_index(package, true)
    }

    pub(super) fn lookup_files<'p>(
        &mut self,
        package: &'p Package,
    ) -> Result<PackageLookupFiles<'p>> {
        Ok(match self.package_index(package, false)? {
            Some(index) => PackageLookupFiles::Indexed(index),
            None => PackageLookupFiles::All(self.files(package)),
        })
    }

    fn package_index(
        &mut self,
        package: &Package,
        populate: bool,
    ) -> Result<Option<Arc<PackageDeclIndex>>> {
        if !package_index::enabled()
            || self.reference_dependencies.is_some()
            || self.owned_reference.is_some()
            || self.use_captured_disk
            || self.completion.as_ref().is_some_and(|request| {
                // Keep source selection on the same bytes as the prefilter
                // while this request owns a frozen batch revision.
                !request.prefetched.is_empty() || request.sources.iter().any(|(_, source)| {
                    matches!(source, CompletionSource::Declarations(source) if source.text.is_captured())
                })
            })
        {
            return Ok(None);
        }
        if let Some(cached) = self.package_indexes.get(&package.id) {
            if cached.is_some() || !populate {
                return Ok(cached.clone());
            }
        }
        let index = if populate || self.shared_disk.has_package_index(&package.id) {
            self.load_package_index(package, populate)?
        } else {
            None
        };
        self.package_indexes
            .insert(package.id.clone(), index.clone());
        Ok(index)
    }

    fn load_package_index(
        &mut self,
        package: &Package,
        populate: bool,
    ) -> Result<Option<Arc<PackageDeclIndex>>> {
        let cancelled = self.cancelled.unwrap_or(&UNCANCELLED);
        let paths = self.files(package);
        let batch = paths.len() >= 8;
        let mut files = Vec::with_capacity(paths.len());
        let mut revisions = Vec::with_capacity(paths.len());
        for path in paths.iter() {
            check_cancelled(cancelled)?;
            let key = path_key(path);
            let Some((declarations, revision)) =
                self.package_decl_file(path, &key, populate, batch)?
            else {
                return Ok(None);
            };
            // Warm callers may have source prefilters that deliberately skip
            // recovery artifacts. Preserve their existing traversal in that case.
            if !populate
                && (!declarations.reliable_package_scope
                    || declarations.index.package_name.as_deref() != Some(&package.name))
            {
                return Ok(None);
            }
            revisions.push((path.clone(), revision));
            files.push(PackageDeclFile {
                path: path.clone(),
                key,
                declarations,
            });
        }
        let key = PackageDeclKey::new(self.workspace, package, &files, revisions);
        self.shared_disk
            .package_index(key, &files, &package.name, populate, cancelled)
    }

    fn package_decl_file(
        &mut self,
        path: &Path,
        key: &Path,
        populate: bool,
        batch: bool,
    ) -> Result<Option<(Arc<SharedDeclarations>, PackageFileRevision)>> {
        if !populate && self.pending_overlay(key).is_some() {
            return Ok(None);
        }
        self.check_pending_overlay(key)?;
        let (declarations, overlay, text, fingerprint) =
            if let Some(source) = self.documents.get(key) {
                let declarations = if populate {
                    self.indexed_declarations(source)?
                } else if let Some(projected) = source.shared_declarations.get() {
                    projected.clone()
                } else {
                    return Ok(None);
                };
                let overlay = source
                    .document_version
                    .map(|_| (source.open_generation, source.declaration_revision));
                let fingerprint = declarations.summary.is_none().then(|| source.fingerprint());
                (declarations, overlay, source.text.clone(), fingerprint)
            } else if let Some(prepared) = self.request_declarations.entries.get(key) {
                (
                    prepared.declarations.clone(),
                    None,
                    prepared.text.text.clone(),
                    Some(prepared.fingerprint),
                )
            } else if populate {
                match self.ensure_declarations_in_batch(path, key, batch) {
                    Ok(()) => {}
                    Err(error) if missing_source(&error) => return Ok(None),
                    Err(error) => return Err(error),
                }
                let prepared = &self.request_declarations.entries[key];
                (
                    prepared.declarations.clone(),
                    None,
                    prepared.text.text.clone(),
                    Some(prepared.fingerprint),
                )
            } else if self.foreground.is_some() {
                let Some((text, declarations)) = self.shared_disk.peek_foreground_declarations(key)
                else {
                    return Ok(None);
                };
                let fingerprint = declarations.summary.is_none().then(|| {
                    text.read_ticket
                        .as_ref()
                        .expect("verified disk read")
                        .fingerprint()
                });
                (declarations, None, text.text, fingerprint)
            } else {
                let Some(text) = self.shared_disk.cached_read(path, key) else {
                    return Ok(None);
                };
                let Some(declarations) = self.shared_disk.cached_declarations(&text) else {
                    return Ok(None);
                };
                let fingerprint = declarations
                    .summary
                    .is_none()
                    .then(|| text_fingerprint(&text.text));
                (declarations, None, text.text, fingerprint)
            };
        let content = match &declarations.summary {
            Some(summary) => DeclarationIdentity::Summary(summary.clone()),
            None => {
                DeclarationIdentity::Exact(fingerprint.expect("exact revision fingerprint"), text)
            }
        };
        let revision = PackageFileRevision {
            overlay,
            reliable_declarations: declarations.reliable_declarations,
            reliable_package_scope: declarations.reliable_package_scope,
            content,
        };
        Ok(Some((declarations, revision)))
    }

    pub(super) fn file_index(&mut self, file: &Path) -> Result<&FileIndex> {
        let key = path_key(file);
        self.file_index_at(file, &key)
    }

    fn file_index_at(&mut self, file: &Path, key: &Path) -> Result<&FileIndex> {
        self.check_pending_overlay(key)?;
        if let Some(source) = self.documents.get(key) {
            record(self.navigation, |counts| {
                counts.declaration_access(source.declarations.get().is_some());
            });
            return if let Some(memo) = self.foreground {
                if source.cached_file_index().is_none() {
                    self.shared_disk
                        .source_declarations_foreground(source, memo)?;
                }
                Ok(source.declarations())
            } else if let Some(cancelled) = self.cancelled {
                source.cancellable_declarations(cancelled)
            } else {
                Ok(source.declarations())
            };
        }
        if self
            .owned_reference
            .as_ref()
            .is_some_and(|(origin, _)| *origin == key)
        {
            return Ok(self.owned_reference.as_ref().unwrap().1.declarations());
        }
        self.ensure_declarations(file, key)?;
        Ok(&self.request_declarations.entries[key].declarations.index)
    }

    pub(super) fn trace(&self) -> Option<&Trace> {
        self.completion.as_ref()?.trace.as_ref()
    }

    fn overlay_or_owned(&self, key: &Path) -> Option<&IndexedSource> {
        self.documents.get(key).or_else(|| {
            let (origin, source) = self.owned_reference.as_ref()?;
            (*origin == key).then_some(source)
        })
    }

    pub(super) fn content_identity(&self, key: &Path) -> Option<ContentIdentity> {
        self.documents
            .get(key)
            .and_then(|source| source.document_version)
            .map(ContentIdentity::Buffer)
            .or_else(|| {
                self.request_declarations
                    .entries
                    .get(key)
                    .map(|index| ContentIdentity::Disk(index.fingerprint))
            })
            .or_else(|| {
                self.disk_index
                    .get(key)
                    .map(|index| ContentIdentity::Disk(index.fingerprint))
            })
    }

    fn completion_text(
        &mut self,
        path: &Path,
        key: &Path,
        prefilter: bool,
        project: bool,
    ) -> Result<NavigationText> {
        check_optional_cancelled(self.cancelled)?;
        if let Some(text) = self
            .completion
            .as_mut()
            .and_then(|request| request.prefetched.remove(key))
        {
            if let Some(trace) = self.trace() {
                trace.text_handoff();
            }
            return Ok(text);
        }
        let (text, produced) =
            self.shared_disk
                .read_with_status(path, self.cancelled, self.navigation)?;
        if produced && let Some(trace) = self.trace() {
            trace.read(key, prefilter, project, self.disk_index.contains_key(key));
        }
        Ok(text)
    }

    pub(super) fn cached_completion_names(
        &mut self,
        path: &Path,
        key: &Path,
    ) -> Option<&FileNames> {
        let names = self.disk_names.get_mut(key)?;
        let Some(request) = &mut self.completion else {
            return names.matches_disk(path).then_some(names);
        };
        let mut sdk = request.sdk_watch.as_mut();
        if let Some(sdk) = &mut sdk
            && names.sdk_generation == Some(sdk.generation)
        {
            sdk.trusted_names = true;
            return Some(names);
        }
        let valid = names.matches_observation(request.disk_observations.observe(path));
        if valid && names.sdk_generation.is_some() {
            names.sdk_generation = sdk.map(|sdk| sdk.generation);
        }
        valid.then_some(names)
    }

    pub(super) fn type_details(
        &mut self,
        package: &Package,
        name: &str,
        project: bool,
    ) -> Result<Option<(PathBuf, TypeDetails)>> {
        check_optional_cancelled(self.cancelled)?;
        if let Some(cached) = self
            .diagnostic_types
            .as_ref()
            .and_then(|types| types.get(&package.id)?.get(name))
        {
            return Ok(cached.clone());
        }
        if let Some(cached) = self
            .completion
            .as_ref()
            .and_then(|request| request.types.get(&package.id)?.get(name))
        {
            if let Some(trace) = self.trace() {
                trace.type_location_hit();
            }
            return Ok(cached.clone());
        }
        if self.reference_dependencies.is_some() {
            return self.find_reference_index(package, name, |index| {
                index.symbols.type_details(name).cloned()
            });
        }
        let mut result = None;
        let files = self.lookup_files(package)?;
        for file in files.iter(PackageLookup::Type(name)) {
            let index = if self.completion.is_some() && !package.load_error {
                self.filtered_index(file, project, |names| names.may_declare_type(name))?
            } else {
                Some(self.file_index(file)?)
            };
            if let Some(details) = index.and_then(|index| index.symbols.type_details(name)) {
                result = Some((file.to_owned(), details.clone()));
                break;
            }
        }
        if let Some(request) = &mut self.completion {
            request
                .types
                .entry(package.id.clone())
                .or_default()
                .insert(name.to_owned(), result.clone());
        }
        if let Some(types) = &mut self.diagnostic_types {
            types
                .entry(package.id.clone())
                .or_default()
                .insert(name.to_owned(), result.clone());
        }
        Ok(result)
    }

    pub(super) fn filtered_index(
        &mut self,
        file: &Path,
        project: bool,
        mut accepts: impl FnMut(&FileNames) -> bool,
    ) -> Result<Option<&FileIndex>> {
        let key = path_key(file);
        self.check_pending_overlay(&key)?;
        if self.overlay_or_owned(&key).is_some()
            || self.request_declarations.entries.contains_key(&key)
        {
            return self.file_index_at(file, &key).map(Some);
        }
        if let Some(prepared) = self.cached_declaration_source(file, &key) {
            self.remember_declarations(key.clone(), prepared);
            return self.file_index_at(file, &key).map(Some);
        }
        if let Some(names) = self.cached_completion_names(file, &key) {
            let matches = accepts(names);
            check_optional_cancelled(self.cancelled)?;
            return if matches {
                self.file_index_at(file, &key).map(Some)
            } else {
                Ok(None)
            };
        }
        let text = self.completion_text(file, &key, true, project)?;
        if let Some(names) = FileNames::from_read(&text, self.cancelled)? {
            let matches = accepts(&names);
            check_optional_cancelled(self.cancelled)?;
            self.disk_names.insert(key.clone(), names);
            if !matches {
                return Ok(None);
            }
        }
        let prepared = self.load_declarations_for(file, Some(text))?;
        self.remember_declarations(key.clone(), prepared);
        self.file_index_at(file, &key).map(Some)
    }

    pub(super) fn find_reference_index<T>(
        &mut self,
        package: &Package,
        spelling: &str,
        lookup: impl Fn(&FileIndex) -> Option<T>,
    ) -> Result<Option<(PathBuf, T)>> {
        const DEPENDENCY_BATCH_SIZE: usize = 16;
        let dependencies = self.reference_dependencies.expect("reference resolver");
        let files = self.files(package);
        let mut files = files.iter();
        while let Some(file) = files.next() {
            let key = path_key(file);
            match self.reference_input(file, &key, spelling, dependencies)? {
                ReferenceInput::Indexed => {
                    if let Some(value) = lookup(self.file_index_at(file, &key)?) {
                        return Ok(Some((file.clone(), value)));
                    }
                }
                ReferenceInput::Absent => {}
                ReferenceInput::Cold(text) => {
                    let mut batch = vec![(file, key, ReferenceInput::Cold(text))];
                    for file in files.by_ref().take(DEPENDENCY_BATCH_SIZE - 1) {
                        let key = path_key(file);
                        let input = self.reference_input(file, &key, spelling, dependencies);
                        batch.push((file, key, input?));
                    }
                    let cold: Vec<_> = batch
                        .iter()
                        .filter_map(|(file, _, input)| match input {
                            ReferenceInput::Cold(text) => Some(((*file).clone(), text.clone())),
                            _ => None,
                        })
                        .collect();
                    let mut loaded = dependencies
                        .load(cold, spelling, self.navigation)?
                        .into_iter();
                    // Resolve in package file order even when loads complete out of order.
                    for (file, key, input) in batch {
                        match input {
                            ReferenceInput::Absent => continue,
                            ReferenceInput::Indexed => {}
                            ReferenceInput::Cold(_) => {
                                match loaded.next().expect("cold reference dependency")? {
                                    DependencySource::Ready(source) => {
                                        dependencies
                                            .observations
                                            .remember(&key, Some(&source.text));
                                        self.remember_declarations(key.clone(), source);
                                    }
                                    DependencySource::Absent(text) => {
                                        dependencies.observations.remember(&key, Some(&text));
                                        continue;
                                    }
                                }
                            }
                        }
                        if let Some(value) = lookup(self.file_index_at(file, &key)?) {
                            return Ok(Some((file.clone(), value)));
                        }
                    }
                }
            }
        }
        Ok(None)
    }

    fn reference_input(
        &mut self,
        file: &Path,
        key: &Path,
        spelling: &str,
        dependencies: &ReferenceDependencies<'_>,
    ) -> Result<ReferenceInput> {
        if self.overlay_or_owned(key).is_some()
            || self.request_declarations.entries.contains_key(key)
        {
            return Ok(ReferenceInput::Indexed);
        }
        let Some(text) = dependencies
            .observations
            .cached_text(self.shared_disk, file, key)
        else {
            return Ok(ReferenceInput::Cold(None));
        };
        if let Some(declarations) = self.shared_disk.cached_declarations(&text) {
            record(self.navigation, |counts| counts.text_hits += 1);
            let fingerprint = text_fingerprint(&text.text);
            self.remember_declarations(
                key.to_owned(),
                PreparedDeclarations {
                    declarations,
                    text,
                    fingerprint,
                    path: file.to_owned(),
                },
            );
            return Ok(ReferenceInput::Indexed);
        }
        if self
            .shared_disk
            .cached_reference_identifiers(&text)
            .and_then(|names| names.offsets(spelling).map(<[_]>::is_empty))
            == Some(true)
            || !super::references::contains_spelling(&text.text, spelling, dependencies.cancelled)?
        {
            record(self.navigation, |counts| counts.text_hits += 1);
            return Ok(ReferenceInput::Absent);
        }
        Ok(ReferenceInput::Cold(Some(text)))
    }

    pub(super) fn find(
        &mut self,
        package: &Package,
        spelling: &str,
        lookup: impl Fn(&FileIndex) -> Option<Range>,
    ) -> Result<Option<(PathBuf, Range)>> {
        if self.reference_dependencies.is_some() {
            return self.find_reference_index(package, spelling, lookup);
        }
        let files = self.lookup_files(package)?;
        for file in files.iter(PackageLookup::Symbol(spelling, None)) {
            if let Some(range) = lookup(self.file_index(file)?) {
                return Ok(Some((file.to_owned(), range)));
            }
        }
        Ok(None)
    }

    pub(super) fn find_dot_import(
        &mut self,
        workspace: &Workspace,
        from: &Package,
        imports: &[Import],
        name: &str,
    ) -> Result<Option<(PathBuf, Range)>> {
        let mut found = None;
        for import in imports
            .iter()
            .filter(|import| import.alias.as_deref() == Some("."))
        {
            check_optional_cancelled(self.cancelled)?;
            if let Some(package) = workspace.resolve_import(from, &import.path)
                && let Some(location) = self.find(package, name, |index| {
                    index.symbols.declaration(name).map(|symbol| symbol.range)
                })?
            {
                if found.is_some() {
                    return Ok(None);
                }
                found = Some(location);
            }
        }
        Ok(found)
    }
}

#[derive(Eq, Hash, PartialEq)]
pub(super) enum ContentIdentity {
    Buffer(i32),
    Disk(u64),
}

pub(super) fn missing_source(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}

impl IndexReader<'_> {
    fn captured_implementation_text(&self, key: &Path) -> Option<NavigationText> {
        if !self.use_captured_disk {
            return None;
        }
        self.request_declarations
            .entries
            .get(key)
            .map(|source| source.text.clone())
            .or_else(|| {
                self.navigation_texts
                    .get(key)
                    .and_then(CachedNavigationText::upgrade)
            })
    }

    fn cached_declaration_source(&self, path: &Path, key: &Path) -> Option<PreparedDeclarations> {
        let index = self.disk_index.get(key)?;
        let text = index.text.as_ref()?.upgrade()?;
        if !self.shared_disk.validates_read(path, &text) {
            return None;
        }
        let declarations = self.shared_disk.cached_declarations(&text)?;
        Some(PreparedDeclarations {
            declarations,
            text,
            fingerprint: index.fingerprint,
            path: path.to_owned(),
        })
    }

    fn load_declarations_for(
        &mut self,
        file: &Path,
        text: Option<NavigationText>,
    ) -> Result<PreparedDeclarations> {
        let phase = self.trace().map(|trace| trace.phase("file_index"));
        let result = match text {
            Some(text) if text.is_captured() => self.shared_disk.declarations_snapshot(
                self.parser,
                file,
                text,
                self.cancelled,
                self.navigation,
            ),
            text => self.shared_disk.load_declarations(
                self.parser,
                file,
                text,
                self.cancelled,
                self.navigation,
            ),
        };
        drop(phase);
        result
    }

    fn ensure_declarations(&mut self, file: &Path, key: &Path) -> Result<()> {
        self.ensure_declarations_in_batch(file, key, false)
    }

    fn ensure_declarations_in_batch(&mut self, file: &Path, key: &Path, batch: bool) -> Result<()> {
        if let Some(cancelled) = self.cancelled {
            check_cancelled(cancelled)?;
        }
        let pinned = self.request_declarations.entries.contains_key(key);
        record(self.navigation, |counts| counts.declaration_access(pinned));
        if pinned {
            return Ok(());
        }
        if let Some(observations) = &mut self.diagnostic_observations {
            let observed = observations.observe(file, self.shared_disk.generation(), batch);
            if let Some(prepared) = self
                .shared_disk
                .cached_declaration_read(file, key, observed)
            {
                self.request_declarations
                    .insert(key.to_owned(), Rc::new(prepared));
                return Ok(());
            }
        }
        self.index_navigation_file(file, key)
    }

    fn index_navigation_file(&mut self, file: &Path, key: &Path) -> Result<()> {
        let started = self.implementation_parse_time.map(|_| Instant::now());
        let probe = self.navigation;
        record(probe, |counts| counts.scanned_files += 1);
        if let Some(memo) = self.foreground {
            let prepared = self
                .shared_disk
                .load_declarations_foreground(file, memo, probe)?;
            self.request_declarations
                .insert(key.to_owned(), Rc::new(prepared));
            return Ok(());
        }
        if let Some(saved) = self
            .completion
            .as_mut()
            .and_then(|request| request.declarations(key))
        {
            if saved.text.is_captured() || self.shared_disk.validates_read(file, &saved.text) {
                self.request_declarations.clear();
                self.request_declarations.insert(key.to_owned(), saved);
                return Ok(());
            }
        }
        let captured = self.captured_implementation_text(key).or_else(|| {
            self.reference_dependencies?
                .observations
                .cached_text(self.shared_disk, file, key)
        });
        let prepared = if let Some(text) = captured {
            // Candidate discovery and exact matching use the same bytes.
            self.shared_disk.declarations_snapshot(
                self.parser,
                file,
                text,
                self.cancelled,
                probe,
            )?
        } else if let Some(cached) = self.cached_declaration_source(file, key) {
            cached
        } else {
            let text = if self.completion.is_some() {
                Some(self.completion_text(file, key, false, false)?)
            } else {
                self.cached_navigation_text(key)
            };
            self.load_declarations_for(file, text)?
        };
        if let Some(dependencies) = self.reference_dependencies {
            dependencies
                .observations
                .remember(key, Some(&prepared.text));
        }
        self.remember_declarations(key.to_owned(), prepared);
        if let Some(started) = started {
            *self.implementation_parse_time.as_mut().unwrap() += started.elapsed();
        }
        Ok(())
    }

    fn remember_declarations(&mut self, key: PathBuf, prepared: PreparedDeclarations) {
        if self
            .shared_disk
            .validates_read(&prepared.path, &prepared.text)
        {
            let mut index = DiskIndex::from_declarations(&prepared);
            if let Some(previous) = self.disk_index.get_mut(&key)
                && previous.matches_revision(&prepared.text)
            {
                index.reference_source = previous.reference_source.take();
            }
            self.disk_index.insert(key.clone(), index);
        }
        let prepared = Rc::new(prepared);
        if let Some(request) = &mut self.completion {
            self.request_declarations.clear();
            request.store(
                key.clone(),
                CompletionSource::Declarations(Rc::clone(&prepared)),
            );
        }
        self.request_declarations.insert(key, prepared);
    }

    pub(super) fn candidate_index(&mut self, file: &Path) -> Result<Option<&FileIndex>> {
        let key = path_key(file);
        self.check_pending_overlay(&key)?;
        if let Some(source) = self.documents.get(&key) {
            return Ok(Some(source.declarations()));
        }
        if !self.request_declarations.entries.contains_key(&key) {
            let text = match self.captured_implementation_text(&key) {
                Some(text) => text.text,
                None => self.navigation_text(file, &key)?.0,
            };
            if !text.contains("type") {
                return Ok(None);
            }
            self.ensure_declarations(file, &key)?;
        }
        Ok(Some(
            &self.request_declarations.entries[&key].declarations.index,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_postings_follow_the_open_document_revision() {
        let cancelled = AtomicBool::new(false);
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
            .unwrap();
        let source = Source::parse(
            &mut parser,
            "sample.go".into(),
            "package sample\nvar Before int\n".into(),
            None,
        )
        .unwrap();
        let mut document = IndexedSource::new(source, Some(1));
        let snapshot = document.revision().into_indexed();
        let before = document.reference_identifiers(&cancelled).unwrap();
        assert!(Arc::ptr_eq(
            &before,
            &snapshot.reference_identifiers(&cancelled).unwrap(),
        ));
        document
            .edit(
                &mut parser,
                &[TextDocumentContentChangeEvent {
                    text: "package sample\nvar After int\n".into(),
                    range: None,
                    range_length: None,
                }],
                2,
                None,
            )
            .unwrap();
        let after = document.reference_identifiers(&cancelled).unwrap();
        assert_eq!(before.offsets("Before").unwrap().len(), 1);
        assert!(after.offsets("Before").unwrap().is_empty());
        assert_eq!(after.offsets("After").unwrap().len(), 1);
    }

    #[test]
    fn an_index_built_after_snapshot_capture_is_shared_only_for_its_revision() {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
            .unwrap();
        let source = Source::parse(
            &mut parser,
            "sample.go".into(),
            "package sample\nvar Before int\n".into(),
            None,
        )
        .unwrap();
        let mut document = IndexedSource::new(source, Some(1));
        let snapshot = document.revision().into_indexed();
        assert!(Arc::ptr_eq(
            document.declaration_summary().unwrap(),
            snapshot.declaration_summary().unwrap()
        ));
        assert!(snapshot.declarations.get().is_none());
        assert!(std::ptr::eq(
            document.declarations(),
            snapshot
                .cancellable_declarations(&AtomicBool::new(false))
                .unwrap()
        ));

        document
            .edit(
                &mut parser,
                &[TextDocumentContentChangeEvent {
                    text: "package sample\nvar After int\n".into(),
                    range: None,
                    range_length: None,
                }],
                2,
                None,
            )
            .unwrap();
        assert_ne!(
            document.declaration_summary(),
            snapshot.declaration_summary()
        );
        assert!(
            document
                .declarations()
                .symbols
                .declaration("After")
                .is_some()
        );
        assert!(
            snapshot
                .declarations()
                .symbols
                .declaration("Before")
                .is_some()
        );
        assert!(!std::ptr::eq(
            document.declarations(),
            snapshot.declarations()
        ));
    }
    use crate::semantic::Engine;

    #[test]
    fn indexes_public_and_private_symbols_in_grouped_declarations() {
        let mut engine = Engine::new(PathBuf::new());
        let source = Source::parse(
            &mut engine.state.parser,
            PathBuf::new(),
            "package test\nvar (First, Second = 1, 2)\nconst (Third, fourth = 3, 4)\ntype (Fifth int)\nfunc Sixth() {}\n".into(),
            None,
        )
        .unwrap();
        let source = IndexedSource::new(source, None);
        let names = &source.declarations().symbols;
        for name in ["First", "Second", "Third", "Fifth", "Sixth"] {
            assert!(names.declaration(name).is_some(), "missing {name}");
        }
        assert!(names.declaration("fourth").is_some());
    }

    #[test]
    fn completion_source_cache_bounds_count_and_bytes_and_drops_the_sources() {
        let mut engine = Engine::new(PathBuf::new());
        let mut file = |name: &str, len: usize| {
            let path = PathBuf::from(name);
            let mut source = Source::parse(
                &mut engine.state.parser,
                path.clone(),
                "package sample\n".into(),
                None,
            )
            .unwrap();
            source.text = " ".repeat(len).into();
            (path, Rc::new(IndexedSource::new(source, None)))
        };
        let mut cache = CompletionRead::new(None);
        let mut weak = Vec::new();
        for index in 0..=SOURCE_LIMIT {
            let (path, source) = file(&format!("{index}.go"), 64);
            weak.push(Rc::downgrade(&source));
            cache.store(path, CompletionSource::Syntax(source));
        }
        assert_eq!(cache.sources.len(), SOURCE_LIMIT);
        assert!(weak[0].upgrade().is_none());
        let (path, source) = file("large.go", BYTE_LIMIT);
        weak.push(Rc::downgrade(&source));
        cache.store(path, CompletionSource::Syntax(source));
        assert_eq!(cache.sources.len(), 1);
        assert_eq!(cache.bytes, BYTE_LIMIT);
        let (path, source) = file("oversize.go", BYTE_LIMIT + 1);
        weak.push(Rc::downgrade(&source));
        cache.store(path, CompletionSource::Syntax(source));
        assert_eq!(cache.sources.len(), 1);
        assert!(weak.last().unwrap().upgrade().is_none());
        drop(cache);
        assert!(weak.iter().all(|source| source.upgrade().is_none()));
    }
}
