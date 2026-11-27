use std::borrow::Cow;
use std::cell::OnceCell;
use std::collections::{HashMap, HashSet, VecDeque, hash_map::Entry};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use lsp_types::{CompletionItemKind, Range, TextDocumentContentChangeEvent};
use tree_sitter::Parser;

use super::completion::{
    InferredMemo, Trace,
    prefilter::{BYTE_LIMIT, FileNames, SOURCE_LIMIT, Stamp},
    render::{Description, describe_token, package_name},
};
use super::resolver::is_exported;
use super::source::{Import, Source, TextEditDelta, node_at_range, node_text, read_source};
use crate::symbols::{self, FileSymbols, TypeDetails};
use crate::workspace::{Package, Workspace, path_key};

pub(super) struct FileIndex {
    pub(super) symbols: FileSymbols,
    pub(super) imports: Vec<Import>,
    pub(super) package_signature: u64,
    pub(super) presentation: HashMap<Range, Description>,
    pub(super) package_name: Option<String>,
}

impl FileIndex {
    fn package_signature(source: &Source) -> u64 {
        let root = source.tree.root_node();
        let mut cursor = root.walk();
        let mut signature = DefaultHasher::new();
        for node in root.named_children(&mut cursor) {
            match node.kind() {
                "package_clause" | "import_declaration" => {
                    node_text(node, &source.text).hash(&mut signature)
                }
                "comment" => {
                    let comment = node_text(node, &source.text);
                    if comment.starts_with("//go:build") || comment.starts_with("// +build") {
                        comment.hash(&mut signature);
                    }
                }
                _ => break,
            }
        }
        signature.finish()
    }

    fn new(source: &Source, public_only: bool) -> Self {
        let mut presentation = HashMap::new();
        let kind = CompletionItemKind::VARIABLE;
        let symbols = symbols::collect(
            &source.tree,
            &source.text,
            |name| source.range(name),
            |token, name, symbol| {
                if public_only && !is_exported(name) {
                    return;
                }
                presentation.entry(symbol.range).or_insert_with(|| {
                    describe_token(Some(token), &source.text, name, kind, true, true)
                });
            },
        );
        for (name, symbol) in symbols.members() {
            if public_only && !is_exported(name) {
                continue;
            }
            presentation.entry(symbol.range).or_insert_with(|| {
                describe_token(
                    node_at_range(source, symbol.range),
                    &source.text,
                    name,
                    kind,
                    true,
                    true,
                )
            });
        }
        Self {
            symbols,
            imports: source.imports.clone(),
            package_signature: Self::package_signature(source),
            presentation,
            package_name: package_name(source).map(str::to_owned),
        }
    }
}

pub(super) struct IndexedSource {
    source: Source,
    declarations: OnceCell<Rc<FileIndex>>,
    pub(super) document_version: Option<i32>,
    declarations_only: bool,
    public_presentation: bool,
}

impl IndexedSource {
    pub(super) fn new(source: Source, document_version: Option<i32>) -> Self {
        Self {
            source,
            declarations: OnceCell::new(),
            document_version,
            declarations_only: false,
            public_presentation: false,
        }
    }

    pub(super) fn declarations(&self) -> &FileIndex {
        self.declarations
            .get_or_init(|| Rc::new(FileIndex::new(&self.source, self.public_presentation)))
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
        self.declarations.take();
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
    pub(super) declarations: Rc<FileIndex>,
    pub(super) fingerprint: u64,
    reference_source: Option<Rc<IndexedSource>>,
}

impl DiskIndex {
    pub(super) fn new(source: &Source) -> Self {
        Self {
            declarations: Rc::new(FileIndex::new(source, false)),
            fingerprint: text_fingerprint(&source.text),
            reference_source: None,
        }
    }

    fn from_indexed(source: &IndexedSource, fingerprint: u64) -> Self {
        source.declarations();
        Self {
            declarations: Rc::clone(source.declarations.get().unwrap()),
            fingerprint,
            reference_source: None,
        }
    }
}

pub(super) fn text_fingerprint(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

pub(super) struct NavigationText {
    text: String,
    stamp: Option<Stamp>,
}

impl NavigationText {
    pub(super) fn matches(&self, disk_text: Option<&str>) -> bool {
        Some(self.text.as_str()) == disk_text
    }

    pub(super) fn is_current(&self, path: &Path) -> bool {
        fs::metadata(path).is_ok_and(|metadata| self.stamp == Some(metadata.into()))
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
    pub(super) documents: &'a HashMap<PathBuf, IndexedSource>,
    pub(super) disk_index: &'a mut HashMap<PathBuf, DiskIndex>,
    pub(super) navigation_texts: &'a mut HashMap<PathBuf, NavigationText>,
    pub(super) implementation_parse_time: Option<Duration>,
    pub(super) drafts: DraftFiles<'a>,
    pub(super) disk_names: &'a mut HashMap<PathBuf, FileNames>,
    pub(super) completion: Option<&'a mut CompletionRead>,
    pub(super) inferred_weights: &'a mut HashMap<InferredMemo, i32>,
}

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

impl AsRef<IndexedSource> for ReferenceSource<'_> {
    fn as_ref(&self) -> &IndexedSource {
        match self {
            Self::Open(source) => source,
            Self::Disk(source) => source,
        }
    }
}

impl<'a> IndexReader<'a> {
    pub(super) fn navigation_text(&mut self, path: &Path, key: &Path) -> Result<(&str, bool)> {
        match self.navigation_texts.entry(key.to_owned()) {
            Entry::Occupied(entry) => Ok((&entry.into_mut().text, false)),
            Entry::Vacant(entry) => {
                let mut file = fs::File::open(path)?;
                let metadata = file.metadata().ok();
                let length = metadata
                    .as_ref()
                    .and_then(|m| usize::try_from(m.len()).ok())
                    .unwrap_or_default();
                let mut text = String::with_capacity(length);
                std::io::Read::read_to_string(&mut file, &mut text)?;
                let stamp = metadata.map(Stamp::from);
                Ok((&entry.insert(NavigationText { text, stamp }).text, true))
            }
        }
    }

    fn navigation_source(&mut self, path: &Path, key: &Path) -> Result<Source> {
        match self.navigation_texts.get(key) {
            Some(cached) => Source::parse(self.parser, path.to_owned(), cached.text.clone(), None),
            None => {
                self.navigation_text(path, key)
                    .with_context(|| format!("reading {}", path.display()))?;
                Source::parse(
                    self.parser,
                    path.to_owned(),
                    self.navigation_texts[key].text.clone(),
                    None,
                )
            }
        }
    }

    pub(super) fn reference_source(&mut self, path: &Path) -> Result<Option<ReferenceSource<'a>>> {
        self.get_source(path, false)
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
        if let Some(source) = self.documents.get(&key) {
            return Ok(Some(ReferenceSource::Open(source)));
        }
        if let Some(source) = self
            .disk_index
            .get(&key)
            .and_then(|entry| entry.reference_source.as_ref())
        {
            return Ok(Some(ReferenceSource::Disk(Rc::clone(source))));
        }
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
        let source = match self.navigation_source(path, &key) {
            Ok(source) => Rc::new(IndexedSource::new(source, None)),
            Err(error) if missing_source(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        let fingerprint = text_fingerprint(&source.text);
        let entry = match self.disk_index.entry(key) {
            Entry::Occupied(mut entry) => {
                if entry.get().fingerprint != fingerprint {
                    entry.insert(DiskIndex::from_indexed(&source, fingerprint));
                }
                entry.into_mut()
            }
            Entry::Vacant(entry) => entry.insert(DiskIndex::from_indexed(&source, fingerprint)),
        };
        entry.reference_source = Some(Rc::clone(&source));
        if let Some(started) = started {
            *self.implementation_parse_time.as_mut().unwrap() += started.elapsed();
        }
        Ok(Some(ReferenceSource::Disk(source)))
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
        if let Some(source) = self.documents.get(&key) {
            return Ok(source.declarations());
        }
        if self.completion.is_some() {
            if !self.disk_index.contains_key(&key) {
                self.completion_source(file, &key, true)?;
            }
            return Ok(&self.disk_index[&key].declarations);
        }
        if !self.disk_index.contains_key(&key) {
            self.index_navigation_file(file, &key, false)?;
        }
        Ok(&self.disk_index[&key].declarations)
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
            public_presentation: external || names.is_some_and(|names| names.elide_bodies),
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
            .completion
            .as_ref()
            .and_then(|request| request.types.get(&package.id)?.get(name))
        {
            if let Some(trace) = self.trace() {
                trace.type_location_hit();
            }
            return Ok(cached.clone());
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

    pub(super) fn find(
        &mut self,
        package: &Package,
        lookup: impl Fn(&FileIndex) -> Option<Range>,
    ) -> Result<Option<(PathBuf, Range)>> {
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
                && let Some(location) = self.find(package, |index| {
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

fn missing_source(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}

impl IndexReader<'_> {
    fn index_navigation_file(&mut self, file: &Path, key: &Path, needs_type: bool) -> Result<()> {
        let started = self.implementation_parse_time.map(|_| Instant::now());
        let from_probe = self.navigation_texts.contains_key(key);
        let source = if needs_type && !from_probe {
            let text =
                fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
            if !text.contains("type") {
                return Ok(());
            }
            Source::parse(self.parser, file.to_owned(), text, None)?
        } else if from_probe {
            if needs_type && !self.navigation_texts[key].text.contains("type") {
                return Ok(());
            }
            self.navigation_source(file, key)?
        } else {
            read_source(self.parser, file)?
        };
        let mut index = DiskIndex::new(&source);
        if from_probe {
            let indexed = IndexedSource::new(source, None);
            let _ = indexed.declarations.set(Rc::clone(&index.declarations));
            index.reference_source = Some(Rc::new(indexed));
        }
        self.disk_index.insert(key.to_owned(), index);
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
            source.text = " ".repeat(len);
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
