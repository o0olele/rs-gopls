use std::fs;
use std::path::Path;

use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, NumberOrString, Position, Range, TextEdit, Uri,
};
use serde_json::{Value, json};

use super::{GofmtCache, format_document, formatting::formatting_edits};
use crate::go_environment::GoEnvironment;
use crate::protocol::path_uri;
use crate::semantic::Engine;
use crate::semantic::diagnostics::{
    DiagnosticCode, DiagnosticFixData, DiagnosticItem, DiagnosticReport, DiagnosticSeverity,
    analyze_document, snapshot_key,
};
use crate::semantic::snapshot::{DocumentSnapshot, EngineSnapshot};
use crate::test_support::TestProject;
use crate::workspace::Workspace;

fn fixture(name: &str) -> String {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tools/fixtures/actions");
    fs::read_to_string(dir.join(name))
        .unwrap()
        .replace("\r\n", "\n")
}

fn open(path: &Path, text: &str) -> Engine {
    let mut engine = Engine::new(path.parent().unwrap().to_owned());
    engine.open(path.to_owned(), text.to_owned(), 7).unwrap();
    engine
}

fn apply(document: &DocumentSnapshot<'_>, edits: &[TextEdit]) -> String {
    let mut result = document.text().to_owned();
    for pair in edits.windows(2) {
        assert!(pair[0].range.end <= pair[1].range.start);
    }
    for edit in edits.iter().rev() {
        let start = document.offset(edit.range.start).unwrap();
        let end = document.offset(edit.range.end).unwrap();
        assert_eq!(document.byte_range(start..end), edit.range);
        result.replace_range(start..end, &edit.new_text);
    }
    result
}

fn format(document: &DocumentSnapshot<'_>, workdir: Option<&Path>) -> Vec<TextEdit> {
    format_document(
        document,
        workdir,
        &GoEnvironment::capture(),
        &mut GofmtCache::default(),
    )
    .unwrap()
}

#[test]
fn formats_the_unsaved_fixture_and_returns_no_edits_for_the_clean_fixture() {
    let project = TestProject::new();
    let path = project.write("buffer.go", "package disk\n");
    let clean = fixture("formatting_clean.go");
    let mut engine = open(&path, &fixture("formatting_unformatted.go"));
    let document = engine.snapshot().document(&path).unwrap().unwrap();
    let edits = format(&document, Some(&project.root));
    assert_eq!(edits.len(), 1);
    assert_eq!(
        apply(&document, &edits),
        clean.replace("FormattedEntry", "UnformattedEntry")
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "package disk\n");
    engine.open(path.clone(), clean, 8).unwrap();
    let document = engine.snapshot().document(&path).unwrap().unwrap();
    assert!(format(&document, None).is_empty());
}

#[test]
fn formatting_uses_exact_utf16_columns_and_keeps_crlf_atomic() {
    let project = TestProject::new();
    let path = project.root.join("buffer.go");
    for (before, after, start, end, replacement) in [
        (
            "package p\r\n\r\nvar _ = \"😀\"+\"ok\"\r\n",
            "package p\r\n\r\nvar _ = \"😀\" + \"ok\"\r\n",
            Position::new(2, 12),
            Position::new(2, 13),
            " + ",
        ),
        (
            "package p\n\nvar _ = 1\r\n",
            "package p\n\nvar _ = 1\n",
            Position::new(2, 9),
            Position::new(3, 0),
            "\n",
        ),
        (
            "package p\n\r\nvar _ = 1\r\n",
            "package p\r\n\r\nvar _ = 1\r\n",
            Position::new(0, 9),
            Position::new(1, 0),
            "\r\n",
        ),
    ] {
        let mut engine = open(&path, before);
        let document = engine.snapshot().document(&path).unwrap().unwrap();
        let edits = format(&document, None);
        assert_eq!(
            edits,
            [TextEdit::new(Range::new(start, end), replacement.into())]
        );
        assert_eq!(apply(&document, &edits), after);
    }
}

#[test]
fn tree_errors_gofmt_parse_errors_and_empty_output_protect_the_buffer() {
    let project = TestProject::new();
    let path = project.root.join("buffer.go");
    let mut engine = open(&path, "package p\nfunc f( {\n");
    let document = engine.snapshot().document(&path).unwrap().unwrap();
    assert!(document.root_node().has_error());
    assert!(format(&document, Some(&project.root.join("absent"))).is_empty());

    engine
        .open(
            path.clone(),
            "package p\n//line virtual.go:10\nvar _ = \"\\uD800\"\n".into(),
            8,
        )
        .unwrap();
    let document = engine.snapshot().document(&path).unwrap().unwrap();
    assert!(!document.root_node().has_error());
    assert!(format(&document, None).is_empty());
    for empty in ["", " \n"] {
        assert!(formatting_edits(&document, empty.into()).is_err());
    }

    engine.open(path.clone(), " \n\r\n\r\n".into(), 9).unwrap();
    let document = engine.snapshot().document(&path).unwrap().unwrap();
    assert!(format(&document, None).is_empty());
    assert_eq!(
        apply(
            &document,
            &formatting_edits(&document, String::new()).unwrap()
        ),
        ""
    );
}

fn package(path: &str, name: &str) -> Value {
    json!({"ImportPath": path, "Name": name, "GoFiles": ["pkg.go"]})
}

fn with_actions(
    text: &str,
    packages: &[Value],
    check: impl FnOnce(&mut EngineSnapshot<'_>, &DocumentSnapshot<'_>, &Uri),
) {
    let project = TestProject::new();
    let path = project.root.join("actions.go");
    let mut engine = open(&path, text);
    let mut listing = json!({
        "Dir": project.root, "ImportPath": "example.test/actions", "Name": "actions",
        "GoFiles": ["actions.go"], "Module": {"Path": "example.test/actions", "Main": true},
    })
    .to_string();
    for mut pkg in packages.iter().cloned() {
        let directory = project
            .root
            .join("packages")
            .join(pkg["ImportPath"].as_str().unwrap());
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("pkg.go"),
            format!("package {}\n", pkg["Name"].as_str().unwrap()),
        )
        .unwrap();
        pkg["Dir"] = json!(directory);
        listing.push_str(&pkg.to_string());
    }
    let graph = Workspace::from_test_listing(std::slice::from_ref(&project.root), &listing, false);
    assert!(engine.install_workspace(engine.workspace_load(), graph));
    let mut snapshot = engine.snapshot();
    let document = snapshot.document(&path).unwrap().unwrap();
    check(&mut snapshot, &document, &path_uri(&path).unwrap());
}

fn report(
    snapshot: &EngineSnapshot<'_>,
    document: &DocumentSnapshot<'_>,
    items: Vec<DiagnosticItem>,
) -> DiagnosticReport {
    let key = snapshot_key(snapshot, document).unwrap();
    let is_partial = key.partial;
    DiagnosticReport::new(key, items, is_partial)
}

fn token(document: &DocumentSnapshot<'_>, text: &str) -> Range {
    let start = document.text().find(text).unwrap();
    document.byte_range(start..start + text.len())
}

fn diagnostic(code: DiagnosticCode, range: Range, fix_data: DiagnosticFixData) -> DiagnosticItem {
    DiagnosticItem {
        fix_data,
        ..DiagnosticItem::new(code, DiagnosticSeverity::Error, range, code.as_str())
    }
}

fn unused_import(document: &DocumentSnapshot<'_>, path: &str) -> DiagnosticItem {
    let spec_range = token(document, &format!("\"{path}\""));
    diagnostic(
        DiagnosticCode::ImportUnused,
        spec_range,
        DiagnosticFixData::RemoveUnusedImport {
            import_path: path.into(),
            spec_range,
        },
    )
}

fn missing_import(document: &DocumentSnapshot<'_>, name: &str) -> DiagnosticItem {
    let ident_range = token(document, name);
    diagnostic(
        DiagnosticCode::NameUnresolved,
        ident_range,
        DiagnosticFixData::ImportMissingPackage {
            qualifier: name.into(),
            ident_range,
        },
    )
}

fn action(value: &CodeActionOrCommand) -> &CodeAction {
    let CodeActionOrCommand::CodeAction(action) = value else {
        panic!("expected an inline code action")
    };
    action
}

fn action_edits<'a>(action: &'a CodeAction, uri: &Uri) -> &'a [TextEdit] {
    let changes = action.edit.as_ref().unwrap().changes.as_ref().unwrap();
    assert_eq!(changes.len(), 1);
    assert!(action.command.is_none());
    &changes[uri]
}

fn code_actions(
    snapshot: &mut EngineSnapshot<'_>,
    document: &DocumentSnapshot<'_>,
    uri: &Uri,
    range: Range,
    only: Option<&[CodeActionKind]>,
    report: Option<&DiagnosticReport>,
) -> anyhow::Result<Vec<CodeActionOrCommand>> {
    super::code_actions(snapshot, document, uri, range, only, report, &[])
}

#[test]
fn organize_imports_groups_the_messy_fixture_and_preserves_comments() {
    let before = fixture("imports_messy.go");
    with_actions(
        &before,
        &[
            package("os", "os"),
            package("example.test/dependency", "dependency"),
        ],
        |snapshot, document, uri| {
            let report = report(snapshot, document, vec![unused_import(document, "os")]);
            let organized = concat!(
                "import (\n\t\"fmt\"\n\t\"strings\"\n\t_ \"time/tzdata\"\n\n",
                "\t// The shared prefix keeps labels consistent across commands.\n\t\"example.test/dependency\"\n\n",
                "\t\"example.test/actions/helper\" // Matches the labels used by built-in commands.\n)",
            );
            let expected = before.replace(
                &before[before.find("import (").unwrap()..before.find("\n)").unwrap() + 2],
                organized,
            );
            for kind in [
                CodeActionKind::SOURCE,
                CodeActionKind::SOURCE_ORGANIZE_IMPORTS,
            ] {
                let actions = code_actions(
                    snapshot,
                    document,
                    uri,
                    Range::default(),
                    Some(&[kind]),
                    Some(&report),
                )
                .unwrap();
                assert_eq!(actions.len(), 1);
                let action = action(&actions[0]);
                assert_eq!(action.title, "Organize Imports");
                assert_eq!(action.kind, Some(CodeActionKind::SOURCE_ORGANIZE_IMPORTS));
                assert_eq!(action.diagnostics, None);
                assert_eq!(apply(document, action_edits(action, uri)), expected);
            }
            let actions =
                code_actions(snapshot, document, uri, Range::default(), None, None).unwrap();
            assert_eq!(actions.len(), 1);
            assert_eq!(
                apply(document, action_edits(action(&actions[0]), uri)),
                expected
            );
        },
    );
}

#[test]
fn organizing_the_cgo_fixture_leaves_the_preamble_outside_the_edits() {
    let before = fixture("imports_cgo.go").replace('\n', "\r\n");
    with_actions(&before, &[], |snapshot, document, uri| {
        let report = report(
            snapshot,
            document,
            vec![unused_import(document, "fmt"), unused_import(document, "C")],
        );
        let actions = code_actions(
            snapshot,
            document,
            uri,
            Range::default(),
            None,
            Some(&report),
        )
        .unwrap();
        assert_eq!(actions.len(), 1);
        let edits = action_edits(action(&actions[0]), uri);
        assert!(
            edits
                .iter()
                .all(|edit| edit.range.start > token(document, "import \"C\"").end)
        );
        assert_eq!(
            apply(document, edits),
            before.replace(
                "import (\r\n\t\"fmt\"\r\n\t\"unsafe\"\r\n)",
                "import \"unsafe\""
            )
        );
    });
}

#[test]
fn fixture_quickfixes_edit_the_selected_diagnostic_and_uri() {
    let before = fixture("quickfix_cases.go").replace('\n', "\r\n");
    with_actions(
        &before,
        &[package("example.test/actions/helper", "helper")],
        |snapshot, document, uri| {
            let unused = diagnostic(
                DiagnosticCode::VarUnused,
                token(document, "unused := 42"),
                DiagnosticFixData::ReplaceUnusedVarWithBlank {
                    name: "unused".into(),
                    ident_range: token(document, "unused"),
                },
            );
            let operator_range = Range::new(Position::new(17, 7), Position::new(17, 9));
            let reassign = diagnostic(
                DiagnosticCode::ShortVarNoNewVars,
                token(document, "value := value + 1"),
                DiagnosticFixData::ChangeShortDeclToAssign { operator_range },
            );
            let cases = [
                (
                    unused_import(document, "strings"),
                    "Remove unused import \"strings\"",
                    "import \"strings\"\r\n\r\n",
                    "",
                ),
                (
                    missing_import(document, "fmt"),
                    "Import \"fmt\"",
                    "import \"strings\"",
                    "import (\r\n\t\"fmt\"\r\n\t\"strings\"\r\n)",
                ),
                (
                    missing_import(document, "helper"),
                    "Import \"example.test/actions/helper\"",
                    "import \"strings\"",
                    "import (\r\n\t\"strings\"\r\n\r\n\t\"example.test/actions/helper\"\r\n)",
                ),
                (
                    unused,
                    "Replace unused variable 'unused' with '_'",
                    "unused := 42",
                    "_ = 42",
                ),
                (
                    reassign,
                    "Replace ':=' with '='",
                    "value := value + 1",
                    "value = value + 1",
                ),
            ];
            let report = report(
                snapshot,
                document,
                cases.iter().map(|(item, ..)| item.clone()).collect(),
            );
            for (item, title, old, new) in cases {
                let cursor = Range::new(item.range.start, item.range.start);
                let actions = code_actions(
                    snapshot,
                    document,
                    uri,
                    cursor,
                    Some(&[CodeActionKind::QUICKFIX]),
                    Some(&report),
                )
                .unwrap();
                assert_eq!(actions.len(), 1, "{title}");
                let action = action(&actions[0]);
                assert_eq!(action.title, title);
                assert_eq!(action.kind, Some(CodeActionKind::QUICKFIX));
                assert_eq!(action.diagnostics, Some(vec![item.to_lsp()]));
                assert_eq!(
                    apply(document, action_edits(action, uri)),
                    before.replacen(old, new, 1)
                );
            }
        },
    );
}

#[test]
fn unused_variable_fix_preserves_short_declarations_with_another_name() {
    for (declaration, fixed, edit_count) in [
        (
            "used, unused := f(); _ = used",
            "used, _ := f(); _ = used",
            1,
        ),
        ("_, /* first */ unused := f()", "_, /* first */ _ = f()", 2),
    ] {
        let text = format!(
            "package actions\nfunc f() (int, int) {{ return 1, 2 }}\nfunc g() {{ {declaration} }}\n"
        );
        with_actions(&text, &[], |snapshot, document, uri| {
            let range = token(document, "unused");
            let item = diagnostic(
                DiagnosticCode::VarUnused,
                range,
                DiagnosticFixData::ReplaceUnusedVarWithBlank {
                    name: "unused".into(),
                    ident_range: range,
                },
            );
            let report = report(snapshot, document, vec![item]);
            let actions =
                code_actions(snapshot, document, uri, range, None, Some(&report)).unwrap();
            let edits = action_edits(action(&actions[0]), uri);
            assert_eq!(edits.len(), edit_count);
            assert_eq!(apply(document, edits), text.replace(declaration, fixed));
        });
    }
}

#[test]
fn only_filters_on_kind_boundaries_and_stale_reports_cannot_remove_imports() {
    with_actions(
        &fixture("quickfix_cases.go"),
        &[],
        |snapshot, document, uri| {
            let item = unused_import(document, "strings");
            let range = item.range;
            let mut report = report(snapshot, document, vec![item]);
            for kinds in [
                vec![],
                vec![CodeActionKind::REFACTOR],
                vec!["quick".into()],
                vec!["source.organize".into()],
                vec!["quickfix.child".into(), CodeActionKind::SOURCE_FIX_ALL],
            ] {
                assert!(
                    code_actions(snapshot, document, uri, range, Some(&kinds), Some(&report))
                        .unwrap()
                        .is_empty()
                );
            }
            let all = code_actions(snapshot, document, uri, range, None, Some(&report)).unwrap();
            assert_eq!(
                all.iter().map(|a| &action(a).kind).collect::<Vec<_>>(),
                [
                    &Some(CodeActionKind::SOURCE_ORGANIZE_IMPORTS),
                    &Some(CodeActionKind::QUICKFIX)
                ]
            );
            let end = Range::new(range.end, range.end);
            assert!(
                code_actions(
                    snapshot,
                    document,
                    uri,
                    end,
                    Some(&[CodeActionKind::QUICKFIX]),
                    Some(&report)
                )
                .unwrap()
                .is_empty()
            );
            assert!(
                code_actions(snapshot, document, uri, range, None, None)
                    .unwrap()
                    .is_empty()
            );

            let mut wrong_version = report.snapshot.clone();
            wrong_version.version = Some(6);
            let mut wrong_scope = report.snapshot.clone();
            wrong_scope.partial = !wrong_scope.partial;
            let mut wrong_path = report.snapshot.clone();
            wrong_path.path_key = "other.go".into();
            for stale in [wrong_version, wrong_scope, wrong_path] {
                report.snapshot = stale;
                assert!(
                    code_actions(snapshot, document, uri, range, None, Some(&report))
                        .unwrap()
                        .is_empty()
                );
            }
        },
    );
}

#[test]
fn import_candidates_are_visible_ranked_deduplicated_and_capped() {
    let mut packages = vec![
        package("fmt", "fmt"),
        package("example.test/actions/codec/v2", "fmt"),
        package("example.test/actions/internal/fmt", "fmt"),
        package("example.test/action/internal/fmt", "fmt"),
        package("example.test/actions/sub/internal/fmt", "fmt"),
        package("internal/fmt", "fmt"),
        package(
            "example.test/actions/testonly [example.test/actions.test]",
            "fmt",
        ),
        package("example.test/actions/vendor/fmt", "fmt"),
        package("example.test/actions/cmd", "main"),
        package("example.test/actions/check_test", "fmt_test"),
    ];
    let mut importer = package("example.test/actions/importer", "fmt");
    importer["Imports"] = json!(["example.test/actions"]);
    packages.push(importer);
    for i in (0..9).rev() {
        packages.push(package(&format!("a.test/fmt{i}"), "fmt"));
    }
    let text = "package actions\nfunc f() { fmt.Print(); fmt.Print(); main.F(); fmt_test.F(); actions.F() }\n";
    with_actions(text, &packages, |snapshot, document, uri| {
        let first = missing_import(document, "fmt");
        let start = text.rfind("fmt.Print").unwrap();
        let second = document.byte_range(start..start + 3);
        let mut duplicate = first.clone();
        duplicate.range = second;
        let report = report(
            snapshot,
            document,
            vec![
                first.clone(),
                duplicate,
                missing_import(document, "main"),
                missing_import(document, "fmt_test"),
                missing_import(document, "actions"),
            ],
        );
        let actions = code_actions(
            snapshot,
            document,
            uri,
            document.byte_range(0..text.len()),
            None,
            Some(&report),
        )
        .unwrap();
        let expected = [
            "fmt",
            "example.test/actions/codec/v2",
            "example.test/actions/internal/fmt",
            "a.test/fmt0",
            "a.test/fmt1",
            "a.test/fmt2",
            "a.test/fmt3",
            "a.test/fmt4",
        ];
        assert_eq!(actions.len(), expected.len());
        for (value, path) in actions.iter().zip(expected) {
            let action = action(value);
            assert_eq!(action.title, format!("Import \"{path}\""));
            assert_eq!(action.diagnostics, Some(vec![first.to_lsp()]));
            assert_eq!(
                apply(document, action_edits(action, uri)),
                text.replace(
                    "package actions\n",
                    &format!("package actions\n\nimport \"{path}\"\n\n")
                )
            );
        }
    });
}

#[test]
fn standard_imports_work_before_a_graph_load_and_skip_existing_imports() {
    let project = TestProject::new();
    let path = project.root.join("buffer.go");
    let text =
        "package p\nimport \"encoding/json\"\nfunc f() { _ = template.New; _ = json.Marshal }\n";
    let mut engine = open(&path, text);
    let mut snapshot = engine.snapshot();
    let document = snapshot.document(&path).unwrap().unwrap();
    let uri = path_uri(&path).unwrap();
    let report = report(
        &snapshot,
        &document,
        vec![
            missing_import(&document, "template"),
            missing_import(&document, "json"),
        ],
    );
    let actions = code_actions(
        &mut snapshot,
        &document,
        &uri,
        document.byte_range(0..text.len()),
        Some(&[CodeActionKind::QUICKFIX]),
        Some(&report),
    )
    .unwrap();
    assert_eq!(actions.len(), 2);
    for (value, path) in actions.iter().zip(["html/template", "text/template"]) {
        let action = action(value);
        assert_eq!(action.title, format!("Import \"{path}\""));
        assert_eq!(
            apply(&document, action_edits(action, &uri)),
            text.replace(
                "import \"encoding/json\"",
                &format!("import (\n\t\"encoding/json\"\n\t\"{path}\"\n)")
            )
        );
    }
}

#[test]
fn an_unsafe_import_header_still_allows_an_independent_utf16_quickfix() {
    let text =
        "package actions\r\nimport @\r\nfunc f() { s := \"😀\"; unused := fmt.Sprint(s) }\r\n";
    with_actions(text, &[], |snapshot, document, uri| {
        let range = token(document, "unused");
        assert_eq!(range.start, Position::new(2, 22));
        let item = diagnostic(
            DiagnosticCode::VarUnused,
            range,
            DiagnosticFixData::ReplaceUnusedVarWithBlank {
                name: "unused".into(),
                ident_range: range,
            },
        );
        let report = report(
            snapshot,
            document,
            vec![item, missing_import(document, "fmt")],
        );
        let actions = code_actions(
            snapshot,
            document,
            uri,
            document.byte_range(0..text.len()),
            None,
            Some(&report),
        )
        .unwrap();
        assert_eq!(actions.len(), 1);
        let edits = action_edits(action(&actions[0]), uri);
        assert_eq!(edits.len(), 2);
        assert_eq!(apply(document, edits), text.replace("unused :=", "_ ="));
    });
}

#[test]
fn organize_imports_matches_complete_diagnostics_and_keeps_live_aliases() {
    let text = concat!(
        "package actions\nimport (\n",
        "  dead \"example.test/shared\"\n  live \"example.test/shared\"\n",
        "  pathalias \"example.test/pathalias\"\n  \"example.test/types\"\n",
        "  . \"example.test/dot\"\n  _ \"example.test/blank\"\n  \"fmt\"\n)\n",
        "type T types.T\n",
        "func f() { fmt := struct{ n int }{}; _, _, _ = fmt.n, live.X, pathalias.X }\n",
    );
    let packages = [
        package("example.test/shared", "shared"),
        package("example.test/pathalias", "actualname"),
        package("example.test/types", "types"),
        package("example.test/dot", "dot"),
        package("example.test/blank", "blank"),
        package("fmt", "fmt"),
    ];
    with_actions(text, &packages, |snapshot, document, uri| {
        let kinds = [CodeActionKind::SOURCE_ORGANIZE_IMPORTS];
        let local = code_actions(
            snapshot,
            document,
            uri,
            Range::default(),
            Some(&kinds),
            None,
        )
        .unwrap();
        let report = analyze_document(snapshot, document).unwrap();
        let complete = code_actions(
            snapshot,
            document,
            uri,
            Range::default(),
            Some(&kinds),
            Some(&report),
        )
        .unwrap();
        assert_eq!(local, complete);
        assert_eq!(local.len(), 1);
        let edited = apply(document, action_edits(action(&local[0]), uri));
        assert!(!edited.contains("dead") && !edited.contains("\"fmt\""));
        for import in [
            "live \"example.test/shared\"",
            "pathalias \"example.test/pathalias\"",
            "\"example.test/types\"",
            ". \"example.test/dot\"",
            "_ \"example.test/blank\"",
        ] {
            assert!(
                edited.contains(import),
                "{import} should be preserved in {edited}"
            );
        }
    });
}

#[test]
fn local_variable_actions_require_current_evidence_and_exact_utf16_ranges() {
    let before =
        "package actions\r\nfunc f() { x := \"😀\"; x, unused := x, 2; _ = x; x := x }\r\n";
    with_actions(before, &[], |snapshot, document, uri| {
        let report = analyze_document(snapshot, document).unwrap();
        let variable = report
            .items
            .iter()
            .find(|item| item.code == DiagnosticCode::VarUnused)
            .unwrap();
        let operator = report
            .items
            .iter()
            .find(|item| item.code == DiagnosticCode::ShortVarNoNewVars)
            .unwrap();
        let only = [CodeActionKind::QUICKFIX];
        let mut hint = variable.to_lsp();
        hint.data = Some(json!({"edit": "untrusted", "version": 1}));
        let cases = [
            (hint.clone(), before.replace("x, unused :=", "x, _ =")),
            (operator.to_lsp(), before.replace("x := x }", "x = x }")),
        ];
        for (hint, expected) in cases {
            let actions = super::code_actions(
                snapshot,
                document,
                uri,
                hint.range,
                Some(&only),
                None,
                &[hint.clone()],
            )
            .unwrap();
            assert_eq!(actions.len(), 1, "{hint:?}");
            let action = action(&actions[0]);
            let mut original = hint;
            original.data = None;
            assert_eq!(action.diagnostics, Some(vec![original]));
            assert_eq!(action.kind, Some(CodeActionKind::QUICKFIX));
            assert_eq!(apply(document, action_edits(action, uri)), expected);
        }
        let mut unavailable = variable.clone();
        unavailable.fix_data = DiagnosticFixData::None;
        let report = self::report(snapshot, document, vec![unavailable]);
        let fixes = code_actions(
            snapshot,
            document,
            uri,
            variable.range,
            Some(&only),
            Some(&report),
        )
        .unwrap();
        assert_eq!(fixes.len(), 1);

        let mut unknown = hint.clone();
        unknown.code = Some(NumberOrString::String("unknown".into()));
        let mut wrong_source = hint.clone();
        wrong_source.source = Some("other".into());
        let mut wrong_message = hint.clone();
        wrong_message.message = "forged".into();
        let mut surrogate = hint.clone();
        let emoji = before.find('😀').unwrap();
        let position = document.byte_range(emoji..emoji + '😀'.len_utf8()).start;
        surrogate.range.start = Position::new(position.line, position.character + 1);
        let mut out_of_bounds = hint.clone();
        out_of_bounds.range.end.character = u32::MAX;
        for invalid in [
            unknown,
            wrong_source,
            wrong_message,
            surrogate,
            out_of_bounds,
        ] {
            assert!(
                super::code_actions(
                    snapshot,
                    document,
                    uri,
                    document.byte_range(0..before.len()),
                    Some(&only),
                    None,
                    &[invalid]
                )
                .unwrap()
                .is_empty()
            );
        }
    });
}

#[test]
fn unused_var_fix_does_not_orphan_existing_assignments() {
    let text = "package actions\nfunc f() { unused := 1; unused = 2 }\n";
    with_actions(text, &[], |snapshot, document, uri| {
        let report = analyze_document(snapshot, document).unwrap();
        let item = report
            .items
            .iter()
            .find(|item| item.code == DiagnosticCode::VarUnused)
            .unwrap();
        let only = [CodeActionKind::QUICKFIX];
        for cached in [Some(&report), None] {
            assert!(
                super::code_actions(
                    snapshot,
                    document,
                    uri,
                    item.range,
                    Some(&only),
                    cached,
                    &[item.to_lsp()]
                )
                .unwrap()
                .is_empty()
            );
        }
    });
}
