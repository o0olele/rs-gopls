use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

use lsp_server::Request;
use serde_json::{Value, json};

use crate::telemetry;

thread_local! {
    static CURRENT: RefCell<Option<(NavigationKind, Rc<NavigationProbe>)>> = const { RefCell::new(None) };
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum NavigationKind {
    Definition,
    References,
}

pub(crate) struct NavigationTrace {
    kind: NavigationKind,
    id: Value,
    document: Option<String>,
    line: Option<u64>,
    character: Option<u64>,
    received: Instant,
    probe: Rc<NavigationProbe>,
}

pub(crate) struct NavigationStep(Option<(NavigationKind, Rc<NavigationProbe>)>);

impl Drop for NavigationStep {
    fn drop(&mut self) {
        CURRENT.with(|current| current.replace(self.0.take()));
    }
}

impl NavigationTrace {
    pub(crate) fn new(
        request: &Request,
        received: Instant,
        document: Option<String>,
    ) -> Option<Self> {
        let kind = match request.method.as_str() {
            "textDocument/definition" | "textDocument/typeDefinition" => NavigationKind::Definition,
            "textDocument/references" => NavigationKind::References,
            _ => return None,
        };
        let position = &request.params["position"];
        Some(Self {
            kind,
            id: crate::rpc_telemetry::request_id(&request.id),
            document,
            line: position["line"].as_u64(),
            character: position["character"].as_u64(),
            received,
            probe: Rc::default(),
        })
    }

    pub(crate) fn step(&self) -> NavigationStep {
        NavigationStep(
            CURRENT.with(|current| current.replace(Some((self.kind, Rc::clone(&self.probe))))),
        )
    }

    pub(crate) fn finish(&self, outcome: &str, result_count: usize) {
        let counts = self.probe.0.borrow();
        let cancelled = matches!(outcome, "cancelled_-32800" | "content_modified_-32801");
        let (event, mut fields) = match self.kind {
            NavigationKind::Definition => {
                let route = match (result_count, counts.definition_route) {
                    (0, route) if route != "waiting_workspace" => "empty",
                    (_, route) => route,
                };
                (
                    "definition_trace",
                    json!({
                        "hit_path": route,
                        "scanned_files": counts.scanned_files,
                        "disk_reads": counts.disk_reads,
                        "ast_parses": counts.ast_parses,
                        "file_index_misses": counts.declaration_misses,
                        "found_count": result_count,
                    }),
                )
            }
            NavigationKind::References => (
                "references_trace",
                json!({
                    "target_type": counts.reference_target,
                    "candidate_packages": counts.candidate_packages,
                    "candidate_files": counts.candidate_files,
                    "processed_files": counts.processed_files,
                    "batches": counts.batches,
                    "wall_ms": telemetry::elapsed(self.received),
                    "disk_reads": counts.disk_reads,
                    "text_cache_hits": counts.text_hits,
                    "text_cache_misses": counts.text_misses,
                    "ast_cache_hits": counts.ast_hits,
                    "ast_cache_misses": counts.ast_misses,
                    "declaration_cache_hits": counts.declaration_hits,
                    "declaration_cache_misses": counts.declaration_misses,
                    "offset_tables_built": counts.offset_tables,
                    "ast_parses": counts.ast_parses,
                    "candidate_ast_parses": counts.reference_candidate_parses,
                    "dependency_ast_parses": counts.reference_dependency_parses,
                    "anchor_ast_parses": counts.reference_anchor_parses,
                    "prefilter_skipped": counts.prefilter_skipped,
                    "prefilter_reads": counts.prefilter_reads,
                    "prefilter_bytes": counts.prefilter_bytes,
                    "cached_offset_files": counts.cached_offset_files,
                    "candidate_scope_cache_hit": counts.reference_scope_cache_hit,
                    "max_step_ms": millis(counts.reference_max_step),
                    "ident_offsets_checked": counts.ident_offsets_checked,
                    "read_ms": millis(counts.read),
                    "parse_ms": millis(counts.parse),
                    "verify_ms": millis(counts.verify),
                    "matches_count": counts.matches_count.max(result_count),
                    "slowest_file": counts.slowest_file,
                    "slowest_file_ms": millis(counts.slowest),
                }),
            ),
        };
        fields["req_id"] = self.id.clone();
        fields["document"] = json!(self.document);
        fields["line"] = json!(self.line);
        fields["char"] = json!(self.character);
        fields["exec_ms"] = json!(millis(counts.exec));
        fields["cancelled"] = json!(cancelled);
        fields["waited_workspace"] = json!(counts.waited_workspace);
        telemetry::emit(event, fields);
    }
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

#[derive(Default)]
pub(super) struct NavigationProbe(RefCell<NavigationCounts>);

impl NavigationProbe {
    pub(super) fn parse_count(&self) -> usize {
        self.0.borrow().ast_parses
    }

    pub(super) fn into_counts(self) -> NavigationCounts {
        self.0.into_inner()
    }
}

#[derive(Clone)]
pub(super) struct NavigationCounts {
    pub(super) definition_route: &'static str,
    pub(super) reference_target: &'static str,
    pub(super) waited_workspace: bool,
    pub(super) scanned_files: usize,
    pub(super) candidate_packages: usize,
    pub(super) candidate_files: usize,
    pub(super) processed_files: usize,
    pub(super) batches: usize,
    pub(super) disk_reads: usize,
    pub(super) ast_parses: usize,
    pub(super) text_hits: usize,
    pub(super) text_misses: usize,
    pub(super) ast_hits: usize,
    pub(super) ast_misses: usize,
    pub(super) declaration_hits: usize,
    pub(super) declaration_misses: usize,
    pub(super) offset_tables: usize,
    pub(super) reference_candidate_parses: usize,
    pub(super) reference_dependency_parses: usize,
    pub(super) reference_anchor_parses: usize,
    pub(super) prefilter_reads: usize,
    pub(super) prefilter_skipped: usize,
    pub(super) prefilter_bytes: usize,
    pub(super) cached_offset_files: usize,
    pub(super) reference_scope_cache_hit: bool,
    pub(super) reference_max_step: Duration,
    pub(super) ident_offsets_checked: usize,
    pub(super) matches_count: usize,
    pub(super) exec: Duration,
    pub(super) read: Duration,
    pub(super) parse: Duration,
    verify: Duration,
    slowest_file: Option<String>,
    slowest: Duration,
}

impl Default for NavigationCounts {
    fn default() -> Self {
        Self {
            definition_route: "empty",
            reference_target: "empty",
            waited_workspace: false,
            scanned_files: 0,
            candidate_packages: 0,
            candidate_files: 0,
            processed_files: 0,
            batches: 0,
            disk_reads: 0,
            ast_parses: 0,
            text_hits: 0,
            text_misses: 0,
            ast_hits: 0,
            ast_misses: 0,
            declaration_hits: 0,
            declaration_misses: 0,
            offset_tables: 0,
            reference_candidate_parses: 0,
            reference_dependency_parses: 0,
            reference_anchor_parses: 0,
            prefilter_reads: 0,
            prefilter_skipped: 0,
            prefilter_bytes: 0,
            cached_offset_files: 0,
            reference_scope_cache_hit: false,
            reference_max_step: Duration::ZERO,
            ident_offsets_checked: 0,
            matches_count: 0,
            exec: Duration::ZERO,
            read: Duration::ZERO,
            parse: Duration::ZERO,
            verify: Duration::ZERO,
            slowest_file: None,
            slowest: Duration::ZERO,
        }
    }
}

impl NavigationCounts {
    pub(super) fn file_elapsed(&mut self, path: &Path, elapsed: Duration) {
        if elapsed >= self.slowest {
            self.slowest = elapsed;
            self.slowest_file = telemetry::enabled().then(|| telemetry::path(path));
        }
    }

    pub(super) fn add_reference_verification(&mut self, work: Self) {
        self.read += work.read;
        self.parse += work.parse;
        self.verify += work.verify;
        self.disk_reads += work.disk_reads;
        self.text_hits += work.text_hits;
        self.text_misses += work.text_misses;
        self.ast_hits += work.ast_hits;
        self.ast_misses += work.ast_misses;
        self.ast_parses += work.ast_parses;
        self.reference_dependency_parses += work.reference_dependency_parses;
        self.reference_candidate_parses += work.reference_candidate_parses;
        self.reference_anchor_parses += work.reference_anchor_parses;
        self.prefilter_reads += work.prefilter_reads;
        self.declaration_hits += work.declaration_hits;
        self.declaration_misses += work.declaration_misses;
        self.ident_offsets_checked += work.ident_offsets_checked;
        self.prefilter_bytes += work.prefilter_bytes;
        self.prefilter_skipped += work.prefilter_skipped;
        self.cached_offset_files += work.cached_offset_files;
        if work.slowest_file.is_some() && work.slowest >= self.slowest {
            self.slowest = work.slowest;
            self.slowest_file = work.slowest_file;
        }
    }

    pub(super) fn workspace_wait(&mut self) {
        self.waited_workspace = true;
        self.definition_route = "waiting_workspace";
    }

    pub(super) fn ast_access(&mut self, hit: bool) {
        if hit {
            self.ast_hits += 1;
            self.text_hits += 1;
        } else {
            self.ast_misses += 1;
        }
    }

    pub(super) fn declaration_access(&mut self, hit: bool) {
        self.scanned_files += 1;
        if hit {
            self.declaration_hits += 1;
        } else {
            self.declaration_misses += 1;
        }
    }
}

pub(super) fn active(kind: NavigationKind) -> Option<Rc<NavigationProbe>> {
    if !telemetry::enabled() {
        return None;
    }
    CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .filter(|(active, _)| *active == kind)
            .map(|(_, probe)| Rc::clone(probe))
    })
}

pub(super) fn record(probe: Option<&NavigationProbe>, update: impl FnOnce(&mut NavigationCounts)) {
    if let Some(probe) = probe {
        update(&mut probe.0.borrow_mut());
    }
}

pub(super) struct Sample<'a> {
    probe: &'a NavigationProbe,
    started: Instant,
    field: fn(&mut NavigationCounts) -> &mut Duration,
}

impl Drop for Sample<'_> {
    fn drop(&mut self) {
        *(self.field)(&mut self.probe.0.borrow_mut()) += self.started.elapsed();
    }
}

pub(super) fn sample(
    probe: Option<&NavigationProbe>,
    field: fn(&mut NavigationCounts) -> &mut Duration,
) -> Option<Sample<'_>> {
    probe.map(|probe| Sample {
        probe,
        started: Instant::now(),
        field,
    })
}

pub(super) struct Verification<'a> {
    probe: &'a NavigationProbe,
    started: Instant,
    nested: Duration,
}

impl Drop for Verification<'_> {
    fn drop(&mut self) {
        let mut counts = self.probe.0.borrow_mut();
        let nested = counts.read + counts.parse - self.nested;
        counts.verify += self.started.elapsed().saturating_sub(nested);
    }
}

pub(super) fn verification(probe: Option<&NavigationProbe>) -> Option<Verification<'_>> {
    probe.map(|probe| {
        let counts = probe.0.borrow();
        Verification {
            probe,
            started: Instant::now(),
            nested: counts.read + counts.parse,
        }
    })
}

pub(super) struct FileSample<'a> {
    probe: &'a NavigationProbe,
    path: &'a Path,
    started: Instant,
}

impl Drop for FileSample<'_> {
    fn drop(&mut self) {
        self.probe
            .0
            .borrow_mut()
            .file_elapsed(self.path, self.started.elapsed());
    }
}

pub(super) fn file_sample<'a>(
    probe: Option<&'a NavigationProbe>,
    path: &'a Path,
) -> Option<FileSample<'a>> {
    probe.map(|probe| {
        probe.0.borrow_mut().processed_files += 1;
        FileSample {
            probe,
            path,
            started: Instant::now(),
        }
    })
}
