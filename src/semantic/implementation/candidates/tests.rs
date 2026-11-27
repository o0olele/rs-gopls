use lsp_types::TextDocumentContentChangeEvent;
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
