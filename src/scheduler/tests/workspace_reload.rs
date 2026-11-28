use super::*;
use lsp_types::notification::DidSaveTextDocument;

fn test_graph(project: &TestProject) -> Workspace {
    let production = json!({
        "Dir": project.root,
        "ImportPath": "example.test/sample",
        "Name": "sample",
        "GoFiles": ["api.go"],
    });
    let tests = json!({
        "Dir": project.root,
        "ImportPath": "example.test/sample [example.test/sample.test]",
        "ForTest": "example.test/sample",
        "Name": "sample",
        "GoFiles": ["api.go", "api_test.go"],
    });
    Workspace::from_test_listing(
        std::slice::from_ref(&project.root),
        &format!("{production}\n{tests}"),
        true,
    )
}

fn start_prewarm(session: &mut Session, project: &TestProject) -> (WorkspaceLoad, Arc<AtomicBool>) {
    let production = session.running().engine.workspace_load();
    session.deliver(production, Ok(project.workspace(&["api.go"], false)));
    let running = session.running();
    let (load, at) = running
        .scheduler
        .prewarm
        .expect("production schedules warmup");
    assert_eq!(load.generation, production.generation);
    assert!(load.include_tests);
    assert!(running.scheduler.requested.is_none());
    assert_eq!(running.scheduler.workspace_deadline(), Some(at));
    assert_eq!(
        running
            .scheduler
            .take_workspace_load(&running.engine, at - Duration::from_millis(1)),
        None,
    );
    assert_eq!(
        running.scheduler.take_workspace_load(&running.engine, at),
        Some((load, TEST_VIEW_PREWARM)),
    );
    let cancelled = Arc::new(AtomicBool::new(false));
    running.scheduler.loading = Some(load);
    running.scheduler.prewarming_tests = Some(PrewarmingTests {
        cancelled: cancelled.clone(),
        promoted: false,
    });
    (load, cancelled)
}

#[test]
fn workspace_references_join_one_test_view_prewarm_or_use_its_installed_view() {
    let project = TestProject::new();
    let api = project.write("api.go", "package sample\nfunc Find() {}\n");
    let test = project.write("api_test.go", "package sample\nfunc test() { Find() }\n");
    for in_flight in [true, false] {
        let mut session = Session::start(&project);
        let (load, cancelled) = start_prewarm(&mut session, &project);
        if !in_flight {
            session.deliver(load, Ok(test_graph(&project)));
        }
        session.request(1, References::METHOD, reference(&api, 1, 5));
        assert_eq!(session.scheduler().pending.len(), usize::from(in_flight));
        assert!(session.scheduler().requested.is_none());
        if in_flight {
            let scheduler = session.scheduler();
            assert_eq!(scheduler.loading, Some(load));
            assert!(scheduler.prewarming_tests.as_ref().unwrap().promoted);
            assert!(!cancelled.load(Ordering::Relaxed));
            session.deliver(load, Ok(test_graph(&project)));
        }
        session.finish_searches();
        let result = session
            .reply(1)
            .result
            .as_ref()
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(result.len(), 2);
        assert!(
            result
                .iter()
                .any(|location| location["uri"] == json!(path_uri(&test).unwrap()))
        );
        assert!(session.scheduler().requested.is_none());
        session.count(2);
    }
}

#[test]
fn workspace_configuration_change_cancels_the_old_test_view_prewarm() {
    let project = TestProject::new();
    project.write("api.go", "package sample\n");
    let mut session = Session::start(&project);
    let (old, cancelled) = start_prewarm(&mut session, &project);
    session.notify(
        DidChangeConfiguration::METHOD,
        json!({ "settings": { "env": { "GOFLAGS": "-tags=new" } } }),
    );
    assert!(cancelled.load(Ordering::Relaxed));
    let current = session.running().engine.workspace_load();
    assert_ne!(old.generation, current.generation);
    assert!(!current.include_tests);
    assert_eq!(session.scheduler().requested, Some(current));
    session.deliver(old, Ok(test_graph(&project)));
    assert_eq!(session.running().engine.workspace_load(), current);
    assert!(session.running().engine.test_view_prewarm().is_none());
    assert_eq!(session.begin_load(), current);
    session.deliver(current, Ok(project.workspace(&["api.go"], false)));
    assert_eq!(
        session.scheduler().prewarm.unwrap().0.generation,
        current.generation
    );
}

#[test]
fn workspace_watches_fold_fifteen_bursts_with_edits_and_queue_one_load_each() {
    let project = TestProject::new();
    let text = "package sample\nvar _ = 0\n";
    let api = project.write("api.go", text);
    let extra = project.root.join("extra.go");
    let mut session = Session::start(&project);
    session.open(&api, text);
    let initial = WorkspaceLoad {
        generation: session.running().engine.workspace_load().generation,
        include_tests: true,
    };
    session.deliver(initial, Ok(project.workspace(&["api.go"], true)));

    for group in 0..15 {
        let before = session.running().engine.workspace_load();
        let present = group % 2 == 0;
        if present {
            project.write("extra.go", "package sample\nvar G int\n");
        } else {
            std::fs::remove_file(&extra).unwrap();
        }
        let now = Instant::now();
        for delta in [0, 20, 45, 60] {
            let at = now + Duration::from_millis(delta);
            session
                .scheduler()
                .watch_file(extra.clone(), FileChangeType::CHANGED, at);
            session.scheduler().watch_file(
                extra.clone(),
                if present {
                    FileChangeType::CREATED
                } else {
                    FileChangeType::DELETED
                },
                at,
            );
        }
        assert_eq!(session.scheduler().watched_files.changes.len(), 1);
        assert!(
            session
                .scheduler()
                .take_watched_files(now + Duration::from_millis(74))
                .is_none()
        );
        assert_eq!(session.running().engine.workspace_load(), before);
        session.edit(
            &api,
            group + 2,
            &format!("package sample\nvar _ = {group}\n"),
        );
        session.flush_watches();

        let requested = session.begin_load();
        assert_eq!(requested.generation, before.generation + 1);
        assert!(requested.include_tests);
        session.deliver(before, Ok(project.workspace(&["api.go"], true)));
        assert_eq!(session.scheduler().loading, Some(requested));
        assert!(session.scheduler().requested.is_none());
        let files: &[&str] = if present {
            &["api.go", "extra.go"]
        } else {
            &["api.go"]
        };
        session.deliver(requested, Ok(project.workspace(files, true)));
        assert!(session.scheduler().requested.is_none());
        assert!(session.scheduler().loading.is_none());
        assert!(session.scheduler().prewarm.is_none());
        assert_eq!(
            session.running().engine.open_document_version(&api),
            Some(group + 2)
        );
    }
    session.count(1);
}

#[test]
fn workspace_load_reading_an_intermediate_watcher_state_is_retried() {
    let project = TestProject::new();
    let original = "package sample\nfunc Find() {}\n";
    let api = project.write("api.go", original);
    project.write("stable.go", "package sample\n");
    let mut session = Session::start(&project);
    let current = session.running().engine.workspace_load();
    let files = ["api.go", "stable.go"];
    session.deliver(current, Ok(project.workspace(&files, false)));
    assert!(
        session
            .running()
            .engine
            .snapshot()
            .document(&api)
            .unwrap()
            .is_some()
    );
    session.scheduler().request_workspace_from(current, "test");
    assert_eq!(session.begin_load(), current);
    let validation = session
        .running()
        .engine
        .begin_workspace_cache_validation(current);

    let now = Instant::now();
    project.write("api.go", "//go:build never_selected\n\npackage sample\n");
    session
        .scheduler()
        .watch_file(api.clone(), FileChangeType::CHANGED, now);
    let intermediate = WorkspaceResult {
        load: current,
        result: Ok(validation.validate(project.workspace(&["stable.go"], false))),
    };
    project.write("api.go", original);
    session.scheduler().watch_file(
        api.clone(),
        FileChangeType::CHANGED,
        now + Duration::from_millis(50),
    );
    session.flush_watches();
    assert_eq!(session.running().engine.workspace_load(), current);

    let State::Running(running) = &mut session.state else {
        unreachable!()
    };
    running
        .workspace_loaded(&session.server, intermediate)
        .unwrap();
    let QueryProgress::Ready(graph) = running.engine.snapshot().active_graph(false) else {
        panic!("the current graph should remain available");
    };
    assert!(graph.package_of_file(&api).is_some());
    assert_eq!(session.begin_load(), current);
    session.deliver(current, Ok(project.workspace(&files, false)));
    assert!(session.scheduler().loading.is_none());
    assert!(session.scheduler().requested.is_none());
}

#[test]
fn save_does_not_replace_queued_create_or_delete() {
    let project = TestProject::new();
    let path = project.root.join("api.go");
    let mut session = Session::start(&project);
    for change in [FileChangeType::CREATED, FileChangeType::DELETED] {
        let now = Instant::now();
        session.scheduler().watch_file(path.clone(), change, now);
        session.notify(
            DidSaveTextDocument::METHOD,
            json!({ "textDocument": { "uri": path_uri(&path).unwrap() } }),
        );
        assert_eq!(
            session.scheduler().take_watched_files(now + WATCH_WINDOW),
            Some(vec![(path.clone(), change)]),
        );
    }
}

#[test]
fn partial_test_view_prewarm_keeps_both_production_groups() {
    let project = TestProject::new();
    let good = project.write("good/api.go", "package good\n");
    let flaky = project.write("flaky/api.go", "package flaky\n");
    project.write("good/go.mod", "module example.test/good\n\ngo 1.22\n");
    project.write("flaky/go.mod", "module example.test/flaky\n\ngo 1.22\n");
    let folders = vec![
        good.parent().unwrap().to_owned(),
        flaky.parent().unwrap().to_owned(),
    ];
    let mut session = Session::start(&project);
    session.running().engine.update_roots(folders.clone());
    let production = session.running().engine.workspace_load();
    session.deliver(production, Workspace::load_roots(&folders, false));
    let (load, _) = session.scheduler().prewarm.take().unwrap();
    session.scheduler().loading = Some(load);
    session.scheduler().prewarming_tests = Some(PrewarmingTests {
        cancelled: Arc::new(AtomicBool::new(false)),
        promoted: false,
    });
    std::fs::remove_dir_all(&folders[1]).unwrap();
    let partial = Workspace::load_roots(&folders, true).unwrap();
    assert!(partial.total_load_failure().is_none());
    assert!(partial.package_of_file(&good).is_some());
    assert!(partial.package_of_file(&flaky).is_none());

    session.deliver(load, Ok(partial));
    let snapshot = session.running().engine.snapshot();
    let QueryProgress::Ready(graph) = snapshot.active_graph(false) else {
        panic!("both production groups should remain available");
    };
    assert!(graph.package_of_file(&good).is_some());
    assert!(graph.package_of_file(&flaky).is_some());
    assert!(matches!(
        snapshot.active_graph(true),
        QueryProgress::Pending(_)
    ));
    assert_eq!(session.running().engine.workspace_load(), production);
    assert!(session.scheduler().requested.is_none());
}

#[test]
fn completed_test_view_prewarm_waits_for_interactive_completion() {
    struct ActiveCompletion;

    impl DeferredQuery for ActiveCompletion {
        fn is_completion(&self) -> bool {
            true
        }

        fn run(&self, _: &mut Engine) -> Result<RequestProgress> {
            Ok(QueryProgress::Searching(Box::new(Self)))
        }
    }

    impl SearchTask for ActiveCompletion {
        fn advance(&mut self, _: &mut Engine) -> Result<bool> {
            Ok(true)
        }

        fn finish(self: Box<Self>) -> RequestResult {
            Ok(Value::Null)
        }
    }

    let project = TestProject::new();
    project.write("api.go", "package sample\n");
    project.write("api_test.go", "package sample\n");
    let mut session = Session::start(&project);
    let (load, _) = start_prewarm(&mut session, &project);
    {
        let State::Running(running) = &mut session.state else {
            unreachable!()
        };
        running
            .scheduler
            .submit(
                &mut running.engine,
                &session.server,
                1.into(),
                ActiveCompletion,
            )
            .unwrap();
    }

    session.deliver(load, Ok(test_graph(&project)));
    let running = session.running();
    assert!(running.scheduler.deferred_test_view.is_some());
    assert!(
        running
            .scheduler
            .take_deferred_test_view(&running.engine)
            .is_none()
    );
    assert!(matches!(
        running.engine.snapshot().active_graph(true),
        QueryProgress::Pending(_)
    ));

    session.step();
    assert!(session.reply(1).error.is_none());
    let State::Running(running) = &mut session.state else {
        unreachable!()
    };
    let deferred = running
        .scheduler
        .take_deferred_test_view(&running.engine)
        .unwrap();
    running.workspace_loaded(&session.server, deferred).unwrap();
    assert!(matches!(
        running.engine.snapshot().active_graph(true),
        QueryProgress::Ready(_)
    ));
}
