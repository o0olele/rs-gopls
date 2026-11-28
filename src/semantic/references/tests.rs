use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::{Duration, Instant};

use lsp_types::{FileChangeType, Position, Range, TextDocumentContentChangeEvent};
use serde_json::json;

use super::{LARGE_FILE_BYTES, PREFILTER_CHUNK_BYTES, ReferenceSearch};
use crate::semantic::navigation_trace::{NavigationProbe, record};
use crate::semantic::shared_disk::CacheEvent;
use crate::semantic::{Engine, QueryProgress, SemanticTask};
use crate::test_support::TestProject;
use crate::workspace::{Workspace, path_key};

fn engine(project: &TestProject, files: &[&str]) -> Engine {
    let mut engine = Engine::new(project.root.clone());
    assert!(engine.install_workspace(engine.workspace_load(), project.workspace(files, true)));
    engine
}

fn references(
    engine: &mut Engine,
    path: &Path,
    position: Position,
    include_declaration: bool,
) -> Vec<(PathBuf, Range)> {
    match engine
        .references(path, position, include_declaration)
        .unwrap()
    {
        QueryProgress::Ready(matches) => matches,
        QueryProgress::Pending(_) => panic!("expected the installed workspace"),
        QueryProgress::Searching(search) => finish(search, engine, None),
    }
}

fn finish(
    mut search: ReferenceSearch,
    engine: &mut Engine,
    probe: Option<&NavigationProbe>,
) -> Vec<(PathBuf, Range)> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "reference search did not finish");
        if !search.ready(engine) {
            std::thread::sleep(Duration::from_millis(1));
            continue;
        }
        let done = match probe {
            Some(probe) => search.step(engine, Some(probe)),
            None => search.advance(engine),
        };
        if done.unwrap() {
            return search.finish();
        }
    }
}

fn span(line: u32, start: u32, end: u32) -> Range {
    Range::new(Position::new(line, start), Position::new(line, end))
}

#[test]
fn diagnostic_and_reference_workers_share_reads_and_declarations() {
    for large in [false, true] {
        let project = TestProject::new();
        let original = "package sample\nconst Needle = 7\n";
        let target = project.write("target.go", original);
        let mut dependent = "package sample\nfunc Use() { _ = Needle }\n".to_owned();
        if large {
            dependent.push_str(&"// padding for large-file verification\n".repeat(4_000));
        }
        let usage = project.write("usage.go", &dependent);
        let mut engine = engine(&project, &["target.go", "usage.go"]);
        engine.open(target.clone(), original.into(), 1).unwrap();
        let key = engine.diagnostic_key(&target).unwrap();
        let input = engine.capture_diagnostics(&target, key.clone());
        let cache = engine.state.shared_disk.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let (reading, reader) = crossbeam_channel::bounded(1);
        let (waiting, waiter) = crossbeam_channel::bounded(1);
        let usage_key = path_key(&usage);
        let cancellation = cancelled.clone();
        cache.set_hook(Some(Arc::new(move |path, event| {
            if path != usage_key {
                return;
            }
            match event {
                CacheEvent::Read => {
                    reading.send(()).unwrap();
                    waiter.recv_timeout(Duration::from_secs(5)).unwrap();
                }
                CacheEvent::WaitRead => {
                    cancellation.store(large, Relaxed);
                    waiting.send(()).unwrap();
                }
                _ => {}
            }
        })));
        let computation = std::thread::spawn(move || input.analyze(&cancelled).unwrap());
        reader.recv_timeout(Duration::from_secs(5)).unwrap();
        let expected = [(usage.clone(), span(1, 17, 23))];
        assert_eq!(
            references(&mut engine, &target, Position::new(1, 6), false),
            expected
        );
        let report = computation.join().unwrap();
        assert_eq!(report.is_none(), large);
        assert_eq!(cache.counts(&usage).0, 1);
        assert_eq!(cache.projection_counts(&usage), (1, 1));

        cache.set_hook(None);
        let report = engine
            .capture_diagnostics(&target, key)
            .analyze(&AtomicBool::new(false))
            .unwrap()
            .unwrap();
        assert!(report.items.is_empty(), "{:?}", report.items);
        assert_eq!(
            references(&mut engine, &target, Position::new(1, 6), false),
            expected
        );
        assert_eq!(cache.counts(&usage).0, 1);
        assert_eq!(cache.projection_counts(&usage), (1, 1));
    }
}

fn scoped_references(
    engine: &mut Engine,
    path: &Path,
    position: Position,
    include_declaration: bool,
    scope_cache_hit: bool,
) -> Vec<(PathBuf, Range)> {
    let QueryProgress::Searching(search) = engine
        .references(path, position, include_declaration)
        .unwrap()
    else {
        panic!("expected a workspace reference search");
    };
    let probe = NavigationProbe::default();
    let results = finish(search, engine, Some(&probe));
    record(Some(&probe), |counts| {
        assert_eq!(counts.reference_scope_cache_hit, scope_cache_hit);
    });
    results
}

fn replace(engine: &mut Engine, path: &Path, text: &str, version: i32) {
    engine
        .edit(
            path,
            &[TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: text.to_owned(),
            }],
            version,
        )
        .unwrap();
}

#[test]
fn function_body_edits_keep_candidate_scope_and_refresh_anchor_and_scan_positions() {
    let project = TestProject::new();
    let declaration = "package sample\nfunc Padding() {\n}\nfunc Needle() int { return 7 }\n";
    let disk_uses = "package sample\nfunc Use() {\n    _ = Needle()\n}\n";
    let target = project.write("a_target.go", declaration);
    let uses = project.write("b_use.go", disk_uses);
    let mut engine = engine(&project, &["a_target.go", "b_use.go"]);
    engine.open(target.clone(), declaration.into(), 1).unwrap();
    engine.open(uses.clone(), disk_uses.into(), 1).unwrap();

    let before = [
        (target.clone(), span(3, 5, 11)),
        (uses.clone(), span(2, 8, 14)),
    ];
    assert_eq!(
        scoped_references(&mut engine, &uses, Position::new(2, 8), true, false),
        before
    );
    let QueryProgress::Searching(mut stale) =
        engine.references(&uses, Position::new(2, 8), true).unwrap()
    else {
        panic!("expected a workspace reference search");
    };
    assert!(!stale.advance(&mut engine).unwrap());
    replace(
        &mut engine,
        &target,
        "package sample\nfunc Padding() {\n    _ = 0\n}\nfunc Needle() int { return 7 }\n",
        2,
    );
    let overlay_uses =
        "package sample\nfunc Use() {\n    _ = \"🙂\"; _ = Needle(); _ = Needle()\n}\n";
    replace(&mut engine, &uses, overlay_uses, 2);
    assert!(stale.ready(&engine));
    assert!(stale.advance(&mut engine).is_err());
    drop(stale);

    let after = [
        (target.clone(), span(4, 5, 11)),
        (uses.clone(), span(2, 18, 24)),
        (uses.clone(), span(2, 32, 38)),
    ];
    assert_eq!(
        scoped_references(&mut engine, &uses, Position::new(2, 32), true, true),
        after
    );
    assert_eq!(
        scoped_references(&mut engine, &target, Position::new(4, 5), false, true),
        after[1..]
    );

    replace(&mut engine, &target, declaration, 3);
    replace(&mut engine, &uses, disk_uses, 3);
    assert_eq!(
        scoped_references(&mut engine, &uses, Position::new(2, 8), true, true),
        before
    );

    engine.close(&uses);
    engine.close(&target);
    assert_eq!(
        scoped_references(&mut engine, &target, Position::new(3, 5), true, true),
        before
    );
    engine.open(uses.clone(), overlay_uses.into(), 1).unwrap();
    assert_eq!(
        scoped_references(&mut engine, &target, Position::new(3, 5), false, true),
        after[1..]
    );
}

#[test]
fn overlay_imports_invalidate_only_scopes_that_consult_the_changed_edges() {
    let project = TestProject::new();
    let target = project.write("lib/lib.go", "package lib\nfunc Needle() {}\n");
    let independent = project.write("other/other.go", "package other\nfunc Other() {}\n");
    let disk = "package consumer\nfunc Use() {}\n";
    let uses = project.write("consumer/use.go", disk);
    let listing = [
        json!({
            "Dir": target.parent(), "ImportPath": "example.test/sample/lib",
            "Name": "lib", "GoFiles": ["lib.go"],
        }),
        json!({
            "Dir": independent.parent(), "ImportPath": "example.test/sample/other",
            "Name": "other", "GoFiles": ["other.go"],
        }),
        json!({
            "Dir": uses.parent(), "ImportPath": "example.test/sample/consumer",
            "Name": "consumer", "GoFiles": ["use.go"],
        }),
    ]
    .map(|package| package.to_string())
    .join("\n");
    let workspace =
        Workspace::from_test_listing(std::slice::from_ref(&project.root), &listing, true);
    let mut engine = Engine::new(project.root.clone());
    assert!(engine.install_workspace(engine.workspace_load(), workspace));
    engine.open(uses.clone(), disk.into(), 1).unwrap();

    let position = Position::new(1, 5);
    assert!(scoped_references(&mut engine, &target, position, false, false).is_empty());
    let other = [(independent.clone(), span(1, 5, 10))];
    assert_eq!(
        scoped_references(&mut engine, &independent, position, true, false),
        other
    );

    let overlay = concat!(
        "package consumer\n",
        "import lib \"example.test/sample/lib\"\n",
        "func Use() { lib.Needle() }\n",
    );
    replace(&mut engine, &uses, overlay, 2);
    let expected = [(uses.clone(), span(2, 17, 23))];
    assert_eq!(
        scoped_references(&mut engine, &target, position, false, false),
        expected
    );
    assert_eq!(
        scoped_references(&mut engine, &independent, position, true, true),
        other
    );

    let aliased = overlay
        .replace("lib \"", "another \"")
        .replace("lib.Needle", "another.Needle");
    replace(&mut engine, &uses, &aliased, 3);
    assert_eq!(
        scoped_references(&mut engine, &target, position, false, true),
        [(uses.clone(), span(2, 21, 27))]
    );
    replace(&mut engine, &uses, disk, 4);
    assert!(scoped_references(&mut engine, &target, position, false, false).is_empty());
    assert_eq!(
        scoped_references(&mut engine, &independent, position, true, true),
        other
    );

    replace(&mut engine, &uses, overlay, 5);
    assert_eq!(
        scoped_references(&mut engine, &target, position, false, false),
        expected
    );
    engine.close(&uses);
    assert!(scoped_references(&mut engine, &target, position, false, false).is_empty());
    engine.open(uses, overlay.into(), 1).unwrap();
    assert_eq!(
        scoped_references(&mut engine, &target, position, false, false),
        expected
    );
}

#[test]
fn reference_scope_follows_draft_build_constraints_and_test_graph_membership() {
    let project = TestProject::new();
    let target = project.write(
        "a_target.go",
        "package sample\nfunc Needle() int { return 7 }\n",
    );
    let inside_text = "package sample\nfunc TestInside() {\n    _ = Needle()\n}\n";
    let inside = project.write("b_inside_test.go", inside_text);
    let outside = project.write(
        "c_outside_test.go",
        concat!(
            "package sample_test\n",
            "import sample \"example.test/sample\"\n",
            "func TestOutside() {\n    _ = sample.Needle()\n}\n",
        ),
    );
    let draft = project.root.join("d_draft.go");
    let workspace = |with_inside: bool| {
        let ordinary = json!({
            "Dir": project.root, "ImportPath": "example.test/sample",
            "Name": "sample", "GoFiles": ["a_target.go"]
        });
        let internal = json!({
            "Dir": project.root, "ImportPath": "example.test/sample [example.test/sample.test]",
            "ForTest": "example.test/sample", "Name": "sample",
            "GoFiles": ["a_target.go", "b_inside_test.go"]
        });
        let external = json!({
            "Dir": project.root, "ImportPath": "example.test/sample_test [example.test/sample.test]",
            "ForTest": "example.test/sample", "Name": "sample_test",
            "GoFiles": ["c_outside_test.go"], "Imports": ["example.test/sample"]
        });
        let listing = [
            Some(ordinary),
            with_inside.then_some(internal),
            Some(external),
        ]
        .into_iter()
        .flatten()
        .map(|package| package.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        Workspace::from_test_listing(std::slice::from_ref(&project.root), &listing, true)
    };
    let mut engine = Engine::new(project.root.clone());
    assert!(engine.install_workspace(engine.workspace_load(), workspace(true)));
    engine.open(inside.clone(), inside_text.into(), 1).unwrap();
    let draft_text = "package sample\nfunc Draft() { _ = Needle() }\n";
    let constrained = format!("//go:build ignore\n\n{draft_text}");
    engine.open(draft.clone(), constrained.clone(), 1).unwrap();
    let position = Position::new(1, 5);
    let before = [
        (inside.clone(), span(2, 8, 14)),
        (outside.clone(), span(3, 15, 21)),
    ];
    assert_eq!(
        scoped_references(&mut engine, &target, position, false, false),
        before
    );
    let moved_inside = "package sample\nfunc TestInside() {\n    _ = 0\n    _ = Needle()\n}\n";
    replace(&mut engine, &inside, moved_inside, 2);
    let moved = [
        (inside.clone(), span(3, 8, 14)),
        (outside.clone(), span(3, 15, 21)),
    ];
    assert_eq!(
        scoped_references(&mut engine, &target, position, false, true),
        moved
    );

    replace(&mut engine, &draft, draft_text, 2);
    assert_eq!(
        scoped_references(&mut engine, &target, position, false, false),
        [
            moved[0].clone(),
            moved[1].clone(),
            (draft.clone(), span(1, 19, 25))
        ]
    );
    replace(&mut engine, &draft, &constrained, 3);
    assert_eq!(
        scoped_references(&mut engine, &target, position, false, false),
        moved
    );
    engine.close(&draft);
    engine.close(&inside);
    assert_eq!(
        scoped_references(&mut engine, &target, position, false, true),
        before
    );

    std::fs::remove_file(&inside).unwrap();
    engine.file_changed(&inside, FileChangeType::DELETED);
    assert!(engine.install_workspace(engine.workspace_load(), workspace(false)));
    assert_eq!(
        scoped_references(&mut engine, &target, position, false, false),
        before[1..]
    );

    project.write("b_inside_test.go", moved_inside);
    engine.file_changed(&inside, FileChangeType::CREATED);
    assert!(engine.install_workspace(engine.workspace_load(), workspace(true)));
    assert_eq!(
        scoped_references(&mut engine, &target, position, false, false),
        moved
    );
}

#[test]
fn references_preserve_binding_order_and_utf16_in_cold_and_warm_searches() {
    let project = TestProject::new();
    let target = project.write(
        "a_target.go",
        "package sample\nconst Needle = 7\nconst 针目 = 8\n",
    );
    let uses = project.write(
        "b_use.go",
        concat!(
            "package sample\n",
            "func Use() { _ = \"🙂\"; _ = Needle; _ = Needle }\n",
            "var _ = 针目\n",
        ),
    );
    let recovery = project.write(
        "c_recovery.go",
        "package sample\nfunc Broken() { _ = Needle; missing( }\n",
    );
    let numeric = project.write("d_numeric.go", "package sample\nvar _ = 1Needle\n");
    project.write(
        "e_noise.go",
        concat!(
            "package sample\n",
            "// Needle 针目\n",
            "var _, _ = `Needle 针目`, \"Needle 针目\"\n",
            "var 甲Needle, Needle2, _Needle, 针目后 int\n",
            "func Shadow() { Needle, 针目 := 0, 1; _, _ = Needle, 针目 }\n",
        ),
    );
    project.write("f_negative.go", "package sample\nconst Unrelated = 0\n");
    let mut engine = engine(
        &project,
        &[
            "a_target.go",
            "b_use.go",
            "c_recovery.go",
            "d_numeric.go",
            "e_noise.go",
            "f_negative.go",
        ],
    );

    let target_position = Position::new(1, 6);
    let expected = [
        (target.clone(), span(1, 6, 12)),
        (uses.clone(), span(1, 27, 33)),
        (uses.clone(), span(1, 39, 45)),
        (recovery, span(1, 20, 26)),
        (numeric, span(1, 9, 15)),
    ];
    assert_eq!(
        references(&mut engine, &target, target_position, true),
        expected
    );
    assert_eq!(
        references(&mut engine, &target, target_position, false),
        expected[1..]
    );
    assert_eq!(
        references(&mut engine, &target, target_position, true),
        expected
    );

    let unicode_position = Position::new(2, 6);
    assert_eq!(
        references(&mut engine, &target, unicode_position, true),
        [
            (target.clone(), span(2, 6, 8)),
            (uses.clone(), span(2, 8, 10))
        ],
    );
    assert_eq!(
        references(&mut engine, &target, unicode_position, false),
        [(uses, span(2, 8, 10))],
    );
}

#[test]
fn references_use_current_overlay_before_cached_disk_text() {
    let project = TestProject::new();
    let target = project.write("a_target.go", "package sample\nconst Needle = 7\n");
    let disk = "package sample\nvar _ = Needle\n";
    let uses = project.write("b_use.go", disk);
    let mut engine = engine(&project, &["a_target.go", "b_use.go"]);
    let target_position = Position::new(1, 6);
    let initial = [(uses.clone(), span(1, 8, 14))];
    assert_eq!(
        references(&mut engine, &target, target_position, false),
        initial
    );

    engine
        .open(uses.clone(), "package sample\nvar _ = 0\n".to_owned(), 1)
        .unwrap();
    assert!(references(&mut engine, &target, target_position, false).is_empty());

    engine
        .edit(
            &uses,
            &[TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: "package sample\n// inserted\nvar _ = Needle\n".to_owned(),
            }],
            2,
        )
        .unwrap();
    assert_eq!(
        references(&mut engine, &target, target_position, false),
        [(uses.clone(), span(2, 8, 14))],
    );

    engine.close(&uses);
    assert_eq!(
        references(&mut engine, &target, target_position, false),
        initial
    );
}

#[test]
fn cancelled_reference_read_cannot_publish_an_outdated_revision() {
    let project = TestProject::new();
    let declaration = "package sample\nconst Needle = 7\n";
    let target = project.write("a_target.go", declaration);
    let uses = project.write("b_use.go", "package sample\nvar _ = Needle\n");
    let mut engine = engine(&project, &["a_target.go", "b_use.go"]);
    engine
        .open(target.clone(), declaration.to_owned(), 1)
        .unwrap();
    let position = Position::new(1, 6);
    let QueryProgress::Searching(mut search) = engine.references(&target, position, false).unwrap()
    else {
        panic!("expected a cold workspace search");
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while search.target.is_none() || search.work.is_none() {
        assert!(
            Instant::now() < deadline,
            "reference worker was never queued"
        );
        assert!(!search.advance(&mut engine).unwrap());
    }

    engine
        .open(
            uses.clone(),
            "package sample\n// overlay\nvar _ = Needle\n".to_owned(),
            1,
        )
        .unwrap();
    assert!(search.ready(&engine));
    assert!(search.advance(&mut engine).is_err());
    drop(search);

    assert_eq!(
        references(&mut engine, &target, position, false),
        [(uses.clone(), span(2, 8, 14))],
    );
    assert!(!engine.state.navigation_texts.contains_key(&path_key(&uses)));
    assert!(!engine.state.disk_index.contains_key(&path_key(&uses)));
}

#[test]
fn cold_references_skip_negative_files_and_verify_offsets_across_chunks() {
    let project = TestProject::new();
    let declaration = "package sample\nconst 针目 = 7\n";
    let target = project.write("a_target.go", declaration);
    let negative = project.write("b_negative.go", "package sample\nconst Unrelated = 0\n");
    let prefix = "package sample\n//";
    let declaration_prefix = "\nvar _ = ";
    let boundary = LARGE_FILE_BYTES.next_multiple_of(PREFILTER_CHUNK_BYTES);
    let padding = "x".repeat(boundary - 1 - prefix.len() - declaration_prefix.len());
    let text = format!("{prefix}{padding}{declaration_prefix}针目 + 针目\n");
    assert_eq!(text.find("针目"), Some(boundary - 1));
    let positive = project.write("c_positive.go", &text);
    let mut engine = engine(&project, &["a_target.go", "b_negative.go", "c_positive.go"]);
    engine
        .open(target.clone(), declaration.to_owned(), 1)
        .unwrap();

    let position = Position::new(1, 6);
    let expected = [
        (positive.clone(), span(2, 8, 10)),
        (positive.clone(), span(2, 13, 15)),
    ];
    let QueryProgress::Searching(search) = engine.references(&target, position, false).unwrap()
    else {
        panic!("expected a cold workspace search");
    };
    let cold = NavigationProbe::default();
    assert_eq!(finish(search, &mut engine, Some(&cold)), expected);
    record(Some(&cold), |counts| {
        assert_eq!(counts.prefilter_skipped, 1);
        assert_eq!(counts.prefilter_reads, 2);
        assert_eq!(counts.disk_reads, 2);
        assert_eq!(counts.ast_parses, 1);
        assert_eq!(counts.reference_candidate_parses, 1);
        assert_eq!(counts.reference_dependency_parses, 0);
        assert_eq!(counts.offset_tables, 0);
    });
    assert!(!engine.state.disk_index.contains_key(&path_key(&negative)));
    assert!(engine.state.disk_index.contains_key(&path_key(&positive)));
    {
        let mut snapshot = engine.snapshot();
        let workspace = snapshot.workspace().unwrap();
        let resolver = snapshot.resolver(workspace);
        let source = resolver
            .indexes
            .cached_reference_source(&path_key(&positive))
            .expect("the positive file should have a cached AST");
        assert_eq!(source.as_ref().ready_name_offsets("针目"), None);
        assert_eq!(
            source.as_ref().name_offsets("针目"),
            [boundary - 1, boundary + 8]
        );
    }
    let QueryProgress::Searching(search) = engine.references(&target, position, false).unwrap()
    else {
        panic!("expected a warm workspace search");
    };
    let hot = NavigationProbe::default();
    assert_eq!(finish(search, &mut engine, Some(&hot)), expected);
    record(Some(&hot), |counts| {
        assert_eq!(counts.cached_offset_files, 1);
        assert_eq!(counts.disk_reads, 0);
        assert_eq!(counts.ast_parses, 0);
        assert_eq!(counts.offset_tables, 0);
    });
}

#[test]
fn reference_prefilter_discards_a_read_ticket_when_the_file_changes_before_handoff() {
    let project = TestProject::new();
    let declaration = "package sample\nconst Needle = 7\n";
    let target = project.write("a_target.go", declaration);
    let candidate = project.write("b_candidate.go", "package sample\nvar _ = 0\n");
    let mut engine = engine(&project, &["a_target.go", "b_candidate.go"]);
    engine.open(target.clone(), declaration.into(), 1).unwrap();
    let position = Position::new(1, 6);
    let QueryProgress::Searching(mut search) = engine.references(&target, position, false).unwrap()
    else {
        panic!("expected a cold workspace search");
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while search.next_file != search.files.len()
        || search.target.is_none()
        || search.work.is_none()
        || !search.ready(&engine)
    {
        assert!(Instant::now() < deadline, "reference text was never read");
        if search.ready(&engine) {
            assert!(!search.advance(&mut engine).unwrap());
        } else {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    project.write("b_candidate.go", "package sample\nvar _ = Needle\n");
    assert_eq!(
        finish(search, &mut engine, None),
        [(candidate.clone(), span(1, 8, 14))]
    );
    project.write("b_candidate.go", "package sample\nvar _ = 1\n");
    assert!(references(&mut engine, &target, position, false).is_empty());
}

#[test]
fn references_retry_a_verified_file_changed_without_a_watcher() {
    let project = TestProject::new();
    let declaration = "package sample\nconst Needle = 7\n";
    let target = project.write("a_target.go", declaration);
    let usage = project.write("b_usage.go", "package sample\nvar _ = Needle\n");
    let mut files = vec!["a_target.go".to_owned(), "b_usage.go".to_owned()];
    for index in 0..super::BATCH_SIZE {
        let file = format!("c_{index:02}.go");
        project.write(&file, "package sample\nvar _ = 0\n");
        files.push(file);
    }
    let files: Vec<_> = files.iter().map(String::as_str).collect();
    let mut engine = engine(&project, &files);
    engine.open(target.clone(), declaration.into(), 1).unwrap();
    let QueryProgress::Searching(mut search) = engine
        .references(&target, Position::new(1, 6), false)
        .unwrap()
    else {
        panic!("expected a workspace search");
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let key = path_key(&usage);
    while !search.verified.contains_key(&key) {
        assert!(Instant::now() < deadline, "candidate was never verified");
        if search.ready(&engine) {
            assert!(!search.advance(&mut engine).unwrap());
        } else {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    project.write("b_usage.go", "package sample\nvar _ = 100\n");
    assert!(finish(search, &mut engine, None).is_empty());
}

#[test]
fn references_preserve_implicit_import_binding_when_path_omits_package_name() {
    let project = TestProject::new();
    let library = project.write("lib/lib.go", "package dep\nconst Value = 7\n");
    let uses = project.write(
        "a_use.go",
        concat!(
            "package sample\n",
            "import \"example.test/sample/lib\"\n",
            "var _ = dep.Value + dep.Value\n",
        ),
    );
    project.write(
        "b_use.go",
        "package sample\nimport \"example.test/sample/lib\"\nvar _ = dep.Value\n",
    );
    let listing = [
        json!({
            "Dir": project.root,
            "ImportPath": "example.test/sample",
            "Name": "sample",
            "GoFiles": ["a_use.go", "b_use.go"],
            "Imports": ["example.test/sample/lib"],
        }),
        json!({
            "Dir": library.parent(),
            "ImportPath": "example.test/sample/lib",
            "Name": "dep",
            "GoFiles": ["lib.go"],
        }),
    ]
    .map(|package| package.to_string())
    .join("\n");
    let mut engine = Engine::new(project.root.clone());
    let workspace =
        Workspace::from_test_listing(std::slice::from_ref(&project.root), &listing, true);
    assert!(engine.install_workspace(engine.workspace_load(), workspace));

    let expected = [
        (uses.clone(), span(2, 8, 11)),
        (uses.clone(), span(2, 20, 23)),
    ];
    for include_declaration in [true, false] {
        assert_eq!(
            references(&mut engine, &uses, Position::new(2, 8), include_declaration),
            expected,
        );
    }
}

#[test]
fn exported_top_level_references_intersect_identifier_postings_with_direct_imports() {
    let project = TestProject::new();
    let target = project.write("api/api.go", "package api\nfunc Get() {}\n");
    let found = project.write(
        "user/found.go",
        "package user\nimport x \"example.test/sample/api\"\nfunc Use() { _ = \"🙂\"; x.Get() }\n",
    );
    let unrelated = project.write(
        "user/no_import.go",
        "package user\nfunc Unrelated() { Get() }\n",
    );
    let mut noise = String::from("package user\nimport x \"example.test/sample/api\"\n");
    noise.push_str(&"// Get\n".repeat(24_000));
    noise.push_str("var _, _ = \"Get\", x.GetUser\n");
    let name_only = project.write("user/name_only.go", &noise);
    let listing = [
        json!({
            "Dir": target.parent(), "ImportPath": "example.test/sample/api",
            "Name": "api", "GoFiles": ["api.go"],
        }),
        json!({
            "Dir": found.parent(), "ImportPath": "example.test/sample/user",
            "Name": "user", "GoFiles": ["found.go", "no_import.go", "name_only.go"],
            "Imports": ["example.test/sample/api"],
        }),
    ]
    .map(|package| package.to_string())
    .join("\n");
    let workspace =
        Workspace::from_test_listing(std::slice::from_ref(&project.root), &listing, true);
    let mut engine = Engine::new(project.root.clone());
    assert!(engine.install_workspace(engine.workspace_load(), workspace));
    let cache = engine.state.shared_disk.clone();
    let expected = [(target.clone(), span(1, 5, 8)), (found, span(2, 25, 28))];
    let position = Position::new(1, 5);
    assert_eq!(references(&mut engine, &target, position, true), expected);
    assert_eq!(
        references(&mut engine, &target, position, false),
        expected[1..]
    );
    assert_eq!(cache.counts(&unrelated), (1, 0));
    assert_eq!(cache.counts(&name_only), (1, 0));
}

#[test]
fn member_reference_batches_overlap_dependency_parses_and_reuse_them_when_warm() {
    let project = TestProject::new();
    let target = project.write(
        "api/api.go",
        "package api\ntype Value struct{}\nfunc (Value) Run() {}\n",
    );
    let first = project.write("factory/a.go", "package factory\nvar _ = Make\n");
    let second = project.write(
        "factory/b.go",
        "package factory\nvar _ = Make\n// distinct\n",
    );
    let factory = project.write(
        "factory/factory.go",
        "package factory\nimport \"example.test/sample/api\"\nfunc Make() api.Value { return api.Value{} }\n",
    );
    let found = project.write(
        "user/user.go",
        "package user\nimport f \"example.test/sample/factory\"\nfunc Use() { f.Make().Run() }\n",
    );
    let listing = [
        json!({
            "Dir": target.parent(), "ImportPath": "example.test/sample/api",
            "Name": "api", "GoFiles": ["api.go"],
        }),
        json!({
            "Dir": factory.parent(), "ImportPath": "example.test/sample/factory",
            "Name": "factory", "GoFiles": ["a.go", "b.go", "factory.go"],
            "Imports": ["example.test/sample/api"],
        }),
        json!({
            "Dir": found.parent(), "ImportPath": "example.test/sample/user",
            "Name": "user", "GoFiles": ["user.go"],
            "Imports": ["example.test/sample/factory"],
        }),
    ]
    .map(|package| package.to_string())
    .join("\n");
    let workspace =
        Workspace::from_test_listing(std::slice::from_ref(&project.root), &listing, true);
    let mut engine = Engine::new(project.root.clone());
    assert!(engine.install_workspace(engine.workspace_load(), workspace));
    let cache = engine.state.shared_disk.clone();
    let first_key = path_key(&first);
    let second_key = path_key(&second);
    let (second_started, second_parsing) = crossbeam_channel::bounded(1);
    cache.set_hook(Some(Arc::new(move |path, event| {
        if !matches!(event, CacheEvent::Parse) {
            return;
        }
        if path == first_key {
            second_parsing.recv_timeout(Duration::from_secs(5)).unwrap();
        } else if path == second_key {
            second_started.send(()).unwrap();
        }
    })));
    let expected = [(target.clone(), span(2, 13, 16)), (found, span(2, 22, 25))];
    let position = Position::new(2, 13);
    assert_eq!(references(&mut engine, &target, position, true), expected);
    cache.set_hook(None);
    assert_eq!(
        references(&mut engine, &target, position, false),
        expected[1..]
    );
    for file in [&first, &second, &factory] {
        assert_eq!(cache.counts(file), (1, 1), "{}", file.display());
    }
}

#[test]
fn reference_batches_verify_container_hits_before_earlier_cross_package_candidates() {
    let project = TestProject::new();
    let target = project.write(
        "api/api.go",
        "package api\ntype Value struct{}\nfunc (Value) Run() {}\n",
    );
    let shared = project.write(
        "user/usage.go",
        "package user\nimport \"example.test/sample/api\"\nvar value api.Value\n",
    );
    let priority = project.write(
        "user/z_priority.go",
        "package user\nimport \"example.test/sample/api\"\nfunc Direct(x api.Value) { x.Run() }\n",
    );
    let mut files = vec!["usage.go".to_owned(), "z_priority.go".to_owned()];
    for index in 0..super::BATCH_SIZE + 1 {
        let file = format!("a_{index:02}.go");
        project.write(
            &format!("user/{file}"),
            &format!("package user\nfunc Indirect{index}() {{ value.Run() }}\n"),
        );
        files.push(file);
    }
    files.sort_unstable();
    let first = shared.with_file_name(&files[0]);
    let listing = [
        json!({
            "Dir": target.parent(), "ImportPath": "example.test/sample/api",
            "Name": "api", "GoFiles": ["api.go"],
        }),
        json!({
            "Dir": shared.parent(), "ImportPath": "example.test/sample/user",
            "Name": "user", "GoFiles": files, "Imports": ["example.test/sample/api"],
        }),
    ]
    .map(|package| package.to_string())
    .join("\n");
    let workspace =
        Workspace::from_test_listing(std::slice::from_ref(&project.root), &listing, true);
    let mut engine = Engine::new(project.root.clone());
    assert!(engine.install_workspace(engine.workspace_load(), workspace));

    let first_key = path_key(&first);
    let priority_key = path_key(&priority);
    let (parsed, parses) = crossbeam_channel::unbounded();
    let cache = engine.state.shared_disk.clone();
    cache.set_hook(Some(Arc::new(move |path, event| {
        if matches!(event, CacheEvent::Parse) && (path == first_key || path == priority_key) {
            parsed.send(path.to_owned()).unwrap();
        }
    })));
    let results = references(&mut engine, &target, Position::new(2, 13), true);
    cache.set_hook(None);
    assert_eq!(results.len(), super::BATCH_SIZE + 3);
    assert_eq!(results[1].0, first);
    assert_eq!(results.last().unwrap().0, priority);
    assert_eq!(
        parses.try_iter().collect::<Vec<_>>(),
        [path_key(&priority), path_key(&first)]
    );
}
