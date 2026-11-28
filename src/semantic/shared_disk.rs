use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::hash::{Hash, Hasher};
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, SendTimeoutError, Sender, bounded};
use tree_sitter::Parser;

use super::completion::prefilter::Stamp;
use super::completion::render::package_name;
use super::diagnostics::has_reliable_package_scope;
use super::index::{
    IndexedSourceRevision, NavigationText, PreparedReferenceSource, ReferenceIdentifiers,
    text_fingerprint,
};
use super::navigation_trace::{NavigationProbe, record, sample};
use super::source::{EditorNames, check_cancelled};
use crate::workspace::path_key;

const TEXT_ENTRIES: usize = 2048;
const TEXT_BYTES: usize = 64 * 1024 * 1024;
const PARSED_ENTRIES: usize = 1024;
const PARSED_BYTES: usize = 256 * 1024 * 1024;
const PACKAGE_NAMES_ENTRIES: usize = 4096;
const PACKAGE_NAMES_BYTES: usize = 16 * 1024 * 1024;
const REFERENCE_INDEX_ENTRIES: usize = 1024;
const REFERENCE_INDEX_BYTES: usize = 64 * 1024 * 1024;
const OBSERVATION_RETRIES: usize = 3;
const LOAD_QUEUE: usize = 32;
const DECLARATION_PARSE_BYTES: usize = 128 * 1024;
const CANCELLATION_INTERVAL: Duration = Duration::from_millis(5);

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

static LOAD_POOL: LazyLock<Sender<LoadJob>> = LazyLock::new(|| {
    let (send, receive) = bounded::<LoadJob>(LOAD_QUEUE);
    let threads = thread::available_parallelism()
        .map_or(4, usize::from)
        .clamp(4, 8);
    for index in 0..threads {
        let receive = receive.clone();
        thread::Builder::new()
            .name(format!("go-shared-load-{index}"))
            .spawn(move || {
                let mut parser = Parser::new();
                parser
                    .set_language(&tree_sitter_go::LANGUAGE.into())
                    .expect("Go parser language");
                while let Ok(job) = receive.recv() {
                    if catch_unwind(AssertUnwindSafe(|| job(&mut parser))).is_err() {
                        parser.reset();
                        let _ = parser.set_included_ranges(&[]);
                    }
                }
            })
            .expect("start Go source loader");
    }
    send
});

#[derive(Clone)]
pub(super) struct SharedDiskCache {
    files: Arc<Mutex<Files>>,
    parsed: Arc<Mutex<Parsed>>,
    reference_identifiers: Arc<Mutex<Lru<ContentKey, Arc<ReferenceIdentifiers>>>>,
    wake: (Sender<()>, Receiver<()>),
    #[cfg(test)]
    checks: Arc<Mutex<TestChecks>>,
}

impl Default for SharedDiskCache {
    fn default() -> Self {
        Self::new(TEXT_ENTRIES, TEXT_BYTES, PARSED_ENTRIES, PARSED_BYTES)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileObservation {
    stamp: Stamp,
    created: Option<SystemTime>,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

impl FileObservation {
    fn read(path: &Path) -> io::Result<Self> {
        let metadata = fs::metadata(path)?;
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
        Ok(Self {
            stamp: metadata.into(),
            created,
            #[cfg(unix)]
            identity,
        })
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
}

impl TextRevision {
    fn navigation_text(&self, key: &Path, generation: u64) -> NavigationText {
        let mut text = self.text.clone();
        text.read_ticket = Some(FileReadTicket {
            key: key.to_owned(),
            observation: self.observation,
            fingerprint: self.fingerprint,
            generation,
        });
        text
    }
}

impl FileReadTicket {
    pub(super) fn same_revision(&self, other: &Self) -> bool {
        self.observation == other.observation && self.fingerprint == other.fingerprint
    }

    pub(super) fn matches_disk(&self, path: &Path) -> bool {
        FileObservation::read(path).ok() == Some(self.observation)
    }
}

struct Files {
    cached: Lru<PathBuf, Arc<TextRevision>>,
    package_names: Lru<PathBuf, PackageNames>,
    loading: HashMap<PathBuf, (FileObservation, Arc<ReadFlight>)>,
    generation: u64,
}

struct PackageNames {
    observation: FileObservation,
    names: Option<(String, Arc<EditorNames>)>,
}

type ReadFlight = Flight<Option<Arc<TextRevision>>>;

#[derive(Clone)]
struct ContentKey {
    fingerprint: u64,
    text: Arc<String>,
}

impl PartialEq for ContentKey {
    fn eq(&self, other: &Self) -> bool {
        self.fingerprint == other.fingerprint
            && (Arc::ptr_eq(&self.text, &other.text) || self.text == other.text)
    }
}

impl Eq for ContentKey {}

impl Hash for ContentKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.fingerprint.hash(state);
        self.text.len().hash(state);
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct ParseKey {
    content: ContentKey,
    allow_elided: bool,
}

struct Parsed {
    cached: Lru<ParseKey, Arc<IndexedSourceRevision>>,
    loading: HashMap<ParseKey, Arc<Flight<Arc<IndexedSourceRevision>>>>,
}

impl SharedDiskCache {
    fn new(
        text_entries: usize,
        text_bytes: usize,
        parse_entries: usize,
        parse_bytes: usize,
    ) -> Self {
        Self {
            files: Arc::new(Mutex::new(Files {
                cached: Lru::new(text_entries, text_bytes),
                package_names: Lru::new(PACKAGE_NAMES_ENTRIES, PACKAGE_NAMES_BYTES),
                loading: HashMap::new(),
                generation: 0,
            })),
            parsed: Arc::new(Mutex::new(Parsed {
                cached: Lru::new(parse_entries, parse_bytes),
                loading: HashMap::new(),
            })),
            reference_identifiers: Arc::new(Mutex::new(Lru::new(
                REFERENCE_INDEX_ENTRIES,
                REFERENCE_INDEX_BYTES,
            ))),
            wake: bounded(1),
            #[cfg(test)]
            checks: Arc::default(),
        }
    }

    pub(super) fn read_pending(&self, key: &Path) -> bool {
        self.files
            .try_lock()
            .map_or(true, |files| files.loading.contains_key(key))
    }

    pub(super) fn wake(&self) -> &Receiver<()> {
        &self.wake.1
    }

    pub(super) fn invalidate(&self, key: &Path) {
        let removed = {
            let mut files = self.files.lock().unwrap();
            files.generation += 1;
            files.loading.remove(key);
            (files.cached.remove(key), files.package_names.remove(key))
        };
        drop(removed);
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
        self.parsed
            .try_lock()
            .ok()?
            .cached
            .peek(&ParseKey {
                content: ContentKey {
                    fingerprint: text.fingerprint,
                    text: text.text.text.clone(),
                },
                allow_elided: false,
            })
            .cloned()
    }

    /// Reuse names for the revision in the captured graph without touching
    /// the independent text and parse caches.
    pub(super) fn cached_package_names(
        &self,
        key: &Path,
        stamp: Stamp,
        package: &str,
    ) -> Option<Arc<EditorNames>> {
        let mut files = self.files.try_lock().ok()?;
        let summary = files.package_names.get(key)?;
        let (name, names) = summary.names.as_ref()?;
        (summary.observation.stamp == stamp && name == package).then(|| names.clone())
    }

    pub(super) fn read(
        &self,
        path: &Path,
        cancelled: Option<&AtomicBool>,
        probe: Option<&NavigationProbe>,
    ) -> Result<NavigationText> {
        let key = path_key(path);
        let (text, generation) = self.read_revision(path, &key, cancelled, probe)?;
        Ok(text.navigation_text(&key, generation))
    }

    pub(super) fn cached_read(&self, path: &Path, key: &Path) -> Option<NavigationText> {
        let observation = FileObservation::read(path).ok()?;
        let mut files = self.files.lock().unwrap();
        let generation = files.generation;
        let text = files.cached.get(key)?;
        (text.observation == observation).then(|| text.navigation_text(key, generation))
    }

    pub(super) fn cached_parse(
        &self,
        path: &Path,
        text: &NavigationText,
        allow_elided: bool,
    ) -> Option<PreparedReferenceSource> {
        let fingerprint = text.read_ticket.as_ref()?.fingerprint;
        let mut parsed = self.parsed.lock().unwrap();
        let mut key = ParseKey {
            content: ContentKey {
                fingerprint,
                text: text.text.clone(),
            },
            allow_elided: false,
        };
        let revision = parsed.cached.get(&key).cloned().or_else(|| {
            key.allow_elided = true;
            allow_elided
                .then(|| parsed.cached.get(&key).cloned())
                .flatten()
        })?;
        Some(PreparedReferenceSource::new(
            revision,
            path,
            text.clone(),
            fingerprint,
        ))
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
        self.load_parse(parser, path, prefetched, cancelled, probe, false)
    }

    pub(super) fn load_declarations(
        &self,
        parser: &mut Parser,
        path: &Path,
        prefetched: NavigationText,
        cancelled: &AtomicBool,
        probe: Option<&NavigationProbe>,
    ) -> Result<PreparedReferenceSource> {
        self.load_parse(parser, path, Some(prefetched), Some(cancelled), probe, true)
    }

    fn load_parse(
        &self,
        parser: &mut Parser,
        path: &Path,
        prefetched: Option<NavigationText>,
        cancelled: Option<&AtomicBool>,
        probe: Option<&NavigationProbe>,
        allow_elided: bool,
    ) -> Result<PreparedReferenceSource> {
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
                self.read_revision(path, &key, cancelled, probe)?
            };
            let revision = self.parse(
                parser,
                path,
                ParseKey {
                    content: ContentKey {
                        fingerprint: text.fingerprint,
                        text: text.text.text.clone(),
                    },
                    allow_elided: allow_elided && text.text.text.len() >= DECLARATION_PARSE_BYTES,
                },
                cancelled,
                probe,
            )?;
            if self.current(path, &key, text.observation, &text.text.text, generation) {
                if !revision.declarations_only {
                    self.remember_package_names(
                        &key,
                        &text,
                        generation,
                        &revision,
                        cancelled.unwrap_or(&SHARED_LOAD),
                    )?;
                }
                return Ok(PreparedReferenceSource::new(
                    revision,
                    path,
                    text.navigation_text(&key, generation),
                    text.fingerprint,
                ));
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

    fn remember_package_names(
        &self,
        key: &Path,
        text: &TextRevision,
        generation: u64,
        revision: &IndexedSourceRevision,
        cancelled: &AtomicBool,
    ) -> Result<()> {
        if self
            .files
            .lock()
            .unwrap()
            .package_names
            .get(key)
            .is_some_and(|summary| summary.observation == text.observation)
        {
            return Ok(());
        }
        let source = revision.clone().into_indexed();
        let names = if has_reliable_package_scope(source.tree.root_node(), Some(cancelled))? {
            package_name(&source).map(|name| (name.to_owned(), source.editor_names.clone()))
        } else {
            None
        };
        let weight = names.as_ref().map_or(0, |(name, names)| {
            name.len().saturating_add(names.cache_weight())
        });
        let retired = {
            let mut files = self.files.lock().unwrap();
            let current = files.generation == generation
                || files.cached.peek(key).is_some_and(|current| {
                    current.observation == text.observation
                        && Arc::ptr_eq(&current.text.text, &text.text.text)
                });
            current.then(|| {
                files.package_names.insert(
                    key.to_owned(),
                    PackageNames {
                        observation: text.observation,
                        names,
                    },
                    weight,
                )
            })
        };
        drop(retired);
        Ok(())
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
        drop(retired);
        Ok(identifiers)
    }

    pub(super) fn cached_reference_identifiers(
        &self,
        text: &NavigationText,
    ) -> Option<Arc<ReferenceIdentifiers>> {
        let key = ContentKey {
            fingerprint: text.read_ticket.as_ref()?.fingerprint,
            text: text.text.clone(),
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
        let fingerprint = text
            .read_ticket
            .as_ref()
            .map_or_else(|| text_fingerprint(&text.text), |ticket| ticket.fingerprint);
        record(probe, |counts| counts.text_hits += 1);
        let revision = self.parse(
            parser,
            path,
            ParseKey {
                content: ContentKey {
                    fingerprint,
                    text: text.text.clone(),
                },
                allow_elided: false,
            },
            None,
            probe,
        )?;
        Ok(PreparedReferenceSource::new(
            revision,
            path,
            text,
            fingerprint,
        ))
    }

    fn read_revision(
        &self,
        path: &Path,
        key: &Path,
        cancelled: Option<&AtomicBool>,
        probe: Option<&NavigationProbe>,
    ) -> Result<(Arc<TextRevision>, u64)> {
        for _ in 0..OBSERVATION_RETRIES {
            check_cancelled(cancelled.unwrap_or(&SHARED_LOAD))?;
            let observation = FileObservation::read(path)
                .map_err(|error| {
                    if error.kind() == io::ErrorKind::NotFound {
                        self.invalidate(key);
                    }
                    error
                })
                .with_context(|| format!("reading {}", path.display()))?;
            let (flight, producer, generation) = {
                let mut files = self.files.lock().unwrap();
                if let Some(text) = files
                    .cached
                    .get(key)
                    .filter(|text| text.observation == observation)
                {
                    record(probe, |counts| counts.text_hits += 1);
                    return Ok((text.clone(), files.generation));
                }
                let generation = files.generation;
                let (flight, producer) = match files.loading.get(key) {
                    Some((current, flight)) if *current == observation => (flight.clone(), false),
                    _ => {
                        let flight = Arc::new(Flight::new(cancelled.map(|_| self.wake.0.clone())));
                        files
                            .loading
                            .insert(key.to_owned(), (observation, flight.clone()));
                        (flight, true)
                    }
                };
                (flight, producer, generation)
            };
            let _read = producer.then(|| sample(probe, |counts| &mut counts.read));
            if producer {
                record(probe, |counts| {
                    counts.text_misses += 1;
                    counts.disk_reads += 1;
                });
                self.start_read(path, key, observation, &flight, cancelled)?;
                diagnostic_cold_read(key);
            } else {
                diagnostic_cold_read(key);
                record(probe, |counts| counts.text_hits += 1);
                #[cfg(test)]
                self.test_event(key, CacheEvent::WaitRead);
            }
            if let Some(text) = flight.wait(cancelled)?
                && self.current(path, key, text.observation, &text.text.text, generation)
            {
                return Ok((text, generation));
            }
        }
        Err(changed_source(path))
    }

    fn start_read(
        &self,
        path: &Path,
        key: &Path,
        observation: FileObservation,
        flight: &Arc<ReadFlight>,
        cancelled: Option<&AtomicBool>,
    ) -> Result<()> {
        let abort = AbortOnDrop(Some({
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
        }));
        let cache = self.clone();
        let path = path.to_owned();
        let key = key.to_owned();
        let flight = flight.clone();
        let job = move || {
            let mut abort = abort;
            cache.produce_read(&path, &key, observation, &flight);
            abort.0.take();
        };
        if let Some(cancelled) = cancelled {
            submit_load(Box::new(move |_| job()), cancelled)
        } else {
            job();
            Ok(())
        }
    }

    fn produce_read(
        &self,
        path: &Path,
        key: &Path,
        observation: FileObservation,
        flight: &Arc<ReadFlight>,
    ) {
        #[cfg(test)]
        self.test_event(key, CacheEvent::Read);
        let result = NavigationText::read_cancellable(path, &SHARED_LOAD);
        #[cfg(test)]
        self.test_event(key, CacheEvent::ReadFinished);
        let result = result.map(|text| {
            (text.stamp == Some(observation.stamp)
                && FileObservation::read(path).ok() == Some(observation))
            .then(|| {
                Arc::new(TextRevision {
                    observation,
                    fingerprint: text_fingerprint(&text.text),
                    text,
                })
            })
        });
        let retired = {
            let mut files = self.files.lock().unwrap();
            if files
                .loading
                .get(key)
                .is_some_and(|(_, current)| Arc::ptr_eq(current, flight))
            {
                let retired = result
                    .as_ref()
                    .ok()
                    .and_then(|text| text.as_ref())
                    .map(|text| {
                        files
                            .cached
                            .insert(key.to_owned(), text.clone(), text.text.text.len())
                    });
                flight.finish(result);
                files.loading.remove(key);
                retired
            } else {
                // An invalidation or a newer observation replaced this flight.
                flight.finish(Ok(None));
                None
            }
        };
        drop(retired);
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
        let files = self.files.lock().unwrap();
        files.generation == generation
            || files.cached.peek(key).is_some_and(|current| {
                current.observation == observation
                    && text.is_some_and(|text| Arc::ptr_eq(&current.text.text, text))
            })
    }

    fn parse(
        &self,
        parser: &mut Parser,
        path: &Path,
        mut key: ParseKey,
        cancelled: Option<&AtomicBool>,
        probe: Option<&NavigationProbe>,
    ) -> Result<Arc<IndexedSourceRevision>> {
        check_cancelled(cancelled.unwrap_or(&SHARED_LOAD))?;
        let (flight, producer) = {
            let mut parsed = self.parsed.lock().unwrap();
            if key.allow_elided {
                key.allow_elided = false;
                if let Some(revision) = parsed.cached.get(&key) {
                    return Ok(revision.clone());
                }
                let full_loading = parsed.loading.contains_key(&key);
                key.allow_elided = true;
                if let Some(revision) = parsed.cached.get(&key) {
                    return Ok(revision.clone());
                }
                key.allow_elided = !full_loading;
            }
            if let Some(revision) = parsed.cached.get(&key) {
                return Ok(revision.clone());
            }
            match parsed.loading.get(&key) {
                Some(flight) => (flight.clone(), false),
                None => {
                    let flight = Arc::new(Flight::new(cancelled.map(|_| self.wake.0.clone())));
                    parsed.loading.insert(key.clone(), flight.clone());
                    (flight, true)
                }
            }
        };
        let _parse = producer.then(|| sample(probe, |counts| &mut counts.parse));
        if producer {
            record(probe, |counts| counts.ast_parses += 1);
            self.start_parse(parser, path, key, &flight, cancelled)?;
            diagnostic_cold_parse(path);
        } else {
            diagnostic_cold_parse(path);
            #[cfg(test)]
            self.test_event(&path_key(path), CacheEvent::WaitParse);
        }
        flight.wait(cancelled)
    }

    fn start_parse(
        &self,
        parser: &mut Parser,
        path: &Path,
        key: ParseKey,
        flight: &Arc<Flight<Arc<IndexedSourceRevision>>>,
        cancelled: Option<&AtomicBool>,
    ) -> Result<()> {
        let abort = AbortOnDrop(Some({
            let parsed = self.parsed.clone();
            let key = key.clone();
            let flight = flight.clone();
            move || {
                let mut parsed = parsed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if parsed
                    .loading
                    .get(&key)
                    .is_some_and(|current| Arc::ptr_eq(current, &flight))
                {
                    parsed.loading.remove(&key);
                }
                flight.finish(Err(anyhow::anyhow!("shared Go source parse aborted")));
            }
        }));
        let cache = self.clone();
        let path = path.to_owned();
        let flight = flight.clone();
        let produce = move |parser: &mut Parser| {
            let mut abort = abort;
            cache.produce_parse(parser, &path, key, &flight);
            abort.0.take();
        };
        if let Some(cancelled) = cancelled {
            submit_load(Box::new(produce), cancelled)
        } else {
            produce(parser);
            Ok(())
        }
    }

    fn produce_parse(
        &self,
        parser: &mut Parser,
        path: &Path,
        key: ParseKey,
        flight: &Arc<Flight<Arc<IndexedSourceRevision>>>,
    ) {
        #[cfg(test)]
        self.test_event(&path_key(path), CacheEvent::Parse);
        let result = IndexedSourceRevision::parse_disk(
            parser,
            path,
            key.content.text.clone(),
            &SHARED_LOAD,
            key.allow_elided,
        );
        let retired = {
            let mut parsed = self.parsed.lock().unwrap();
            let mut retired = Vec::new();
            let result = result.map(|(revision, nodes)| {
                let cache_key = ParseKey {
                    content: key.content.clone(),
                    allow_elided: revision.declarations_only,
                };
                if let Some(existing) = parsed.cached.get(&cache_key) {
                    return existing.clone();
                }
                let revision = Arc::new(revision);
                // Tree-sitter does not expose an allocation size. Nodes and
                // input bytes also bound the size of the derived indexes.
                let weight = nodes
                    .saturating_mul(64)
                    .saturating_add(key.content.text.len());
                retired = parsed.cached.insert(cache_key, revision.clone(), weight);
                revision
            });
            flight.finish(result);
            parsed.loading.remove(&key);
            retired
        };
        drop(retired);
    }
}

fn submit_load(mut job: LoadJob, cancelled: &AtomicBool) -> Result<()> {
    loop {
        check_cancelled(cancelled)?;
        match LOAD_POOL.send_timeout(job, CANCELLATION_INTERVAL) {
            Ok(()) => return Ok(()),
            Err(SendTimeoutError::Timeout(returned)) => job = returned,
            Err(SendTimeoutError::Disconnected(_)) => {
                anyhow::bail!("Go source loading pool stopped")
            }
        }
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
pub(crate) struct SharedLoadPending(Arc<AtomicBool>);

impl SharedLoadPending {
    pub(crate) fn new(finished: Arc<AtomicBool>) -> Self {
        Self(finished)
    }

    pub(crate) fn ready(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

impl std::fmt::Display for SharedLoadPending {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Go source is loading in the background")
    }
}

impl std::error::Error for SharedLoadPending {}

struct Flight<T> {
    result: Mutex<Option<FlightResult<T>>>,
    ready: Condvar,
    finished: Arc<AtomicBool>,
    wake: Option<Sender<()>>,
}

impl<T> Flight<T> {
    fn new(wake: Option<Sender<()>>) -> Self {
        Self {
            result: Mutex::new(None),
            ready: Condvar::new(),
            finished: Arc::default(),
            wake,
        }
    }
}

impl<T: Clone> Flight<T> {
    fn finish(&self, result: Result<T>) {
        let mut published = self
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if published.is_some() {
            return;
        }
        *published = Some(result.map_err(Arc::new));
        drop(published);
        self.finished.store(true, Ordering::Release);
        if let Some(wake) = &self.wake {
            let _ = wake.try_send(());
        }
        self.ready.notify_all();
    }

    fn wait(&self, cancelled: Option<&AtomicBool>) -> Result<T> {
        let Some(cancelled) = cancelled else {
            let result = self
                .result
                .try_lock()
                .ok()
                .and_then(|result| result.clone());
            return result.map_or_else(
                || Err(SharedLoadPending::new(self.finished.clone()).into()),
                Self::clone_result,
            );
        };
        let mut result = self.result.lock().unwrap();
        loop {
            check_cancelled(cancelled)?;
            if let Some(result) = result.as_ref() {
                return Self::clone_result(result.clone());
            }
            result = self
                .ready
                .wait_timeout(result, CANCELLATION_INTERVAL)
                .unwrap()
                .0;
        }
    }

    fn clone_result(result: FlightResult<T>) -> Result<T> {
        result.map_err(|error| match error.downcast_ref::<io::Error>() {
            Some(io) => io::Error::new(io.kind(), format!("{error:#}")).into(),
            None => anyhow::anyhow!("{error:#}"),
        })
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

    fn insert(&mut self, key: K, value: V, bytes: usize) -> Vec<V> {
        let mut retired = self.remove(&key).into_iter().collect::<Vec<_>>();
        if self.entry_limit == 0 || bytes > self.byte_limit {
            retired.push(value);
            return retired;
        }
        while self.entries.len() >= self.entry_limit || self.bytes + bytes > self.byte_limit {
            let (_, oldest) = self.age.pop_first().expect("cache is over capacity");
            if let Some(old) = self.remove(&oldest) {
                retired.push(old);
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

    pub(super) fn set_hook(&self, hook: Option<TestHook>) {
        self.checks.lock().unwrap().hook = hook;
    }

    fn test_event(&self, key: &Path, event: CacheEvent) {
        let hook = {
            let mut checks = self.checks.lock().unwrap();
            if matches!(event, CacheEvent::Read | CacheEvent::Parse) {
                let counts = checks.counts.entry(key.to_owned()).or_default();
                if event == CacheEvent::Read {
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
    use std::sync::Barrier;
    use std::sync::atomic::Ordering::Relaxed;
    use std::thread;

    fn parser() -> Parser {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
            .unwrap();
        parser
    }

    #[test]
    fn body_elided_dependency_index_is_cached_separately_from_full_source() {
        let project = TestProject::new();
        let mut text = "package p\nvar Inferred = func() int { return 1 }()\n".to_owned();
        text.push_str("func Body(n int) {");
        text.push_str(&"_ = n + 1; ".repeat(15_000));
        text.push_str("}\n");
        let file = project.write("body.go", &text);
        let cache = SharedDiskCache::default();
        let cancelled = AtomicBool::new(false);
        let text = cache.read(&file, None, None).unwrap();
        let mut parser = parser();
        let dependency = cache
            .load_declarations(&mut parser, &file, text.clone(), &cancelled, None)
            .unwrap();
        assert!(dependency.revision.declarations_only);
        let declarations = dependency.into_indexed();
        assert!(
            declarations
                .declarations()
                .symbols
                .declaration("Body")
                .is_some()
        );
        assert!(matches!(
            declarations
                .declarations()
                .symbols
                .declaration("Inferred")
                .unwrap()
                .kind,
            crate::symbols::SymbolKind::InferredValue { .. }
        ));
        assert!(cache.cached_parse(&file, &text, true).is_some());
        assert!(cache.cached_parse(&file, &text, false).is_none());
        let complete = cache
            .load_prefetched(&mut parser, &file, Some(text), None, None)
            .unwrap();
        assert!(!complete.revision.declarations_only);
        assert!(
            complete.into_indexed().tree.root_node().descendant_count()
                > declarations.tree.root_node().descendant_count()
        );
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
                CacheEvent::ReadFinished => return,
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

        assert!(text.upgrade().is_none());
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
