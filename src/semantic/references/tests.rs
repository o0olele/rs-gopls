use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use lsp_types::{Position, Range, TextDocumentContentChangeEvent};
use serde_json::json;

use super::{LARGE_FILE_BYTES, PREFILTER_CHUNK_BYTES, ReferenceSearch};
use crate::semantic::navigation_trace::{NavigationProbe, record};
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
            Some(probe) => search.step(engine, Some(probe), Instant::now()),
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
