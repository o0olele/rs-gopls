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
    prefilter::{BYTE_LIMIT, FileNames, SOURCE_LIMIT, Stamp},
    render::package_name,
    sdk_names::scan_function_bodies,
};
use super::navigation_trace::{NavigationProbe, record};
use super::references::{DependencySource, ReferenceDependencies};
use super::shared_disk::{FileReadTicket, SharedDiskCache};
use super::source::{Import, Source, SourceRevision, TextEditDelta, check_cancelled, node_text};
use crate::symbols::{self, FileSymbols, TypeDetails};
use crate::workspace::{Package, Workspace, path_key};

mod declaration_summary;
mod reference_identifiers;

use declaration_summary::DeclarationSummary;
pub(super) use reference_identifiers::ReferenceIdentifiers;

pub(super) struct FileIndex {
    pub(super) symbols: FileSymbols,
    pub(super) imports: Arc<[Import]>,
    pub(super) package_signature: u64,
    pub(super) package_name: Option<String>,
}

impl FileIndex {
    fn package_signature(source: &Source) -> u64 {
        let root = source.tree.root_node();
        let mut cursor = root.walk();
        let mut signature = DefaultHasher::new();
        hash_build_constraints(&source.text, &mut signature);
        for node in root.named_children(&mut cursor) {
            match node.kind() {
                "package_clause" | "import_declaration" => {
                    node_text(node, &source.text).hash(&mut signature)
                }
                "comment" => {}
                _ => break,
            }
        }
        signature.finish()
    }

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
            package_signature: Self::package_signature(source),
            package_name: package_name(source).map(str::to_owned),
        }
    }
}

fn hash_build_constraints(text: &str, signature: &mut DefaultHasher) {
    // go/build accepts legacy constraints in the leading run of line comments
    // only through its last blank line. Modern directives take precedence.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut legacy = Vec::new();
    let mut legacy_end = 0;
    let mut modern = Vec::new();
    let mut leading_line_comments = true;
    let mut block_comment = false;
    'lines: for line in text.lines() {
        let mut line = line.trim();
        if leading_line_comments {
            if line.is_empty() {
                legacy_end = legacy.len();
                continue;
            }
            if let Some(comment) = line.strip_prefix("//") {
                if let Some(expression) = build_expression(comment.trim_start(), "+build") {
                    legacy.push(expression);
                }
            } else {
                leading_line_comments = false;
            }
        }
        if !block_comment && let Some(expression) = build_expression(line, "//go:build") {
            modern.push(expression);
        }
        while !line.is_empty() {
            if block_comment {
                let Some((_, rest)) = line.split_once("*/") else {
                    continue 'lines;
                };
                line = rest.trim_start();
                block_comment = false;
            } else if line.starts_with("//") {
                continue 'lines;
            } else if let Some(rest) = line.strip_prefix("/*") {
                line = rest.trim_start();
                block_comment = true;
            } else {
                break 'lines;
            }
        }
    }
    modern.hash(signature);
    if modern.is_empty() {
        legacy[..legacy_end].hash(signature);
    }
}

fn build_expression<'a>(line: &'a str, directive: &str) -> Option<&'a str> {
    let suffix = line.strip_prefix(directive)?;
    let expression = suffix.trim_start();
    (suffix.is_empty() || suffix.len() != expression.len()).then_some(expression)
}

pub(super) struct IndexedSource {
    source: Source,
    declarations: Arc<OnceLock<Arc<FileIndex>>>,
    declaration_summary: Arc<OnceLock<Option<Arc<DeclarationSummary>>>>,
    reference_identifiers: Arc<OnceLock<Arc<ReferenceIdentifiers>>>,
    pub(super) document_version: Option<i32>,
    pub(super) open_generation: u64,
    pub(super) content_revision: u64,
    pub(super) declaration_revision: u64,
    declarations_only: bool,
}

#[derive(Clone)]
pub(super) struct IndexedSourceRevision {
    source: SourceRevision,
    declarations: Arc<OnceLock<Arc<FileIndex>>>,
    declaration_summary: Arc<OnceLock<Option<Arc<DeclarationSummary>>>>,
    reference_identifiers: Arc<OnceLock<Arc<ReferenceIdentifiers>>>,
    document_version: Option<i32>,
    open_generation: u64,
    content_revision: u64,
    declaration_revision: u64,
    pub(super) declarations_only: bool,
}

impl IndexedSourceRevision {
    pub(super) fn into_indexed(self) -> IndexedSource {
        IndexedSource {
            source: self.source.into_source(),
            declarations: self.declarations,
            declaration_summary: self.declaration_summary,
            reference_identifiers: self.reference_identifiers,
            document_version: self.document_version,
            open_generation: self.open_generation,
            content_revision: self.content_revision,
            declaration_revision: self.declaration_revision,
            declarations_only: self.declarations_only,
        }
    }
}

impl IndexedSource {
    pub(super) fn new(source: Source, document_version: Option<i32>) -> Self {
        Self {
            source,
            declarations: Arc::default(),
            declaration_summary: Arc::default(),
            reference_identifiers: Arc::default(),
            document_version,
            open_generation: 0,
            content_revision: 0,
            declaration_revision: 0,
            declarations_only: false,
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
        check_cancelled(cancelled)?;
        if let Some(declarations) = self.declarations.get() {
            return Ok(declarations);
        }
        let declarations = Arc::new(FileIndex::cancellable(&self.source, cancelled)?);
        check_cancelled(cancelled)?;
        Ok(self.declarations.get_or_init(|| declarations))
    }

    pub(super) fn revision(&self) -> IndexedSourceRevision {
        IndexedSourceRevision {
            source: self.source.revision(),
            declarations: self.declarations.clone(),
            declaration_summary: Arc::clone(&self.declaration_summary),
            reference_identifiers: Arc::clone(&self.reference_identifiers),
            document_version: self.document_version,
            open_generation: self.open_generation,
            content_revision: self.content_revision,
            declaration_revision: self.declaration_revision,
            declarations_only: self.declarations_only,
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
    pub(super) declarations: Arc<FileIndex>,
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
            declarations: prepared
                .revision
                .declarations
                .get()
                .expect("prepared source must have declarations")
                .clone(),
            fingerprint: prepared.fingerprint,
            stamp: prepared.text.stamp,
            reference_source: None,
        }
    }

    pub(super) fn references_text(&self, text: *const String) -> bool {
        self.reference_source
            .as_ref()
            .is_some_and(|source| source.text.text.as_ptr() == text)
    }

    #[cfg(test)]
    pub(super) fn new(source: &Source) -> Self {
        Self {
            declarations: Arc::new(FileIndex::new(source)),
            fingerprint: source.fingerprint(),
            stamp: None,
            reference_source: None,
        }
    }

    fn from_indexed(source: &IndexedSource, fingerprint: u64) -> Self {
        source.declarations();
        Self {
            declarations: Arc::clone(source.declarations.get().unwrap()),
            fingerprint,
            stamp: None,
            reference_source: None,
        }
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
    pub(super) fn read_cancellable(path: &Path, cancelled: &AtomicBool) -> Result<Self> {
        Self::read_chunks(path, cancelled).with_context(|| format!("reading {}", path.display()))
    }

    fn read_chunks(path: &Path, cancelled: &AtomicBool) -> Result<Self> {
        check_cancelled(cancelled)?;
        let mut file = fs::File::open(path)?;
        let metadata = file.metadata().ok();
        let length = metadata
            .as_ref()
            .and_then(|metadata| usize::try_from(metadata.len()).ok())
            .unwrap_or_default();
        let mut text = Vec::with_capacity(length);
        let mut buffer = [0; 64 * 1024];
        loop {
            check_cancelled(cancelled)?;
            match file.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => text.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            }
        }
        let text = String::from_utf8(text)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.utf8_error()))?;
        check_cancelled(cancelled)?;
        Ok(Self {
            text: Arc::new(text),
            stamp: metadata.map(Stamp::from),
            read_ticket: None,
        })
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
    pub(super) documents: &'a HashMap<PathBuf, IndexedSource>,
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
}

enum ReferenceInput {
    Indexed,
    Absent,
    Cold(Option<NavigationText>),
}

type TypeDetailsMemo = HashMap<String, Option<(PathBuf, TypeDetails)>>;

pub(super) struct CompletionRead {
    pub(super) trace: Option<Trace>,
    pub(super) prefetched: HashMap<PathBuf, String>,
    sources: VecDeque<(PathBuf, Rc<IndexedSource>)>,
    pub(super) document: Option<(Rc<IndexedSource>, Stamp)>,
    bytes: usize,
    // The completion task owns this value for a single graph and set of
    // overlays. Edits and graph replacements discard the task and its keys.
    types: HashMap<String, HashMap<String, Option<(PathBuf, TypeDetails)>>>,
    pub(super) type_kinds: HashMap<(String, String), (Option<CompletionItemKind>, bool)>,
}

impl CompletionRead {
    pub(super) fn new(trace: Option<Trace>) -> Self {
        Self {
            trace,
            prefetched: HashMap::new(),
            sources: VecDeque::new(),
            document: None,
            bytes: 0,
            types: HashMap::new(),
            type_kinds: HashMap::new(),
        }
    }

    fn get(&mut self, key: &Path, allow_elided: bool) -> Option<Rc<IndexedSource>> {
        let position = self.sources.iter().position(|(path, source)| {
            path == key && (allow_elided || !source.declarations_only)
        })?;
        let entry = self.sources.remove(position).unwrap();
        let source = Rc::clone(&entry.1);
        self.sources.push_back(entry);
        Some(source)
    }

    fn store(&mut self, key: PathBuf, source: Rc<IndexedSource>) {
        let len = source.text.len();
        let doc_size = self.document.as_ref().map_or(0, |(doc, _)| doc.text.len());
        if len > BYTE_LIMIT - doc_size {
            return;
        }
        if let Some(position) = self.sources.iter().position(|(path, _)| path == &key) {
            self.bytes -= self.sources.remove(position).unwrap().1.text.len();
        }
        self.make_room(1, len);
        self.bytes += len;
        self.sources.push_back((key, source));
        self.report_size();
    }

    pub(super) fn pin_document(&mut self, source: IndexedSource, stamp: Stamp) {
        let len = source.text.len();
        if len <= BYTE_LIMIT {
            self.make_room(1, len);
            self.bytes += len;
            self.document = Some((Rc::new(source), stamp));
            self.report_size();
        }
    }

    fn make_room(&mut self, count: usize, len: usize) {
        let pinned = usize::from(self.document.is_some());
        while self.sources.len() + pinned + count > SOURCE_LIMIT || self.bytes + len > BYTE_LIMIT {
            self.bytes -= self.sources.pop_front().unwrap().1.text.len();
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
    pub(super) fn retained_sources(&self) -> Vec<std::rc::Weak<IndexedSource>> {
        self.sources
            .iter()
            .map(|(_, source)| Rc::downgrade(source))
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
        indexed.source.path = path;
        indexed
    }
}

impl IndexedSourceRevision {
    pub(super) fn parse_disk(
        parser: &mut Parser,
        path: &Path,
        text: Arc<String>,
        cancelled: &AtomicBool,
        allow_elided: bool,
    ) -> Result<(Self, usize)> {
        let bodies = allow_elided.then(|| scan_function_bodies(&text)).flatten();
        let (source, declarations_only) = if let Some(bodies) = bodies {
            Source::parse_dependency_declarations(parser, path, text, &bodies, cancelled)?
        } else {
            (
                Source::parse_cancellable(parser, path, text, cancelled)?,
                false,
            )
        };
        let source = IndexedSource {
            declarations_only,
            ..IndexedSource::new(source, None)
        };
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
    pub(super) fn navigation_text(
        &mut self,
        path: &Path,
        key: &Path,
    ) -> Result<(Arc<String>, bool)> {
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
        self.get_source(path, false)
    }

    pub(super) fn cached_reference_source(&self, key: &Path) -> Option<ReferenceSource<'a>> {
        if let Some(source) = self.documents.get(key) {
            return Some(ReferenceSource::Open(source));
        }
        if let Some((origin, source)) = &self.owned_reference
            && *origin == key
        {
            return Some(ReferenceSource::Disk(Rc::clone(source)));
        }
        let cached = self.disk_index.get(key)?.reference_source.as_ref()?;
        let source = cached.revision.upgrade()?;
        if self.reference_dependencies.is_none()
            && !cached.text.ticket.as_ref().is_some_and(|ticket| {
                self.shared_disk
                    .validates_ticket(key, ticket, cached.text.text.upgrade().as_ref())
            })
        {
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
        if self.documents.contains_key(&key)
            || !self.shared_disk.validates_read(&key, &prepared.text)
        {
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
    ) -> Result<Option<ReferenceSource<'a>>> {
        self.get_source(path, true)
    }

    fn get_source(
        &mut self,
        path: &Path,
        allow_elided: bool,
    ) -> Result<Option<ReferenceSource<'a>>> {
        let key = path_key(path);
        let probe = self.navigation;
        record(probe, |counts| counts.scanned_files += 1);
        if let Some(source) = self.cached_reference_source(&key) {
            record(probe, |counts| counts.ast_access(true));
            return Ok(Some(source));
        }
        record(probe, |counts| counts.ast_access(false));
        if let Some(request) = &mut self.completion {
            if let Some(source) = request.get(&key, allow_elided) {
                return Ok(Some(ReferenceSource::Disk(source)));
            }
            return match self.completion_source(path, &key, allow_elided) {
                Ok(source) => Ok(Some(ReferenceSource::Disk(source))),
                Err(error) if missing_source(&error) => Ok(None),
                Err(error) => Err(error),
            };
        }
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

    fn load_navigation_source(
        &mut self,
        path: &Path,
        key: &Path,
    ) -> Result<PreparedReferenceSource> {
        if let Some(dependencies) = self.reference_dependencies {
            let prefetched = dependencies
                .observations
                .cached_text(self.shared_disk, path, key);
            if let Some(source) = prefetched
                .as_ref()
                .and_then(|text| self.shared_disk.cached_parse(path, text, false))
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
        let prefetched = self
            .navigation_texts
            .get(key)
            .and_then(CachedNavigationText::upgrade);
        match (prefetched, self.cancelled) {
            (Some(text), None) => {
                // Exact implementation matching follows the bytes used by
                // candidate discovery even if the file changes mid-search.
                self.shared_disk
                    .parse_snapshot(self.parser, path, text, self.navigation)
            }
            (prefetched, cancelled) => self.shared_disk.load_prefetched(
                self.parser,
                path,
                prefetched,
                cancelled,
                self.navigation,
            ),
        }
    }

    fn store_disk_source(
        &mut self,
        key: PathBuf,
        prepared: PreparedReferenceSource,
    ) -> Rc<IndexedSource> {
        self.store_reference_index(key, &prepared);
        Rc::new(prepared.into_indexed())
    }

    fn store_reference_index(&mut self, key: PathBuf, prepared: &PreparedReferenceSource) {
        let mut index = DiskIndex::from_prepared(prepared);
        index.reference_source =
            (!prepared.revision.declarations_only).then(|| CachedReferenceSource {
                revision: Arc::downgrade(&prepared.revision),
                text: prepared.text.downgrade(),
                path: prepared.path.clone(),
            });
        let hit = self
            .disk_index
            .get(&key)
            .is_some_and(|disk| disk.fingerprint == index.fingerprint);
        record(self.navigation, |counts| counts.declaration_access(hit));
        self.disk_index.insert(key, index);
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

    pub(super) fn file_index(&mut self, file: &Path) -> Result<&FileIndex> {
        let key = path_key(file);
        self.file_index_at(file, &key)
    }

    fn file_index_at(&mut self, file: &Path, key: &Path) -> Result<&FileIndex> {
        if let Some(source) = self.documents.get(key) {
            record(self.navigation, |counts| {
                counts.declaration_access(source.declarations.get().is_some());
            });
            return if let Some(cancelled) = self.cancelled {
                source.cancellable_declarations(cancelled)
            } else {
                Ok(source.declarations())
            };
        }
        let indexed = self.disk_index.contains_key(key);
        record(self.navigation, |counts| counts.declaration_access(indexed));
        if self.completion.is_some() {
            if !indexed {
                self.completion_source(file, key, true)?;
            }
            return Ok(&self.disk_index[key].declarations);
        }
        if !indexed {
            self.index_navigation_file(file, key, false)?;
        }
        Ok(&self.disk_index[key].declarations)
    }

    pub(super) fn trace(&self) -> Option<&Trace> {
        self.completion.as_ref()?.trace.as_ref()
    }

    pub(super) fn known_index(&self, key: &Path) -> Option<&FileIndex> {
        if let Some(source) = self.documents.get(key) {
            return Some(source.declarations());
        }
        self.disk_index
            .get(key)
            .map(|entry| entry.declarations.as_ref())
    }

    pub(super) fn content_identity(&self, key: &Path) -> Option<ContentIdentity> {
        self.documents
            .get(key)
            .and_then(|source| source.document_version)
            .map(ContentIdentity::Buffer)
            .or_else(|| {
                self.disk_index
                    .get(key)
                    .map(|index| ContentIdentity::Disk(index.fingerprint))
            })
    }

    fn completion_source(
        &mut self,
        path: &Path,
        key: &Path,
        allow_elided: bool,
    ) -> Result<Rc<IndexedSource>> {
        let text = self.completion_text(path, key, false, false)?;
        self.index_text(path, key, text, allow_elided)
    }

    fn completion_text(
        &mut self,
        path: &Path,
        key: &Path,
        prefilter: bool,
        project: bool,
    ) -> Result<String> {
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
        if let Some(trace) = self.trace() {
            trace.read(key, prefilter, project, self.disk_index.contains_key(key));
        }
        fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
    }

    fn index_text(
        &mut self,
        path: &Path,
        key: &Path,
        text: String,
        allow_elided: bool,
    ) -> Result<Rc<IndexedSource>> {
        let _phase = self.trace().map(|trace| trace.phase("file_index"));
        let sample = self
            .trace()
            .map(|trace| (trace.clone(), std::time::Instant::now()));
        let fingerprint = text_fingerprint(&text);
        let external = self.workspace.package_of_file(path).is_some_and(|package| {
            package.is_external() && !self.workspace.is_project_package(package)
        });
        if external
            && allow_elided
            && !self.disk_names.contains_key(key)
            && let Ok(metadata) = fs::metadata(path)
        {
            self.disk_names
                .insert(key.to_owned(), FileNames::new(metadata.into(), &text, true));
        }
        let names = self
            .disk_names
            .get(key)
            .filter(|names| names.fingerprint == fingerprint);
        let bodies = if allow_elided {
            names
                .filter(|names| names.elide_bodies)
                .and_then(|names| names.names.as_ref())
                .map_or(&[][..], |names| names.bodies.as_slice())
        } else {
            &[]
        };
        let (source, declarations_only) = {
            let _parse = self.trace().map(|trace| trace.phase("tree_sitter"));
            Source::parse_declarations(self.parser, path.to_owned(), text, bodies)?
        };
        if let Some(trace) = self.trace() {
            if declarations_only {
                trace.declaration_parse();
            } else {
                trace.parse();
            }
        }
        let source = Rc::new(IndexedSource {
            declarations_only,
            ..IndexedSource::new(source, None)
        });
        if self
            .disk_index
            .get(key)
            .is_none_or(|entry| entry.fingerprint != fingerprint)
        {
            let _phase = self.trace().map(|trace| trace.phase("index_summaries"));
            self.disk_index.insert(
                key.to_owned(),
                DiskIndex::from_indexed(&source, fingerprint),
            );
        }
        if let Some(request) = &mut self.completion {
            request.store(key.to_owned(), Rc::clone(&source));
        }
        if let Some((trace, started)) = sample {
            trace.index_time(path, source.text.len(), declarations_only, started);
        }
        Ok(source)
    }

    pub(super) fn type_details(
        &mut self,
        package: &Package,
        name: &str,
        project: bool,
    ) -> Result<Option<(PathBuf, TypeDetails)>> {
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
        for file in self.files(package).iter() {
            let index = if self.completion.is_some() && !package.load_error {
                self.filtered_index(file, project, package.is_external(), |names| {
                    names.may_declare_type(name)
                })?
            } else {
                Some(self.file_index(file)?)
            };
            if let Some(details) = index.and_then(|index| index.symbols.type_details(name)) {
                result = Some((file.clone(), details.clone()));
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
        external: bool,
        mut accepts: impl FnMut(&FileNames) -> bool,
    ) -> Result<Option<&FileIndex>> {
        let key = path_key(file);
        if self.documents.contains_key(&key) || self.disk_index.contains_key(&key) {
            return Ok(self.known_index(&key));
        }
        if let Some(names) = self.disk_names.get(&key) {
            return if accepts(names) {
                self.file_index(file).map(Some)
            } else {
                Ok(None)
            };
        }
        let text = self.completion_text(file, &key, true, project)?;
        if let Ok(metadata) = fs::metadata(file) {
            let names = FileNames::new(metadata.into(), &text, external && !project);
            let matches = accepts(&names);
            self.disk_names.insert(key.clone(), names);
            if !matches {
                return Ok(None);
            }
        }
        self.index_text(file, &key, text, true)?;
        Ok(self.known_index(&key))
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
                                        self.store_reference_index(key.clone(), &source);
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
        if self.documents.contains_key(key) || self.disk_index.contains_key(key) {
            return Ok(ReferenceInput::Indexed);
        }
        let Some(text) = dependencies
            .observations
            .cached_text(self.shared_disk, file, key)
        else {
            return Ok(ReferenceInput::Cold(None));
        };
        if let Some(source) = self.shared_disk.cached_parse(file, &text, true) {
            record(self.navigation, |counts| counts.text_hits += 1);
            self.store_reference_index(key.to_owned(), &source);
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
        for file in self.files(package).iter() {
            if let Some(range) = lookup(self.file_index(file)?) {
                return Ok(Some((file.clone(), range)));
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
    fn index_navigation_file(&mut self, file: &Path, key: &Path, needs_type: bool) -> Result<()> {
        let started = self.implementation_parse_time.map(|_| Instant::now());
        let probe = self.navigation;
        record(probe, |counts| counts.ast_access(false));
        if needs_type {
            let (text, _) = self.navigation_text(file, key)?;
            if !text.contains("type") {
                return Ok(());
            }
        }
        let prepared = self.load_navigation_source(file, key)?;
        self.store_reference_index(key.to_owned(), &prepared);
        if let Some(started) = started {
            *self.implementation_parse_time.as_mut().unwrap() += started.elapsed();
        }
        Ok(())
    }

    pub(super) fn candidate_index(&mut self, file: &Path) -> Result<Option<&FileIndex>> {
        let key = path_key(file);
        if let Some(source) = self.documents.get(&key) {
            return Ok(Some(source.declarations()));
        }
        if !self.disk_index.contains_key(&key) {
            self.index_navigation_file(file, &key, true)?;
        }
        Ok(self
            .disk_index
            .get(&key)
            .map(|index| index.declarations.as_ref()))
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
            cache.store(path, source);
        }
        assert_eq!(cache.sources.len(), SOURCE_LIMIT);
        assert!(weak[0].upgrade().is_none());
        let (path, source) = file("large.go", BYTE_LIMIT);
        weak.push(Rc::downgrade(&source));
        cache.store(path, source);
        assert_eq!(cache.sources.len(), 1);
        assert_eq!(cache.bytes, BYTE_LIMIT);
        let (path, source) = file("oversize.go", BYTE_LIMIT + 1);
        weak.push(Rc::downgrade(&source));
        cache.store(path, source);
        assert_eq!(cache.sources.len(), 1);
        assert!(weak.last().unwrap().upgrade().is_none());
        drop(cache);
        assert!(weak.iter().all(|source| source.upgrade().is_none()));
    }
}
