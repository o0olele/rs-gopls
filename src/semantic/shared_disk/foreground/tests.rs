use std::time::Instant;

use super::*;
use crate::test_support::TestProject;

fn wait(cache: &SharedDiskCache, pending: &SharedLoadPending) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !pending.ready() {
        cache
            .wake()
            .recv_deadline(deadline)
            .expect("foreground source wake");
    }
}

fn replay<T>(cache: &SharedDiskCache, mut run: impl FnMut() -> Result<T>) -> Result<T> {
    for _ in 0..12 {
        match run() {
            Err(error) if error.is::<SharedLoadPending>() => {
                wait(cache, error.downcast_ref().unwrap());
            }
            result => return result,
        }
    }
    panic!("foreground source replay did not terminate");
}

#[test]
fn oversized_text_ast_and_declarations_survive_foreground_replay() {
    let project = TestProject::new();
    let large_comment = "x".repeat(DECLARATION_ITEM_BYTES);
    let path = project.write(
        "large.go",
        &format!("package p\n// {large_comment}\nfunc Value() int {{ return 1 }}\n"),
    );
    let cache = SharedDiskCache::new(1, 1, 1, 1);
    let memo = QueryReadMemo::default();
    let declarations = replay(&cache, || {
        cache.load_declarations_foreground(&path, &memo, None)
    })
    .unwrap();
    let source = replay(&cache, || cache.load_foreground(&path, &memo, None)).unwrap();

    assert!(
        cache
            .files
            .lock()
            .unwrap()
            .cached
            .peek(&path_key(&path))
            .is_none()
    );
    let key = ContentKey::new(&source.text);
    {
        let parsed = cache.parsed.lock().unwrap();
        assert!(parsed.cached.peek(&key).is_none());
        assert!(parsed.declarations.peek(&key).is_none());
    }
    let repeated = cache.load_foreground(&path, &memo, None).unwrap();
    let repeated_declarations = cache
        .load_declarations_foreground(&path, &memo, None)
        .unwrap();
    assert!(Arc::ptr_eq(&source.revision, &repeated.revision));
    assert!(Arc::ptr_eq(&source.text.text, &repeated.text.text));
    assert!(Arc::ptr_eq(
        &declarations.declarations,
        &repeated_declarations.declarations
    ));
    assert_eq!(cache.counts(&path), (1, 2));
}

#[test]
fn missing_and_unreadable_revisions_are_terminal_for_a_query() {
    let project = TestProject::new();
    let missing = project.root.join("missing.go");
    let invalid = project.root.join("invalid.go");
    fs::write(&invalid, [0xff, 0xff]).unwrap();
    let cache = SharedDiskCache::default();
    let memo = QueryReadMemo::default();
    for (path, kind) in [
        (&missing, io::ErrorKind::NotFound),
        (&invalid, io::ErrorKind::InvalidData),
    ] {
        let check = |error: anyhow::Error| {
            assert_eq!(error.downcast_ref::<io::Error>().unwrap().kind(), kind)
        };
        check(
            replay(&cache, || cache.read_foreground(path, &memo, None))
                .err()
                .unwrap(),
        );
        fs::write(path, "package p\n").unwrap();
        check(cache.read_foreground(path, &memo, None).err().unwrap());
        let fresh = QueryReadMemo::default();
        assert!(replay(&cache, || cache.read_foreground(path, &fresh, None)).is_ok());
    }
    assert_eq!(cache.counts(&missing), (1, 0));
    assert_eq!(cache.counts(&invalid), (2, 0));
}

#[test]
fn rewrite_after_artifact_start_is_detected_without_a_file_event() {
    let project = TestProject::new();
    let path = project.write("p.go", "package p\nvar Old = 1\n");
    let cache = SharedDiskCache::default();
    let memo = QueryReadMemo::default();
    replay(&cache, || cache.read_foreground(&path, &memo, None)).unwrap();
    let (entered, started) = bounded(1);
    let (resume, blocked) = bounded(1);
    cache.set_hook(Some(Arc::new(move |_, event| {
        if event == CacheEvent::Parse {
            entered.send(()).unwrap();
            blocked.recv_timeout(Duration::from_secs(5)).unwrap();
        }
    })));

    let pending = cache.load_foreground(&path, &memo, None).err().unwrap();
    assert!(pending.is::<SharedLoadPending>());
    started.recv_timeout(Duration::from_secs(5)).unwrap();
    let result = fs::write(&path, "package p\nvar Replacement = 2\n");
    resume.send(()).unwrap();
    result.unwrap();
    wait(&cache, pending.downcast_ref().unwrap());
    let changed = replay(&cache, || cache.load_foreground(&path, &memo, None))
        .err()
        .unwrap();
    assert_eq!(
        changed.downcast_ref::<io::Error>().unwrap().kind(),
        io::ErrorKind::Interrupted
    );
    cache.set_hook(None);
    let fresh = QueryReadMemo::default();
    let source = replay(&cache, || cache.load_foreground(&path, &fresh, None)).unwrap();
    assert!(source.text.text.contains("Replacement"));
}

#[test]
fn contended_cache_probe_wakes_on_unlock() {
    let cache = SharedDiskCache::default();
    let files = cache.files.lock().unwrap();
    let pending = cache
        .read_foreground(Path::new("contended.go"), &QueryReadMemo::default(), None)
        .err()
        .unwrap();
    let pending = pending.downcast::<SharedLoadPending>().unwrap();
    assert!(!pending.ready());
    drop(files);
    cache.wake().recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(pending.ready());
}

#[test]
fn disk_revision_wire_preserves_signed_timestamps_and_large_integers() {
    let revision = DiskSourceRev {
        key: PathBuf::from("p.go"),
        modified: Some(timestamp(
            SystemTime::UNIX_EPOCH - Duration::from_nanos(100),
        )),
        created: Some(1_800_000_000_000_000_007),
        length: 14,
        fingerprint: u64::MAX,
        #[cfg(unix)]
        identity: (7, u64::MAX, -1, 3),
    };
    let wire = serde_json::to_value(&revision).unwrap();
    assert_eq!(wire["modified"], "-100");
    assert_eq!(wire["created"], "1800000000000000007");
    assert_eq!(wire["fingerprint"], "18446744073709551615");
    assert_eq!(
        serde_json::from_value::<DiskSourceRev>(wire).unwrap(),
        revision
    );
}
