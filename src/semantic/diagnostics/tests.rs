use std::fs;
use std::path::{Path, PathBuf};

use lsp_types::{Position, Range, TextDocumentContentChangeEvent};
use serde_json::json;

use super::*;
use crate::semantic::Engine;
use crate::test_support::TestProject;
use crate::workspace::path_key;
use DiagnosticCode::*;

fn analyze_file(engine: &mut Engine, path: &Path) -> DiagnosticReport {
    let mut snapshot = engine.snapshot();
    let document = snapshot.document(path).unwrap().unwrap();
    analyze_document(&mut snapshot, &document).unwrap()
}

fn single_file(text: &str) -> DiagnosticReport {
    let project = TestProject::new();
    let path = project.write("sample.go", text);
    let mut engine = Engine::new(project.root.clone());
    assert!(engine.install_workspace(
        engine.workspace_load(),
        project.workspace(&["sample.go"], false)
    ));
    analyze_file(&mut engine, &path)
}

fn occurrence(text: &str, context: &str, spelling: &str) -> Range {
    let offset = text
        .find(context)
        .unwrap_or_else(|| panic!("missing context {context:?}"))
        + context.find(spelling).unwrap();
    let prefix = &text[..offset];
    let line = prefix.bytes().filter(|b| *b == b'\n').count() as u32;
    let col = prefix.rsplit('\n').next().unwrap().encode_utf16().count() as u32;
    Range::new(
        Position::new(line, col),
        Position::new(line, col + spelling.encode_utf16().count() as u32),
    )
}

fn item_at<'a>(
    report: &'a DiagnosticReport,
    text: &str,
    code: DiagnosticCode,
    context: &str,
    spelling: &str,
) -> &'a DiagnosticItem {
    let range = occurrence(text, context, spelling);
    report
        .items
        .iter()
        .find(|item| item.code == code && item.range == range)
        .unwrap_or_else(|| {
            panic!(
                "expected {code:?} at {range:?} in {context:?}, got {:#?}",
                report.items
            )
        })
}

#[test]
fn real_workspace_matches_every_diagnostics_fixture() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tools/fixtures/diagnostics");
    let graph = Workspace::load(&root, true).unwrap();
    let mut engine = Engine::new(root.clone());
    assert!(engine.install_workspace(engine.workspace_load(), graph));
    type Expected = (DiagnosticCode, &'static str, &'static str);
    let cases: &[(&str, &[Expected])] = &[
        ("clean/clean.go", &[]),
        ("clean/clean_test.go", &[]),
        (
            "broken_syntax/missing_paren.go",
            &[(SyntaxError, "r\"", "\"")],
        ),
        (
            "broken_syntax/missing_expression.go",
            &[(SyntaxError, "+ )", ")")],
        ),
        (
            "semantic_errors/imports.go",
            &[(ImportUnused, "\"strings\"", "\"strings\"")],
        ),
        (
            "semantic_errors/invalid_import/invalid.go",
            &[(
                ImportInvalid,
                "\"example.test/diagnostics/nonexistent\"",
                "\"example.test/diagnostics/nonexistent\"",
            )],
        ),
        (
            "semantic_errors/self_import/self.go",
            &[(
                ImportSelf,
                "\"example.test/diagnostics/semantic_errors/self_import\"",
                "\"example.test/diagnostics/semantic_errors/self_import\"",
            )],
        ),
        (
            "semantic_errors/names.go",
            &[
                (NameUnresolved, "return missingValue", "missingValue"),
                (TypeUnresolved, "UnresolvedType MissingType", "MissingType"),
                (
                    FieldOrMethodUnresolved,
                    "record.MissingField",
                    "MissingField",
                ),
                (
                    FieldOrMethodUnresolved,
                    "record.MissingMethod",
                    "MissingMethod",
                ),
            ],
        ),
        (
            "semantic_errors/declarations.go",
            &[
                (NameRedeclared, "var Repeated", "Repeated"),
                (VarUnused, "unused := 1", "unused"),
                (ShortVarNoNewVars, "value := value", ":="),
                (ConstReassign, "Fixed = 2", "Fixed"),
            ],
        ),
        (
            "semantic_errors/calls.go",
            &[
                (CallArity, "return Add(1)", "(1)"),
                (ReturnArity, "return 1", "return 1"),
                (CallNonFunction, "value()", "value"),
            ],
        ),
        (
            "semantic_errors/types.go",
            &[
                (TypeMismatch, "= \"text\"", "\"text\""),
                (InterfaceNotImplemented, "= Item{}", "Item{}"),
                (CompositeLiteralUnknownField, "Item{Missing", "Missing"),
                (DuplicateSwitchCase, "case 1:\n\t\treturn 2", "1"),
                (BlankAsValue, "BlankValue = _", "_"),
            ],
        ),
    ];
    let manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(root.join("diagnostics-queries.json")).unwrap())
            .unwrap();
    assert_eq!(cases.len(), manifest["cases"].as_array().unwrap().len());
    for (file, expected) in cases {
        assert!(
            manifest["cases"]
                .as_array()
                .unwrap()
                .iter()
                .any(|case| case["path"] == *file)
        );
        let path = root.join(file);
        let text = fs::read_to_string(&path).unwrap();
        let report = analyze_file(&mut engine, &path);
        assert_eq!(
            report.context_completeness,
            ContextCompleteness::Complete,
            "{file}"
        );
        if expected.is_empty() {
            assert!(report.items.is_empty(), "{file}: {:#?}", report.items);
        }
        assert_eq!(
            report
                .items
                .iter()
                .filter(|item| item.code != PackageLoad)
                .count(),
            expected.len(),
            "{file}: {:#?}",
            report.items
        );
        for (code, context, spelling) in *expected {
            let context = context.replace('\n', if text.contains("\r\n") { "\r\n" } else { "\n" });
            let item = item_at(&report, &text, *code, &context, spelling);
            match code {
                ImportUnused => {
                    assert_eq!(item.tags, [DiagnosticTag::Unnecessary]);
                    assert_eq!(
                        item.fix_data,
                        DiagnosticFixData::RemoveUnusedImport {
                            import_path: "strings".into(),
                            spec_range: item.range
                        }
                    );
                }
                VarUnused => {
                    assert_eq!(item.tags, [DiagnosticTag::Unnecessary]);
                    assert_eq!(
                        item.fix_data,
                        DiagnosticFixData::ReplaceUnusedVarWithBlank {
                            name: "unused".into(),
                            ident_range: item.range
                        }
                    );
                }
                ShortVarNoNewVars => assert_eq!(
                    item.fix_data,
                    DiagnosticFixData::ChangeShortDeclToAssign {
                        operator_range: item.range
                    }
                ),
                _ => {}
            }
        }
    }
}

#[test]
fn cached_diagnostics_follow_unsaved_sibling_bindings() {
    let project = TestProject::new();
    let source = project.write("source.go", "package sample\nconst Value = 1\n");
    let caller = project.write("caller.go", "package sample\nvar X = Value\n");
    let mut engine = Engine::new(project.root.clone());
    engine.install_workspace(
        engine.workspace_load(),
        project.workspace(&["source.go", "caller.go"], false),
    );
    assert!(
        engine
            .diagnostics_report(&caller)
            .unwrap()
            .0
            .items
            .is_empty()
    );
    engine
        .open(source.clone(), "package sample\n".into(), 1)
        .unwrap();
    assert!(
        engine
            .diagnostics_report(&caller)
            .unwrap()
            .0
            .items
            .iter()
            .any(|d| d.code == NameUnresolved)
    );
    engine.close(&source);
    assert!(
        engine
            .diagnostics_report(&caller)
            .unwrap()
            .0
            .items
            .is_empty()
    );
}

#[test]
fn partial_reports_upgrade_and_cache_checks_all_snapshot_fields() {
    let project = TestProject::new();
    let text = "package sample\nvar X = missing\n";
    let path = project.write("sample.go", text);
    let mut engine = Engine::new(project.root.clone());
    engine.open(path.clone(), text.into(), 4).unwrap();
    let mut cache = DiagnosticsCache::default();
    let (partial, partial_key) = {
        let mut s = engine.snapshot();
        let d = s.document(&path).unwrap().unwrap();
        let report = get_or_compute(&mut cache, &mut s, &d).unwrap();
        assert_eq!(report.context_completeness, ContextCompleteness::Partial);
        assert!(report.items.is_empty());
        (report.result_id.clone(), report.snapshot.clone())
    };
    engine.install_workspace(
        engine.workspace_load(),
        project.workspace(&["sample.go"], false),
    );
    let original = {
        let mut s = engine.snapshot();
        let d = s.document(&path).unwrap().unwrap();
        let report = get_or_compute(&mut cache, &mut s, &d).unwrap();
        assert_eq!(report.context_completeness, ContextCompleteness::Complete);
        assert_ne!(partial, report.result_id);
        item_at(report, text, NameUnresolved, "missing", "missing");
        let address = std::ptr::from_ref(report);
        assert_eq!(
            address,
            std::ptr::from_ref(get_or_compute(&mut cache, &mut s, &d).unwrap())
        );
        assert!(cache.get(&snapshot_key(&s, &d).unwrap()).is_some());
        assert_eq!(
            cache.get(&partial_key).unwrap().context_completeness,
            ContextCompleteness::Partial
        );
        snapshot_key(&s, &d).unwrap()
    };
    for (version, content) in [(5, text), (4, "package sample\nvar X = 1\n")] {
        engine.open(path.clone(), content.into(), version).unwrap();
        assert!(cache.get(&current_key(&mut engine, &path)).is_none());
    }
    engine.open(path.clone(), text.into(), 4).unwrap();
    let key = current_key(&mut engine, &path);
    assert!(cache.get(&key).is_none());
    let report = cache.get(&original).unwrap().clone();
    cache.invalidate_document(&path_key(&path));
    assert!(cache.get(&original).is_none());
    cache.insert(report.clone());
    for i in 0..256 {
        let mut another = report.clone();
        another.snapshot.path_key = PathBuf::from(format!("doc{i}.go"));
        cache.insert(another);
    }
    assert!(cache.get(&original).is_none());
    cache.insert(report);
    cache.clear();
    assert!(cache.get(&original).is_none());
}

fn current_key(engine: &mut Engine, path: &Path) -> DiagnosticSnapshotKey {
    snapshot_key_for_path(&engine.snapshot(), path).unwrap()
}

fn replace(engine: &mut Engine, path: &Path, text: &str, version: i32) {
    engine
        .edit(
            path,
            &[TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: text.into(),
            }],
            version,
        )
        .unwrap();
}

#[test]
fn body_edits_reuse_semantic_inputs_and_refresh_the_edited_report() {
    let project = TestProject::new();
    let original = "package sample\nfunc Use() { _ = \"🙂\"; _ = 10 }\nvar Value = 1\n";
    let path = project.write("sample.go", original);
    let same_package = project.write("peer.go", "package sample\nvar _ int = Value\n");
    let dependent = project.write(
        "app/app.go",
        "package app\nimport \"sample\"\nvar _ int = sample.Value\n",
    );
    let graph = format!(
        "{}\n{}",
        json!({"Dir": project.root, "ImportPath": "sample", "Name": "sample", "GoFiles": ["sample.go", "peer.go"]}),
        json!({"Dir": project.root.join("app"), "ImportPath": "app", "Name": "app", "GoFiles": ["app.go"], "Imports": ["sample"]}),
    );
    let mut engine = Engine::new(project.root.clone());
    assert!(engine.install_workspace(
        engine.workspace_load(),
        Workspace::from_test_listing(std::slice::from_ref(&project.root), &graph, false),
    ));
    engine.open(path.clone(), original.into(), 1).unwrap();
    let own = engine.diagnostics_report(&path).unwrap().0.clone();
    let peer = engine.diagnostics_report(&same_package).unwrap().0.clone();
    let caller = engine.diagnostics_report(&dependent).unwrap().0.clone();
    for report in [&own, &peer, &caller] {
        assert_eq!(report.context_completeness, ContextCompleteness::Complete);
        assert!(report.items.is_empty(), "{:?}", report.items);
    }
    let computations = engine.state.diagnostic_keys.borrow().computations();
    let generation = engine.state.declaration_generation;
    engine
        .edit(
            &path,
            &[TextDocumentContentChangeEvent {
                range: Some(occurrence(original, "_ = 10", "10")),
                range_length: None,
                text: "missing\n".into(),
            }],
            2,
        )
        .unwrap();
    let edited = original.replace("10", "missing\n");
    let changed = engine.diagnostics_report(&path).unwrap().0.clone();
    assert_ne!(changed.snapshot.revision, own.snapshot.revision);
    assert_eq!(
        changed.snapshot.semantic_inputs,
        own.snapshot.semantic_inputs
    );
    item_at(&changed, &edited, NameUnresolved, "_ = missing", "missing");
    assert_eq!(engine.diagnostics_report(&same_package).unwrap().0, &peer);
    assert_eq!(engine.diagnostics_report(&dependent).unwrap().0, &caller);
    assert_eq!(engine.state.declaration_generation, generation);
    assert_eq!(
        engine.state.diagnostic_keys.borrow().computations(),
        computations
    );

    replace(&mut engine, &path, original, 3);
    let restored = engine.diagnostics_report(&path).unwrap().0;
    assert!(restored.items.is_empty(), "{:?}", restored.items);
    assert_ne!(restored.snapshot.revision, own.snapshot.revision);
    assert_eq!(
        engine.state.diagnostic_keys.borrow().computations(),
        computations
    );
}

#[test]
fn declaration_edits_invalidate_dependent_memos_and_keep_unrelated_memos() {
    let project = TestProject::new();
    let original = "package sample\nimport alias \"dep\"\nfunc F(int) {}\nfunc local() {}\nvar Value = alias.Number()\n";
    let path = project.write("sample.go", original);
    let peer = project.write("peer.go", "package sample\nvar _ int = Value\n");
    project.write(
        "dep/dep.go",
        "package dep\nfunc Number() int { return 7 }\nfunc Text() string { return \"text\" }\n",
    );
    let other = project.write("other/other.go", "package other\n");
    let graph = [
        json!({"Dir": project.root, "ImportPath": "sample", "Name": "sample", "GoFiles": ["sample.go", "peer.go"], "Imports": ["dep"]}),
        json!({"Dir": project.root.join("dep"), "ImportPath": "dep", "Name": "dep", "GoFiles": ["dep.go"]}),
        json!({"Dir": project.root.join("other"), "ImportPath": "other", "Name": "other", "GoFiles": ["other.go"]}),
    ].map(|package| package.to_string()).join("\n");
    let mut engine = Engine::new(project.root.clone());
    assert!(engine.install_workspace(
        engine.workspace_load(),
        Workspace::from_test_listing(std::slice::from_ref(&project.root), &graph, false),
    ));
    engine.open(path.clone(), original.into(), 1).unwrap();
    let clean = engine.diagnostics_report(&peer).unwrap().0.clone();
    assert!(clean.items.is_empty(), "{:?}", clean.items);
    let unrelated = engine.diagnostics_report(&other).unwrap().0.clone();
    let edits = [
        original
            .replace("import alias", "import another")
            .replace("alias.Number", "another.Number"),
        original.replace("F(int)", "F(string)"),
        original.replace("local()", "local(int)"),
        original.replace("alias.Number()", "alias.Text()"),
        format!("//go:build custom\n\n{original}"),
        format!("{original}var Broken = )\n"),
    ];
    let mut previous = clean.snapshot;
    let mut computations = engine.state.diagnostic_keys.borrow().computations();
    for (version, edit) in (2..).zip(&edits) {
        let generation = engine.state.declaration_generation;
        replace(&mut engine, &path, edit, version);
        let key = current_key(&mut engine, &peer);
        assert_ne!(key.semantic_inputs, previous.semantic_inputs);
        assert_eq!(engine.state.declaration_generation, generation + 1);
        assert_eq!(engine.diagnostics_report(&other).unwrap().0, &unrelated);
        computations += 1;
        assert_eq!(
            engine.state.diagnostic_keys.borrow().computations(),
            computations
        );
        if version == 5 {
            let report = engine.diagnostics_report(&peer).unwrap().0;
            item_at(
                report,
                "package sample\nvar _ int = Value\n",
                TypeMismatch,
                "= Value",
                "Value",
            );
        }
        previous = key;
    }
    let recovered = original.replace("alias.Number()", "alias.Text()");
    replace(&mut engine, &path, &recovered, 8);
    assert_ne!(current_key(&mut engine, &peer), previous);
    replace(&mut engine, &path, original, 9);
    assert!(engine.diagnostics_report(&peer).unwrap().0.items.is_empty());
}

#[test]
fn draft_membership_and_reopening_use_the_current_declarations() {
    let project = TestProject::new();
    let text = "package sample\nvar _ = Draft\n";
    let path = project.write("sample.go", text);
    let draft = project.root.join("draft.go");
    let draft_text = "package sample\nvar Draft = 1\n";
    let mut engine = Engine::new(project.root.clone());
    assert!(engine.install_workspace(
        engine.workspace_load(),
        project.workspace(&["sample.go"], false)
    ));
    let initial = engine.diagnostics_report(&path).unwrap().0.clone();
    item_at(&initial, text, NameUnresolved, "= Draft", "Draft");
    engine.open(draft.clone(), draft_text.into(), 1).unwrap();
    assert!(engine.diagnostics_report(&path).unwrap().0.items.is_empty());
    let opened = current_key(&mut engine, &draft);
    replace(
        &mut engine,
        &draft,
        &format!("//go:build custom\n\n{draft_text}"),
        2,
    );
    let excluded = engine.diagnostics_report(&path).unwrap().0.clone();
    item_at(&excluded, text, NameUnresolved, "= Draft", "Draft");
    replace(&mut engine, &draft, draft_text, 3);
    assert!(engine.diagnostics_report(&path).unwrap().0.items.is_empty());
    engine.close(&draft);
    let closed = engine.diagnostics_report(&path).unwrap().0.clone();
    item_at(&closed, text, NameUnresolved, "= Draft", "Draft");
    engine.open(draft.clone(), draft_text.into(), 1).unwrap();
    assert_ne!(current_key(&mut engine, &draft), opened);
    assert!(engine.diagnostics_report(&path).unwrap().0.items.is_empty());
}

#[test]
fn production_report_keeps_its_result_id_when_test_view_is_installed() {
    let project = TestProject::new();
    let text = "package sample\nvar _ = missing\n";
    let path = project.write("sample.go", text);
    let test = project.write("sample_test.go", "package sample\nvar _ = helper\n");
    let listing = json!({"Dir": project.root, "ImportPath": "example.test/sample", "Name": "sample", "GoFiles": ["sample.go"]});
    let variant = json!({"Dir": project.root, "ImportPath": "example.test/sample [example.test/sample.test]", "ForTest": "example.test/sample", "Name": "sample", "GoFiles": ["sample.go", "sample_test.go"]});
    let roots = std::slice::from_ref(&project.root);
    let mut engine = Engine::new(project.root.clone());
    engine.open(path.clone(), text.into(), 1).unwrap();
    assert!(engine.install_workspace(
        engine.workspace_load(),
        Workspace::from_test_listing(roots, &listing.to_string(), false),
    ));
    let initial = engine.diagnostics_report(&path).unwrap().0.clone();
    let partial = engine.diagnostics_report(&test).unwrap().0.clone();
    assert_eq!(partial.context_completeness, ContextCompleteness::Partial);
    assert!(engine.install_workspace(
        engine.workspace_load(),
        Workspace::from_test_listing(roots, &format!("{listing}\n{variant}"), true),
    ));

    let key = current_key(&mut engine, &path);
    assert_eq!(engine.diagnostics_cache.get(&key), Some(&initial));
    let report = engine.diagnostics_report(&test).unwrap().0.clone();
    assert_eq!(report.context_completeness, ContextCompleteness::Complete);
    assert_ne!(report.result_id, partial.result_id);
    assert!(report.items.iter().any(|item| item.code == NameUnresolved));

    engine
        .open(test.clone(), "package sample\nvar helper int\n".into(), 2)
        .unwrap();
    let key = current_key(&mut engine, &path);
    assert_eq!(engine.diagnostics_cache.get(&key), Some(&initial));
    let repaired = engine.diagnostics_report(&test).unwrap().0;
    assert_ne!(repaired.result_id, report.result_id);
    assert!(repaired.items.is_empty(), "{:#?}", repaired.items);

    engine.close(&test);
    project.write("sample_test.go", "package sample\nimport _ \"other\"\n");
    engine.file_changed(&test, lsp_types::FileChangeType::CHANGED);
    let key = current_key(&mut engine, &path);
    assert_eq!(engine.diagnostics_cache.get(&key), Some(&initial));
    assert!(current_key(&mut engine, &test).partial);
}

#[test]
fn a_watched_disk_edit_invalidates_a_dependent_report_without_a_new_graph() {
    let project = TestProject::new();
    let dependency = project.write("dependency.go", "package sample\nvar Defined int\n");
    let text = "package sample\nvar _ = Defined\n";
    let caller = project.write("caller.go", text);
    let mut engine = Engine::new(project.root.clone());
    let load = engine.workspace_load();
    assert!(engine.install_workspace(
        load,
        project.workspace(&["caller.go", "dependency.go"], false)
    ));
    engine.open(caller.clone(), text.into(), 1).unwrap();
    let initial = engine.diagnostics_report(&caller).unwrap().0.clone();
    assert!(initial.items.is_empty(), "{:#?}", initial.items);

    project.write(
        "dependency.go",
        "package sample\nvar RenamedDeclaration int\n",
    );
    engine.file_changed(&dependency, lsp_types::FileChangeType::CHANGED);
    assert_eq!(engine.workspace_load(), load);
    let key = current_key(&mut engine, &caller);
    assert!(!key.partial);
    assert!(engine.diagnostics_cache.get(&key).is_none());
    let changed = engine.diagnostics_report(&caller).unwrap().0.clone();
    item_at(&changed, text, NameUnresolved, "= Defined", "Defined");

    let test_load = crate::workspace::WorkspaceLoad {
        include_tests: true,
        ..load
    };
    assert!(engine.install_workspace(
        test_load,
        project.workspace(&["caller.go", "dependency.go"], true)
    ));
    let key = current_key(&mut engine, &caller);
    assert_eq!(engine.diagnostics_cache.get(&key), Some(&changed));
}

#[test]
fn changed_transitive_overlays_only_invalidate_dependent_views() {
    let project = TestProject::new();
    let app_text = "package app\nimport \"middle\"\nvar _ = middle.Value\n";
    let app = project.write("app/app.go", app_text);
    let other = project.write("other/other.go", "package other\n");
    project.write(
        "middle/middle.go",
        "package middle\nimport \"leaf\"\nvar Value = leaf.Value\n",
    );
    let leaf = project.write("leaf/leaf.go", "package leaf\nconst Value = 1\n");
    let packages = [
        json!({"Dir": project.root.join("app"), "ImportPath": "app", "Name": "app", "GoFiles": ["app.go"], "Imports": ["middle"]}),
        json!({"Dir": project.root.join("other"), "ImportPath": "other", "Name": "other", "GoFiles": ["other.go"]}),
        json!({"Dir": project.root.join("middle"), "ImportPath": "middle", "Name": "middle", "GoFiles": ["middle.go"], "Imports": ["leaf"]}),
        json!({"Dir": project.root.join("leaf"), "ImportPath": "leaf", "Name": "leaf", "GoFiles": ["leaf.go"]}),
    ];
    let listing = packages
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    let mut engine = Engine::new(project.root.clone());
    assert!(engine.install_workspace(
        engine.workspace_load(),
        Workspace::from_test_listing(std::slice::from_ref(&project.root), &listing, false),
    ));
    engine.open(app.clone(), app_text.into(), 1).unwrap();
    let initial = engine.diagnostics_report(&app).unwrap().0.clone();
    let other_report = engine.diagnostics_report(&other).unwrap().0.clone();
    assert!(initial.items.is_empty(), "{:#?}", initial.items);

    engine
        .open(leaf, "package leaf\nconst Value = 2\n".into(), 2)
        .unwrap();
    let unaffected = current_key(&mut engine, &other);
    assert_eq!(
        engine.diagnostics_cache.get(&unaffected),
        Some(&other_report)
    );
    let changed = current_key(&mut engine, &app);
    assert!(engine.diagnostics_cache.get(&changed).is_none());
    assert_ne!(
        engine.diagnostics_report(&app).unwrap().0.result_id,
        initial.result_id
    );
}

#[test]
fn import_target_and_package_errors_are_diagnostic_inputs() {
    let project = TestProject::new();
    let text = "package sample\nimport alias \"alias/path\"\nvar _ = alias.Value\n";
    let path = project.write("sample.go", text);
    project.write("first/dep.go", "package first\nconst Value = 1\n");
    project.write("second/dep.go", "package second\n");
    let roots = std::slice::from_ref(&project.root);
    let mut engine = Engine::new(project.root.clone());
    engine.open(path.clone(), text.into(), 1).unwrap();
    let graph = |target: &str, error: Option<&str>| {
        let mut owner = json!({"Dir": project.root, "ImportPath": "sample", "Name": "sample", "GoFiles": ["sample.go"], "Imports": ["alias/path"], "ImportMap": {"alias/path": target}});
        if let Some(message) = error {
            owner["Error"] = json!({"Err": message});
        }
        let first = json!({"Dir": project.root.join("first"), "ImportPath": "first", "Name": "first", "GoFiles": ["dep.go"]});
        let second = json!({"Dir": project.root.join("second"), "ImportPath": "second", "Name": "second", "GoFiles": ["dep.go"]});
        Workspace::from_test_listing(roots, &format!("{first}\n{second}\n{owner}"), false)
    };

    assert!(engine.install_workspace(engine.workspace_load(), graph("first", None)));
    let clean = engine.diagnostics_report(&path).unwrap().0.clone();
    assert!(clean.items.is_empty(), "{:#?}", clean.items);
    assert!(engine.install_workspace(engine.workspace_load(), graph("second", None)));
    let key = current_key(&mut engine, &path);
    assert!(engine.diagnostics_cache.get(&key).is_none());
    let changed = engine.diagnostics_report(&path).unwrap().0.clone();
    item_at(&changed, text, NameUnresolved, "alias.Value", "Value");

    for message in ["temporary package failure", "new package failure"] {
        assert!(engine.install_workspace(engine.workspace_load(), graph("second", Some(message))));
        let key = current_key(&mut engine, &path);
        assert!(engine.diagnostics_cache.get(&key).is_none());
        assert!(
            engine
                .diagnostics_report(&path)
                .unwrap()
                .0
                .items
                .iter()
                .any(|item| { item.code == PackageLoad && item.message == message })
        );
    }
}

#[test]
fn package_errors_follow_go_byte_columns_and_unsaved_syntax_repairs() {
    let project = TestProject::new();
    let text = "package sample\r\nvar X = \"🙂\" + )\r\n";
    let path = project.write("sample.go", text);
    project.write("other.go", "package sample\n");
    let column = text.lines().nth(1).unwrap().find(')').unwrap() + 1;
    let listing = json!({"Dir":project.root,"ImportPath":"sample","Name":"sample","GoFiles":["sample.go","other.go"],
        "Error":{"Err":"expected operand, found ')'","Pos":format!("sample.go:2:{column}")},
        "DepsErrors":[{"Err":"dependency metadata unavailable"},{"Err":"unrelated file","Pos":"other.go:1:1"}]});
    let graph = Workspace::from_test_listing(
        std::slice::from_ref(&project.root),
        &listing.to_string(),
        false,
    );
    let mut engine = Engine::new(project.root.clone());
    engine.install_workspace(engine.workspace_load(), graph);
    let report = analyze_file(&mut engine, &path);
    item_at(&report, text, PackageLoad, "+ )", ")");
    item_at(
        &report,
        text,
        PackageLoad,
        "package sample",
        "package sample",
    );
    assert!(!report.items.iter().any(|i| i.message == "unrelated file"));
    assert!(
        report
            .items
            .iter()
            .any(|i| i.code == SyntaxError && i.range.start < i.range.end)
    );
    engine
        .open(path.clone(), text.replace("+ )", ""), 2)
        .unwrap();
    let repaired = analyze_file(&mut engine, &path);
    assert_eq!(repaired.items.len(), 1, "{:?}", repaired.items);
    assert_eq!(repaired.items[0].message, "dependency metadata unavailable");
}

#[test]
fn unresolved_qualifier_after_astral_character_carries_exact_import_fix() {
    let text = "package sample\r\nfunc f(){ _ = \"🙂\"; ghost.Use() }\r\n";
    let report = single_file(text);
    let item = item_at(&report, text, NameUnresolved, "ghost.Use()", "ghost");
    assert_eq!(
        item.fix_data,
        DiagnosticFixData::ImportMissingPackage {
            qualifier: "ghost".into(),
            ident_range: item.range
        }
    );
}

#[test]
fn valid_scopes_calls_promotions_and_assignability_stay_clean() {
    let text = r#"package sample
type N int64
type Label struct { Text string }
func (Label) Name(x int) string { return "" }
type Box struct { Label }
type Reader interface { Name(int) string }
type Nested interface { Reader }
type Callback func(int) string
func pair() (int, string) { return 1, "ok" }
func take(x int, values ...string) {}
func forward() (int, string) { return pair() }
func named() (a, b int) { return }
func generic[T any](x T) T { return x }
func test(c chan int, v any, cb Callback, n N, box Box, args ...string) {
    var _ Nested = box
    take(pair())
    take(1, args...)
    take(2)
    _ = cb(1)
    _ = Box.Name(box, 1)
    _ = (*Box).Name(&box, 1)
    _ = box.Text
    _ = Box{Label: Label{Text:"text"}}
    _ = []byte("hi")
    _ = int64(n)
    var count N = 1
    count = 100
    _ = count
    _ = generic[int](1)
    a := 1
    a, b := a, 2
    if b := b; b != 0 { f := func(){ _ = a }; f() }
    if x := true; x { _ = x } else if !x {}
    for i := 0; i < b; i++ { b := i; _ = b }
    for _, word := range args { _ = word }
    select { case x := <-c: x, y := x, 2; _ = x+y; default: }
    switch t := v.(type) { case int: _ = t; case string: _ = t; default: }
    key := 1; _ = map[int]string{key: "value"}
    _ = make([]int, 1, 2)
    const zero = 0
    var _ N = zero
}
"#;
    let report = single_file(text);
    assert!(report.items.is_empty(), "{:#?}", report.items);
}

#[test]
fn checks_arity_types_and_builtin_shadowing_only_when_determined() {
    let text = r#"package sample
type Count int
type Base struct{}
func (*Base) Mark(int) string { return "" }
type Marker interface { Mark(int) string }
type Wrong struct{}
func (Wrong) Mark(string) string { return "" }
func two() (int,int) { return 1,2 }
func one(n int) {}
func typed() int { return "bad" }
func check(b Base, n int, s string, args []string) {
    var p = nil
    q := nil
    _, _ = p, q
    if n {}
    for ; "bad"; {}
    one(two())
    one("argument")
    n = s
    var _ Marker = b
    var _ Marker = &b
    var _ Marker = Wrong{}
    _ = (*Base).Mark(&b)
    _ = len()
    _ = make([]int)
    _ = append()
    _ = copy(args)
    delete()
    close()
    _ = new()
    len := func() int { return 1 }
    _ = len()
}
"#;
    let report = single_file(text);
    for (code, context, spelling) in [
        (NilWithoutType, "p = nil", "nil"),
        (NilWithoutType, "q := nil", "nil"),
        (ConditionNotBool, "if n", "n"),
        (ConditionNotBool, "; \"bad\";", "\"bad\""),
        (CallArity, "one(two())", "(two())"),
        (TypeMismatch, "one(\"argument\")", "\"argument\""),
        (TypeMismatch, "n = s", "s"),
        (TypeMismatch, "return \"bad\"", "\"bad\""),
        (InterfaceNotImplemented, "Marker = b", "b"),
        (InterfaceNotImplemented, "Marker = Wrong{}", "Wrong{}"),
        (CallArity, ".Mark(&b)", "(&b)"),
        (CallArity, "= len()", "()"),
        (CallArity, "make([]int)", "([]int)"),
        (CallArity, "= append()", "()"),
        (CallArity, "copy(args)", "(args)"),
        (CallArity, "delete()", "()"),
        (CallArity, "close()", "()"),
        (CallArity, "new()", "()"),
    ] {
        item_at(&report, text, code, context, spelling);
    }
    assert_eq!(report.items.len(), 18, "{:#?}", report.items);
}

#[test]
fn unused_reads_and_redeclarations_respect_go_blocks() {
    let text = r#"package sample
type S struct { X, X int; Field int }
func (S) Field() {}
type I interface { A(); A() }
func dup[T, T any](x, x int) {}
func f(v any) {
    var a int
    a = 1
    b := 1
    b++
    const c = 1
    c += 2
    var c = 2
    switch r := v.(type) { case int: default: }
    d := 0
    { d := 1; _ = d }
    _ = func() int { return d }
    var x int
    var x string
    _, x := 1, 2
    _ = x
}
func init() {}
func init() {}
"#;
    let report = single_file(text);
    for (code, context, spelling) in [
        (NameRedeclared, ", X int", "X"),
        (NameRedeclared, ") Field()", "Field"),
        (NameRedeclared, "; A()", "A"),
        (NameRedeclared, ", T any", "T"),
        (NameRedeclared, ", x int", "x"),
        (VarUnused, "a int\n    a =", "a"),
        (ConstReassign, "c +=", "c"),
        (NameRedeclared, "var c = 2", "c"),
        (VarUnused, "r := v", "r"),
        (NameRedeclared, "var x string", "x"),
        (ShortVarNoNewVars, "_, x :=", ":="),
    ] {
        item_at(&report, text, code, context, spelling);
    }
    assert_eq!(report.items.len(), 11, "{:#?}", report.items);
}

#[test]
fn graph_edges_package_names_and_private_members() {
    let project = TestProject::new();
    let text = r#"package sample
import "p/v2"
import again "q"
import again "other"
import . "dot"
import _ "cycle"
type Wrapper struct { named.Public }
var _ = named.secret
var _ = named.Public{}.hidden
var _ = named.Public{}.Visible
var _ = Wrapper{}.Visible
var _ = FromDot
func f() { var _ External; const n = 1; _ = n }
"#;
    let path = project.write("sample.go", text);
    let mut listing = vec![
        json!({"Dir":project.root,"ImportPath":"sample","Name":"sample","GoFiles":["sample.go"],"Imports":["p/v2","q","other","dot","cycle"]}),
    ];
    for (dir, id, name, source, imports) in [
        (
            "p",
            "p/v2",
            "named",
            "const secret = 1\ntype Public struct { hidden, Visible int }",
            vec![],
        ),
        ("q", "q", "q", "", vec![]),
        ("other", "other", "other", "", vec![]),
        (
            "dot",
            "dot",
            "dot",
            "var FromDot = 1\ntype External int",
            vec![],
        ),
        (
            "cycle",
            "cycle",
            "cycle",
            "import _ \"bridge\"",
            vec!["bridge"],
        ),
        (
            "bridge",
            "bridge",
            "bridge",
            "import _ \"sample\"",
            vec!["sample"],
        ),
    ] {
        project.write(
            &format!("{dir}/file.go"),
            &format!("package {name}\n{source}\n"),
        );
        listing.push(json!({"Dir":project.root.join(dir),"ImportPath":id,"Name":name,"GoFiles":["file.go"],"Imports":imports}));
    }
    let graph = Workspace::from_test_listing(
        std::slice::from_ref(&project.root),
        &listing
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
        false,
    );
    let mut engine = Engine::new(project.root.clone());
    engine.install_workspace(engine.workspace_load(), graph);
    let report = analyze_file(&mut engine, &path);
    for (code, context, spelling) in [
        (ImportCycle, "\"cycle\"", "\"cycle\""),
        (ImportDuplicateAlias, "again \"other\"", "again"),
        (ImportUnused, "again \"q\"", "again"),
        (NameUnexported, "named.secret", "secret"),
        (NameUnexported, "Public{}.hidden", "hidden"),
    ] {
        item_at(&report, text, code, context, spelling);
    }
    assert_eq!(report.items.len(), 6, "{:#?}", report.items);
}

#[test]
fn clean_standard_library_sources_from_the_installed_graph() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tools/fixtures/diagnostics");
    let graph = Workspace::load(&root, true).unwrap();
    let mut files = Vec::new();
    for package in [
        "errors", "io", "strings", "fmt", "bytes", "bufio", "strconv", "slices", "reflect", "sync",
    ] {
        let package = graph.package(package).unwrap();
        files.extend(package.files.iter().cloned());
    }
    let mut engine = Engine::new(root);
    engine.install_workspace(engine.workspace_load(), graph);
    let mut errors = Vec::new();
    for file in files {
        let report = analyze_file(&mut engine, &file);
        for item in report.items {
            errors.push((
                file.file_name().unwrap().to_owned(),
                item.range.start,
                item.code,
                item.message,
            ));
        }
    }
    assert!(errors.is_empty(), "{errors:#?}");
}

#[test]
fn multi_value_types_and_variadic_element_types() {
    let text = r#"package sample
func pair() (string, int) { return "", 1 }
func want(a int, b int) {}
func forward() (string,string) { return pair() }
func call() {
    want(pair())
    var a, b int = pair()
    _, _ = a, b
}
func first(xs ...[]int) []int { return xs[0] }
type BadError struct{}
func (BadError) Error() int { return 0 }
var _ error = BadError{}
var number = 7
func bad(){ number() }
"#;
    let report = single_file(text);
    for (code, context, spelling) in [
        (TypeMismatch, "return pair()", "pair()"),
        (TypeMismatch, "want(pair())", "pair()"),
        (TypeMismatch, "int = pair()", "pair()"),
        (InterfaceNotImplemented, "error = BadError{}", "BadError{}"),
        (CallNonFunction, "number()", "number"),
    ] {
        item_at(&report, text, code, context, spelling);
    }
    assert_eq!(report.items.len(), 5, "{:#?}", report.items);
}

#[test]
fn syntax_recovery_checks_intact_statements_within_a_damaged_function() {
    let text = "package sample\r\nfunc bad(){ _ = \"🙂\" + ); _ = nearby }\r\nfunc good() { _ = missing }\r\n";
    let project = TestProject::new();
    let path = project.write("sample.go", text);
    let mut engine = Engine::new(project.root.clone());
    let partial = analyze_file(&mut engine, &path);
    assert_eq!(partial.context_completeness, ContextCompleteness::Partial);
    assert_eq!(partial.items.len(), 1, "{:#?}", partial.items);
    item_at(&partial, text, SyntaxError, "+ )", ")");
    engine.install_workspace(
        engine.workspace_load(),
        project.workspace(&["sample.go"], false),
    );
    let report = analyze_file(&mut engine, &path);
    item_at(&report, text, NameUnresolved, "= missing", "missing");
    item_at(&report, text, NameUnresolved, "= nearby", "nearby");
    assert_eq!(report.items.len(), 3, "{:#?}", report.items);
    let fast = first_stage(&mut engine, &path, None, &mut HashSet::new());
    assert_eq!(fast.items, report.items);
}

fn first_stage(
    engine: &mut Engine,
    path: &Path,
    dirty: Option<Bytes<usize>>,
    verify: &mut HashSet<(usize, usize)>,
) -> DiagnosticReport {
    let mut snapshot = engine.snapshot();
    let document = snapshot.document(path).unwrap().unwrap();
    let key = snapshot_key(&snapshot, &document).unwrap();
    analyze_stage1(
        &mut snapshot,
        &document,
        key,
        dirty,
        verify,
        None,
        &mut None,
    )
    .unwrap()
}

#[test]
fn stage1_respects_lexical_shadowing_generics_and_literal_keys() {
    let text = r#"package sample
type record struct { Field int }
type Box[T any] struct { Value T }
func (box Box[P]) method(v P) { _ = v; _ = box }
func f[T any](value T) {
    value := value
    if value := value; true { _ = value }
    { local := value; _ = local }
    _ = record{Field: 2}
    _ = map[int]int{keyDeferred: 3}
    _ = value.Deferred
    _ = "🙂"; _ = missing
}
"#;
    let project = TestProject::new();
    let path = project.write("sample.go", text);
    let mut engine = Engine::new(project.root.clone());
    engine.install_workspace(
        engine.workspace_load(),
        project.workspace(&["sample.go"], false),
    );
    let fast = first_stage(&mut engine, &path, None, &mut HashSet::new());
    assert_eq!(fast.diagnostic_stage, DiagnosticStage::Stage1);
    assert_eq!(fast.context_completeness, ContextCompleteness::Complete);
    assert_eq!(fast.items.len(), 1, "{:#?}", fast.items);
    let missing = item_at(&fast, text, NameUnresolved, "= missing", "missing");
    assert_eq!(missing.severity, DiagnosticSeverity::Error);
    assert_eq!(missing.message, "Undefined missing");
    let full = analyze_file(&mut engine, &path);
    assert_eq!(full.diagnostic_stage, DiagnosticStage::Stage2);
    assert!(full.items.contains(missing));
    assert!(full.items.iter().any(|item| item.code == ShortVarNoNewVars));
}

#[test]
fn stage1_uses_overlay_and_warm_package_names_without_loading_dot_imports() {
    let project = TestProject::new();
    let text =
        "package sample\nimport . \"dot\"\nvar _ = samePackage\nvar _ = FromDot\nvar _ = missing\n";
    let path = project.write("sample.go", text);
    let sibling = project.write("other.go", "package sample\nconst samePackage = 1\n");
    let dep = project.write("dot/dot.go", "package dot\nconst FromDot = 2\n");
    let listing = format!(
        "{}{}",
        json!({"Dir":project.root,"ImportPath":"sample","Name":"sample","GoFiles":["sample.go", "other.go"],"Imports":["dot"]}),
        json!({"Dir":dep.parent().unwrap(),"ImportPath":"dot","Name":"dot","GoFiles":["dot.go"]}),
    );
    let mut engine = Engine::new(project.root.clone());
    engine.install_workspace(
        engine.workspace_load(),
        Workspace::from_test_listing(std::slice::from_ref(&project.root), &listing, false),
    );
    engine.open(path.clone(), text.into(), 1).unwrap();
    let cold = first_stage(&mut engine, &path, None, &mut HashSet::new());
    assert_eq!(cold.context_completeness, ContextCompleteness::Partial);
    assert!(cold.items.is_empty());
    assert_eq!(engine.state.shared_disk.counts(&sibling), (0, 0));
    assert_eq!(engine.state.shared_disk.counts(&dep), (0, 0));

    {
        let mut snapshot = engine.snapshot();
        assert!(snapshot.document(&sibling).unwrap().is_some());
    }
    let warm = first_stage(&mut engine, &path, None, &mut HashSet::new());
    assert_eq!(warm.context_completeness, ContextCompleteness::Complete);
    assert_eq!(warm.items.len(), 1, "{:#?}", warm.items);
    item_at(&warm, text, NameUnresolved, "= missing", "missing");
    assert_eq!(engine.state.shared_disk.counts(&sibling), (1, 1));
    assert_eq!(engine.state.shared_disk.counts(&dep), (0, 0));

    engine
        .open(sibling, "package sample\nconst missing = 1\n".into(), 2)
        .unwrap();
    let overlay = first_stage(&mut engine, &path, None, &mut HashSet::new());
    assert_eq!(overlay.items.len(), 1, "{:#?}", overlay.items);
    item_at(
        &overlay,
        text,
        NameUnresolved,
        "= samePackage",
        "samePackage",
    );
    assert_eq!(engine.state.shared_disk.counts(&dep), (0, 0));
}

#[test]
fn stage1_defers_names_for_unreliable_top_level_signatures() {
    let text = "package sample\nfunc bad(x @) {}\nfunc good() { _ = missing }\n";
    let project = TestProject::new();
    let path = project.write("sample.go", text);
    let mut engine = Engine::new(project.root.clone());
    engine.install_workspace(
        engine.workspace_load(),
        project.workspace(&["sample.go"], false),
    );
    let fast = first_stage(&mut engine, &path, None, &mut HashSet::new());
    assert_eq!(fast.context_completeness, ContextCompleteness::Partial);
    assert!(!fast.items.is_empty());
    assert!(fast.items.iter().all(|item| item.code == SyntaxError));
}

#[test]
fn stage1_limits_work_to_the_edited_function_and_verifies_a_fixed_binding() {
    let text = "package sample\nfunc a() { _ = stillMissing }\nfunc b() { local := 1; _ = local; _ = newMissing }\n";
    let project = TestProject::new();
    let path = project.write("sample.go", text);
    let mut engine = Engine::new(project.root.clone());
    engine.install_workspace(
        engine.workspace_load(),
        project.workspace(&["sample.go"], false),
    );
    let start = text.find("local :=").unwrap();
    let fixed = text.find("= local").unwrap() + 2;
    let mut verify = HashSet::from([(fixed, fixed + "local".len())]);
    let report = first_stage(&mut engine, &path, Some(start..start + 5), &mut verify);
    assert!(verify.is_empty());
    assert_eq!(report.items.len(), 1, "{:#?}", report.items);
    item_at(&report, text, NameUnresolved, "= newMissing", "newMissing");
}

#[test]
fn ast_walk_checks_cancellation_while_inside_a_large_declaration() {
    let text = format!(
        "package sample\nfunc f() {{\n{}}}\n",
        "_ = 1 + 2\n".repeat(8192)
    );
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_go::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(text, None).unwrap();
    let cancelled = AtomicBool::new(false);
    let mut visited = 0;
    let mut signalled = None;
    let result = walk(
        tree.root_node(),
        Some(&cancelled),
        WalkMode::Intact,
        |_, _, _, _| {
            visited += 1;
            if visited == 100 {
                signalled = Some(Instant::now());
                cancelled.store(true, Relaxed);
            }
            Ok(WalkControl::Children)
        },
    );
    assert!(result.is_err());
    assert!((100..=356).contains(&visited), "{visited}");
    assert!(signalled.unwrap().elapsed() <= Duration::from_millis(50));
}

#[test]
fn package_declarations_and_new_imports_use_open_buffers() {
    let project = TestProject::new();
    let sibling = project.write("a.go", "package sample\nvar Defined int\n");
    let text =
        "package sample\nvar Defined = 1\nvar _ = Overlay\nfunc f(){ var x Defined; _ = x }\n";
    let path = project.write("b.go", text);
    let mut engine = Engine::new(project.root.clone());
    engine.install_workspace(
        engine.workspace_load(),
        project.workspace(&["a.go", "b.go"], false),
    );
    let report = analyze_file(&mut engine, &path);
    item_at(&report, text, NameRedeclared, "var Defined", "Defined");
    item_at(&report, text, TypeUnresolved, "x Defined", "Defined");
    engine
        .open(sibling, "package sample\nvar Overlay = 2\n".into(), 2)
        .unwrap();
    engine.open(path.clone(),"package sample\nimport \"math/bits\"\nimport \"C\"\nvar _ = bits.Len(Overlay)\nvar _ C.int\n".into(),3).unwrap();
    let report = analyze_file(&mut engine, &path);
    assert!(report.items.is_empty(), "{:#?}", report.items);
}

#[test]
fn repaired_go_list_parse_error_disappears_before_the_file_is_saved() {
    let project = TestProject::new();
    project.write("go.mod", "module example.test/parse\n\ngo 1.24\n");
    let path = project.write("sample.go", "package 123\n");
    let graph = Workspace::load(&project.root, false).unwrap();
    let mut engine = Engine::new(project.root.clone());
    engine.install_workspace(engine.workspace_load(), graph);
    let disk = analyze_file(&mut engine, &path);
    assert!(
        disk.items.iter().any(|item| item.code == PackageLoad),
        "{:?}",
        disk.items
    );
    engine
        .open(path.clone(), "package sample\n".into(), 1)
        .unwrap();
    let fixed = analyze_file(&mut engine, &path);
    assert!(fixed.items.is_empty(), "{:?}", fixed.items);
}

#[test]
fn positionless_import_cycle_belongs_to_its_canonical_package() {
    let project = TestProject::new();
    let module = "module example.test/same\n\ngo 1.24\n";
    project.write("a-healthy/go.mod", module);
    project.write("b-broken/go.mod", module);
    let healthy = project.write("a-healthy/sample.go", "package sample\n");
    let text = "package sample\nimport _ \"example.test/same/child\"\n";
    let broken = project.write("b-broken/sample.go", text);
    project.write(
        "b-broken/child/child.go",
        "package child\nimport _ \"example.test/same\"\n",
    );
    let roots = ["a-healthy", "b-broken"].map(|dir| project.root.join(dir));
    let graph = Workspace::load_roots(&roots, false).unwrap();
    let broken_id = &graph.package_of_file(&broken).unwrap().id;
    assert_ne!(broken_id, &graph.package_of_file(&healthy).unwrap().id);
    assert_ne!(broken_id, "example.test/same");
    assert!(graph.package_errors().iter().any(|error| {
        error.package_id.as_ref() == Some(broken_id)
            && error.path.is_none()
            && error.message.contains("import cycle")
    }));
    let mut engine = Engine::with_roots(roots.to_vec());
    assert!(engine.install_workspace(engine.workspace_load(), graph));
    let healthy = analyze_file(&mut engine, &healthy);
    assert!(healthy.items.is_empty(), "{:#?}", healthy.items);
    let broken = analyze_file(&mut engine, &broken);
    let cycle = item_at(
        &broken,
        text,
        PackageLoad,
        "package sample",
        "package sample",
    );
    assert!(cycle.message.contains("import cycle"), "{cycle:?}");
}

#[test]
fn unsaved_imports_use_error_evidence_from_the_owners_build_group() {
    let project = TestProject::new();
    project.write(
        "healthy/go.mod",
        "module example.test/same\n\ngo 1.24\n\nrequire example.test/local v0.0.0\nreplace example.test/local => ../local\n",
    );
    project.write("broken/go.mod", "module example.test/same\n\ngo 1.24\n");
    project.write("local/go.mod", "module example.test/local\n\ngo 1.24\n");
    let local = project.write("local/known/known.go", "package known\nconst Value = 1\n");
    project.write("local/unsaved/unsaved.go", "package unsaved\n");
    project.write(
        "healthy/anchor/anchor.go",
        "package anchor\nimport _ \"example.test/local/known\"\n",
    );
    let healthy = project.write("healthy/sample.go", "package sample\n");
    let broken_text = "package sample\nimport _ \"example.test/local/unsaved\"\n";
    let broken = project.write("broken/sample.go", broken_text);
    let roots = ["healthy", "broken"].map(|dir| project.root.join(dir));
    let graph = Workspace::load_roots(&roots, false).unwrap();
    let owner = graph.package_of_file(&healthy).unwrap();
    let known = graph
        .resolve_import(owner, "example.test/local/known")
        .unwrap();
    assert_eq!(known.files.len(), 1);
    assert_eq!(path_key(&known.files[0]), path_key(&local));
    assert!(
        graph
            .resolve_import(owner, "example.test/local/unsaved")
            .is_none()
    );
    let mut engine = Engine::with_roots(roots.to_vec());
    assert!(engine.install_workspace(engine.workspace_load(), graph));
    engine
        .open(
            healthy.clone(),
            "package sample\nimport \"example.test/local/known\"\nimport _ \"example.test/local/unsaved\"\nvar _ = known.Value\n".into(),
            1,
        )
        .unwrap();
    engine.open(broken.clone(), broken_text.into(), 1).unwrap();
    let healthy = analyze_file(&mut engine, &healthy);
    assert!(healthy.items.is_empty(), "{:#?}", healthy.items);
    let broken = analyze_file(&mut engine, &broken);
    item_at(
        &broken,
        broken_text,
        ImportInvalid,
        "\"example.test/local/unsaved\"",
        "\"example.test/local/unsaved\"",
    );
}

#[test]
fn test_variant_cycle_stays_off_shared_ordinary_and_external_test_files() {
    let project = TestProject::new();
    project.write("go.mod", "module example.test/sample\n\ngo 1.24\n");
    let ordinary = project.write("sample.go", "package sample\n");
    let test_text = "package sample\nimport _ \"example.test/sample/child\"\n";
    let test = project.write("sample_test.go", test_text);
    let external = project.write("external_test.go", "package sample_test\n");
    let child = project.write(
        "child/child.go",
        "package child\nimport _ \"example.test/sample\"\n",
    );
    let graph = Workspace::load(&project.root, true).unwrap();
    let mut engine = Engine::new(project.root.clone());
    assert!(engine.install_workspace(engine.workspace_load(), graph));
    for good in [ordinary, external, child] {
        let report = analyze_file(&mut engine, &good);
        assert!(
            report.items.iter().all(|item| item.code != PackageLoad),
            "{good:?}: {:#?}",
            report.items
        );
    }
    let report = analyze_file(&mut engine, &test);
    let cycle = item_at(
        &report,
        test_text,
        PackageLoad,
        "package sample",
        "package sample",
    );
    assert!(cycle.message.contains("import cycle"), "{cycle:?}");
}

#[test]
fn group_load_failure_is_visible_in_its_source_documents() {
    let project = TestProject::new();
    project.write(
        "healthy/go.mod",
        "module example.test/healthy\n\ngo 1.24\n\nrequire example.test/broken v0.0.0\nreplace example.test/broken => ../broken\n",
    );
    let healthy = project.write(
        "healthy/sample.go",
        "package sample\nimport _ \"example.test/broken\"\n",
    );
    project.write("broken/go.mod", "module example.test/broken\n\ngo 1.24\n");
    project.write("broken/go.work", "go 1.24\ninvalid_group_setting 1\n");
    let broken = project.write("broken/sample.go", "package sample\n");
    let roots = ["healthy", "broken"].map(|dir| project.root.join(dir));
    let graph = Workspace::load_roots(&roots, false).unwrap();
    // The unhealthy root is still a usable replacement in the healthy build.
    assert!(graph.package_of_file(&broken).is_some());
    let mut engine = Engine::with_roots(roots.to_vec());
    assert!(engine.install_workspace(engine.workspace_load(), graph));
    let healthy_report = engine.diagnostics_report(&healthy).unwrap().0.clone();
    assert!(
        healthy_report.items.is_empty(),
        "{:#?}",
        healthy_report.items
    );
    let broken_report = engine.diagnostics_report(&broken).unwrap().0.clone();
    let item = item_at(
        &broken_report,
        "package sample\n",
        PackageLoad,
        "package sample",
        "package sample",
    );
    assert!(item.message.contains("invalid_group_setting"), "{item:?}");

    project.write("broken/go.work", "go 1.24\nchanged_group_setting 1\n");
    let graph = Workspace::load_roots(&roots, false).unwrap();
    assert!(engine.install_workspace(engine.workspace_load(), graph));
    let healthy_key = current_key(&mut engine, &healthy);
    assert_eq!(
        engine.diagnostics_cache.get(&healthy_key),
        Some(&healthy_report)
    );
    let broken_key = current_key(&mut engine, &broken);
    assert!(engine.diagnostics_cache.get(&broken_key).is_none());
    let changed = engine.diagnostics_report(&broken).unwrap().0;
    assert_ne!(changed.result_id, broken_report.result_id);
    assert!(
        changed
            .items
            .iter()
            .any(|item| item.message.contains("changed_group_setting"))
    );

    assert!(
        engine
            .update_environment([("GOFLAGS".into(), "-tags=diagnostic_cache_test".into())].into())
    );
    let graph = Workspace::load_roots_in_environment(&roots, false, engine.environment()).unwrap();
    assert!(engine.install_workspace(engine.workspace_load(), graph));
    let changed = engine.diagnostics_report(&healthy).unwrap().0;
    assert_ne!(changed.result_id, healthy_report.result_id);
    assert!(changed.items.is_empty(), "{:#?}", changed.items);
}

#[test]
fn positioned_package_errors_can_reach_unindexed_local_dependencies() {
    let project = TestProject::new();
    project.write(
        "app/go.mod",
        "module example.test/app\n\ngo 1.22\n\nrequire example.test/local v0.0.0\nreplace example.test/local => ../local\n",
    );
    project.write(
        "app/app.go",
        "package app\nimport _ \"example.test/local\"\n",
    );
    project.write("local/go.mod", "module example.test/local\n\ngo 1.22\n");
    let broken = project.write("local/local.go", "package 123\n");
    let root = project.root.join("app");
    let graph = Workspace::load(&root, false).unwrap();
    assert!(graph.package_of_file(&broken).is_none());
    assert!(graph.group_for_path(&broken).is_none());
    let mut engine = Engine::new(root);
    assert!(engine.install_workspace(engine.workspace_load(), graph));
    let report = analyze_file(&mut engine, &broken);
    assert!(
        report
            .items
            .iter()
            .any(|item| { item.code == PackageLoad && item.range.start == Position::new(0, 8) }),
        "{:#?}",
        report.items
    );
}

#[test]
fn escaped_imports_do_not_suggest_importing_an_existing_qualifier() {
    let text = r#"package sample
import "ma\x74h"
var _ = math.Pi
"#;
    assert!(single_file(text).items.is_empty());
}

#[test]
fn named_alias_and_anonymous_struct_literals_keep_field_diagnostics() {
    let text = r#"package sample
type Empty struct{}
type Named struct{ Present int }
type Alias = Named
type InlineAlias = struct{ Present int }
var _ = Empty{Missing: 1}
var _ = Alias{Present: 1, Missing: 2}
var _ = InlineAlias{Present: 1, Missing: 2}
var _ = struct{ Present int }{Present: 1, Missing: 2}
var _ = map[int]int{Undefined: 1}
"#;
    let report = single_file(text);
    for context in [
        "Empty{Missing",
        "Alias{Present: 1, Missing",
        "InlineAlias{Present: 1, Missing",
        "}{Present: 1, Missing",
    ] {
        item_at(
            &report,
            text,
            CompositeLiteralUnknownField,
            context,
            "Missing",
        );
    }
    item_at(&report, text, NameUnresolved, "{Undefined", "Undefined");
    assert_eq!(report.items.len(), 5, "{:#?}", report.items);
}

#[test]
fn zero_parameter_calls_and_nested_returns_use_their_own_signatures() {
    let text = r#"package sample
func Getter() int { return 1 }
var _ = Getter()
var _ = Getter(1)
func Outer() int {
    func() (int, int) { return 1 }()
    return Getter()
}
"#;
    let report = single_file(text);
    item_at(&report, text, CallArity, "Getter(1)", "(1)");
    item_at(&report, text, ReturnArity, "{ return 1 }()", "return 1");
    assert_eq!(report.items.len(), 2, "{:#?}", report.items);
}

#[test]
fn escaped_explicit_import_only_suppresses_the_matching_qualifier() {
    let text = r#"package sample
import alias "ma\x74h"
var _ = alias.Pi
var _ = unknown.Pi
"#;
    let report = single_file(text);
    let missing = item_at(&report, text, NameUnresolved, "unknown.Pi", "unknown");
    assert!(matches!(
        missing.fix_data,
        DiagnosticFixData::ImportMissingPackage { .. }
    ));
    assert_eq!(report.items.len(), 1, "{:#?}", report.items);
}

#[test]
fn methods_conflict_with_direct_fields_in_their_file_and_siblings() {
    let project = TestProject::new();
    let text = r#"package sample
type Local struct{ Same int }
func (Local) Same() {}
func (Sibling) Embedded() {}
func (Sibling) Nested() {}
"#;
    let path = project.write("main.go", text);
    project.write(
        "sibling.go",
        r#"package sample
type Embedded struct{}
type Sibling struct{ Embedded; Outer struct{ Nested int } }
"#,
    );
    let mut engine = Engine::new(project.root.clone());
    assert!(engine.install_workspace(
        engine.workspace_load(),
        project.workspace(&["main.go", "sibling.go"], false),
    ));
    let report = analyze_file(&mut engine, &path);
    item_at(&report, text, NameRedeclared, "(Local) Same", "Same");
    item_at(
        &report,
        text,
        NameRedeclared,
        "(Sibling) Embedded",
        "Embedded",
    );
    assert_eq!(report.items.len(), 2, "{:#?}", report.items);
}
