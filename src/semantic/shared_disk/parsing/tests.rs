use super::super::tests::parser;
use super::*;
use crate::test_support::TestProject;

const LIMIT: Duration = Duration::from_secs(5);

#[test]
fn full_first_supplies_concurrent_declarations() {
    let project = TestProject::new();
    let path = project.write("p.go", "package p\nfunc F() { println(1) }\n");
    let cache = SharedDiskCache::default();
    let (started, full_started) = bounded(1);
    let (joined, declarations_joined) = bounded(1);
    let (release, blocked) = bounded(1);
    cache.set_hook(Some(Arc::new(move |_, event| match event {
        CacheEvent::Parse => {
            started.send(()).unwrap();
            blocked.recv_timeout(LIMIT).unwrap();
        }
        CacheEvent::WaitDeclarations => joined.send(()).unwrap(),
        _ => {}
    })));
    thread::scope(|scope| {
        let full = scope.spawn(|| cache.load(&mut parser(), &path, Some(&SHARED_LOAD), None));
        full_started.recv_timeout(LIMIT).unwrap();
        let declarations = scope.spawn(|| {
            cache.load_declarations(&mut parser(), &path, None, Some(&SHARED_LOAD), None)
        });
        let joined = declarations_joined.recv_timeout(LIMIT);
        release.send(()).unwrap();
        joined.unwrap();
        full.join().unwrap().unwrap();
        let declarations = declarations.join().unwrap().unwrap();
        assert!(Arc::ptr_eq(
            &declarations.declarations,
            &cache.cached_declarations(&declarations.text).unwrap()
        ));
    });
    assert_eq!(cache.projection_counts(&path), (1, 1));
    assert_eq!(cache.counts(&path), (1, 1));
}

#[test]
fn malformed_stub_hands_off_to_running_full_parse() {
    let project = TestProject::new();
    let path = project.write("p.go", "package p\nvar Broken = (\n");
    let cache = SharedDiskCache::default();
    let (stub_started, stub_entered) = bounded(1);
    let (full_started, full_entered) = bounded(1);
    let (release_stub, blocked_stub) = bounded(1);
    let (release_full, blocked_full) = bounded(1);
    cache.set_hook(Some(Arc::new(move |_, event| match event {
        CacheEvent::Declarations => {
            stub_started.send(()).unwrap();
            blocked_stub.recv_timeout(LIMIT).unwrap();
        }
        CacheEvent::Parse => {
            full_started.send(()).unwrap();
            blocked_full.recv_timeout(LIMIT).unwrap();
        }
        _ => {}
    })));
    thread::scope(|scope| {
        let stub = scope.spawn(|| {
            cache.load_declarations(&mut parser(), &path, None, Some(&SHARED_LOAD), None)
        });
        stub_entered.recv_timeout(LIMIT).unwrap();
        let full = scope.spawn(|| cache.load(&mut parser(), &path, Some(&SHARED_LOAD), None));
        let started = full_entered.recv_timeout(LIMIT);
        release_stub.send(()).unwrap();
        started.unwrap();
        let deadline = std::time::Instant::now() + LIMIT;
        while std::time::Instant::now() < deadline {
            let parsed = cache.parsed.lock().unwrap();
            if parsed
                .loading
                .values()
                .any(|loads| matches!(loads.declarations, Some((DeclarationProducer::Full, _))))
            {
                break;
            }
            drop(parsed);
            thread::sleep(CANCELLATION_INTERVAL);
        }
        release_full.send(()).unwrap();
        assert!(std::time::Instant::now() < deadline);
        let stub = stub.join().unwrap().unwrap();
        let full = full.join().unwrap().unwrap().into_indexed();
        assert!(full.tree.root_node().has_error());
        assert!(!stub.declarations.reliable_declarations);
        assert!(matches!(
            cache
                .parsed
                .lock()
                .unwrap()
                .declarations
                .peek(&ContentKey::new(&stub.text)),
            Some(DeclarationProjection::FullRecovery(_))
        ));
        let key = ContentKey::new(&stub.text);
        cache.parsed.lock().unwrap().declarations.remove(&key);
        let refilled = cache.source_declarations(&full, None).unwrap();
        assert!(!refilled.reliable_declarations);
        assert!(matches!(
            cache.parsed.lock().unwrap().declarations.peek(&key),
            Some(DeclarationProjection::FullRecovery(cached)) if Arc::ptr_eq(cached, &refilled)
        ));
    });
    assert_eq!(cache.projection_counts(&path), (1, 2));
}

#[test]
fn projection_eviction_and_oversized_delivery_release_unpinned_entries() {
    let project = TestProject::new();
    let first = project.write("one.go", "package p\nvar One int\n");
    let second = project.write("two.go", "package p\nvar Two int\n");
    let large = project.write(
        "large.go",
        &format!(
            "package p\nfunc F() {{/*{}*/}}\n",
            " ".repeat(DECLARATION_ITEM_BYTES)
        ),
    );
    let cache = SharedDiskCache::default();
    cache.parsed.lock().unwrap().declarations.entry_limit = 1;
    let mut parser = parser();
    let first_result = cache
        .load_declarations(&mut parser, &first, None, None, None)
        .unwrap();
    let first_weak = Arc::downgrade(&first_result.declarations);
    let first_text = first_result.text.clone();
    drop(first_result);
    let second_result = cache
        .load_declarations(&mut parser, &second, None, None, None)
        .unwrap();
    assert!(first_weak.upgrade().is_none());
    assert!(cache.cached_declarations(&first_text).is_none());
    let large = cache
        .load_declarations(&mut parser, &large, None, None, None)
        .unwrap();
    assert!(large.declarations.index.symbols.declaration("F").is_some());
    assert!(cache.cached_declarations(&large.text).is_none());
    assert!(cache.cached_declarations(&second_result.text).is_some());
    assert!(cache.parsed.lock().unwrap().loading.is_empty());
}

#[test]
fn registration_defers_new_contents_when_all_slots_are_occupied() {
    let cache = SharedDiskCache::default();
    let mut registered = Vec::new();
    for index in 0..SOURCE_FLIGHTS {
        let key = ContentKey {
            fingerprint: index as u64,
            text: Arc::new(format!("package p\nvar N{index} int\n")),
            overlay_syntax: None,
        };
        let flight = Arc::new(Flight::new(Some(cache.wake.0.clone())));
        cache.parsed.lock().unwrap().loading.insert(
            key.clone(),
            SourceFlights {
                declarations: Some((DeclarationProducer::Stub, flight.clone())),
                ..Default::default()
            },
        );
        registered.push((key, flight));
    }
    let text = NavigationText {
        text: Arc::new("package p\nvar NewlyRequested int\n".to_owned()),
        stamp: None,
        read_ticket: None,
    };
    let result = cache.declarations_snapshot(&mut parser(), Path::new("p.go"), text, None, None);
    let pending = result
        .err()
        .unwrap()
        .downcast::<SharedLoadPending>()
        .unwrap();
    assert!(!pending.ready());
    assert_eq!(cache.parsed.lock().unwrap().loading.len(), SOURCE_FLIGHTS);
    for (key, flight) in registered {
        let mut parsed = cache.parsed.lock().unwrap();
        parsed.loading.remove(&key);
        flight.finish(Err(anyhow::anyhow!("test released registration")));
    }
    assert!(pending.ready());
}

#[test]
fn watched_change_during_stub_build_cannot_bind_old_declarations() {
    let project = TestProject::new();
    let path = project.write("p.go", "package p\nvar Old int\n");
    let cache = SharedDiskCache::default();
    let (started, waiting) = bounded(1);
    let (release, blocked) = bounded(1);
    let first = AtomicBool::new(true);
    cache.set_hook(Some(Arc::new(move |_, event| {
        if event == CacheEvent::Declarations && first.swap(false, Ordering::Relaxed) {
            started.send(()).unwrap();
            blocked.recv_timeout(LIMIT).unwrap();
        }
    })));
    thread::scope(|scope| {
        let reader = scope.spawn(|| {
            cache.load_declarations(&mut parser(), &path, None, Some(&SHARED_LOAD), None)
        });
        waiting.recv_timeout(LIMIT).unwrap();
        project.write("p.go", "package p\nvar Replacement int\n");
        cache.invalidate(&path_key(&path));
        release.send(()).unwrap();
        let actual = reader.join().unwrap().unwrap();
        assert!(
            actual
                .declarations
                .index
                .symbols
                .declaration("Replacement")
                .is_some()
        );
        assert!(
            actual
                .declarations
                .index
                .symbols
                .declaration("Old")
                .is_none()
        );
        assert!(cache.validates_read(&path, &actual.text));
    });
    assert_eq!(cache.projection_counts(&path), (0, 2));
}

#[test]
fn cancelled_declaration_producer_and_subscriber_leave_other_waiter_intact() {
    let project = TestProject::new();
    let path = project.write("p.go", "package p\nfunc F() { println(1) }\n");
    let cache = SharedDiskCache::default();
    let (started, waiting) = bounded(1);
    let (joined, subscribers) = bounded(2);
    let (release, blocked) = bounded(1);
    cache.set_hook(Some(Arc::new(move |_, event| match event {
        CacheEvent::Declarations => {
            started.send(()).unwrap();
            blocked.recv_timeout(LIMIT).unwrap();
        }
        CacheEvent::WaitDeclarations => joined.send(()).unwrap(),
        _ => {}
    })));
    let producer_cancelled = &AtomicBool::new(false);
    let subscriber_cancelled = &AtomicBool::new(false);
    thread::scope(|scope| {
        let load =
            |cancelled| cache.load_declarations(&mut parser(), &path, None, Some(cancelled), None);
        let producer = scope.spawn(move || load(producer_cancelled));
        waiting.recv_timeout(LIMIT).unwrap();
        let subscriber = scope.spawn(move || load(subscriber_cancelled));
        let survivor = scope.spawn(move || load(&SHARED_LOAD));
        subscribers.recv_timeout(LIMIT).unwrap();
        subscribers.recv_timeout(LIMIT).unwrap();
        producer_cancelled.store(true, Ordering::Relaxed);
        subscriber_cancelled.store(true, Ordering::Relaxed);
        assert!(producer.join().unwrap().is_err());
        assert!(subscriber.join().unwrap().is_err());
        release.send(()).unwrap();
        let survivor = survivor.join().unwrap().unwrap();
        assert!(Arc::ptr_eq(
            &survivor.declarations,
            &cache.cached_declarations(&survivor.text).unwrap()
        ));
    });
    assert_eq!(cache.projection_counts(&path), (0, 1));
}
