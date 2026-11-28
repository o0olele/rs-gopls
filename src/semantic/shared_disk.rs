use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs;
use std::hash::{Hash, Hasher};
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, OnceLock, Weak};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, bounded};
use tree_sitter::Parser;

use super::completion::prefilter::Stamp;
use super::completion::sdk_names::scan_function_bodies;
use super::diagnostics::has_reliable_package_scope;
use super::index::{
    IndexedSource, IndexedSourceRevision, NavigationText, PreparedReferenceSource,
    ReferenceIdentifiers, text_fingerprint,
};
use super::navigation_trace::{NavigationProbe, record, sample};
use super::package_index::{PackageDeclFile, PackageDeclIndex, PackageDeclKey};
use super::source::{EditorNames, Source, check_cancelled};
use crate::telemetry;
use crate::workspace::path_key;

mod declarations;
mod foreground;
mod parsing;
mod queue;
mod trace;
pub(super) use declarations::SharedDeclarations;
use foreground::CacheLock;
pub(crate) use foreground::QueryReadMemo;
pub(super) use foreground::{DiskSourceRev, QueryReadLimit};
pub(super) use parsing::NeedsFullDeclarations;
use queue::{LoadSlot, QueueWake};
use trace::{Artifact, FlightTrace, Operation, Removal, Retired};

const TEXT_ENTRIES: usize = 2048;
const TEXT_BYTES: usize = 64 * 1024 * 1024;
const PARSED_ENTRIES: usize = 1024;
const PARSED_BYTES: usize = 256 * 1024 * 1024;
const DECLARATION_ENTRIES: usize = 4096;
const DECLARATION_BYTES: usize = 64 * 1024 * 1024;
const DECLARATION_ITEM_BYTES: usize = 1024 * 1024;
const PACKAGE_NAMES_ENTRIES: usize = 4096;
const PACKAGE_NAMES_BYTES: usize = 16 * 1024 * 1024;
const REFERENCE_INDEX_ENTRIES: usize = 1024;
const REFERENCE_INDEX_BYTES: usize = 64 * 1024 * 1024;
const OBSERVATION_RETRIES: usize = 3;
const LOAD_QUEUE: usize = 32;
const MAX_LOAD_THREADS: usize = 8;
const SOURCE_FLIGHTS: usize = LOAD_QUEUE + MAX_LOAD_THREADS;
const CANCELLATION_INTERVAL: Duration = Duration::from_millis(5);
const INVALIDATION_HISTORY: usize = 128;

// A subscriber's cancellation does not cancel the shared producer.
static SHARED_LOAD: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Default, Debug, Eq, PartialEq)]
pub(crate) struct ColdDependencyCounts {
    pub(crate) reads: usize,
    pub(crate) parses: usize,
}

thread_local! {
    static DIAGNOSTIC_IO: RefCell<Option<(PathBuf, ColdDependencyCounts)>> = const { RefCell::new(None) };
}

/// Scoped to the diagnostic subscriber, including its part in shared flights.
pub(crate) struct DiagnosticIo(Option<(PathBuf, ColdDependencyCounts)>);

impl DiagnosticIo {
    pub(crate) fn start(document: &Path) -> Self {
        Self(DIAGNOSTIC_IO.replace(Some((document.to_owned(), ColdDependencyCounts::default()))))
    }

    pub(crate) fn counts(&self) -> ColdDependencyCounts {
        DIAGNOSTIC_IO.with_borrow(|active| {
            active
                .as_ref()
                .map_or(ColdDependencyCounts::default(), |(_, counts)| *counts)
        })
    }
}

impl Drop for DiagnosticIo {
    fn drop(&mut self) {
        DIAGNOSTIC_IO.replace(self.0.take());
    }
}

fn diagnostic_cold_read(key: &Path) {
    DIAGNOSTIC_IO.with_borrow_mut(|active| {
        if let Some((document, counts)) = active
            && document != key
        {
            counts.reads += 1;
        }
    });
}

fn diagnostic_cold_parse(path: &Path) {
    DIAGNOSTIC_IO.with_borrow_mut(|active| {
        if let Some((document, counts)) = active
            && *document != path_key(path)
        {
            counts.parses += 1;
        }
    });
}

type LoadJob = Box<dyn FnOnce(&mut Parser) + Send>;

#[derive(Clone)]
pub(super) struct SharedDiskCache {
    files: Arc<CacheLock<Files>>,
    parsed: Arc<CacheLock<Parsed>>,
    disk_generation: Arc<AtomicU64>,
    queue_wake: Arc<QueueWake>,
    builtin_paths: Arc<CacheLock<foreground::BuiltinPaths>>,
    package_indexes: Arc<CacheLock<Lru<Arc<PackageDeclKey>, Arc<PackageDeclIndex>>>>,
    reference_identifiers: Arc<CacheLock<Lru<ContentKey, Arc<ReferenceIdentifiers>>>>,
    wake: (Sender<()>, Receiver<()>),
    #[cfg(test)]
    checks: Arc<Mutex<TestChecks>>,
}

impl Default for SharedDiskCache {
    fn default() -> Self {
        Self::new(TEXT_ENTRIES, TEXT_BYTES, PARSED_ENTRIES, PARSED_BYTES)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct FileObservation {
    pub(super) stamp: Stamp,
    created: Option<SystemTime>,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

impl FileObservation {
    pub(super) fn read(path: &Path) -> io::Result<Self> {
        fs::metadata(path).map(Self::from_metadata)
    }

    pub(super) fn from_metadata(metadata: fs::Metadata) -> Self {
        let created = metadata.created().ok();
        #[cfg(unix)]
        let identity = {
            use std::os::unix::fs::MetadataExt;
            (
                metadata.dev(),
                metadata.ino(),
                metadata.ctime(),
                metadata.ctime_nsec(),
            )
        };
        Self {
            stamp: metadata.into(),
            created,
            #[cfg(unix)]
            identity,
        }
    }
}

struct TextRevision {
    observation: FileObservation,
    text: NavigationText,
    fingerprint: u64,
}

#[derive(Clone)]
pub(super) struct FileReadTicket {
    key: PathBuf,
    observation: FileObservation,
    fingerprint: u64,
    generation: u64,
    captured: bool,
}

impl TextRevision {
    fn navigation_text(&self, key: &Path, generation: u64) -> NavigationText {
        let mut text = self.text.clone();
        text.read_ticket = Some(FileReadTicket {
            key: key.to_owned(),
            observation: self.observation,
            fingerprint: self.fingerprint,
            generation,
            captured: false,
        });
        text
    }
}

impl FileReadTicket {
    pub(super) fn is_captured(&self) -> bool {
        self.captured
    }

    pub(super) fn observation(&self) -> FileObservation {
        self.observation
    }

    pub(super) fn fingerprint(&self) -> u64 {
        self.fingerprint
    }

    pub(super) fn same_revision(&self, other: &Self) -> bool {
        self.key == other.key
            && self.observation == other.observation
            && self.fingerprint == other.fingerprint
    }

    pub(super) fn matches_disk(&self, path: &Path) -> bool {
        FileObservation::read(path).ok() == Some(self.observation)
    }
}

impl NavigationText {
    /// Keep the verified bytes for this request. The ticket still has to pass
    /// normal validation before it can be published under the path.
    pub(super) fn capture(mut self) -> Self {
        if let Some(ticket) = &mut self.read_ticket {
            ticket.captured = true;
        }
        self
    }
}

struct Files {
    cached: Lru<PathBuf, Arc<TextRevision>>,
    package_names: Lru<PathBuf, PackageNames>,
    loading: HashMap<PathBuf, (Option<FileObservation>, Arc<ReadFlight>)>,
    checking: HashMap<foreground::ValidationKey, Arc<Flight<()>>>,
    generation: u64,
    invalidations: VecDeque<PathBuf>,
}

struct PackageNames {
    observation: FileObservation,
    fingerprint: u64,
    text: Weak<String>,
}

type ReadFlight = Flight<Option<Arc<TextRevision>>>;

#[derive(Clone)]
struct ContentKey {
    fingerprint: u64,
    text: Arc<String>,
    overlay_syntax: Option<u64>,
}

impl ContentKey {
    fn new(text: &NavigationText) -> Self {
        Self {
            fingerprint: text
                .read_ticket
                .as_ref()
                .map_or_else(|| text_fingerprint(&text.text), |ticket| ticket.fingerprint),
            text: text.text.clone(),
            overlay_syntax: None,
        }
    }
}

impl PartialEq for ContentKey {
    fn eq(&self, other: &Self) -> bool {
        self.fingerprint == other.fingerprint
            && self.overlay_syntax == other.overlay_syntax
            && (Arc::ptr_eq(&self.text, &other.text) || self.text == other.text)
    }
}

impl Eq for ContentKey {}

impl Hash for ContentKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.fingerprint.hash(state);
        self.text.len().hash(state);
        self.overlay_syntax.hash(state);
    }
}

#[derive(Clone)]
pub(super) struct PreparedDeclarations {
    pub(super) declarations: Arc<SharedDeclarations>,
    pub(super) text: NavigationText,
    pub(super) fingerprint: u64,
    pub(super) path: PathBuf,
}

struct Parsed {
    cached: Lru<ContentKey, Arc<IndexedSourceRevision>>,
    declarations: Lru<ContentKey, DeclarationProjection>,
    loading: HashMap<ContentKey, SourceFlights>,
}

enum DeclarationProjection {
    Reliable(Arc<SharedDeclarations>),
    FullRecovery(Arc<SharedDeclarations>),
}

impl DeclarationProjection {
    fn declarations(&self) -> &Arc<SharedDeclarations> {
        match self {
            Self::Reliable(declarations) | Self::FullRecovery(declarations) => declarations,
        }
    }
}

type FullFlight = Flight<Arc<IndexedSourceRevision>>;
type DeclarationFlight = Flight<Arc<SharedDeclarations>>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum DeclarationProducer {
    Stub,
    Light,
    Full,
}

#[derive(Default)]
struct SourceFlights {
    full: Option<Arc<FullFlight>>,
    declarations: Option<(DeclarationProducer, Arc<DeclarationFlight>)>,
}

enum SourceRequest<T> {
    Cached(T),
    Join(Arc<Flight<T>>),
    Produce(Arc<Flight<T>>),
    Busy(SharedLoadPending),
}

impl<T> SourceRequest<T> {
    fn trace(&self, artifact: Artifact) {
        let (outcome, flight) = match self {
            Self::Cached(_) => (telemetry::DiskLookup::Hit, None),
            Self::Join(flight) => (telemetry::DiskLookup::Join, flight.trace),
            Self::Produce(flight) => (telemetry::DiskLookup::Produce, flight.trace),
            Self::Busy(_) => return,
        };
        trace::lookup(artifact, outcome, flight);
    }
}

impl Parsed {
    fn busy(&self) -> Option<SharedLoadPending> {
        let active = self
            .loading
            .values()
            .map(|loads| {
                usize::from(loads.active_full().is_some())
                    + usize::from(matches!(
                        loads.declarations,
                        Some((DeclarationProducer::Stub | DeclarationProducer::Light, _))
                    ))
            })
            .sum::<usize>();
        if active < SOURCE_FLIGHTS {
            return None;
        }
        self.loading.values().find_map(|loads| {
            loads
                .active_full()
                .map(|full| full.finished.clone())
                .or_else(|| {
                    loads
                        .declarations
                        .as_ref()
                        .map(|(_, flight)| flight.finished.clone())
                })
                .map(SharedLoadPending::new)
        })
    }

    fn retire_flights(&mut self, key: &ContentKey) {
        if self.loading.get(key).is_some_and(|loads| {
            loads.declarations.is_none()
                && loads
                    .full
                    .as_ref()
                    .is_none_or(|full| full.finished.load(Ordering::Acquire))
        }) {
            self.loading.remove(key);
        }
    }
}

impl SourceFlights {
    fn active_full(&self) -> Option<&Arc<FullFlight>> {
        self.full
            .as_ref()
            .filter(|full| !full.finished.load(Ordering::Acquire))
    }
}

impl SharedDiskCache {
    fn new(
        text_entries: usize,
        text_bytes: usize,
        parse_entries: usize,
        parse_bytes: usize,
    ) -> Self {
        let wake = bounded(1);
        Self {
            files: Arc::new(CacheLock::new(
                Files {
                    cached: Lru::new(text_entries, text_bytes),
                    package_names: Lru::new(PACKAGE_NAMES_ENTRIES, PACKAGE_NAMES_BYTES),
                    loading: HashMap::new(),
                    checking: HashMap::new(),
                    generation: 0,
                    invalidations: VecDeque::new(),
                },
                &wake.0,
            )),
            parsed: Arc::new(CacheLock::new(
                Parsed {
                    cached: Lru::new(parse_entries, parse_bytes),
                    declarations: Lru::new(DECLARATION_ENTRIES, DECLARATION_BYTES),
                    loading: HashMap::new(),
                },
                &wake.0,
            )),
            disk_generation: Arc::default(),
            queue_wake: QueueWake::register(&wake.0),
            builtin_paths: Arc::new(CacheLock::new(foreground::BuiltinPaths::default(), &wake.0)),
            package_indexes: Arc::new(CacheLock::registry(Lru::new(256, 32 * 1024 * 1024))),
            reference_identifiers: Arc::new(CacheLock::registry(Lru::new(
                REFERENCE_INDEX_ENTRIES,
                REFERENCE_INDEX_BYTES,
            ))),
            wake,
            #[cfg(test)]
            checks: Arc::default(),
        }
    }

    pub(super) fn read_pending(&self, key: &Path) -> bool {
        self.files
            .try_lock()
            .map_or(true, |files| files.loading.contains_key(key))
    }

    pub(super) fn has_package_index(&self, package_id: &str) -> bool {
        self.package_indexes
            .try_lock()
            .is_ok_and(|indexes| indexes.entries.keys().any(|key| key.id == package_id))
    }

    pub(super) fn package_index(
        &self,
        key: PackageDeclKey,
        files: &[PackageDeclFile],
        package_name: &str,
        populate: bool,
        cancelled: &AtomicBool,
    ) -> Result<Option<Arc<PackageDeclIndex>>> {
        // Holding the registry lock through construction gives each key one
        // producer. All I/O and source preparation happens before this call.
        let mut indexes = if populate {
            self.package_indexes.lock().unwrap()
        } else if let Ok(indexes) = self.package_indexes.try_lock() {
            indexes
        } else {
            return Ok(None);
        };
        check_cancelled(cancelled)?;
        if let Some(index) = indexes.get(&key) {
            return Ok(Some(index.clone()));
        }
        if !populate {
            return Ok(None);
        }
        let index = Arc::new(PackageDeclIndex::collect(files, package_name, cancelled)?);
        check_cancelled(cancelled)?;
        let weight = key.cache_weight() + index.cache_weight();
        let retired = (weight <= 4 * 1024 * 1024)
            .then(|| indexes.insert(Arc::new(key), index.clone(), weight));
        drop(indexes);
        if let Some(retired) = retired {
            retired.report("package_indexes");
        } else {
            Removal::OversizedReject.report("package_indexes", 1);
        }
        Ok(Some(index))
    }

    pub(super) fn wake(&self) -> &Receiver<()> {
        &self.wake.1
    }

    pub(super) fn invalidate(&self, key: &Path) {
        self.invalidate_key(key, false, Removal::Closed);
    }

    pub(super) fn invalidate_disk_change(&self, key: &Path, watcher: bool) {
        let reason = if watcher {
            Removal::Watcher
        } else {
            Removal::Modified
        };
        self.invalidate_key(key, false, reason);
    }

    fn invalidate_key(&self, key: &Path, known_only: bool, reason: Removal) {
        let (text, names, _loading) = {
            let mut files = self.files.lock().unwrap();
            if known_only
                && files.cached.peek(key).is_none()
                && files.package_names.peek(key).is_none()
                && !files.loading.contains_key(key)
            {
                return;
            }
            files.generation += 1;
            if files.invalidations.len() == INVALIDATION_HISTORY {
                files.invalidations.pop_front();
            }
            files.invalidations.push_back(key.to_owned());
            self.disk_generation
                .store(files.generation, Ordering::Release);
            (
                files.cached.remove(key),
                files.package_names.remove(key),
                files.loading.remove(key),
            )
        };
        reason.report("text", usize::from(text.is_some()));
        reason.report("package_names", usize::from(names.is_some()));
    }

    pub(super) fn generation(&self) -> u64 {
        self.disk_generation.load(Ordering::Acquire)
    }

    pub(super) fn invalidated_since(&self, generation: u64) -> Option<Vec<PathBuf>> {
        let files = self.files.lock().unwrap();
        let count = usize::try_from(files.generation.checked_sub(generation)?).ok()?;
        let start = files.invalidations.len().checked_sub(count)?;
        Some(files.invalidations.range(start..).cloned().collect())
    }

    pub(super) fn cached_revision(
        &self,
        key: &Path,
        stamp: Stamp,
    ) -> Option<Arc<IndexedSourceRevision>> {
        let text = self.files.try_lock().ok()?.cached.peek(key)?.clone();
        if text.observation.stamp != stamp {
            return None;
        }
        let revision = self
            .parsed
            .try_lock()
            .ok()?
            .cached
            .peek(&ContentKey {
                fingerprint: text.fingerprint,
                text: text.text.text.clone(),
                overlay_syntax: None,
            })
            .cloned()?;
        trace::lookup(Artifact::FullAst, telemetry::DiskLookup::Hit, None);
        Some(revision)
    }

    /// Reuse names for the revision in the captured graph without touching
    /// the independent text and parse caches.
    pub(super) fn cached_package_names(
        &self,
        key: &Path,
        stamp: Stamp,
        package: &str,
    ) -> Option<Arc<EditorNames>> {
        let declarations = self.cached_declaration_revision(key, stamp)?;
        (declarations.reliable_package_scope
            && declarations.index.package_name.as_deref() == Some(package))
        .then(|| declarations.editor_names.clone())
    }

    /// Looks up an already verified path binding for a captured graph stamp.
    pub(super) fn cached_declaration_revision(
        &self,
        key: &Path,
        stamp: Stamp,
    ) -> Option<Arc<SharedDeclarations>> {
        let content = {
            let mut files = self.files.try_lock().ok()?;
            let summary = files.package_names.get(key)?;
            if summary.observation.stamp != stamp {
                return None;
            }
            ContentKey {
                fingerprint: summary.fingerprint,
                text: summary.text.upgrade()?,
                overlay_syntax: None,
            }
        };
        let declarations = self
            .parsed
            .try_lock()
            .ok()?
            .declarations
            .get(&content)?
            .declarations()
            .clone();
        trace::lookup(Artifact::Declarations, telemetry::DiskLookup::Hit, None);
        Some(declarations)
    }

    pub(super) fn read(
        &self,
        path: &Path,
        cancelled: Option<&AtomicBool>,
        probe: Option<&NavigationProbe>,
    ) -> Result<NavigationText> {
        self.read_with_status(path, cancelled, probe)
            .map(|(text, _)| text)
    }

    pub(super) fn read_with_status(
        &self,
        path: &Path,
        cancelled: Option<&AtomicBool>,
        probe: Option<&NavigationProbe>,
    ) -> Result<(NavigationText, bool)> {
        let key = path_key(path);
        let (text, generation, produced) = self.read_revision(path, &key, cancelled, probe)?;
        Ok((text.navigation_text(&key, generation), produced))
    }

    /// The caller reserved space for this particular observation. A turnover
    /// is retried by the normal consumer, outside the prefilter's byte budget.
    pub(super) fn read_for_prefilter(
        &self,
        path: &Path,
        key: &Path,
        observation: FileObservation,
        cancelled: &AtomicBool,
        metadata_ms: Option<f64>,
    ) -> Result<Option<(NavigationText, bool)>> {
        self.read_observation(path, key, observation, Some(cancelled), None, metadata_ms)
            .map(|product| {
                product.map(|(text, generation, produced)| {
                    (text.navigation_text(key, generation), produced)
                })
            })
    }

    pub(super) fn cached_read(&self, path: &Path, key: &Path) -> Option<NavigationText> {
        let observation = FileObservation::read(path).ok()?;
        let text = {
            let mut files = self.files.lock().unwrap();
            let generation = files.generation;
            let text = files.cached.get(key)?;
            (text.observation == observation).then(|| text.navigation_text(key, generation))
        }?;
        trace::lookup(Artifact::Text, telemetry::DiskLookup::Hit, None);
        Some(text)
    }

    pub(super) fn cached_declaration_read(
        &self,
        path: &Path,
        key: &Path,
        observed: Option<(FileObservation, u64)>,
    ) -> Option<PreparedDeclarations> {
        let (text, generation, bound_package_names) = {
            let mut files = self.files.lock().unwrap();
            let text = files.cached.get(key)?.clone();
            let bound_package_names = files.package_names.peek(key).is_some_and(|bound| {
                bound.observation == text.observation
                    && bound.fingerprint == text.fingerprint
                    && bound.text.strong_count() > 0
            });
            (text, files.generation, bound_package_names)
        };
        let observation = match observed {
            Some((observation, observed_generation)) if observed_generation == generation => {
                observation
            }
            _ => FileObservation::read(path).ok()?,
        };
        if text.observation != observation {
            return None;
        }
        let verified = text.navigation_text(key, generation);
        let declarations = self.cached_declarations(&verified)?;
        if self.generation() != generation {
            return None;
        }
        if !bound_package_names {
            self.remember_package_names(key, &text, generation);
        }
        trace::lookup(Artifact::Text, telemetry::DiskLookup::Hit, None);
        Some(PreparedDeclarations {
            declarations,
            text: verified,
            fingerprint: text.fingerprint,
            path: path.to_owned(),
        })
    }

    pub(super) fn cached_parse(
        &self,
        path: &Path,
        text: &NavigationText,
    ) -> Option<PreparedReferenceSource> {
        let key = ContentKey::new(text);
        let revision = self.parsed.lock().unwrap().cached.get(&key)?.clone();
        trace::lookup(Artifact::FullAst, telemetry::DiskLookup::Hit, None);
        Some(PreparedReferenceSource::new(
            revision,
            path,
            text.clone(),
            key.fingerprint,
        ))
    }

    /// Looks up a projection of the supplied bytes. Publication under a path
    /// also requires checking its read ticket.
    pub(super) fn cached_declarations(
        &self,
        text: &NavigationText,
    ) -> Option<Arc<SharedDeclarations>> {
        let declarations = self
            .parsed
            .lock()
            .unwrap()
            .declarations
            .get(&ContentKey::new(text))
            .map(|projection| projection.declarations().clone())?;
        trace::lookup(Artifact::Declarations, telemetry::DiskLookup::Hit, None);
        Some(declarations)
    }

    pub(super) fn load(
        &self,
        parser: &mut Parser,
        path: &Path,
        cancelled: Option<&AtomicBool>,
        probe: Option<&NavigationProbe>,
    ) -> Result<PreparedReferenceSource> {
        self.load_prefetched(parser, path, None, cancelled, probe)
    }

    pub(super) fn load_prefetched(
        &self,
        parser: &mut Parser,
        path: &Path,
        prefetched: Option<NavigationText>,
        cancelled: Option<&AtomicBool>,
        probe: Option<&NavigationProbe>,
    ) -> Result<PreparedReferenceSource> {
        self.load_observed(path, prefetched, cancelled, probe, |text| {
            self.parse_snapshot_with(parser, path, text, cancelled, probe)
        })
        .map(|(source, text, generation)| {
            self.remember_package_names(&path_key(path), &text, generation);
            source
        })
    }

    pub(super) fn load_declarations(
        &self,
        parser: &mut Parser,
        path: &Path,
        prefetched: Option<NavigationText>,
        cancelled: Option<&AtomicBool>,
        probe: Option<&NavigationProbe>,
    ) -> Result<PreparedDeclarations> {
        let (source, text, generation) =
            self.load_observed(path, prefetched, cancelled, probe, |text| {
                self.declarations_snapshot(parser, path, text, cancelled, probe)
            })?;
        self.remember_package_names(&path_key(path), &text, generation);
        Ok(source)
    }

    pub(super) fn load_documentation(
        &self,
        parser: &mut Parser,
        path: &Path,
        prefetched: Option<NavigationText>,
        cancelled: Option<&AtomicBool>,
        probe: Option<&NavigationProbe>,
    ) -> Result<PreparedDeclarations> {
        let (source, text, generation) =
            self.load_observed(path, prefetched, cancelled, probe, |text| {
                self.projected_snapshot(
                    parser,
                    path,
                    text,
                    cancelled,
                    probe,
                    DeclarationProducer::Light,
                )
            })?;
        self.remember_package_names(&path_key(path), &text, generation);
        Ok(source)
    }

    fn load_observed<T>(
        &self,
        path: &Path,
        prefetched: Option<NavigationText>,
        cancelled: Option<&AtomicBool>,
        probe: Option<&NavigationProbe>,
        mut project: impl FnMut(NavigationText) -> Result<T>,
    ) -> Result<(T, Arc<TextRevision>, u64)> {
        let key = path_key(path);
        let mut prefetched = prefetched.and_then(|text| {
            let ticket = text.read_ticket.as_ref()?;
            let observation = ticket.observation;
            let fingerprint = ticket.fingerprint;
            let generation = ticket.generation;
            if ticket.key != key || !self.current(path, &key, observation, &text.text, generation) {
                return None;
            }
            record(probe, |counts| counts.text_hits += 1);
            Some((
                Arc::new(TextRevision {
                    observation,
                    text,
                    fingerprint,
                }),
                generation,
            ))
        });
        for _ in 0..OBSERVATION_RETRIES {
            let (text, generation) = if let Some(prefetched) = prefetched.take() {
                prefetched
            } else {
                let (text, generation, _) = self.read_revision(path, &key, cancelled, probe)?;
                (text, generation)
            };
            let product = project(text.navigation_text(&key, generation))?;
            check_cancelled(cancelled.unwrap_or(&SHARED_LOAD))?;
            if self.current(path, &key, text.observation, &text.text.text, generation) {
                return Ok((product, text, generation));
            }
        }
        Err(changed_source(path))
    }

    pub(super) fn validates_read(&self, path: &Path, text: &NavigationText) -> bool {
        text.read_ticket
            .as_ref()
            .is_some_and(|ticket| self.validates_ticket(path, ticket, Some(&text.text)))
    }

    pub(super) fn validates_ticket(
        &self,
        path: &Path,
        ticket: &FileReadTicket,
        text: Option<&Arc<String>>,
    ) -> bool {
        ticket.key == path_key(path)
            && self.current_observation(
                path,
                &ticket.key,
                ticket.observation,
                text,
                ticket.generation,
            )
    }

    fn remember_package_names(&self, key: &Path, text: &TextRevision, generation: u64) {
        let content = ContentKey {
            fingerprint: text.fingerprint,
            text: text.text.text.clone(),
            overlay_syntax: None,
        };
        let weak_text = {
            let parsed = self.parsed.lock().unwrap();
            let Some((retained, _)) = parsed.declarations.entries.get_key_value(&content) else {
                return;
            };
            Arc::downgrade(&retained.text)
        };
        let weight = key
            .as_os_str()
            .len()
            .saturating_add(size_of::<PackageNames>());
        let retired = {
            let mut files = self.files.lock().unwrap();
            let current = files.generation == generation
                || files.cached.peek(key).is_some_and(|current| {
                    current.observation == text.observation
                        && Arc::ptr_eq(&current.text.text, &text.text.text)
                });
            if !current
                || files.package_names.get(key).is_some_and(|saved| {
                    saved.observation == text.observation
                        && saved.fingerprint == content.fingerprint
                        && Weak::ptr_eq(&saved.text, &weak_text)
                })
            {
                return;
            }
            files.package_names.insert(
                key.to_owned(),
                PackageNames {
                    observation: text.observation,
                    fingerprint: content.fingerprint,
                    text: weak_text,
                },
                weight,
            )
        };
        retired.report("package_names");
    }

    pub(super) fn reference_identifiers(
        &self,
        text: &NavigationText,
        cancelled: &AtomicBool,
    ) -> Result<Arc<ReferenceIdentifiers>> {
        check_cancelled(cancelled)?;
        let key = ContentKey {
            fingerprint: text
                .read_ticket
                .as_ref()
                .map_or_else(|| text_fingerprint(&text.text), |ticket| ticket.fingerprint),
            text: Arc::clone(&text.text),
            overlay_syntax: None,
        };
        if let Some(identifiers) = self.reference_identifiers.lock().unwrap().get(&key) {
            return Ok(Arc::clone(identifiers));
        }
        let identifiers = Arc::new(ReferenceIdentifiers::scan(&text.text, cancelled)?);
        let weight = identifiers.weight().saturating_add(text.text.capacity());
        let retired = {
            let mut cached = self.reference_identifiers.lock().unwrap();
            if let Some(existing) = cached.get(&key) {
                return Ok(Arc::clone(existing));
            }
            cached.insert(key, Arc::clone(&identifiers), weight)
        };
        retired.report("reference_identifiers");
        Ok(identifiers)
    }

    pub(super) fn cached_reference_identifiers(
        &self,
        text: &NavigationText,
    ) -> Option<Arc<ReferenceIdentifiers>> {
        let key = ContentKey {
            fingerprint: text.read_ticket.as_ref()?.fingerprint,
            text: text.text.clone(),
            overlay_syntax: None,
        };
        self.reference_identifiers
            .lock()
            .unwrap()
            .get(&key)
            .cloned()
    }

    /// Parses the bytes a caller already captured for its own query.
    /// They can populate the content cache without updating the path cache.
    pub(super) fn parse_snapshot(
        &self,
        parser: &mut Parser,
        path: &Path,
        text: NavigationText,
        probe: Option<&NavigationProbe>,
    ) -> Result<PreparedReferenceSource> {
        record(probe, |counts| counts.text_hits += 1);
        self.parse_snapshot_with(parser, path, text, None, probe)
    }

    fn read_revision(
        &self,
        path: &Path,
        key: &Path,
        cancelled: Option<&AtomicBool>,
        probe: Option<&NavigationProbe>,
    ) -> Result<(Arc<TextRevision>, u64, bool)> {
        for _ in 0..OBSERVATION_RETRIES {
            check_cancelled(cancelled.unwrap_or(&SHARED_LOAD))?;
            let started = telemetry::now();
            let observed = FileObservation::read(path);
            let metadata_ms = started.map(telemetry::elapsed);
            let observation = observed
                .map_err(|error| {
                    if error.kind() == io::ErrorKind::NotFound {
                        self.invalidate_key(key, true, Removal::Missing);
                    }
                    error
                })
                .with_context(|| format!("reading {}", path.display()))?;
            if let Some(product) =
                self.read_observation(path, key, observation, cancelled, probe, metadata_ms)?
            {
                return Ok(product);
            }
        }
        Err(changed_source(path))
    }

    fn read_observation(
        &self,
        path: &Path,
        key: &Path,
        observation: FileObservation,
        cancelled: Option<&AtomicBool>,
        probe: Option<&NavigationProbe>,
        metadata_ms: Option<f64>,
    ) -> Result<Option<(Arc<TextRevision>, u64, bool)>> {
        loop {
            check_cancelled(cancelled.unwrap_or(&SHARED_LOAD))?;
            let mut slot = None;
            let (request, generation) = {
                let mut files = self.files.lock().unwrap();
                let generation = files.generation;
                let request = if let Some(text) = files
                    .cached
                    .get(key)
                    .filter(|text| text.observation == observation)
                {
                    SourceRequest::Cached(Some(text.clone()))
                } else {
                    match files.loading.get(key) {
                        Some((current, flight))
                            if current.is_none_or(|current| current == observation) =>
                        {
                            SourceRequest::Join(flight.clone())
                        }
                        _ if let Err(pending) = self.reserve_for(cancelled, &mut slot) => {
                            SourceRequest::Busy(pending)
                        }
                        _ => {
                            let flight = Arc::new(Flight::traced(&self.wake.0, Artifact::Text));
                            files
                                .loading
                                .insert(key.to_owned(), (Some(observation), flight.clone()));
                            SourceRequest::Produce(flight)
                        }
                    }
                };
                (request, generation)
            };
            request.trace(Artifact::Text);
            let (flight, producer) = match request {
                SourceRequest::Produce(flight) => (flight, true),
                SourceRequest::Join(flight) => (flight, false),
                SourceRequest::Busy(pending) => {
                    pending.wait_for_slot(cancelled)?;
                    continue;
                }
                SourceRequest::Cached(text) => {
                    record(probe, |counts| counts.text_hits += 1);
                    return Ok(text.map(|text| (text, generation, false)));
                }
            };
            let _read = producer.then(|| sample(probe, |counts| &mut counts.read));
            if producer {
                record(probe, |counts| {
                    counts.text_misses += 1;
                    counts.disk_reads += 1;
                });
                self.start_read(path, key, observation, &flight, slot, metadata_ms)?;
            } else {
                record(probe, |counts| counts.text_hits += 1);
                #[cfg(test)]
                self.test_event(key, CacheEvent::WaitRead);
            }
            diagnostic_cold_read(key);
            if let Some(text) = flight.wait(cancelled)?
                && text.observation == observation
                && self.current(path, key, text.observation, &text.text.text, generation)
            {
                return Ok(Some((text, generation, producer)));
            }
            return Ok(None);
        }
    }

    fn reserve_for(
        &self,
        cancelled: Option<&AtomicBool>,
        slot: &mut Option<LoadSlot>,
    ) -> Result<(), SharedLoadPending> {
        if cancelled.is_some() {
            // Admit before publishing the flight. A cancelled waiter waiting
            // for capacity cannot strand or abort a shared producer.
            *slot = Some(self.queue_wake.try_reserve()?);
        }
        Ok(())
    }

    fn start_read(
        &self,
        path: &Path,
        key: &Path,
        observation: FileObservation,
        flight: &Arc<ReadFlight>,
        slot: Option<LoadSlot>,
        metadata_ms: Option<f64>,
    ) -> Result<()> {
        let abort = self.abort_read(key, flight);
        let cache = self.clone();
        let path = path.to_owned();
        let key = key.to_owned();
        let flight = flight.clone();
        let trace = flight.trace;
        let job = move || {
            let mut abort = abort;
            if let Some((trace, metadata_ms)) = trace.zip(metadata_ms) {
                trace.phase("metadata", metadata_ms, None);
            }
            let result = cache.read_observed_text(&path, &key, observation, trace);
            cache.publish_read(&key, &flight, result);
            abort.0.take();
        };
        if let Some(slot) = slot {
            slot.submit_traced(trace, Box::new(move |_| job()))
        } else {
            job();
            Ok(())
        }
    }

    fn abort_read(
        &self,
        key: &Path,
        flight: &Arc<ReadFlight>,
    ) -> AbortOnDrop<impl FnOnce() + use<>> {
        AbortOnDrop(Some({
            let files = self.files.clone();
            let key = key.to_owned();
            let flight = flight.clone();
            move || {
                let mut files = files
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if files
                    .loading
                    .get(&key)
                    .is_some_and(|(_, current)| Arc::ptr_eq(current, &flight))
                {
                    files.loading.remove(&key);
                }
                flight.finish(Err(anyhow::anyhow!("shared Go source read aborted")));
            }
        }))
    }

    fn read_observed_text(
        &self,
        path: &Path,
        key: &Path,
        observation: FileObservation,
        trace: Option<FlightTrace>,
    ) -> Result<Option<Arc<TextRevision>>> {
        #[cfg(not(test))]
        let _ = key;
        #[cfg(test)]
        self.test_event(key, CacheEvent::Read);
        let result = trace::measure(trace, "read", None, || {
            NavigationText::read_observation(path, observation, &SHARED_LOAD)
        });
        #[cfg(test)]
        self.test_event(key, CacheEvent::ReadFinished);
        result.map(|text| {
            text.filter(|_| {
                trace::measure(trace, "metadata", None, || FileObservation::read(path)).ok()
                    == Some(observation)
            })
            .map(|text| {
                Arc::new(TextRevision {
                    observation,
                    fingerprint: text_fingerprint(&text.text),
                    text,
                })
            })
        })
    }

    fn publish_read(
        &self,
        key: &Path,
        flight: &Arc<ReadFlight>,
        result: Result<Option<Arc<TextRevision>>>,
    ) {
        let (retired, names_removed, text_removed) = {
            let mut files = self.files.lock().unwrap();
            if files
                .loading
                .get(key)
                .is_some_and(|(_, current)| Arc::ptr_eq(current, flight))
            {
                let (retired, names_removed, text_removed) = match &result {
                    Ok(Some(text)) => {
                        let names_removed = files.package_names.peek(key).is_some_and(|cached| {
                            cached.observation != text.observation
                                || cached.fingerprint != text.fingerprint
                        }) && files.package_names.remove(key).is_some();
                        (
                            Some(files.cached.insert(
                                key.to_owned(),
                                text.clone(),
                                text.text.text.len(),
                            )),
                            names_removed,
                            None,
                        )
                    }
                    _ => (
                        None,
                        files.package_names.remove(key).is_some(),
                        files.cached.remove(key),
                    ),
                };
                flight.finish(result);
                files.loading.remove(key);
                (retired, names_removed, text_removed)
            } else {
                // An invalidation or a newer observation replaced this flight.
                flight.finish(Ok(None));
                (None, false, None)
            }
        };
        if let Some(retired) = retired {
            retired.report("text");
        }
        Removal::Validation.report("package_names", usize::from(names_removed));
        Removal::Validation.report("text", usize::from(text_removed.is_some()));
    }

    fn current(
        &self,
        path: &Path,
        key: &Path,
        observation: FileObservation,
        text: &Arc<String>,
        generation: u64,
    ) -> bool {
        self.current_observation(path, key, observation, Some(text), generation)
    }

    fn current_observation(
        &self,
        path: &Path,
        key: &Path,
        observation: FileObservation,
        text: Option<&Arc<String>>,
        generation: u64,
    ) -> bool {
        if FileObservation::read(path).ok() != Some(observation) {
            return false;
        }
        if self.generation() == generation {
            return true;
        }
        let files = self.files.lock().unwrap();
        files.cached.peek(key).is_some_and(|current| {
            current.observation == observation
                && text.is_some_and(|text| Arc::ptr_eq(&current.text.text, text))
        })
    }
}

struct AbortOnDrop<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> Drop for AbortOnDrop<F> {
    fn drop(&mut self) {
        if let Some(abort) = self.0.take() {
            abort();
        }
    }
}

fn changed_source(path: &Path) -> anyhow::Error {
    io::Error::new(
        io::ErrorKind::Interrupted,
        format!("{} changed while reading Go source", path.display()),
    )
    .into()
}

type FlightResult<T> = std::result::Result<T, Arc<anyhow::Error>>;

#[derive(Debug)]
pub(crate) struct SharedLoadPending(PendingLoad);

#[derive(Debug)]
enum PendingLoad {
    Flight(Arc<AtomicBool>),
    Epoch(Arc<AtomicU64>, u64),
}

impl SharedLoadPending {
    pub(crate) fn new(finished: Arc<AtomicBool>) -> Self {
        Self(PendingLoad::Flight(finished))
    }

    fn epoch(epoch: Arc<AtomicU64>, observed: u64) -> Self {
        Self(PendingLoad::Epoch(epoch, observed))
    }

    pub(crate) fn ready(&self) -> bool {
        match &self.0 {
            PendingLoad::Flight(finished) => finished.load(Ordering::Acquire),
            PendingLoad::Epoch(epoch, observed) => epoch.load(Ordering::Acquire) != *observed,
        }
    }

    fn wait_for_slot(self, cancelled: Option<&AtomicBool>) -> Result<()> {
        let Some(cancelled) = cancelled else {
            return Err(self.into());
        };
        while !self.ready() {
            check_cancelled(cancelled)?;
            thread::sleep(CANCELLATION_INTERVAL);
        }
        Ok(())
    }
}

impl std::fmt::Display for SharedLoadPending {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Go source is loading in the background")
    }
}

impl std::error::Error for SharedLoadPending {}

struct Flight<T> {
    result: OnceLock<FlightResult<T>>,
    gate: Mutex<()>,
    ready: Condvar,
    finished: Arc<AtomicBool>,
    wake: Option<Sender<()>>,
    trace: Option<FlightTrace>,
}

impl<T> Flight<T> {
    fn new(wake: Option<Sender<()>>) -> Self {
        Self::with_trace(wake, None)
    }

    fn traced(wake: &Sender<()>, artifact: Artifact) -> Self {
        Self::with_trace(
            Some(wake.clone()),
            FlightTrace::new(artifact, Operation::Source),
        )
    }

    fn validation(wake: &Sender<()>, artifact: Artifact) -> Self {
        Self::with_trace(
            Some(wake.clone()),
            FlightTrace::new(artifact, Operation::Validation),
        )
    }

    fn with_trace(wake: Option<Sender<()>>, trace: Option<FlightTrace>) -> Self {
        Self {
            result: OnceLock::new(),
            gate: Mutex::new(()),
            ready: Condvar::new(),
            finished: Arc::default(),
            wake,
            trace,
        }
    }
}

impl<T: Clone> Flight<T> {
    fn completed(&self) -> Option<T> {
        self.result.get()?.as_ref().ok().cloned()
    }

    fn finish(&self, result: Result<T>) {
        let published = self
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.result.set(result.map_err(Arc::new)).is_err() {
            return;
        }
        self.finished.store(true, Ordering::Release);
        drop(published);
        if let Some(wake) = &self.wake {
            let _ = wake.try_send(());
        }
        self.ready.notify_all();
    }

    fn wait(&self, cancelled: Option<&AtomicBool>) -> Result<T> {
        let start = self.trace.map(|_| Instant::now());
        let result = self.wait_result(cancelled);
        if let Some((trace, start)) = self.trace.zip(start) {
            let outcome = match &result {
                Ok(_) => "ready",
                Err(error) if error.is::<SharedLoadPending>() => "pending",
                Err(_) if cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) => "cancelled",
                Err(_) => "failed",
            };
            trace.wait(start, outcome);
        }
        result
    }

    fn wait_result(&self, cancelled: Option<&AtomicBool>) -> Result<T> {
        let Some(cancelled) = cancelled else {
            return self.result.get().cloned().map_or_else(
                || Err(SharedLoadPending::new(self.finished.clone()).into()),
                Self::clone_result,
            );
        };
        let mut gate = self.gate.lock().unwrap();
        loop {
            check_cancelled(cancelled)?;
            if let Some(result) = self.result.get() {
                return Self::clone_result(result.clone());
            }
            gate = self
                .ready
                .wait_timeout(gate, CANCELLATION_INTERVAL)
                .unwrap()
                .0;
        }
    }

    fn clone_result(result: FlightResult<T>) -> Result<T> {
        result.map_err(clone_load_error)
    }
}

fn clone_load_error(error: Arc<anyhow::Error>) -> anyhow::Error {
    if error.is::<NeedsFullDeclarations>() {
        return NeedsFullDeclarations.into();
    }
    if let Some(limit) = error.downcast_ref::<QueryReadLimit>() {
        return (*limit).into();
    }
    match error.downcast_ref::<io::Error>() {
        Some(io) => io::Error::new(io.kind(), format!("{error:#}")).into(),
        None => anyhow::anyhow!("{error:#}"),
    }
}

struct Lru<K, V> {
    entries: HashMap<K, (u64, usize, V)>,
    age: BTreeMap<u64, K>,
    clock: u64,
    bytes: usize,
    entry_limit: usize,
    byte_limit: usize,
}

impl<K: Clone + Eq + Hash, V> Lru<K, V> {
    fn new(entry_limit: usize, byte_limit: usize) -> Self {
        Self {
            entries: HashMap::new(),
            age: BTreeMap::new(),
            clock: 0,
            bytes: 0,
            entry_limit,
            byte_limit,
        }
    }

    fn peek<Q: Eq + Hash + ?Sized>(&self, key: &Q) -> Option<&V>
    where
        K: std::borrow::Borrow<Q>,
    {
        self.entries.get(key).map(|(_, _, value)| value)
    }

    fn get<Q: Eq + Hash + ?Sized>(&mut self, key: &Q) -> Option<&V>
    where
        K: std::borrow::Borrow<Q>,
    {
        let (age, _, value) = self.entries.get_mut(key)?;
        self.clock += 1;
        let key = self.age.remove(age).expect("cache entry has an age");
        *age = self.clock;
        self.age.insert(*age, key);
        Some(value)
    }

    fn remove<Q: Eq + Hash + ?Sized>(&mut self, key: &Q) -> Option<V>
    where
        K: std::borrow::Borrow<Q>,
    {
        let (age, bytes, value) = self.entries.remove(key)?;
        self.age.remove(&age);
        self.bytes -= bytes;
        Some(value)
    }

    fn insert(&mut self, key: K, value: V, bytes: usize) -> Retired<V> {
        self.insert_with_counts(key, value, bytes, telemetry::enabled())
    }

    fn insert_with_counts(&mut self, key: K, value: V, bytes: usize, count: bool) -> Retired<V> {
        let mut retired = Retired::empty();
        retired.values.extend(self.remove(&key));
        if count {
            retired.record(Removal::Replacement, retired.values.len());
        }
        if self.entry_limit == 0 || bytes > self.byte_limit {
            retired.values.push(value);
            if count {
                let reason = if bytes > self.byte_limit {
                    Removal::OversizedReject
                } else {
                    Removal::Entries
                };
                retired.record(reason, 1);
            }
            return retired;
        }
        while self.entries.len() >= self.entry_limit || self.bytes + bytes > self.byte_limit {
            if count {
                let entries = self.entries.len() >= self.entry_limit;
                let bytes = self.bytes + bytes > self.byte_limit;
                let reason = match (entries, bytes) {
                    (true, true) => Removal::Both,
                    (true, false) => Removal::Entries,
                    (false, _) => Removal::Bytes,
                };
                retired.record(reason, 1);
            }
            let (_, oldest) = self.age.pop_first().expect("cache is over capacity");
            if let Some(old) = self.remove(&oldest) {
                retired.values.push(old);
            }
        }
        self.clock += 1;
        self.bytes += bytes;
        self.age.insert(self.clock, key.clone());
        self.entries.insert(key, (self.clock, bytes, value));
        retired
    }
}

#[cfg(test)]
type TestHook = Arc<dyn Fn(&Path, CacheEvent) + Send + Sync>;

#[cfg(test)]
#[derive(Default)]
struct TestChecks {
    counts: HashMap<PathBuf, (usize, usize)>,
    projections: HashMap<PathBuf, (usize, usize)>,
    hook: Option<TestHook>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CacheEvent {
    Read,
    ReadFinished,
    WaitRead,
    Parse,
    WaitParse,
    Declarations,
    WaitDeclarations,
    DeclarationBuild,
}

#[cfg(test)]
impl SharedDiskCache {
    pub(super) fn counts(&self, path: &Path) -> (usize, usize) {
        self.checks
            .lock()
            .unwrap()
            .counts
            .get(&path_key(path))
            .copied()
            .unwrap_or_default()
    }

    pub(super) fn projection_counts(&self, path: &Path) -> (usize, usize) {
        self.checks
            .lock()
            .unwrap()
            .projections
            .get(&path_key(path))
            .copied()
            .unwrap_or_default()
    }

    pub(super) fn clear_full_cache(&self) {
        let retired = {
            let mut parsed = self.parsed.lock().unwrap();
            let empty = Lru::new(parsed.cached.entry_limit, parsed.cached.byte_limit);
            std::mem::replace(&mut parsed.cached, empty)
        };
        drop(retired);
    }

    pub(super) fn clear_declaration_cache(&self) {
        let retired = {
            let mut parsed = self.parsed.lock().unwrap();
            let empty = Lru::new(
                parsed.declarations.entry_limit,
                parsed.declarations.byte_limit,
            );
            std::mem::replace(&mut parsed.declarations, empty)
        };
        drop(retired);
    }

    pub(super) fn set_hook(&self, hook: Option<TestHook>) {
        self.checks.lock().unwrap().hook = hook;
    }

    fn test_event(&self, key: &Path, event: CacheEvent) {
        let hook = {
            let mut checks = self.checks.lock().unwrap();
            if matches!(
                event,
                CacheEvent::Read | CacheEvent::Parse | CacheEvent::Declarations
            ) {
                let counts = checks.counts.entry(key.to_owned()).or_default();
                if event == CacheEvent::Read {
                    counts.0 += 1;
                } else {
                    counts.1 += 1;
                }
            }
            if matches!(event, CacheEvent::Parse | CacheEvent::DeclarationBuild) {
                let counts = checks.projections.entry(key.to_owned()).or_default();
                if event == CacheEvent::Parse {
                    counts.0 += 1;
                } else {
                    counts.1 += 1;
                }
            }
            checks.hook.clone()
        };
        if let Some(hook) = hook {
            hook(key, event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestProject;
    use std::process::Command;
    use std::sync::Barrier;
    use std::sync::atomic::Ordering::Relaxed;
    use std::thread;

    pub(super) fn parser() -> Parser {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
            .unwrap();
        parser
    }

    #[test]
    fn lru_reports_capacity_rejection_and_replacement_counts() {
        let cases = [
            (1, 10, 5, 5, Removal::Entries),
            (2, 9, 5, 5, Removal::Bytes),
            (1, 9, 5, 5, Removal::Both),
            (1, 9, 5, 10, Removal::OversizedReject),
        ];
        for (entries, budget, first, second, reason) in cases {
            let mut lru = Lru::new(entries, budget);
            assert_eq!(
                lru.insert_with_counts(1, "first", first, true).counts,
                [0; 5]
            );
            let mut expected = [0; 5];
            expected[reason as usize] = 1;
            assert_eq!(
                lru.insert_with_counts(2, "second", second, true).counts,
                expected
            );
        }
        let mut lru = Lru::new(1, 9);
        lru.insert_with_counts(1, "original", 5, true);
        let replaced = lru.insert_with_counts(1, "newer", 5, true);
        assert_eq!(replaced.counts, [0, 0, 0, 0, 1]);
        assert_eq!(replaced.values, ["original"]);
        let rejected = lru.insert_with_counts(1, "too large", 10, true);
        assert_eq!(rejected.counts, [0, 0, 0, 1, 1]);
        assert_eq!(rejected.values, ["newer", "too large"]);
        lru.insert_with_counts(1, "unobserved", 5, false);
        assert_eq!(lru.insert_with_counts(2, "second", 5, false).counts, [0; 5]);
    }

    #[test]
    fn telemetry_distinguishes_producer_join_and_hit() {
        const FIXTURE: &str = "RS_GOPLS_TEST_SHARED_DISK_TELEMETRY_SOURCE";
        if let Some(path) = std::env::var_os(FIXTURE) {
            let _session = telemetry::Session::start();
            assert!(telemetry::enabled());
            let path = PathBuf::from(path);
            let cache = SharedDiskCache::default();
            let (started, reading) = bounded(1);
            let (joined, waiting) = bounded(1);
            let (release, blocked) = bounded(1);
            let timeout = Duration::from_secs(5);
            cache.set_hook(Some(Arc::new(move |_, event| match event {
                CacheEvent::Read => {
                    started.send(()).unwrap();
                    blocked.recv_timeout(timeout).unwrap();
                }
                CacheEvent::WaitRead => joined.send(()).unwrap(),
                _ => {}
            })));
            let read = || cache.read(&path, Some(&SHARED_LOAD), None).unwrap();
            thread::scope(|scope| {
                let producer = scope.spawn(read);
                reading.recv_timeout(timeout).unwrap();
                let subscriber = scope.spawn(read);
                let observed_join = waiting.recv_timeout(timeout);
                release.send(()).unwrap();
                observed_join.unwrap();
                let produced = producer.join().unwrap();
                let subscribed = subscriber.join().unwrap();
                assert!(Arc::ptr_eq(&produced.text, &subscribed.text));
            });
            let request = lsp_server::RequestId::from(42);
            telemetry::with_context(Some(telemetry::Context::request(&request)), || {
                for _ in 0..100 {
                    read();
                }
            });
            cache.invalidate_disk_change(&path_key(&path), true);
            let mut parser = parser();
            let malformed = NavigationText {
                text: Arc::new("package p\nvar Broken = (\n".to_owned()),
                stamp: None,
                read_ticket: None,
            };
            cache
                .declarations_snapshot(&mut parser, &path, malformed, None, None)
                .unwrap();
            return;
        }

        let project = TestProject::new();
        let path = project.write("private-shared-disk-source.go", "package p\n");
        let log = project.root.join("shared-disk.jsonl");
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "semantic::shared_disk::tests::telemetry_distinguishes_producer_join_and_hit",
            ])
            .env(FIXTURE, path)
            .env(telemetry::FILE_ENV, &log)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let raw = fs::read_to_string(log).unwrap();
        assert!(!raw.contains("private-shared-disk-source"));
        let rows = raw
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        let lookups = rows
            .iter()
            .filter(|row| row["event"] == "shared_disk_lookup" && row["artifact"] == "text")
            .collect::<Vec<_>>();
        let outcomes = lookups
            .iter()
            .map(|row| row["outcome"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(outcomes, ["producer_requested", "join"]);
        let flight_id = lookups[0]["flight_id"].as_u64().unwrap();
        assert_eq!(lookups[1]["flight_id"], flight_id);
        let summaries = rows
            .iter()
            .filter(|row| row["event"] == "shared_disk_summary" && row["req_id"] == 42)
            .collect::<Vec<_>>();
        assert_eq!(summaries.len(), 1);
        let stats = &summaries[0]["shared_disk"];
        assert_eq!(stats["cache_hits"], 100);
        assert_eq!(stats["artifacts"]["text"]["cache_hits"], 100);
        assert_eq!(stats["producer_requested"], 0);
        assert_eq!(stats["joins"], 0);
        assert_eq!(stats["mutex_acquired"], 100);
        assert!(!rows.iter().any(|row| row["event"] == "shared_disk_mutex"));
        for (phase, expected) in [("metadata", 2), ("pool_queue", 1), ("read", 1)] {
            let samples = rows.iter().filter(|row| {
                row["event"] == "shared_disk_flight"
                    && row["flight_id"] == flight_id
                    && row["phase"] == phase
            });
            assert_eq!(samples.count(), expected, "{phase}");
        }
        assert_eq!(
            rows.iter()
                .filter(|row| {
                    row["event"] == "shared_disk_wait"
                        && row["flight_id"] == flight_id
                        && row["outcome"] == "ready"
                })
                .count(),
            2
        );
        assert!(rows.iter().any(|row| {
            row["event"] == "shared_disk_lru"
                && row["cache"] == "text"
                && row["reason"] == "watcher"
                && row["count"] == 1
        }));
        let declarations = rows
            .iter()
            .find(|row| {
                row["event"] == "shared_disk_lookup"
                    && row["artifact"] == "declarations"
                    && row["outcome"] == "producer_requested"
            })
            .unwrap();
        let full = rows
            .iter()
            .find(|row| {
                row["event"] == "shared_disk_flight"
                    && row["artifact"] == "full_ast"
                    && row["phase"] == "parse"
                    && row["parse_mode"] == "fallback_full"
            })
            .unwrap();
        assert_ne!(declarations["flight_id"], full["flight_id"]);
        assert!(rows.iter().any(|row| {
            row["event"] == "shared_disk_flight"
                && row["artifact"] == "declarations"
                && row["phase"] == "handoff"
                && row["flight_id"] == declarations["flight_id"]
                && row["producer_flight_id"] == full["flight_id"]
        }));
    }

    #[test]
    fn missing_sources_invalidate_only_a_known_revision() {
        let project = TestProject::new();
        let path = project.root.join("missing.go");
        let key = path_key(&path);
        let cache = SharedDiskCache::default();
        let initial = cache.generation();
        assert!(cache.read(&path, Some(&SHARED_LOAD), None).is_err());
        assert_eq!(cache.generation(), initial);

        project.write("missing.go", "package p\n");
        cache.read(&path, Some(&SHARED_LOAD), None).unwrap();
        fs::remove_file(&path).unwrap();
        assert!(cache.read(&path, Some(&SHARED_LOAD), None).is_err());
        assert_eq!(cache.invalidated_since(initial), Some(vec![key.clone()]));
        assert!(cache.read(&path, Some(&SHARED_LOAD), None).is_err());
        assert_eq!(cache.generation(), initial + 1);

        for _ in 0..INVALIDATION_HISTORY {
            cache.invalidate(&key);
        }
        assert_eq!(cache.invalidated_since(initial), None);
        assert_eq!(
            cache.invalidated_since(initial + 1),
            Some(vec![key; INVALIDATION_HISTORY])
        );
    }

    #[test]
    fn body_elided_dependency_index_is_cached_separately_from_full_source() {
        let project = TestProject::new();
        let mut text = "package p\nvar Inferred = func() int { return 1 }()\n".to_owned();
        text.push_str("func Body(n int) {");
        text.push_str("println(\"𐐀\", n + 1)");
        text.push_str("}\n");
        let file = project.write("body.go", &text);
        let cache = SharedDiskCache::default();
        let cancelled = AtomicBool::new(false);
        let text = cache.read(&file, None, None).unwrap();
        let mut parser = parser();
        let dependency = cache
            .load_declarations(
                &mut parser,
                &file,
                Some(text.clone()),
                Some(&cancelled),
                None,
            )
            .unwrap();
        assert!(
            dependency
                .declarations
                .index
                .symbols
                .declaration("Body")
                .is_some()
        );
        assert!(matches!(
            dependency
                .declarations
                .index
                .symbols
                .declaration("Inferred")
                .unwrap()
                .kind,
            crate::symbols::SymbolKind::InferredValue { .. }
        ));
        assert!(cache.cached_parse(&file, &text).is_none());
        assert_eq!(cache.projection_counts(&file), (0, 1));
        let complete = cache
            .load_prefetched(&mut parser, &file, Some(text.clone()), None, None)
            .unwrap();
        let source = complete.into_indexed();
        let body = source
            .tree
            .root_node()
            .named_child(2)
            .unwrap()
            .child_by_field_name("body")
            .unwrap();
        assert!(source.text[body.byte_range()].contains("println"));
        assert!(Arc::ptr_eq(
            &dependency.declarations,
            &cache
                .source_declarations(&source, Some(&cancelled))
                .unwrap()
        ));
        assert_eq!(cache.projection_counts(&file), (1, 1));

        cache
            .parsed
            .lock()
            .unwrap()
            .declarations
            .remove(&ContentKey::new(&text));
        assert!(cache.cached_declarations(&text).is_none());
        let refilled = cache
            .source_declarations(&source, Some(&cancelled))
            .unwrap();
        cache.clear_full_cache();
        assert!(cache.cached_parse(&file, &text).is_none());
        assert!(Arc::ptr_eq(
            &refilled,
            &cache.cached_declarations(&text).unwrap()
        ));
        assert_eq!(cache.projection_counts(&file), (1, 2));
    }

    #[test]
    fn lexical_postings_reuse_content_across_paths_without_reusing_changed_text() {
        let project = TestProject::new();
        let old_text = "package p\nvar Before int\n";
        let first = project.write("first.go", old_text);
        let second = project.write("second.go", old_text);
        let cache = SharedDiskCache::default();
        let cancelled = AtomicBool::new(false);
        let first_text = cache.read(&first, None, None).unwrap();
        let second_text = cache.read(&second, None, None).unwrap();
        let original = cache
            .reference_identifiers(&first_text, &cancelled)
            .unwrap();
        assert!(Arc::ptr_eq(
            &original,
            &cache
                .reference_identifiers(&second_text, &cancelled)
                .unwrap(),
        ));

        project.write("first.go", "package p\nvar Changed int\n");
        cache.invalidate(&path_key(&first));
        let first_text = cache.read(&first, None, None).unwrap();
        let changed = cache
            .reference_identifiers(&first_text, &cancelled)
            .unwrap();
        assert!(!Arc::ptr_eq(&original, &changed));
        assert_eq!(original.offsets("Before").unwrap().len(), 1);
        assert!(changed.offsets("Before").unwrap().is_empty());
        assert_eq!(changed.offsets("Changed").unwrap().len(), 1);
        assert_eq!(cache.counts(&first), (2, 0));
        assert_eq!(cache.counts(&second), (1, 0));
    }

    #[test]
    fn cold_read_and_parse_are_each_shared_across_consumers() {
        let project = TestProject::new();
        let path = project.write("p.go", "package p\nvar Value = 1\n");
        let document = path_key(&project.write("document.go", "package p\n"));
        let cache = SharedDiskCache::default();
        let read = Arc::new(Barrier::new(2));
        let parse = Arc::new(Barrier::new(2));
        cache.set_hook(Some(Arc::new(move |_, event| {
            match event {
                CacheEvent::Read | CacheEvent::WaitRead => read.wait(),
                CacheEvent::Parse | CacheEvent::WaitParse => parse.wait(),
                _ => return,
            };
        })));
        thread::scope(|scope| {
            let load = || {
                let trace = DiagnosticIo::start(&document);
                let source = cache
                    .load(&mut parser(), &path, Some(&SHARED_LOAD), None)
                    .unwrap();
                assert_eq!(
                    trace.counts(),
                    ColdDependencyCounts {
                        reads: 1,
                        parses: 1
                    }
                );
                source
            };
            let first = scope.spawn(load);
            let second = scope.spawn(load);
            let first = first.join().unwrap();
            let second = second.join().unwrap();
            assert!(Arc::ptr_eq(&first.revision, &second.revision));
            assert_eq!(first.into_indexed().path, path);
        });
        cache.set_hook(None);
        assert_eq!(cache.counts(&path), (1, 1));
        let trace = DiagnosticIo::start(&document);
        cache.load(&mut parser(), &path, None, None).unwrap();
        cache.load(&mut parser(), &document, None, None).unwrap();
        assert_eq!(cache.counts(&path), (1, 1));
        assert_eq!(cache.counts(&document), (1, 1));
        assert_eq!(trace.counts(), ColdDependencyCounts::default());
    }

    #[test]
    fn cancelled_producer_preserves_waiters_and_the_warm_artifact() {
        let project = TestProject::new();
        let path = project.write("p.go", "package p\nvar Value = 1\n");
        let cache = SharedDiskCache::default();
        let (entered, parsing) = crossbeam_channel::bounded(1);
        let (joined, waiting) = crossbeam_channel::bounded(1);
        let producer_release = Arc::new(Barrier::new(2));
        cache.set_hook(Some(Arc::new({
            let release = producer_release.clone();
            move |_, event| match event {
                CacheEvent::Parse => {
                    entered.send(()).unwrap();
                    release.wait();
                }
                CacheEvent::WaitParse => joined.send(()).unwrap(),
                _ => {}
            }
        })));
        let cancelled = AtomicBool::new(false);
        thread::scope(|scope| {
            let producer = scope.spawn(|| cache.load(&mut parser(), &path, Some(&cancelled), None));
            parsing.recv().unwrap();
            let waiter = scope.spawn(|| cache.load(&mut parser(), &path, Some(&SHARED_LOAD), None));
            waiting.recv().unwrap();
            cancelled.store(true, Relaxed);
            assert!(producer.join().unwrap().is_err());
            producer_release.wait();
            assert!(waiter.join().unwrap().is_ok());
        });
        cache.set_hook(None);
        assert_eq!(cache.counts(&path), (1, 1));
        cache.load(&mut parser(), &path, None, None).unwrap();
        assert_eq!(cache.counts(&path), (1, 1));
    }

    #[test]
    fn producer_panics_wake_every_subscriber_and_allow_retry() {
        for (producing, subscribing, expected) in [
            (CacheEvent::Read, CacheEvent::WaitRead, (2, 1)),
            (CacheEvent::Parse, CacheEvent::WaitParse, (1, 2)),
        ] {
            let project = TestProject::new();
            let path = project.write("p.go", "package p\nvar Value int\n");
            let cache = SharedDiskCache::default();
            let (entered, started) = bounded(1);
            let (joined, subscribed) = bounded(1);
            let (release, blocked) = bounded(1);
            cache.set_hook(Some(Arc::new(move |_, event| {
                if event == producing {
                    entered.send(()).unwrap();
                    blocked.recv_timeout(Duration::from_secs(5)).unwrap();
                    panic!("injected Go source producer panic at {producing:?}");
                }
                if event == subscribing {
                    joined.send(()).unwrap();
                }
            })));
            let cancelled = AtomicBool::new(false);
            thread::scope(|scope| {
                let (done, result) = bounded(2);
                let limit = Duration::from_secs(5);
                let load = |done: Sender<_>| {
                    let result = cache
                        .load(&mut parser(), &path, Some(&cancelled), None)
                        .map(|_| ());
                    let _ = done.send(result);
                };
                let producer_done = done.clone();
                scope.spawn(move || load(producer_done));
                let started = started.recv_timeout(limit);
                scope.spawn(move || load(done));
                let subscribed = subscribed.recv_timeout(limit);
                let _ = release.send_timeout((), limit);
                let outcomes = [result.recv_timeout(limit), result.recv_timeout(limit)];
                cancelled.store(true, Relaxed);
                started.unwrap();
                subscribed.unwrap();
                for outcome in outcomes {
                    assert!(
                        outcome
                            .unwrap()
                            .unwrap_err()
                            .to_string()
                            .contains("aborted")
                    );
                }
            });
            cache.set_hook(None);
            assert!(
                cache
                    .load(&mut parser(), &path, Some(&SHARED_LOAD), None)
                    .is_ok()
            );
            assert_eq!(cache.counts(&path), expected);
        }
    }

    #[test]
    fn synchronous_consumers_defer_competing_reads_and_parses() {
        for event in [CacheEvent::Read, CacheEvent::Parse] {
            let project = TestProject::new();
            let path = project.write("p.go", "package p\nvar Value int\n");
            let cache = SharedDiskCache::default();
            let (entered, started) = bounded(1);
            let (release, blocked) = bounded(1);
            cache.set_hook(Some(Arc::new(move |_, observed| {
                if observed == event {
                    entered.send(()).unwrap();
                    blocked.recv_timeout(Duration::from_secs(5)).unwrap();
                }
            })));
            thread::scope(|scope| {
                let producer =
                    scope.spawn(|| cache.load(&mut parser(), &path, Some(&SHARED_LOAD), None));
                started.recv_timeout(Duration::from_secs(5)).unwrap();
                let (done, response) = bounded(1);
                let cache = &cache;
                let path = &path;
                scope.spawn(move || {
                    let result = cache.load(&mut parser(), path, None, None);
                    let _ = done.send(result);
                });
                let result = response.recv_timeout(Duration::from_millis(250));
                let pending = result
                    .ok()
                    .and_then(Result::err)
                    .and_then(|error| error.downcast::<SharedLoadPending>().ok());
                let premature = pending.as_ref().is_some_and(SharedLoadPending::ready);
                release.send(()).unwrap();
                assert!(producer.join().unwrap().is_ok());
                assert!(!premature);
                assert!(
                    pending
                        .expect("a synchronous competitor must defer")
                        .ready()
                );
            });
            cache.set_hook(None);
            assert_eq!(cache.counts(&path), (1, 1));
        }
    }

    #[test]
    fn watched_replacement_prevents_an_older_read_from_installing() {
        let project = TestProject::new();
        let path = project.write("p.go", "package p\nvar Old = 1\n");
        let cache = SharedDiskCache::default();
        let (read_done, read) = crossbeam_channel::bounded(1);
        let resume = Arc::new(Barrier::new(2));
        let first = AtomicBool::new(true);
        cache.set_hook(Some(Arc::new({
            let resume = resume.clone();
            move |_, event| {
                if event == CacheEvent::ReadFinished && first.swap(false, Relaxed) {
                    read_done.send(()).unwrap();
                    resume.wait();
                }
            }
        })));
        thread::scope(|scope| {
            let loading = scope.spawn(|| {
                cache
                    .load(&mut parser(), &path, Some(&SHARED_LOAD), None)
                    .unwrap()
            });
            read.recv().unwrap();
            fs::remove_file(&path).unwrap();
            project.write("p.go", "package p\nvar New = 2\n");
            cache.invalidate(&path_key(&path));
            let replaced = cache.load(&mut parser(), &path, None, None).unwrap();
            resume.wait();
            let concurrent = loading.join().unwrap();
            assert!(Arc::ptr_eq(&replaced.revision, &concurrent.revision));
            assert!(replaced.into_indexed().text.contains("New"));
        });
        assert_eq!(cache.counts(&path), (2, 1));
    }

    #[test]
    fn content_reuse_preserves_path_and_eviction_preserves_the_lru() {
        let project = TestProject::new();
        let original = "package p\nvar Value = 1\n";
        let a = project.write("a.go", original);
        let b = project.write("b_test.go", original);
        let c = project.write("c.go", "package p\nvar Other string\n");
        let cache = SharedDiskCache::new(2, TEXT_BYTES, 1, PARSED_BYTES);
        let mut parser = parser();
        let first = cache.load(&mut parser, &a, None, None).unwrap();
        let alias = cache.load(&mut parser, &b, None, None).unwrap();
        assert!(Arc::ptr_eq(&first.revision, &alias.revision));
        assert_eq!(alias.into_indexed().path, b);
        assert_eq!(first.into_indexed().path, a);
        assert_eq!(cache.counts(&b), (1, 0));

        cache.read(&a, None, None).unwrap();
        cache.load(&mut parser, &c, None, None).unwrap();
        cache.load(&mut parser, &a, None, None).unwrap();
        assert_eq!(cache.counts(&a), (1, 2));
        cache.load(&mut parser, &b, None, None).unwrap();
        assert_eq!(cache.counts(&b), (2, 0));

        fs::remove_file(&a).unwrap();
        assert!(cache.read(&a, None, None).is_err());
        project.write("a.go", original);
        cache.load(&mut parser, &a, None, None).unwrap();
        assert_eq!(cache.counts(&a), (2, 2));
    }

    #[test]
    fn package_names_outlive_evicted_text_and_parse_artifacts() {
        let project = TestProject::new();
        let small = project.write("small.go", "package sample\nvar Old int\n");
        let replacement = project.write("replacement.go", "package sample\nvar Large string\n");
        let cache = SharedDiskCache::new(1, TEXT_BYTES, 1, PARSED_BYTES);
        let loaded = cache.load(&mut parser(), &small, None, None).unwrap();
        let stamp = loaded.text.stamp.unwrap();
        let text = Arc::downgrade(&loaded.text.text);
        let revision = Arc::downgrade(&loaded.revision);
        drop(loaded);
        cache.load(&mut parser(), &replacement, None, None).unwrap();

        assert!(cache.cached_read(&small, &path_key(&small)).is_none());
        assert!(text.upgrade().is_some()); // The declaration key retains its verified bytes.
        assert!(revision.upgrade().is_none());
        let names = cache
            .cached_package_names(&path_key(&small), stamp, "sample")
            .unwrap();
        assert_eq!(names.file_declarations().collect::<Vec<_>>(), ["Old"]);
        cache.invalidate(&path_key(&small));
        assert!(
            cache
                .cached_package_names(&path_key(&small), stamp, "sample")
                .is_none()
        );
    }

    #[test]
    fn prefetched_oversized_text_stays_reusable_until_invalidated() {
        let project = TestProject::new();
        let path = project.write("p.go", "package p\nvar Original = 1\n");
        let cache = SharedDiskCache::new(TEXT_ENTRIES, 1, PARSED_ENTRIES, PARSED_BYTES);
        let prefetched = cache.read(&path, None, None).unwrap();
        let mut parser = parser();
        let source = cache
            .load_prefetched(&mut parser, &path, Some(prefetched.clone()), None, None)
            .unwrap();
        assert!(source.into_indexed().text.contains("Original"));
        assert_eq!(cache.counts(&path), (1, 1));

        project.write("p.go", "package p\nvar Updated = 2\n");
        cache.invalidate(&path_key(&path));
        let source = cache
            .load_prefetched(&mut parser, &path, Some(prefetched), None, None)
            .unwrap();
        assert!(source.into_indexed().text.contains("Updated"));
        assert_eq!(cache.counts(&path), (2, 2));
    }
}
