use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use lsp_types::Position;
use serde::Serialize;

use crate::{telemetry, workspace::path_key};

use super::sdk::{InventoryStats, unix_ms};

const MAX_SLOW_INDEXES: usize = 32;

#[derive(Clone)]
pub(crate) struct Trace {
    profile: Option<Arc<Mutex<Profile>>>,
    #[cfg(test)]
    hook: Option<(Arc<PathBuf>, CompletionHook)>,
}

#[cfg(test)]
pub(crate) type CompletionHook = Arc<dyn Fn(&Path, &'static str, &AtomicBool) + Send + Sync>;

struct Profile {
    context: Option<telemetry::Context>,
    started: Instant,
    stats: Stats,
    reads: HashMap<PathBuf, usize>,
}

#[derive(Serialize)]
struct Stats {
    document: String,
    position: Position,
    started_unix_ms: u64,
    total_ms: f64,
    phases_ms: BTreeMap<&'static str, f64>,
    sdk_state: &'static str,
    sdk_at_import_search: &'static str,
    sdk: Option<InventoryStats>,
    import_search_unix_ms: Option<u64>,
    prefilter_reads: usize,
    project_prefilter_reads: usize,
    disk_reads: usize,
    second_reads: usize,
    third_or_later_reads: usize,
    indexed_source_reads: usize,
    text_handoffs: usize,
    full_parses: usize,
    declaration_parses: usize,
    type_location_hits: usize,
    expected_weight_hits: usize,
    peak_sources: usize,
    peak_source_bytes: usize,
    request_cache_released: bool,
    outcome: &'static str,
    slow_indexes: Vec<IndexSample>,
}

#[derive(Serialize)]
struct IndexSample {
    file: String,
    bytes: usize,
    declaration_parse: bool,
    elapsed_ms: f64,
}

pub(in crate::semantic) struct Phase(Trace, &'static str, Instant);

impl Trace {
    pub(crate) fn start(path: &Path, position: Position) -> Option<Self> {
        Some(Self::new(path, position, telemetry::now()?))
    }

    fn new(path: &Path, position: Position, started: Instant) -> Self {
        Self {
            profile: Some(Arc::new(Mutex::new(Profile {
                context: telemetry::context(),
                started,
                stats: Stats {
                    document: telemetry::path_key(&path_key(path)),
                    position,
                    started_unix_ms: unix_ms(),
                    total_ms: 0.0,
                    phases_ms: BTreeMap::new(),
                    sdk_state: "not used",
                    sdk_at_import_search: "not used",
                    sdk: None,
                    import_search_unix_ms: None,
                    prefilter_reads: 0,
                    project_prefilter_reads: 0,
                    disk_reads: 0,
                    second_reads: 0,
                    third_or_later_reads: 0,
                    indexed_source_reads: 0,
                    text_handoffs: 0,
                    full_parses: 0,
                    declaration_parses: 0,
                    type_location_hits: 0,
                    expected_weight_hits: 0,
                    peak_sources: 0,
                    peak_source_bytes: 0,
                    request_cache_released: false,
                    outcome: "discarded",
                    slow_indexes: Vec::new(),
                },
                reads: HashMap::new(),
            }))),
            #[cfg(test)]
            hook: None,
        }
    }

    #[cfg(test)]
    pub(super) fn with_hook(trace: Option<Self>, path: PathBuf, hook: CompletionHook) -> Self {
        Self {
            profile: trace.and_then(|trace| trace.profile),
            hook: Some((Arc::new(path), hook)),
        }
    }

    #[cfg(test)]
    pub(super) fn checkpoint(&self, phase: &'static str, cancelled: Option<&AtomicBool>) {
        if let (Some((path, hook)), Some(cancelled)) = (&self.hook, cancelled) {
            hook(path, phase, cancelled);
        }
    }

    fn update(&self, update: impl FnOnce(&mut Profile)) {
        if let Some(profile) = &self.profile {
            update(&mut profile.lock().unwrap());
        }
    }

    pub(in crate::semantic) fn phase(&self, name: &'static str) -> Phase {
        Phase(self.clone(), name, Instant::now())
    }

    pub(crate) fn phases(&self) -> BTreeMap<&'static str, f64> {
        self.profile
            .as_ref()
            .map(|profile| profile.lock().unwrap().stats.phases_ms.clone())
            .unwrap_or_default()
    }

    pub(super) fn sdk(&self, state: &'static str, stats: Option<InventoryStats>) {
        self.update(|profile| {
            if profile.stats.import_search_unix_ms.is_none() {
                profile.stats.import_search_unix_ms = Some(unix_ms());
                profile.stats.sdk_at_import_search = state;
            }
            profile.stats.sdk_state = state;
            profile.stats.sdk = stats;
        });
    }

    pub(in crate::semantic) fn read(
        &self,
        key: &Path,
        prefilter: bool,
        project: bool,
        indexed: bool,
    ) {
        self.update(|profile| {
            let reads = profile.reads.entry(key.to_owned()).or_default();
            *reads += 1;
            let reads = *reads;
            let stats = &mut profile.stats;
            stats.disk_reads += 1;
            stats.prefilter_reads += usize::from(prefilter);
            stats.project_prefilter_reads += usize::from(prefilter && project);
            stats.indexed_source_reads += usize::from(indexed);
            stats.second_reads += usize::from(reads == 2);
            stats.third_or_later_reads += usize::from(reads >= 3);
        });
    }

    pub(in crate::semantic) fn parse(&self) {
        self.update(|profile| profile.stats.full_parses += 1);
    }

    pub(in crate::semantic) fn declaration_parse(&self) {
        self.update(|profile| profile.stats.declaration_parses += 1);
    }

    pub(in crate::semantic) fn index_time(
        &self,
        file: &Path,
        bytes: usize,
        declaration_parse: bool,
        started: Instant,
    ) {
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        if elapsed_ms >= 2.0 {
            self.update(|profile| {
                if profile.stats.slow_indexes.len() < MAX_SLOW_INDEXES {
                    profile.stats.slow_indexes.push(IndexSample {
                        file: telemetry::path_key(&path_key(file)),
                        bytes,
                        declaration_parse,
                        elapsed_ms,
                    });
                }
            });
        }
    }

    pub(in crate::semantic) fn text_handoff(&self) {
        self.update(|profile| profile.stats.text_handoffs += 1);
    }

    pub(in crate::semantic) fn type_location_hit(&self) {
        self.update(|profile| profile.stats.type_location_hits += 1);
    }

    pub(super) fn expected_weight_hit(&self) {
        self.update(|profile| profile.stats.expected_weight_hits += 1);
    }

    pub(in crate::semantic) fn cache_size(&self, count: usize, bytes: usize) {
        self.update(|profile| {
            profile.stats.peak_sources = profile.stats.peak_sources.max(count);
            profile.stats.peak_source_bytes = profile.stats.peak_source_bytes.max(bytes);
        });
    }

    pub(in crate::semantic) fn cache_released(&self) {
        self.update(|profile| profile.stats.request_cache_released = true);
    }

    pub(crate) fn outcome(&self, outcome: &'static str) {
        self.update(|profile| profile.stats.outcome = outcome);
    }

    pub(crate) fn error(&self, code: lsp_server::ErrorCode) {
        use lsp_server::ErrorCode;
        self.outcome(match code {
            ErrorCode::RequestCanceled => "cancelled",
            ErrorCode::ContentModified => "stale",
            _ => "failed",
        });
    }
}

impl Drop for Phase {
    fn drop(&mut self) {
        self.0.update(|profile| {
            *profile.stats.phases_ms.entry(self.1).or_default() +=
                self.2.elapsed().as_secs_f64() * 1000.0;
        });
    }
}

impl Drop for Profile {
    fn drop(&mut self) {
        if telemetry::enabled() {
            self.stats.total_ms = telemetry::elapsed(self.started);
            telemetry::with_context(self.context, || {
                telemetry::emit("completion_profile", serde_json::json!(&self.stats));
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn profile_accumulates_phases_and_serializes_hashed_paths() {
        let document = Path::new("private-workspace").join("secret-request.go");
        let dependency = Path::new("private-sdk").join("secret-dependency.go");
        let trace = Trace::new(&document, Position::new(3, 4), Instant::now());
        for _ in 0..2 {
            let started = Instant::now() - Duration::from_millis(2);
            drop(Phase(trace.clone(), "expected_type", started));
            trace.read(&dependency, true, false, false);
        }
        trace.index_time(
            &dependency,
            100,
            true,
            Instant::now() - Duration::from_millis(5),
        );
        trace.sdk(
            "ready",
            Some(InventoryStats {
                started_unix_ms: 0,
                ready_unix_ms: 1,
                context_ms: 0.2,
                discovery_ms: 0.8,
                method: "go list std",
                reused: false,
                fallback: Some("native_failed"),
                prewarmed_names: 0,
            }),
        );
        assert!(trace.phases()["expected_type"] >= 4.0);
        trace.update(|profile| {
            let value = serde_json::to_value(&profile.stats).unwrap();
            assert_eq!(value["document"], telemetry::path_key(&path_key(&document)));
            assert_eq!(
                value["slow_indexes"][0]["file"],
                telemetry::path_key(&path_key(&dependency))
            );
            assert_eq!(value["sdk"]["fallback"], "native_failed");
            assert_eq!(value["disk_reads"], 2);
            assert_eq!(value["second_reads"], 1);
            let row = value.to_string();
            for secret in [
                "private-workspace",
                "secret-request",
                "private-sdk",
                "secret-dependency",
            ] {
                assert!(!row.contains(secret));
            }
        });
    }
}
