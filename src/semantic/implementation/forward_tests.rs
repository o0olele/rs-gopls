use lsp_types::{FileChangeType, TextDocumentContentChangeEvent};
use serde_json::{Value, json};

use super::*;
use crate::test_support::TestProject;
use crate::workspace::Workspace;

const CONTRACT: &str = "package api\ntype Reader interface { Read() string }\ntype ByteReader interface { Read([]byte) (int, error) }\n";

fn package(dir: &Path, id: &str, name: &str, files: &[&str], imports: &[&str]) -> Value {
    json!({"Dir": dir, "ImportPath": id, "Name": name, "GoFiles": files, "Imports": imports})
}

fn install(engine: &mut Engine, project: &TestProject, packages: &[Value]) {
    let listing = packages
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    let workspace =
        Workspace::from_test_listing(std::slice::from_ref(&project.root), &listing, true);
    assert!(engine.install_workspace(engine.workspace_load(), workspace));
}

fn query(engine: &mut Engine, file: &Path, line: u32) -> Vec<Location> {
    let locations = match engine
        .implementations(file, Position::new(line, 6))
        .unwrap()
    {
        QueryProgress::Ready(found) => found,
        QueryProgress::Searching(mut search) => {
            while !search.advance(engine).unwrap() {}
            search.finish()
        }
        QueryProgress::Pending(_) => panic!("expected the installed test graph"),
    };
    locations
        .into_iter()
        .map(|(file, range)| (path_key(&file), range))
        .collect()
}

fn decl(path: &Path, line: u32, name: &str) -> Location {
    (
        path_key(path),
        Range::new(
            Position::new(line, 5),
            Position::new(line, 5 + name.encode_utf16().count() as u32),
        ),
    )
}

fn sorted(mut locations: Vec<Location>) -> Vec<Location> {
    locations.sort_by_cached_key(|(path, range)| (path_key(path), range.start, range.end));
    locations
}

#[test]
fn finds_cross_package_promotions_and_generic_aliases_without_parsing_unrelated_files() {
    let project = TestProject::new();
    let dependency = TestProject::new();
    let api = project.write("api/api.go", CONTRACT);
    let base = dependency.write(
        "src/base/base.go",
        "package base\ntype Base struct{}\nfunc (*Base) Read() string { return \"\" }\ntype Generic[T any] struct{}\nfunc (Generic[T]) Read() string { return \"\" }\n",
    );
    let adapter = project.write(
        "adapter/adapter.go",
        "package adapter\nimport alt \"base\"\ntype Alias = alt.Base\ntype Box[T any] = alt.Generic[T]\ntype Text = Box[string]\ntype DefinedBase alt.Base\ntype Embedded struct { *alt.Base }\ntype DefinedEmbed Embedded\n",
    );
    let transitive = project.write(
        "transitive/uses.go",
        "package transitive\nimport . \"adapter\"\ntype Final struct { DefinedEmbed }\ntype Specialized struct { Text }\n",
    );
    let mismatch = project.write(
        "mismatch/mismatch.go",
        "package mismatch\nimport _ \"base\"\ntype Noise struct{}\nfunc (Noise) Read([]byte) (int, error) { return 0, nil }\n",
    );
    let broken = project.write(
        "broken/broken.go",
        "package broken\ntype Recovered struct{}\nfunc (Recovered) Read() string { return \"\" }\nfunc unfinished(\n",
    );
    let loose = project.write(
        "loose/loose.go",
        "package loose\nimport \"base\"\ntype Recovered struct { *base.Base @ }\n",
    );

    let mut engine = Engine::new(project.root.clone());
    let mut external = package(base.parent().unwrap(), "base", "base", &["base.go"], &[]);
    external["Goroot"] = json!(true);
    let mut incomplete = package(
        broken.parent().unwrap(),
        "broken",
        "broken",
        &["broken.go"],
        &[],
    );
    incomplete["Incomplete"] = json!(true);
    incomplete["Error"] = json!({"Err": "expected ')', found EOF"});
    install(
        &mut engine,
        &project,
        &[
            package(api.parent().unwrap(), "api", "api", &["api.go"], &[]),
            external,
            package(
                adapter.parent().unwrap(),
                "adapter",
                "adapter",
                &["adapter.go"],
                &["base"],
            ),
            package(
                transitive.parent().unwrap(),
                "transitive",
                "transitive",
                &["uses.go"],
                &["adapter"],
            ),
            package(
                mismatch.parent().unwrap(),
                "mismatch",
                "mismatch",
                &["mismatch.go"],
                &["base"],
            ),
            incomplete,
            package(
                loose.parent().unwrap(),
                "loose",
                "loose",
                &["loose.go"],
                &["base"],
            ),
        ],
    );
    assert_eq!(
        query(&mut engine, &api, 1),
        sorted(vec![
            decl(&base, 1, "Base"),
            decl(&base, 3, "Generic"),
            decl(&adapter, 2, "Alias"),
            decl(&adapter, 3, "Box"),
            decl(&adapter, 4, "Text"),
            decl(&adapter, 6, "Embedded"),
            decl(&adapter, 7, "DefinedEmbed"),
            decl(&transitive, 2, "Final"),
            decl(&transitive, 3, "Specialized"),
            decl(&broken, 1, "Recovered"),
            decl(&loose, 2, "Recovered"),
        ])
    );
    assert!(!engine.state.disk_index.contains_key(&path_key(&mismatch)));
    assert_eq!(query(&mut engine, &api, 2), [decl(&mismatch, 2, "Noise")]);
}

#[test]
fn chooses_the_rarest_required_shape_and_checks_the_complete_signature() {
    let project = TestProject::new();
    let api = project.write(
        "api/api.go",
        "package api\ntype Contract interface { Read() string; Write(a, b int, rest ...string) (first, second bool) }\n",
    );
    let matching = project.write(
        "works/recv.go",
        "package works\ntype Receiver struct{}\nfunc (Receiver) Read() string { return \"\" }\nfunc (Receiver) Write(a, b int, rest ...string) (first, second bool) { return }\n",
    );
    let non_variadic = project.write(
        "slice/recv.go",
        "package slice\ntype Receiver struct{}\nfunc (Receiver) Read() string { return \"\" }\nfunc (Receiver) Write(a, b int, rest []string) (first, second bool) { return }\n",
    );
    let wrong_types = project.write(
        "wide/recv.go",
        "package wide\ntype Receiver struct{}\nfunc (Receiver) Read() string { return \"\" }\nfunc (Receiver) Write(a, b int64, rest ...string) (first, second bool) { return }\n",
    );
    let packages = [
        package(api.parent().unwrap(), "api", "api", &["api.go"], &[]),
        package(
            matching.parent().unwrap(),
            "works",
            "works",
            &["recv.go"],
            &[],
        ),
        package(
            non_variadic.parent().unwrap(),
            "slice",
            "slice",
            &["recv.go"],
            &[],
        ),
        package(
            wrong_types.parent().unwrap(),
            "wide",
            "wide",
            &["recv.go"],
            &[],
        ),
    ];
    let mut engine = Engine::new(project.root.clone());
    install(&mut engine, &project, &packages);
    assert_eq!(
        query(&mut engine, &api, 1),
        [decl(&matching, 1, "Receiver")]
    );
    assert!(
        !engine
            .state
            .disk_index
            .contains_key(&path_key(&non_variadic))
    );
}

#[test]
fn finds_overlay_receivers_and_new_embeddings_then_restores_disk_results_on_close() {
    let project = TestProject::new();
    let api = project.write("api/api.go", CONTRACT);
    let disk = "package recv\ntype Item struct{}\nfunc (Item) Read([]byte) (int, error) { return 0, nil }\n";
    let receiver = project.write("recv/item.go", disk);
    let draft_dir = project.write("draft/seed.go", "package draft\n");
    let draft = draft_dir.with_file_name("overlay.go");
    let mut engine = Engine::new(project.root.clone());
    install(
        &mut engine,
        &project,
        &[
            package(api.parent().unwrap(), "api", "api", &["api.go"], &[]),
            package(
                receiver.parent().unwrap(),
                "recv",
                "recv",
                &["item.go"],
                &[],
            ),
            package(draft.parent().unwrap(), "draft", "draft", &["seed.go"], &[]),
        ],
    );
    assert!(query(&mut engine, &api, 1).is_empty());
    engine.open(receiver.clone(), disk.into(), 1).unwrap();
    engine
        .edit(
            &receiver,
            &[TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text:
                    "package recv\ntype Item struct{}\nfunc (Item) Read() string { return \"\" }\n"
                        .into(),
            }],
            2,
        )
        .unwrap();
    assert_eq!(query(&mut engine, &api, 1), [decl(&receiver, 1, "Item")]);
    engine
        .open(
            draft.clone(),
            "package draft\nimport \"recv\"\ntype Promoted struct { recv.Item }\n".into(),
            1,
        )
        .unwrap();
    assert_eq!(
        query(&mut engine, &api, 1),
        [decl(&draft, 2, "Promoted"), decl(&receiver, 1, "Item")]
    );
    engine.close(&receiver);
    assert!(query(&mut engine, &api, 1).is_empty());
    assert_eq!(
        query(&mut engine, &api, 2),
        [decl(&draft, 2, "Promoted"), decl(&receiver, 1, "Item")]
    );
    engine.close(&draft);
    assert_eq!(query(&mut engine, &api, 2), [decl(&receiver, 1, "Item")]);
}

#[test]
fn cancelled_forward_search_recovers_after_reinstall_and_invalidates_shared_test_views() {
    let project = TestProject::new();
    let api = project.write("api/api.go", CONTRACT);
    let production = project.write(
        "recv/item.go",
        "package recv\ntype Item struct{}\nfunc (Item) Read() string { return \"\" }\n",
    );
    let test = project.write(
        "recv/item_test.go",
        "package recv\ntype TestReader struct{}\nfunc (TestReader) Read() string { return \"\" }\n",
    );
    let dir = production.parent().unwrap();
    let extra_files: Vec<_> = (0..34).map(|index| format!("extra{index}.go")).collect();
    for file in &extra_files {
        project.write(&format!("recv/{file}"), "package recv\n");
    }
    let mut normal_files: Vec<_> = extra_files.iter().map(String::as_str).collect();
    normal_files.push("item.go");
    let mut packages = vec![
        package(api.parent().unwrap(), "api", "api", &["api.go"], &[]),
        package(dir, "recv", "recv", &normal_files, &[]),
    ];
    let mut engine = Engine::new(project.root.clone());
    install(&mut engine, &project, &packages);
    let QueryProgress::Searching(mut search) =
        engine.implementations(&api, Position::new(1, 6)).unwrap()
    else {
        panic!("expected a cancellable cold search");
    };
    assert!(!search.advance(&mut engine).unwrap());
    drop(search);

    engine.file_changed(&test, FileChangeType::CREATED);
    assert!(matches!(
        engine.implementations(&api, Position::new(1, 6)).unwrap(),
        QueryProgress::Pending(_)
    ));
    normal_files.push("item_test.go");
    let mut variant = package(dir, "recv [recv.test]", "recv", &normal_files, &[]);
    variant["ForTest"] = json!("recv");
    packages.push(variant);
    install(&mut engine, &project, &packages);
    assert_eq!(
        query(&mut engine, &api, 1),
        [decl(&production, 1, "Item"), decl(&test, 1, "TestReader")]
    );

    project.write(
        "recv/item.go",
        "package recv\ntype Item struct{}\nfunc (Item) Read([]byte) (int, error) { return 0, nil }\n",
    );
    engine.file_changed(&production, FileChangeType::CHANGED);
    assert_eq!(query(&mut engine, &api, 1), [decl(&test, 1, "TestReader")]);
    assert_eq!(query(&mut engine, &api, 2), [decl(&production, 1, "Item")]);
}

#[test]
fn reverse_embedding_edges_resolve_equal_import_paths_in_their_own_build_groups() {
    let project = TestProject::new();
    let folders = ["first", "second"].map(|folder| project.root.join(folder));
    for folder in ["first", "second"] {
        project.write(
            &format!("{folder}/go.mod"),
            "module example.test/same\n\ngo 1.22\n",
        );
        project.write(
            &format!("{folder}/holder/holder.go"),
            "package holder\nimport \"example.test/same/dependency\"\ntype Holder struct { dep.Item }\n",
        );
    }
    let apis = [
        project.write("first/api/api.go", CONTRACT),
        project.write("second/api/api.go", CONTRACT),
    ];
    let first = project.write(
        "first/dependency/item.go",
        "package dep\ntype Item struct{}\nfunc (Item) Read() string { return \"\" }\n",
    );
    let second = project.write(
        "second/dependency/item.go",
        "package dep\ntype Item struct{}\nfunc (Item) Read([]byte) (int, error) { return 0, nil }\n",
    );
    let workspace = Workspace::load_roots(&folders, true).unwrap();
    assert_ne!(
        workspace.package_of_file(&first).unwrap().id,
        workspace.package_of_file(&second).unwrap().id,
    );
    let mut engine = Engine::with_roots(folders.to_vec());
    assert!(engine.install_workspace(engine.workspace_load(), workspace));
    for (index, dependency) in [&first, &second].into_iter().enumerate() {
        let holder = folders[index].join("holder/holder.go");
        assert_eq!(
            query(&mut engine, &apis[index], index as u32 + 1),
            sorted(vec![
                decl(dependency, 1, "Item"),
                decl(&holder, 2, "Holder")
            ]),
        );
        assert!(query(&mut engine, &apis[index], 2 - index as u32).is_empty());
    }
}

#[test]
fn exact_search_uses_the_source_version_already_read_by_the_probe() {
    let project = TestProject::new();
    let api = project.write("api/api.go", CONTRACT);
    let implemented = project.write(
        "impls/reader.go",
        "package impls\ntype Reader struct{}\nfunc (Reader) Read() string { return \"\" }\n",
    );
    let packages = [
        package(&project.root.join("api"), "api", "api", &["api.go"], &[]),
        package(
            &project.root.join("impls"),
            "impls",
            "impls",
            &["reader.go"],
            &[],
        ),
    ];
    let mut engine = Engine::new(project.root.clone());
    install(&mut engine, &project, &packages);

    let QueryProgress::Searching(mut search) =
        engine.implementations(&api, Position::new(1, 6)).unwrap()
    else {
        panic!("expected a cold search");
    };
    while search.package_filter.is_some() {
        assert!(!search.advance(&mut engine).unwrap());
    }
    assert!(
        !engine
            .state
            .disk_index
            .contains_key(&path_key(&implemented))
    );

    // A change without a watcher notification must not mix two versions
    // between candidate discovery and exact matching.
    project.write(
        "impls/reader.go",
        "package impls\ntype Reader struct{}\nfunc (Reader) Read([]byte) (int, error) { return 0, nil }\n",
    );
    while !search.advance(&mut engine).unwrap() {}
    let matches = search.finish();
    assert_eq!(matches.len(), 1);
    assert_eq!(
        (path_key(&matches[0].0), matches[0].1),
        decl(&implemented, 1, "Reader")
    );

    engine.file_changed(&implemented, FileChangeType::CHANGED);
    install(&mut engine, &project, &packages);
    assert!(query(&mut engine, &api, 1).is_empty());
}

#[test]
fn reinstated_package_uses_current_disk_contents_without_a_watch_event() {
    let project = TestProject::new();
    let api = project.write("api/api.go", CONTRACT);
    let implementation = project.write(
        "impls/reader.go",
        "package impls\ntype Reader struct{}\nfunc (Reader) Read() string { return \"\" }\n",
    );
    let packages = [
        package(&project.root.join("api"), "api", "api", &["api.go"], &[]),
        package(
            &project.root.join("impls"),
            "impls",
            "impls",
            &["reader.go"],
            &[],
        ),
    ];
    let mut engine = Engine::new(project.root.clone());
    install(&mut engine, &project, &packages);
    assert_eq!(
        query(&mut engine, &api, 1),
        [decl(&implementation, 1, "Reader")]
    );

    install(&mut engine, &project, &packages[..1]);
    project.write(
        "impls/reader.go",
        "package impls\ntype Replacement struct{}\nfunc (Replacement) Read() string { return \"\" }\n",
    );
    install(&mut engine, &project, &packages);
    assert_eq!(
        query(&mut engine, &api, 1),
        [decl(&implementation, 1, "Replacement")]
    );
}

#[test]
fn shared_interface_declaration_searches_all_of_its_build_groups() {
    let project = TestProject::new();
    project.write("api/go.mod", "module example.test/api\n\ngo 1.22\n");
    let contract = project.write("api/api.go", CONTRACT);
    let mut implementations = Vec::new();
    for folder in ["first", "second"] {
        project.write(
            &format!("{folder}/go.mod"),
            &format!(
                "module example.test/{folder}\n\ngo 1.22\n\nrequire example.test/api v0.0.0\nreplace example.test/api => ../api\n",
            ),
        );
        let path = project.write(
            &format!("{folder}/reader.go"),
            &format!(
                "package {folder}\nimport _ \"example.test/api\"\ntype Reader struct{{}}\nfunc (Reader) Read() string {{ return \"\" }}\n",
            ),
        );
        implementations.push(decl(&path, 2, "Reader"));
    }
    let folders = ["api", "first", "second"].map(|name| project.root.join(name));
    let workspace = Workspace::load_roots(&folders, true).unwrap();
    let mut engine = Engine::with_roots(folders.to_vec());
    assert!(engine.install_workspace(engine.workspace_load(), workspace));

    assert_eq!(query(&mut engine, &contract, 1), sorted(implementations));
}
