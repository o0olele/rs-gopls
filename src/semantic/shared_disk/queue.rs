use super::*;

static QUEUE_EPOCH: LazyLock<Arc<AtomicU64>> = LazyLock::new(Arc::default);
static QUEUE_WAITERS: Mutex<Vec<Weak<QueueWake>>> = Mutex::new(Vec::new());

pub(super) struct QueueWake {
    sender: Sender<()>,
    waiting: AtomicBool,
}

impl QueueWake {
    pub(super) fn register(sender: &Sender<()>) -> Arc<Self> {
        let wake = Arc::new(Self {
            sender: sender.clone(),
            waiting: AtomicBool::new(false),
        });
        let mut waiters = QUEUE_WAITERS.lock().unwrap();
        waiters.retain(|waiter| waiter.strong_count() != 0);
        waiters.push(Arc::downgrade(&wake));
        // Initialize the loader outside a navigation request.
        LazyLock::force(&LOAD_POOL);
        wake
    }

    pub(super) fn try_reserve(&self) -> Result<LoadSlot, SharedLoadPending> {
        let observed = QUEUE_EPOCH.load(Ordering::SeqCst);
        if LOAD_POOL.slots.try_recv().is_ok() {
            return Ok(LoadSlot(true));
        }
        self.waiting.store(true, Ordering::SeqCst);
        let pending = SharedLoadPending::epoch(QUEUE_EPOCH.clone(), observed);
        if pending.ready() {
            let _ = self.sender.try_send(());
        }
        Err(pending)
    }
}

struct LoadPool {
    jobs: Sender<LoadJob>,
    slots: Receiver<()>,
    dequeue: Sender<()>,
}

static LOAD_POOL: LazyLock<LoadPool> = LazyLock::new(|| {
    let (jobs, receive) = bounded::<LoadJob>(LOAD_QUEUE);
    let (dequeue, slots) = bounded(LOAD_QUEUE);
    for _ in 0..LOAD_QUEUE {
        dequeue.try_send(()).expect("initialize loader capacity");
    }
    let threads = thread::available_parallelism()
        .map_or(4, usize::from)
        .clamp(4, MAX_LOAD_THREADS);
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
                    release_load_slot();
                    if catch_unwind(AssertUnwindSafe(|| job(&mut parser))).is_err() {
                        parser.reset();
                        let _ = parser.set_included_ranges(&[]);
                    }
                }
            })
            .expect("start Go source loader");
    }
    LoadPool {
        jobs,
        slots,
        dequeue,
    }
});

fn release_load_slot() {
    LOAD_POOL
        .dequeue
        .try_send(())
        .expect("reserved loader capacity");
    QUEUE_EPOCH.fetch_add(1, Ordering::SeqCst);
    QUEUE_WAITERS.lock().unwrap().retain(|waiter| {
        let Some(wake) = waiter.upgrade() else {
            return false;
        };
        if wake.waiting.swap(false, Ordering::SeqCst) {
            let _ = wake.sender.try_send(());
        }
        true
    });
}

pub(super) struct LoadSlot(bool);

impl LoadSlot {
    pub(super) fn submit_traced(self, trace: Option<FlightTrace>, job: LoadJob) -> Result<()> {
        let Some(trace) = trace else {
            return self.submit(job);
        };
        let context = telemetry::context();
        let enqueued = Instant::now();
        self.submit(Box::new(move |parser| {
            telemetry::with_context(context, || {
                trace.phase("pool_queue", telemetry::elapsed(enqueued), None);
                job(parser);
                trace.report_stats(telemetry::take_shared_disk_stats());
            });
        }))
    }

    pub(super) fn submit(mut self, job: LoadJob) -> Result<()> {
        LOAD_POOL
            .jobs
            .try_send(job)
            .map_err(|_| anyhow::anyhow!("Go source loading pool stopped"))?;
        self.0 = false;
        Ok(())
    }
}

impl Drop for LoadSlot {
    fn drop(&mut self) {
        if self.0 {
            release_load_slot();
        }
    }
}

#[cfg(test)]
#[test]
fn full_queue_waits_for_a_capacity_generation() {
    let (notify, wake) = bounded(1);
    let subscriber = QueueWake::register(&notify);
    let mut slots = Vec::new();
    let pending = loop {
        match subscriber.try_reserve() {
            Ok(slot) => slots.push(slot),
            Err(pending) => break pending,
        }
    };
    // A live worker may also return capacity while the suite is running.
    drop(slots);
    assert!(pending.ready());
    wake.recv_timeout(Duration::from_secs(5)).unwrap();
}
