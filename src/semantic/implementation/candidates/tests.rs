use lsp_types::{Position, Range, TextDocumentContentChangeEvent};
use serde_json::json;

use super::*;
use crate::test_support::TestProject;

fn start_build(engine: &mut Engine) {
    assert!(engine.implementation_sources.pending());
    while engine.implementation_sources.build.is_none() {
        assert!(engine.implementation_index_ready());
        engine.advance_implementation_index().unwrap();
    }
}

fn implementations(engine: &mut Engine, path: &Path, line: u32) -> Vec<(PathBuf, Range)> {
    match engine
        .implementations(path, Position::new(line, 6))
        .unwrap()
    {
        QueryProgress::Ready(results) => results,
        QueryProgress::Searching(mut search) => {
            while !search.advance(engine).unwrap() {}
            search.finish()
        }
        QueryProgress::Pending(_) => panic!("expected the installed test view"),
    }
}

fn replace_source(engine: &mut Engine, path: &Path, text: &str, version: i32) {
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
fn equivalent_edits_keep_in_flight_and_published_postings() {
    let project = TestProject::new();
    let disk = concat!(
        "package sample\ntype Contract interface { Read(int) }\n",
        "func exercise() { type Local struct { Receiver }; _ = Local{} }\n",
        "type Receiver struct{}\nfunc (Receiver) Read(int) {}\n",
    );
    let path = project.write("sample.go", disk);
    let open = |text: &str| {
        let mut engine = Engine::new(project.root.clone());
        engine.include_tests = true;
        assert!(engine.install_workspace(
            engine.workspace_load(),
            project.workspace(&["sample.go"], true),
        ));
        engine.open(path.clone(), text.into(), 1).unwrap();
        engine
    };
    let mut engine = open(disk);
    let (queue, jobs) = bounded(1);
    engine.implementation_sources.worker.queue = Some(queue);
    start_build(&mut engine);
    let build = jobs.try_recv().unwrap();

    let body_edit = disk.replace("_ = Local{}", "\n _ = Local{}\n _ = 1");
    replace_source(&mut engine, &path, &body_edit, 2);
    assert!(!build.cancelled.load(Relaxed));
    build.run();
    engine.advance_implementation_index().unwrap();
    assert!(!engine.implementation_sources.pending());
    assert_eq!(implementations(&mut engine, &path, 1).len(), 1);

    let comment = "// Contract reads a value.\n";
    let at = Position::new(1, 0);
    engine
        .edit(
            &path,
            &[TextDocumentContentChangeEvent {
                range: Some(Range::new(at, at)),
                range_length: None,
                text: comment.into(),
            }],
            3,
        )
        .unwrap();
    assert!(!engine.implementation_sources.pending());
    let shifted = implementations(&mut engine, &path, 2);
    assert_eq!(shifted[0].1.start, Position::new(6, 5));
    let current = body_edit
        .replace("type Contract", &format!("{comment}type Contract"))
        .replace("_ = 1", "_ = 2");
    replace_source(&mut engine, &path, &current, 4);
    assert!(!engine.implementation_sources.pending());
    assert!(jobs.is_empty());
    let actual = implementations(&mut engine, &path, 2);
    assert_eq!(actual, shifted);
    assert_eq!(actual, implementations(&mut open(&current), &path, 2));
}

#[test]
fn local_types_import_bindings_signatures_and_recovery_rebuild_postings() {
    let project = TestProject::new();
    let disk = concat!(
        "package sample\nimport alias \"left\"\n",
        "type Contract interface { alias.Reader }\n",
        "type Receiver struct{}\nfunc (Receiver) Read(int) {}\n",
        "func exercise() { type Local struct { Receiver }; _ = Local{} }\n",
    );
    let path = project.write("sample.go", disk);
    project.write(
        "left/types.go",
        "package left\ntype Reader interface { Read(int) }\n",
    );
    project.write(
        "right/types.go",
        "package right\ntype Reader interface { Read(string) }\n",
    );
    let listing = [
        json!({"Dir": project.root, "ImportPath": "sample", "Name": "sample", "GoFiles": ["sample.go"], "Imports": ["left", "right"]}),
        json!({"Dir": project.root.join("left"), "ImportPath": "left", "Name": "left", "GoFiles": ["types.go"]}),
        json!({"Dir": project.root.join("right"), "ImportPath": "right", "Name": "right", "GoFiles": ["types.go"]}),
    ]
    .map(|package| package.to_string())
    .join("\n");
    let open = |text: &str| {
        let mut engine = Engine::new(project.root.clone());
        engine.include_tests = true;
        assert!(engine.install_workspace(
            engine.workspace_load(),
            Workspace::from_test_listing(std::slice::from_ref(&project.root), &listing, true),
        ));
        engine.open(path.clone(), text.into(), 1).unwrap();
        engine
    };
    let has_receiver = |locations: &[(PathBuf, Range)]| {
        locations
            .iter()
            .any(|(file, range)| file == &path && range.start == Position::new(3, 5))
    };
    let mut engine = open(disk);
    assert!(has_receiver(&implementations(&mut engine, &path, 2)));

    let mut text = disk.to_owned();
    for (index, (old, new, expected)) in [
        ("struct { Receiver }", "struct { *Receiver }", true),
        ("\"left\"", "\"right\"", false),
        ("Read(int)", "Read(string)", true),
        ("_ = Local{}", "_ =", true),
    ]
    .into_iter()
    .enumerate()
    {
        text = text.replace(old, new);
        assert!(!engine.implementation_sources.pending());
        replace_source(&mut engine, &path, &text, 2 + index as i32);
        assert_eq!(
            engine.state.documents[&path_key(&path)]
                .tree
                .root_node()
                .has_error(),
            new == "_ =",
        );
        assert!(engine.implementation_sources.pending(), "edit of {old:?}");
        let actual = implementations(&mut engine, &path, 2);
        assert_eq!(actual, implementations(&mut open(&text), &path, 2));
        assert_eq!(has_receiver(&actual), expected, "edit of {old:?}");
    }
}

#[test]
fn large_open_discards_postings_probed_before_the_overlay_was_parsed() {
    let project = TestProject::new();
    let disk = "package sample\ntype T struct{}\nfunc (T) Before() {}\n";
    let path = project.write("sample.go", disk);
    let mut engine = Engine::new(project.root.clone());
    assert!(engine.install_workspace(
        engine.workspace_load(),
        project.workspace(&["sample.go"], false)
    ));
    let overlay = disk.replace("Before", "After") + &" ".repeat(source::LARGE_OPEN_BYTES);
    engine.open(path.clone(), overlay, 1).unwrap();
    assert!(engine.document_parse_pending(&path));

    let (queue, jobs) = bounded(1);
    engine.implementation_sources.worker.queue = Some(queue);
    start_build(&mut engine);
    let disk_build = jobs.try_recv().unwrap();

    let parsed = engine
        .open_parse_wake()
        .recv_timeout(Duration::from_secs(10))
        .unwrap();
    assert_eq!(engine.complete_open_parse(parsed).unwrap(), Some(path));
    assert!(disk_build.cancelled.load(Relaxed));
    start_build(&mut engine);
    jobs.try_recv().unwrap().run();
    engine.advance_implementation_index().unwrap();
    let postings = engine.implementation_sources.postings.as_ref().unwrap();
    assert!(!postings.receivers.contains_key("Before"));
    assert!(postings.receivers.contains_key("After"));
}

#[test]
fn source_edit_and_view_upgrade_discard_work_before_publication() {
    let project = TestProject::new();
    let disk = "package sample\ntype T struct{}\nfunc (T) Before() {}\n";
    let path = project.write("sample.go", disk);
    let mut engine = Engine::new(project.root.clone());
    assert!(engine.install_workspace(
        engine.workspace_load(),
        project.workspace(&["sample.go"], false)
    ));
    engine.open(path.clone(), disk.to_owned(), 1).unwrap();

    // Hold the job off-thread until the test chooses when it can complete.
    let (queue, jobs) = bounded(1);
    engine.implementation_sources.worker.queue = Some(queue);
    start_build(&mut engine);
    let old = jobs.try_recv().unwrap();
    assert!(!engine.implementation_index_ready());
    assert!(engine.implementation_sources.postings.is_none());

    engine
        .edit(
            &path,
            &[TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: "package sample\ntype T struct{}\nfunc (T) After() {}\n".to_owned(),
            }],
            2,
        )
        .unwrap();
    assert!(old.cancelled.load(Relaxed));
    old.run();
    start_build(&mut engine);
    let production = jobs.try_recv().unwrap();
    let cancelled = production.cancelled.clone();
    production.run();
    assert!(engine.implementation_index_ready());
    assert!(engine.implementation_sources.postings.is_none());

    project.write(
        "sample_test.go",
        "package sample\ntype TestOnly struct{}\nfunc (TestOnly) TestMethod() {}\n",
    );
    engine.include_tests = true;
    assert!(engine.install_workspace(
        engine.workspace_load(),
        project.workspace(&["sample.go", "sample_test.go"], true)
    ));
    assert!(cancelled.load(Relaxed));
    start_build(&mut engine);
    jobs.try_recv().unwrap().run();
    assert!(engine.implementation_sources.postings.is_none());
    engine.advance_implementation_index().unwrap();
    let postings = engine.implementation_sources.postings.as_ref().unwrap();
    assert!(!postings.receivers.contains_key("Before"));
    assert!(postings.receivers.contains_key("After"));
    assert!(postings.receivers.contains_key("TestMethod"));
    assert!(!engine.implementation_index_ready());
}

#[test]
fn background_publication_preserves_conservative_candidates() {
    let project = TestProject::new();
    let bad = project.write("bad/base.go", "package bad\ntype Incomplete struct {\n");
    let error = project.write("underlying/error.go", "package underlying\n");
    let derived = project.write("wrapper/wrap.go", "package wrapper\nimport b \"bad\"\ntype Derived struct { b.Unseen }\nfunc f() { type Local struct{} }\n");
    let listing = [
        json!({"Dir": project.root.join("bad"), "ImportPath": "bad", "Name": "bad", "GoFiles": ["base.go"]}),
        json!({"Dir": project.root.join("underlying"), "ImportPath": "underlying", "Name": "underlying", "GoFiles": ["error.go"], "Error": {"Err": "package listing incomplete"}}),
        json!({"Dir": project.root.join("wrapper"), "ImportPath": "wrapper", "Name": "wrapper", "GoFiles": ["wrap.go"], "Imports": ["bad"]}),
    ].map(|package| package.to_string()).join("\n");
    let workspace =
        Workspace::from_test_listing(std::slice::from_ref(&project.root), &listing, false);
    let mut engine = Engine::new(project.root.clone());
    assert!(engine.install_workspace(engine.workspace_load(), workspace));
    start_build(&mut engine);
    while !engine.implementation_index_ready() {
        engine
            .implementation_wake()
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
    }
    assert!(engine.implementation_sources.postings.is_none());
    engine.advance_implementation_index().unwrap();
    let sources = &engine.implementation_sources;
    let workspace = engine.state.workspace.as_deref().unwrap();
    let mut candidates = CandidatePackages::new(PackageProbeKey::Implementations(Vec::new()));
    candidates.collect(workspace, sources, &[0]);
    assert_eq!(
        candidates.eligible,
        HashSet::from([
            "bad".to_owned(),
            "underlying".to_owned(),
            "wrapper".to_owned()
        ])
    );
    assert!(candidates.types["wrapper"].names.contains("Derived"));
    let files = sources.embedding_files(&derived, "Local", true).unwrap();
    assert_eq!(
        files,
        HashSet::from([path_key(&bad), path_key(&derived), path_key(&error)])
    );
}
