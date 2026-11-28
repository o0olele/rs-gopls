use std::cell::Cell;
use std::ffi::OsString;
use std::ops::{Deref, DerefMut};
use std::sync::{LockResult, MutexGuard, PoisonError, TryLockError, TryLockResult};

use crate::go_environment::GoEnvironment;
use crate::workspace::find_builtin_source;

use super::*;

#[cfg(test)]
mod tests;

const QUERY_ENTRIES: usize = 4096;
const QUERY_BYTES: usize = 512 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub(in crate::semantic) enum QueryReadLimit {
    Sources,
    Bytes,
}

impl std::fmt::Display for QueryReadLimit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Sources => "Go query source count limit exceeded",
            Self::Bytes => "Go query source memory limit exceeded",
        })
    }
}

impl std::error::Error for QueryReadLimit {}

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub(in crate::semantic) struct DiskSourceRev {
    key: PathBuf,
    #[serde(with = "wire_integer")]
    length: u64,
    #[serde(with = "wire_integer")]
    modified: Option<i128>,
    #[serde(with = "wire_integer")]
    created: Option<i128>,
    #[cfg(unix)]
    #[serde(with = "wire_integer")]
    identity: (u64, u64, i64, i64),
    #[serde(with = "wire_integer")]
    fingerprint: u64,
}

impl FileReadTicket {
    pub(in crate::semantic) fn source_revision(&self) -> DiskSourceRev {
        let (modified, length) = self.observation.stamp.revision_parts();
        DiskSourceRev {
            key: self.key.clone(),
            length,
            modified: modified.map(timestamp),
            created: self.observation.created.map(timestamp),
            #[cfg(unix)]
            identity: self.observation.identity,
            fingerprint: self.fingerprint,
        }
    }
}

fn timestamp(time: SystemTime) -> i128 {
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos() as i128,
        Err(before_epoch) => -(before_epoch.duration().as_nanos() as i128),
    }
}

// Client implementations may pass item data through JavaScript's Number.
// Encode fields wider than 53 bits as strings to preserve their exact value.
mod wire_integer {
    use serde::{Deserialize, Deserializer, Serialize, Serializer, de, ser};

    pub(super) fn serialize<T: Serialize, S: Serializer>(
        value: &T,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let encoded = serde_json::to_string(value).map_err(ser::Error::custom)?;
        serializer.serialize_str(&encoded)
    }

    pub(super) fn deserialize<'de, T: de::DeserializeOwned, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<T, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        serde_json::from_str(&encoded).map_err(de::Error::custom)
    }
}

/// A replay retains its dependencies even when the shared LRU declines them.
#[derive(Default)]
pub(crate) struct QueryReadMemo {
    generation: Cell<Option<u64>>,
    text: RefCell<HashMap<PathBuf, ReadState<Option<Arc<TextRevision>>>>>,
    verified: RefCell<HashMap<PathBuf, ReadState<Option<Arc<TextRevision>>>>>,
    full: RefCell<HashMap<ContentKey, ReadState<Arc<IndexedSourceRevision>>>>,
    declarations: RefCell<HashMap<ContentKey, ReadState<Arc<SharedDeclarations>>>>,
    documentation: RefCell<HashMap<ContentKey, ReadState<Arc<SharedDeclarations>>>>,
    recovered: RefCell<HashMap<ContentKey, ReadState<Arc<SharedDeclarations>>>>,
    builtin: RefCell<HashMap<BuiltinKey, ReadState<Arc<PathBuf>>>>,
    validated: RefCell<HashMap<ValidationKey, ReadState<()>>>,
    retention: Retention,
    pub(in crate::semantic) overlays: RefCell<HashMap<PathBuf, Option<(u64, u64)>>>,
}

#[derive(Default)]
struct Retention {
    entries: Cell<usize>,
    bytes: Cell<usize>,
}

enum ReadState<T> {
    InFlight {
        flight: Arc<Flight<T>>,
        subscribed: Option<Instant>,
    },
    Ready {
        value: T,
        from_flight: bool,
    },
    Failed(Arc<anyhow::Error>),
}

impl<T: Clone> ReadState<T> {
    fn poll(&mut self, retention: &Retention, weight: impl FnOnce(&T) -> usize) -> Result<T> {
        if let Self::InFlight { flight, subscribed } = self {
            let result = flight
                .result
                .get()
                .ok_or_else(|| SharedLoadPending::new(flight.finished.clone()))?;
            if let Some((trace, subscribed)) = flight.trace.zip(*subscribed) {
                trace.wait(subscribed, if result.is_ok() { "ready" } else { "failed" });
            }
            *self = match result {
                Ok(value) => Self::retain(value.clone(), true, retention, weight),
                Err(error) => Self::Failed(error.clone()),
            };
        }
        match self {
            Self::Ready { value, .. } => Ok(value.clone()),
            Self::Failed(error) => Err(clone_load_error(error.clone())),
            Self::InFlight { .. } => unreachable!("ready flight result"),
        }
    }

    fn retain(
        value: T,
        from_flight: bool,
        retention: &Retention,
        weight: impl FnOnce(&T) -> usize,
    ) -> Self {
        let bytes = retention.bytes.get().saturating_add(weight(&value));
        if bytes > QUERY_BYTES {
            return Self::Failed(Arc::new(QueryReadLimit::Bytes.into()));
        }
        retention.bytes.set(bytes);
        Self::Ready { value, from_flight }
    }
}

impl QueryReadMemo {
    fn check_generation(&self, cache: &SharedDiskCache, path: &Path) -> Result<u64> {
        let generation = cache.generation();
        if self
            .generation
            .get()
            .is_some_and(|captured| captured != generation)
        {
            return Err(changed_source(path));
        }
        self.generation.set(Some(generation));
        Ok(generation)
    }

    fn product<K: Clone + Eq + Hash, T: Clone>(
        &self,
        entries: &RefCell<HashMap<K, ReadState<T>>>,
        key: &K,
        weight: impl FnOnce(&T) -> usize + Copy,
        start: impl FnOnce() -> Result<SourceRequest<T>>,
    ) -> Result<T> {
        let mut entries = entries.borrow_mut();
        if let Some(state) = entries.get_mut(key) {
            return state.poll(&self.retention, weight);
        }
        if self.retention.entries.get() == QUERY_ENTRIES {
            return Err(QueryReadLimit::Sources.into());
        }
        let state = match start()? {
            SourceRequest::Cached(value) => {
                ReadState::retain(value, false, &self.retention, weight)
            }
            SourceRequest::Join(flight) | SourceRequest::Produce(flight) => ReadState::InFlight {
                subscribed: flight.trace.map(|_| Instant::now()),
                flight,
            },
            SourceRequest::Busy(pending) => return Err(pending.into()),
        };
        self.retention.entries.set(self.retention.entries.get() + 1);
        entries
            .entry(key.clone())
            .or_insert(state)
            .poll(&self.retention, weight)
    }
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum ParsedArtifact {
    Full,
    Declarations,
}

impl ParsedArtifact {
    fn trace_artifact(self) -> Artifact {
        match self {
            Self::Full => Artifact::FullAst,
            Self::Declarations => Artifact::Declarations,
        }
    }
}

#[derive(Clone, Eq, Hash, PartialEq)]
pub(super) struct ValidationKey {
    path: PathBuf,
    observation: FileObservation,
    generation: u64,
    artifact: ParsedArtifact,
}

pub(super) struct CacheLock<T> {
    value: Mutex<T>,
    wake: Option<CacheWake>,
}

struct CacheWake {
    epoch: Arc<AtomicU64>,
    waiting: AtomicBool,
    sender: Sender<()>,
}

impl<T> CacheLock<T> {
    pub(super) fn new(value: T, wake: &Sender<()>) -> Self {
        Self {
            value: Mutex::new(value),
            wake: Some(CacheWake {
                epoch: Arc::default(),
                waiting: AtomicBool::default(),
                sender: wake.clone(),
            }),
        }
    }

    pub(super) fn registry(value: T) -> Self {
        Self {
            value: Mutex::new(value),
            wake: None,
        }
    }

    fn guard<'a>(&'a self, guard: MutexGuard<'a, T>, start: Option<Instant>) -> CacheGuard<'a, T> {
        CacheGuard {
            guard: Some(guard),
            cache: self,
            wait: start.map(|start| start.elapsed()),
        }
    }

    pub(super) fn lock(&self) -> LockResult<CacheGuard<'_, T>> {
        let start = telemetry::now();
        self.value
            .lock()
            .map(|guard| self.guard(guard, start))
            .map_err(|error| PoisonError::new(self.guard(error.into_inner(), start)))
    }

    pub(super) fn try_lock(&self) -> TryLockResult<CacheGuard<'_, T>> {
        self.value
            .try_lock()
            .map(|guard| self.guard(guard, None))
            .map_err(|error| match error {
                TryLockError::WouldBlock => {
                    if telemetry::enabled() {
                        telemetry::disk_mutex(None);
                    }
                    TryLockError::WouldBlock
                }
                TryLockError::Poisoned(error) => {
                    TryLockError::Poisoned(PoisonError::new(self.guard(error.into_inner(), None)))
                }
            })
    }

    fn foreground(&self) -> Result<CacheGuard<'_, T>, SharedLoadPending> {
        let wake = self
            .wake
            .as_ref()
            .expect("foreground cache lock has a wake");
        let observed = wake.epoch.load(Ordering::SeqCst);
        match self.try_lock() {
            Ok(guard) => Ok(guard),
            Err(TryLockError::Poisoned(error)) => Ok(error.into_inner()),
            Err(TryLockError::WouldBlock) => {
                wake.waiting.store(true, Ordering::SeqCst);
                let pending = SharedLoadPending::epoch(wake.epoch.clone(), observed);
                if pending.ready() {
                    let _ = wake.sender.try_send(());
                }
                Err(pending)
            }
        }
    }
}

pub(super) struct CacheGuard<'a, T> {
    guard: Option<MutexGuard<'a, T>>,
    cache: &'a CacheLock<T>,
    wait: Option<Duration>,
}

impl<T> Deref for CacheGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.guard.as_deref().unwrap()
    }
}

impl<T> DerefMut for CacheGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.guard.as_deref_mut().unwrap()
    }
}

impl<T> Drop for CacheGuard<'_, T> {
    fn drop(&mut self) {
        drop(self.guard.take());
        if let Some(wake) = &self.cache.wake {
            wake.epoch.fetch_add(1, Ordering::SeqCst);
            if wake.waiting.load(Ordering::SeqCst) && wake.waiting.swap(false, Ordering::SeqCst) {
                let _ = wake.sender.try_send(());
            }
        }
        if let Some(wait) = self.wait {
            telemetry::disk_mutex(Some(wait));
        }
    }
}

#[derive(Clone)]
struct BuiltinKey {
    root: PathBuf,
    environment: Arc<BTreeMap<OsString, OsString>>,
}

impl PartialEq for BuiltinKey {
    fn eq(&self, other: &Self) -> bool {
        self.root == other.root && Arc::ptr_eq(&self.environment, &other.environment)
    }
}

impl Eq for BuiltinKey {}

impl Hash for BuiltinKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.root.hash(state);
        Arc::as_ptr(&self.environment).hash(state);
    }
}

pub(super) struct BuiltinPaths {
    cached: Lru<BuiltinKey, Arc<PathBuf>>,
    loading: HashMap<BuiltinKey, Arc<Flight<Arc<PathBuf>>>>,
}

impl Default for BuiltinPaths {
    fn default() -> Self {
        Self {
            cached: Lru::new(16, 64 * 1024),
            loading: HashMap::new(),
        }
    }
}

impl SharedDiskCache {
    pub(in crate::semantic) fn peek_foreground_declarations(
        &self,
        key: &Path,
    ) -> Option<(NavigationText, Arc<SharedDeclarations>)> {
        let text = {
            let files = self.files.try_lock().ok()?;
            files
                .cached
                .peek(key)?
                .navigation_text(key, files.generation)
        };
        let declarations = self
            .parsed
            .try_lock()
            .ok()?
            .declarations
            .peek(&ContentKey::new(&text))?
            .declarations()
            .clone();
        trace::lookup(Artifact::Text, telemetry::DiskLookup::Hit, None);
        trace::lookup(Artifact::Declarations, telemetry::DiskLookup::Hit, None);
        Some((text, declarations))
    }

    pub(in crate::semantic) fn read_foreground(
        &self,
        path: &Path,
        memo: &QueryReadMemo,
        probe: Option<&NavigationProbe>,
    ) -> Result<NavigationText> {
        let verified = memo.verified.borrow().contains_key(&path_key(path));
        self.read_query_text(path, memo, probe, verified)
    }

    pub(in crate::semantic) fn read_foreground_verified(
        &self,
        path: &Path,
        memo: &QueryReadMemo,
        probe: Option<&NavigationProbe>,
    ) -> Result<NavigationText> {
        self.read_query_text(path, memo, probe, true)
    }

    fn read_query_text(
        &self,
        path: &Path,
        memo: &QueryReadMemo,
        probe: Option<&NavigationProbe>,
        verify: bool,
    ) -> Result<NavigationText> {
        let generation = memo.check_generation(self, path)?;
        let key = path_key(path);
        let entries = if verify { &memo.verified } else { &memo.text };
        let text = memo.product(
            entries,
            &key,
            |value| value.as_ref().map_or(0, |text| text.text.text.capacity()),
            || {
                let (flight, slot) = {
                    let mut files = self.files.foreground()?;
                    if files.generation != generation {
                        return Err(changed_source(path));
                    }
                    if !verify && let Some(text) = files.cached.get(&key) {
                        let request = SourceRequest::Cached(Some(text.clone()));
                        drop(files);
                        record(probe, |counts| counts.text_hits += 1);
                        request.trace(Artifact::Text);
                        return Ok(request);
                    }
                    if let Some((_, flight)) = files.loading.get(&key) {
                        let request = SourceRequest::Join(flight.clone());
                        drop(files);
                        request.trace(Artifact::Text);
                        return Ok(request);
                    }
                    let slot = self.queue_wake.try_reserve()?;
                    let flight = Arc::new(Flight::traced(&self.wake.0, Artifact::Text));
                    files.loading.insert(key.clone(), (None, flight.clone()));
                    (flight, slot)
                };
                let request = SourceRequest::Produce(flight.clone());
                request.trace(Artifact::Text);
                record(probe, |counts| counts.text_misses += 1);
                let abort = self.abort_read(&key, &flight);
                let cache = self.clone();
                let load_path = path.to_owned();
                let load_key = key.clone();
                let produce = flight.clone();
                slot.submit_traced(
                    flight.trace,
                    Box::new(move |_| {
                        let mut abort = abort;
                        let result = cache.observe_and_read(&load_path, &load_key, produce.trace);
                        cache.publish_read(&load_key, &produce, result);
                        abort.0.take();
                    }),
                )?;
                Ok(request)
            },
        )?;
        let text = text.ok_or_else(|| changed_source(path))?;
        memo.check_generation(self, path)?;
        Ok(text.navigation_text(&key, generation))
    }

    fn observe_and_read(
        &self,
        path: &Path,
        key: &Path,
        trace: Option<FlightTrace>,
    ) -> Result<Option<Arc<TextRevision>>> {
        for _ in 0..OBSERVATION_RETRIES {
            let observation =
                trace::measure(trace, "metadata", None, || FileObservation::read(path))
                    .with_context(|| format!("reading {}", path.display()))?;
            let cached = self
                .files
                .lock()
                .unwrap()
                .cached
                .get(key)
                .filter(|text| text.observation == observation)
                .cloned();
            if cached.is_some() {
                return Ok(cached);
            }
            if let Some(text) = self.read_observed_text(path, key, observation, trace)? {
                return Ok(Some(text));
            }
        }
        Err(changed_source(path))
    }

    pub(in crate::semantic) fn load_foreground(
        &self,
        path: &Path,
        memo: &QueryReadMemo,
        probe: Option<&NavigationProbe>,
    ) -> Result<PreparedReferenceSource> {
        let text = self.read_foreground(path, memo, probe)?;
        self.parse_foreground(path, text, memo, probe)
    }

    pub(in crate::semantic) fn parse_foreground(
        &self,
        path: &Path,
        text: NavigationText,
        memo: &QueryReadMemo,
        probe: Option<&NavigationProbe>,
    ) -> Result<PreparedReferenceSource> {
        let key = ContentKey::new(&text);
        let revision = memo.product(
            &memo.full,
            &key,
            |revision| {
                let source = revision.as_ref().clone().into_indexed();
                source
                    .tree
                    .root_node()
                    .descendant_count()
                    .saturating_mul(64)
            },
            || {
                let mut slot = None;
                let request = self
                    .parsed
                    .foreground()?
                    .request_full(&key, &self.wake.0, || self.reserve(&mut slot));
                request.trace(Artifact::FullAst);
                if let (Some(slot), SourceRequest::Produce(flight)) = (slot, &request) {
                    slot.submit_traced(
                        flight.trace,
                        Box::new(self.full_job(path.to_owned(), key.clone(), flight.trace, "full")),
                    )?;
                    record(probe, |counts| counts.ast_parses += 1);
                }
                Ok(request)
            },
        )?;
        if text.read_ticket.is_some()
            && matches!(
                memo.full.borrow().get(&key),
                Some(ReadState::Ready {
                    from_flight: true,
                    ..
                })
            )
        {
            self.validate_artifact(path, &text, memo, ParsedArtifact::Full)?;
        }
        Ok(PreparedReferenceSource::new(
            revision,
            path,
            text,
            key.fingerprint,
        ))
    }

    pub(in crate::semantic) fn load_declarations_foreground(
        &self,
        path: &Path,
        memo: &QueryReadMemo,
        probe: Option<&NavigationProbe>,
    ) -> Result<PreparedDeclarations> {
        self.project_foreground(path, memo, probe, DeclarationProducer::Stub)
    }

    pub(in crate::semantic) fn load_documentation_foreground(
        &self,
        path: &Path,
        memo: &QueryReadMemo,
        probe: Option<&NavigationProbe>,
    ) -> Result<PreparedDeclarations> {
        self.project_foreground(path, memo, probe, DeclarationProducer::Light)
    }

    fn project_foreground(
        &self,
        path: &Path,
        memo: &QueryReadMemo,
        probe: Option<&NavigationProbe>,
        producer: DeclarationProducer,
    ) -> Result<PreparedDeclarations> {
        let text = self.read_foreground(path, memo, probe)?;
        let key = ContentKey::new(&text);
        let result = self.declarations_foreground(path, &key, memo, producer, None);
        let entries = if producer == DeclarationProducer::Light {
            &memo.documentation
        } else {
            &memo.declarations
        };
        if text.read_ticket.is_some()
            && (matches!(
                entries.borrow().get(&key),
                Some(ReadState::Ready {
                    from_flight: true,
                    ..
                })
            ) || result
                .as_ref()
                .is_err_and(|error| error.is::<NeedsFullDeclarations>()))
        {
            self.validate_artifact(path, &text, memo, ParsedArtifact::Declarations)?;
        }
        let declarations = result?;
        Ok(PreparedDeclarations {
            declarations,
            text,
            fingerprint: key.fingerprint,
            path: path.to_owned(),
        })
    }

    pub(in crate::semantic) fn source_declarations_foreground(
        &self,
        source: &IndexedSource,
        memo: &QueryReadMemo,
    ) -> Result<Arc<SharedDeclarations>> {
        if let Some(declarations) = source.cached_overlay_declarations() {
            return Ok(declarations.clone());
        }
        // Open documents already have a unique immutable syntax revision. Key
        // their exact overlays without hashing the document on the main loop.
        let overlay_syntax = source.document_version.map(|_| source.syntax_revision);
        let key = ContentKey {
            fingerprint: overlay_syntax.map_or_else(|| source.fingerprint(), |_| 0),
            text: source.text.clone(),
            overlay_syntax,
        };
        let declarations = self.declarations_foreground(
            &source.path,
            &key,
            memo,
            DeclarationProducer::Stub,
            Some(source),
        )?;
        source.remember_file_index(declarations.index.clone());
        Ok(declarations)
    }

    fn declarations_foreground(
        &self,
        path: &Path,
        key: &ContentKey,
        memo: &QueryReadMemo,
        producer: DeclarationProducer,
        overlay: Option<&IndexedSource>,
    ) -> Result<Arc<SharedDeclarations>> {
        let entries = if producer == DeclarationProducer::Light {
            &memo.documentation
        } else {
            &memo.declarations
        };
        let result = memo.product(
            entries,
            key,
            |declarations| declarations.cache_weight(),
            || {
                let mut slot = None;
                let request = self.parsed.foreground()?.request_declarations(
                    key,
                    &self.wake.0,
                    producer,
                    || self.reserve(&mut slot),
                );
                request.trace(Artifact::Declarations);
                if let (Some(slot), SourceRequest::Produce(flight)) = (slot, &request) {
                    let trace = flight.trace;
                    let job: LoadJob = if let Some(overlay) = overlay {
                        let abort = self.abort_parse(key, producer);
                        let revision = overlay.revision();
                        let key = key.clone();
                        let cache = self.clone();
                        Box::new(move |_| {
                            let mut abort = abort;
                            let source = revision.into_indexed();
                            #[cfg(test)]
                            cache.test_event(&path_key(&source.path), CacheEvent::DeclarationBuild);
                            let result = trace::measure(trace, "index", None, || {
                                source.shared_declarations(&SHARED_LOAD)
                            });
                            cache.publish_declarations(&key, producer, result);
                            abort.0.take();
                        })
                    } else {
                        Box::new(self.declaration_job(
                            path.to_owned(),
                            key.clone(),
                            producer,
                            trace,
                        ))
                    };
                    slot.submit_traced(trace, job)?;
                }
                Ok(request)
            },
        );
        // Navigation may have joined a docs-only flight that found malformed syntax.
        // Its recovery follows the same full flight as an explicit AST request.
        if producer == DeclarationProducer::Stub
            && result
                .as_ref()
                .is_err_and(|error| error.is::<NeedsFullDeclarations>())
        {
            if let Some(source) = overlay {
                return self.recover_overlay_declarations(source, memo);
            }
            let text = self.read_foreground(path, memo, None)?;
            let full = self.parse_foreground(path, text, memo, None)?;
            return self.recover_overlay_declarations(&full.into_indexed(), memo);
        }
        result
    }

    fn recover_overlay_declarations(
        &self,
        source: &IndexedSource,
        memo: &QueryReadMemo,
    ) -> Result<Arc<SharedDeclarations>> {
        let key = ContentKey {
            fingerprint: source.fingerprint(),
            text: source.text.clone(),
            overlay_syntax: None,
        };
        memo.product(
            &memo.recovered,
            &key,
            |declarations| declarations.cache_weight(),
            || {
                let slot = self.queue_wake.try_reserve()?;
                let flight = Arc::new(Flight::traced(&self.wake.0, Artifact::Declarations));
                let request = SourceRequest::Produce(flight.clone());
                request.trace(Artifact::Declarations);
                let produce = flight.clone();
                let abort = AbortOnDrop(Some({
                    let flight = flight.clone();
                    move || flight.finish(Err(anyhow::anyhow!("shared Go declarations aborted")))
                }));
                let revision = source.revision();
                let cache = self.clone();
                slot.submit_traced(
                    flight.trace,
                    Box::new(move |_| {
                        let mut abort = abort;
                        let source = revision.into_indexed();
                        let declarations = trace::measure(produce.trace, "index", None, || {
                            cache.collect_declarations(&source.path, &source)
                        });
                        produce.finish(declarations);
                        abort.0.take();
                    }),
                )?;
                Ok(request)
            },
        )
    }

    fn validate_artifact(
        &self,
        path: &Path,
        text: &NavigationText,
        memo: &QueryReadMemo,
        artifact: ParsedArtifact,
    ) -> Result<()> {
        memo.check_generation(self, path)?;
        let trace_artifact = artifact.trace_artifact();
        let ticket = text.read_ticket.as_ref().expect("disk artifact ticket");
        let key = ValidationKey {
            path: ticket.key.clone(),
            generation: ticket.generation,
            observation: ticket.observation,
            artifact,
        };
        memo.product(
            &memo.validated,
            &key,
            |_| 0,
            || {
                let (flight, slot) = {
                    let mut files = self.files.foreground()?;
                    if let Some(flight) = files.checking.get(&key) {
                        let request = SourceRequest::Join(flight.clone());
                        drop(files);
                        request.trace(trace_artifact);
                        return Ok(request);
                    }
                    let slot = self.queue_wake.try_reserve()?;
                    let flight = Arc::new(Flight::validation(&self.wake.0, trace_artifact));
                    files.checking.insert(key.clone(), flight.clone());
                    (flight, slot)
                };
                let request = SourceRequest::Produce(flight.clone());
                request.trace(trace_artifact);
                let path = path.to_owned();
                let key = key.clone();
                let text = TextRevision {
                    observation: ticket.observation,
                    text: text.clone(),
                    fingerprint: ticket.fingerprint,
                };
                let producing = flight.clone();
                let abort = AbortOnDrop(Some({
                    let files = self.files.clone();
                    let key = key.clone();
                    let flight = flight.clone();
                    move || {
                        files
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .checking
                            .remove(&key);
                        flight.finish(Err(anyhow::anyhow!("Go source verification aborted")));
                    }
                }));
                let cache = self.clone();
                slot.submit_traced(
                    flight.trace,
                    Box::new(move |_| {
                        let mut abort = abort;
                        let matches = trace::measure(producing.trace, "metadata", None, || {
                            FileObservation::read(&path)
                        })
                        .ok()
                            == Some(key.observation);
                        let result = if matches && cache.generation() == key.generation {
                            cache.remember_package_names(&key.path, &text, key.generation);
                            Ok(())
                        } else {
                            if !matches && cache.generation() == key.generation {
                                cache.invalidate_key(&key.path, false, Removal::Validation);
                            }
                            Err(changed_source(&path))
                        };
                        let mut files = cache.files.lock().unwrap();
                        files.checking.remove(&key);
                        producing.finish(result);
                        abort.0.take();
                    }),
                )?;
                Ok(request)
            },
        )?;
        memo.check_generation(self, path)?;
        Ok(())
    }

    pub(in crate::semantic) fn builtin_path_cancellable(
        &self,
        root: &Path,
        environment: &GoEnvironment,
        cancelled: &AtomicBool,
    ) -> Result<PathBuf> {
        let memo = QueryReadMemo::default();
        loop {
            check_cancelled(cancelled)?;
            match self.builtin_path_foreground(root, environment, &memo) {
                Err(error) if error.is::<SharedLoadPending>() => {
                    error
                        .downcast::<SharedLoadPending>()?
                        .wait_for_slot(Some(cancelled))?;
                }
                result => return result,
            }
        }
    }

    pub(in crate::semantic) fn builtin_path_foreground(
        &self,
        root: &Path,
        environment: &GoEnvironment,
        memo: &QueryReadMemo,
    ) -> Result<PathBuf> {
        let key = BuiltinKey {
            root: root.to_owned(),
            environment: environment.variables().clone(),
        };
        let path = memo.product(
            &memo.builtin,
            &key,
            |path| path.as_os_str().len(),
            || {
                let (flight, slot) = {
                    let mut builtin = self.builtin_paths.foreground()?;
                    if let Some(path) = builtin.cached.get(&key) {
                        return Ok(SourceRequest::Cached(path.clone()));
                    }
                    if let Some(flight) = builtin.loading.get(&key) {
                        return Ok(SourceRequest::Join(flight.clone()));
                    }
                    let slot = self.queue_wake.try_reserve()?;
                    let flight = Arc::new(Flight::new(Some(self.wake.0.clone())));
                    builtin.loading.insert(key.clone(), flight.clone());
                    (flight, slot)
                };
                let paths = self.builtin_paths.clone();
                let environment = environment.clone();
                let key = key.clone();
                let producing = flight.clone();
                let abort = AbortOnDrop(Some({
                    let paths = paths.clone();
                    let key = key.clone();
                    let flight = flight.clone();
                    move || {
                        paths.lock().unwrap().loading.remove(&key);
                        flight.finish(Err(anyhow::anyhow!("Go builtin path load aborted")));
                    }
                }));
                slot.submit(Box::new(move |_| {
                    let mut abort = abort;
                    let result = find_builtin_source(&key.root, &environment).map(Arc::new);
                    let mut builtin = paths.lock().unwrap();
                    let retired = if let Ok(path) = &result {
                        let weight = key
                            .root
                            .as_os_str()
                            .len()
                            .saturating_add(path.as_os_str().len());
                        Some(builtin.cached.insert(key.clone(), path.clone(), weight))
                    } else {
                        None
                    };
                    builtin.loading.remove(&key);
                    producing.finish(result);
                    abort.0.take();
                    drop(builtin);
                    if let Some(retired) = retired {
                        retired.report("builtin_paths");
                    }
                }))?;
                Ok(SourceRequest::Produce(flight))
            },
        )?;
        Ok(path.as_ref().clone())
    }

    fn reserve(&self, slot: &mut Option<LoadSlot>) -> Result<(), SharedLoadPending> {
        *slot = Some(self.queue_wake.try_reserve()?);
        Ok(())
    }
}
