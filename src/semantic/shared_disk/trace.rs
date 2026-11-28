use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crate::telemetry;

static NEXT_FLIGHT: AtomicU64 = AtomicU64::new(1);

pub(super) use telemetry::DiskArtifact as Artifact;

#[derive(Clone, Copy)]
pub(super) struct FlightTrace {
    id: u64,
    artifact: Artifact,
    operation: Operation,
}

#[derive(Clone, Copy)]
pub(super) enum Operation {
    Source,
    Validation,
}

impl Operation {
    const fn name(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Validation => "validation",
        }
    }
}

impl FlightTrace {
    pub(super) fn new(artifact: Artifact, operation: Operation) -> Option<Self> {
        telemetry::enabled().then(|| Self {
            id: NEXT_FLIGHT.fetch_add(1, Ordering::Relaxed),
            artifact,
            operation,
        })
    }

    pub(super) fn phase(
        self,
        phase: &'static str,
        duration_ms: f64,
        parse_mode: Option<&'static str>,
    ) {
        telemetry::event!("shared_disk_flight", {
            "flight_id": self.id, "artifact": self.artifact.name(), "phase": phase,
            "operation": self.operation.name(),
            "duration_ms": duration_ms, "parse_mode": parse_mode,
        });
    }

    pub(super) fn handoff(self, producer: Self) {
        telemetry::event!("shared_disk_flight", {
            "flight_id": self.id, "artifact": self.artifact.name(), "phase": "handoff",
            "operation": self.operation.name(),
            "producer_flight_id": producer.id,
        });
    }

    pub(super) fn wait(self, start: Instant, outcome: &'static str) {
        telemetry::event!("shared_disk_wait", {
            "flight_id": self.id, "artifact": self.artifact.name(),
            "operation": self.operation.name(),
            "wait_ms": telemetry::elapsed(start), "outcome": outcome,
        });
    }

    pub(super) fn report_stats(self, stats: telemetry::SharedDiskStats) {
        let mut fields = serde_json::json!({
            "flight_id": self.id, "artifact": self.artifact.name(),
            "operation": self.operation.name(), "phase": "cache_stats",
        });
        stats.add_fields(&mut fields);
        if fields.get("shared_disk").is_some() {
            telemetry::emit("shared_disk_flight", fields);
        }
    }
}

pub(super) fn measure<T>(
    trace: Option<FlightTrace>,
    phase: &'static str,
    parse_mode: Option<&'static str>,
    run: impl FnOnce() -> T,
) -> T {
    let start = trace.map(|_| Instant::now());
    let result = run();
    if let Some((trace, start)) = trace.zip(start) {
        trace.phase(phase, telemetry::elapsed(start), parse_mode);
    }
    result
}

pub(super) fn lookup(
    artifact: Artifact,
    outcome: telemetry::DiskLookup,
    trace: Option<FlightTrace>,
) {
    telemetry::disk_lookup(artifact, outcome);
    if let Some(trace) = trace {
        telemetry::event!("shared_disk_lookup", {
            "artifact": artifact.name(),
            "outcome": match outcome {
                telemetry::DiskLookup::Hit => "cache_hit",
                telemetry::DiskLookup::Produce => "producer_requested",
                telemetry::DiskLookup::Join => "join",
            },
            "flight_id": trace.id,
            "operation": trace.operation.name(),
        });
    }
}

#[derive(Clone, Copy)]
#[repr(usize)]
pub(super) enum Removal {
    Entries,
    Bytes,
    Both,
    OversizedReject,
    Replacement,
    Watcher,
    Modified,
    Closed,
    Missing,
    Validation,
    Recovery,
}

impl Removal {
    const fn name(self) -> &'static str {
        match self {
            Self::Entries => "entries",
            Self::Bytes => "bytes",
            Self::Both => "both",
            Self::OversizedReject => "oversized_reject",
            Self::Replacement => "replacement",
            Self::Watcher => "watcher",
            Self::Modified => "modified",
            Self::Closed => "closed",
            Self::Missing => "missing",
            Self::Validation => "validation",
            Self::Recovery => "recovery",
        }
    }

    pub(super) fn report(self, cache: &'static str, count: usize) {
        if count > 0 {
            telemetry::event!("shared_disk_lru", {
                "cache": cache, "reason": self.name(), "count": count,
            });
        }
    }
}

pub(super) struct Retired<V> {
    pub(super) values: Vec<V>,
    pub(super) counts: [usize; 5],
}

impl<V> Retired<V> {
    pub(super) fn empty() -> Self {
        Self {
            values: Vec::new(),
            counts: [0; 5],
        }
    }

    pub(super) fn report(self, cache: &'static str) {
        if telemetry::enabled() {
            let reasons = [
                Removal::Entries,
                Removal::Bytes,
                Removal::Both,
                Removal::OversizedReject,
                Removal::Replacement,
            ];
            for (reason, count) in reasons.into_iter().zip(self.counts) {
                reason.report(cache, count);
            }
        }
        drop(self.values);
    }

    pub(super) fn record(&mut self, reason: Removal, count: usize) {
        self.counts[reason as usize] += count;
    }
}
