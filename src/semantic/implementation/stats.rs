use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub(in crate::semantic) fn clock() -> Option<Instant> {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    ENABLED
        .get_or_init(|| {
            std::env::var_os("RS_GOPLS_IMPLEMENTATION_STATS").is_some_and(|value| value == "1")
        })
        .then(Instant::now)
}

#[derive(Default)]
pub(super) struct IndexStats {
    scheduled: Option<Instant>,
    build: Duration,
    reads: usize,
    bytes: usize,
}

impl IndexStats {
    pub(super) fn new() -> Self {
        Self {
            scheduled: clock(),
            ..Self::default()
        }
    }

    pub(super) fn begin(&mut self) {
        if self.scheduled.is_none() {
            self.scheduled = clock();
        }
    }

    pub(super) fn read(&mut self, bytes: usize) {
        if self.scheduled.is_some() {
            self.reads += 1;
            self.bytes += bytes;
        }
    }

    pub(super) fn add_build(&mut self, started: Instant) {
        if self.scheduled.is_some() {
            self.add_duration(started.elapsed());
        }
    }

    pub(super) fn add_duration(&mut self, duration: Duration) {
        if self.scheduled.is_some() {
            self.build += duration;
        }
    }

    pub(super) fn ready(&mut self, packages: usize, files: usize) {
        let stats = std::mem::take(self);
        if let Some(started) = stats.scheduled {
            eprintln!(
                "rs-gopls-wp2 {{\"kind\":\"index\",\"packages\":{packages},\"files\":{files},\"build_us\":{},\"ready_us\":{},\"disk_reads\":{},\"disk_bytes\":{}}}",
                stats.build.as_micros(),
                started.elapsed().as_micros(),
                stats.reads,
                stats.bytes,
            );
        }
    }
}

#[derive(Default)]
pub(super) struct QueryStats {
    pub(super) index_wait: Duration,
    pub(super) probe: Duration,
    pub(super) parse: Duration,
    pub(super) matching: Duration,
}

impl QueryStats {
    pub(super) fn report(&self) {
        eprintln!(
            "rs-gopls-wp2 {{\"kind\":\"implementation\",\"index_wait_us\":{},\"probe_us\":{},\"parse_us\":{},\"match_us\":{}}}",
            self.index_wait.as_micros(),
            self.probe.as_micros(),
            self.parse.as_micros(),
            self.matching.as_micros(),
        );
    }
}
