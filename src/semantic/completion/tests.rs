use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use lsp_types::{
    CompletionItem, CompletionItemKind as Kind, CompletionResponse, CompletionTextEdit, Position,
    TextEdit,
};
use serde_json::{Value, json};

use crate::semantic::source::{LineIndex, Source, node_text};
use crate::test_support::{TestProject, TestSession};
use crate::workspace::Workspace;

use super::cursor::{LiteralMode, Probe, Site};
use super::*;

fn marked(text: &str) -> (String, Position) {
    let (left, right) = text.split_once('¦').expect("source needs a cursor");
    let line = left.bytes().filter(|byte| *byte == b'\n').count();
    let column = left.rsplit('\n').next().unwrap().encode_utf16().count();
    (
        format!("{left}{right}"),
        Position::new(line as u32, column as u32),
    )
}

fn install(engine: &mut Engine, workspace: Workspace, use_sdk: bool) {
    assert!(engine.install_workspace(engine.workspace_load(), workspace));
    if !use_sdk {
        engine.completion_imports.use_empty_sdk(
            engine.state.completion_graph_generation,
            engine.state.workspace.as_ref().unwrap(),
        );
    }
}

fn engine() -> (TestProject, Engine, PathBuf) {
    let project = TestProject::new();
    let path = project.write("sample.go", "package sample\n");
    let mut engine = Engine::new(project.root.clone());
    install(&mut engine, project.workspace(&["sample.go"], false), false);
    (project, engine, path)
}

fn open(engine: &mut Engine, path: &Path, text: &str, version: i32) -> (String, Position) {
    let (text, position) = marked(text);
    engine.open(path.to_owned(), text.clone(), version).unwrap();
    (text, position)
}

fn items(
    engine: &mut Engine,
    path: &Path,
    position: Position,
    caps: &CompletionClientCapabilities,
) -> Vec<CompletionItem> {
    let result = match engine.complete(path, position, None, caps).unwrap() {
        QueryProgress::Ready(result) => result,
        QueryProgress::Pending(load) => panic!("unexpected workspace load {load:?}"),
        QueryProgress::Searching(mut task) => {
            while !task.advance(engine).unwrap() {}
            task.finish()
        }
    };
    let CompletionResponse::Array(items) = result else {
        panic!("expected a complete response")
    };
    items
}

fn complete(engine: &mut Engine, path: &Path, text: &str) -> Vec<CompletionItem> {
    let (_, position) = open(engine, path, text, 1);
    items(
        engine,
        path,
        position,
        &CompletionClientCapabilities::default(),
    )
}

fn item<'a>(items: &'a [CompletionItem], label: &str) -> &'a CompletionItem {
    items
        .iter()
        .find(|item| item.label == label)
        .unwrap_or_else(|| panic!("missing {label:?} in {:?}", labels(items)))
}

fn labels(items: &[CompletionItem]) -> Vec<&str> {
    items.iter().map(|item| item.label.as_str()).collect()
}

fn assert_labels(items: &[CompletionItem], present: &[&str], absent: &[&str]) {
    let labels = labels(items);
    for name in present {
        assert!(labels.contains(name), "missing {name:?} in {labels:?}");
    }
    for name in absent {
        assert!(!labels.contains(name), "unexpected {name:?} in {labels:?}");
    }
}

fn edit(item: &CompletionItem) -> TextEdit {
    match item.text_edit.as_ref().unwrap() {
        CompletionTextEdit::Edit(edit) => edit.clone(),
        CompletionTextEdit::InsertAndReplace(edit) => {
            TextEdit::new(edit.replace, edit.new_text.clone())
        }
    }
}

fn apply(text: &str, item: &CompletionItem) -> String {
    let lines = LineIndex::new(text);
    let mut edits = item.additional_text_edits.clone().unwrap_or_default();
    edits.push(edit(item));
    let mut edits: Vec<_> = edits
        .into_iter()
        .map(|edit| {
            (
                lines.offset(text, edit.range.start).unwrap()
                    ..lines.offset(text, edit.range.end).unwrap(),
                edit.new_text,
            )
        })
        .collect();
    edits.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
    let mut text = text.to_owned();
    let mut last = text.len();
    for (range, insert) in edits {
        assert!(range.end <= last);
        last = range.start;
        text.replace_range(range, &insert);
    }
    text
}

fn assert_go_syntax(engine: &mut Engine, text: String) {
    let source = Source::parse(
        engine.state.snapshot(false).parser(),
        PathBuf::new(),
        text,
        None,
    )
    .unwrap();
    let root = source.tree.root_node();
    assert!(!root.has_error(), "{}\n{}", source.text, root.to_sexp());
}

#[test]
fn unfinished_selectors_preserve_the_receiver_at_eof_before_a_closer_and_before_statements() {
    let (_project, mut engine, path) = engine();
    let header = "package sample\ntype T struct{ Bar int; Baz int }\nfunc f(foo T) {\n";
    for (suffix, expected) in [
        ("_ = foo.¦", "Bar"),
        ("_ = foo.¦\n}", "Bar"),
        ("println(foo.¦)\n}", "Bar"),
        ("_ = foo.¦\nbar()\n}", "Bar"),
        ("_ = foo.Ba¦r\n}", "Bar"),
        ("_ = foo.¦\r\nbar()\r\n}", "Bar"),
    ] {
        let text = format!("{header}{suffix}");
        let (buffer, position) = open(&mut engine, &path, &text, 1);
        let items = items(
            &mut engine,
            &path,
            position,
            &CompletionClientCapabilities::default(),
        );
        let result = item(&items, expected);
        assert_eq!(result.kind, Some(Kind::FIELD), "{text}");
        let applied = apply(&buffer, result);
        assert!(applied.contains("foo.Bar"), "{applied}");
        assert_eq!(applied.contains("bar()"), text.contains("bar()"));
        assert!(!applied.contains("foo.Barr"));
    }
}

#[test]
fn computes_utf16_replacements_in_the_middle_of_a_selector() {
    let (_project, mut engine, path) = engine();
    let source =
        "package sample\ntype T struct{ 名称 int }\nfunc f(foo T) { _ = \"😀中\"; _ = foo.名¦称 }";
    let (buffer, position) = open(&mut engine, &path, source, 1);
    let items = items(
        &mut engine,
        &path,
        position,
        &CompletionClientCapabilities::default(),
    );
    let edit = edit(item(&items, "名称"));
    assert_eq!(edit.range.start, Position::new(2, position.character - 1));
    assert_eq!(edit.range.end, Position::new(2, position.character + 1));
    assert_eq!(edit.new_text, "名称");
    assert_eq!(
        position.character as usize,
        buffer
            .lines()
            .nth(2)
            .unwrap()
            .split("称 }")
            .next()
            .unwrap()
            .encode_utf16()
            .count()
    );
}

#[test]
fn syntax_only_contexts_complete_without_an_installed_graph() {
    let project = TestProject::new();
    let path = project.root.join("go-name/future_test.go");
    project.write("go-name/old.go", "// 包\npackage real\n");
    let mut engine = Engine::new(project.root.clone());
    let caps = CompletionClientCapabilities::default();
    for (text, present) in [
        ("package ¦", &["main", "real", "real_test", "go_name"][..]),
        (
            "package sample\n¦",
            &["package", "import", "func", "type", "const", "var"][..],
        ),
        (
            "package sample\nfunc f() { goto ¦\nAfter: return }",
            &["After"][..],
        ),
    ] {
        let (_, position) = open(&mut engine, &path, text, 1);
        let QueryProgress::Ready(CompletionResponse::Array(items)) =
            engine.complete(&path, position, None, &caps).unwrap()
        else {
            panic!("syntax-only completion waited for graph at {text}");
        };
        assert_labels(&items, present, &[]);
    }
}

#[test]
fn suppresses_comments_strings_runes_and_numbers_even_in_malformed_source() {
    let project = TestProject::new();
    let path = project.root.join("sample.go");
    let mut engine = Engine::new(project.root.clone());
    let caps = CompletionClientCapabilities::default();
    for text in [
        "package sample\n// abc¦",
        "package sample\n/* abc¦ def */",
        "package sample\n/* abc\n def¦",
        "package sample\nvar _ = \"abc¦\"",
        "package sample\nvar _ = \"abc¦",
        "package sample\nvar _ = `abc\n¦def`",
        "package sample\nvar _ = `abc\n¦def",
        "package sample\nvar _ = '\\n¦'",
        "package sample\nvar _ = 42¦",
        "package sample\nvar _ = 3.14¦",
        "package sample\nvar _ = 2.5i¦",
        "package sample\nvar _ = 1.¦",
    ] {
        let (_, position) = open(&mut engine, &path, text, 1);
        assert!(
            matches!(
                engine.complete(&path, position, None, &caps).unwrap(),
                QueryProgress::Ready(CompletionResponse::Array(items)) if items.is_empty()
            ),
            "{text}"
        );
    }
}

#[test]
fn classifies_incomplete_imports_types_struct_literals_and_qualified_types() {
    let (_project, mut engine, path) = engine();
    for (text, context) in [
        ("package sample\nimport \"fmt/¦", "import"),
        ("package sample\nimport \"¦", "import"),
        ("package sample\nimport (\n  alias \"ab/¦cd\"\n)", "import"),
        ("package sample\nfunc f(a ¦", "type"),
        ("package sample\nvar t pkg.¦", "qualified type"),
        ("package sample\nvar _ = S{¦", "both"),
        (
            "package sample\nfunc f() {\n\t_ = S{¦\n\t_ = S{1, 2}\n}\n",
            "both",
        ),
        ("package sample\nvar _ = S{Fi¦: 1}", "key"),
        ("package sample\nvar _ = S{Fi: 1¦}", "suppressed"),
        ("package sample\nvar _ = S{Fi: ¦}", "value"),
        ("package sample\nvar _ = S{Fi: input.(T¦)}", "type"),
    ] {
        let (_, position) = open(&mut engine, &path, text, 1);
        let mut snapshot = engine.snapshot();
        let document = snapshot.document(&path).unwrap().unwrap();
        let probe = Probe::new(&document, snapshot.parser(), position, None, None)
            .unwrap()
            .unwrap();
        let source = probe.source(&document);
        let actual = match probe.site(source, document.text(), None).unwrap() {
            Site::ImportPath { .. } => "import",
            Site::TypePosition { .. } => "type",
            Site::Selector {
                types_only: true,
                receiver,
                ..
            } => {
                assert_eq!(node_text(receiver, &source.text), "pkg");
                "qualified type"
            }
            Site::StructLiteral {
                mode: LiteralMode::KeysOnly,
                ..
            } => "key",
            Site::StructLiteral {
                mode: LiteralMode::ValuesOnly,
                ..
            } => "value",
            Site::StructLiteral {
                mode: LiteralMode::KeysAndValues,
                ..
            } => "both",
            Site::Suppressed => "suppressed",
            _ => "other",
        };
        assert_eq!(
            actual,
            context,
            "{text}\n{}",
            source.tree.root_node().to_sexp()
        );
    }
}

fn listing(project: &TestProject, packages: &[Value], tests: bool) -> Workspace {
    Workspace::from_test_listing(
        std::slice::from_ref(&project.root),
        &packages
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
        tests,
    )
}

fn pkg(dir: impl AsRef<Path>, import_path: &str, name: &str, files: &[&str]) -> Value {
    json!({ "Dir": dir.as_ref(), "ImportPath": import_path, "Name": name, "GoFiles": files })
}

#[test]
fn unqualified_completion_obeys_shadowing_named_results_and_generic_scope() {
    let (_project, mut engine, path) = engine();
    let items = complete(
        &mut engine,
        &path,
        concat!(
            "package sample\n",
            "var Outer int\nvar len int\nfunc init() {}\n",
            "type Box[T any] struct{ V T }\n",
            "func (receiver *Box[T]) f(Param T) (Result string) {\n",
            "  Outer := 32\n  Available := Param\n  _ = 0\n",
            "  { Outer := \"inner\"; len := true; Self := ¦ }\n}",
        ),
    );
    assert_labels(
        &items,
        &[
            "receiver",
            "Param",
            "Result",
            "T",
            "Available",
            "Outer",
            "len",
            "append",
            "nil",
            "clear",
        ],
        &["_", "init", "Self", "iota"],
    );
    assert_eq!(items.iter().filter(|item| item.label == "Outer").count(), 1);
    assert_eq!(
        item(&items, "Outer").detail.as_deref(),
        Some("var Outer string")
    );
    assert_eq!(item(&items, "len").kind, Some(Kind::VARIABLE));
    assert_eq!(item(&items, "T").kind, Some(Kind::TYPE_PARAMETER));
}

#[test]
fn ordinary_in_package_tests_and_external_tests_have_distinct_package_scopes() {
    let project = TestProject::new();
    let normal = project.write("normal.go", "package p\nvar Production int\n");
    project.write("more.go", "package p\nvar Sibling int\n");
    let internal = project.write("internal_test.go", "package p\nvar Internal int\n");
    let external = project.write("external_test.go", "package p_test\nvar External int\n");
    project.write("extra_test.go", "package p_test\nvar ExternalSibling int\n");
    let mut regular = pkg(
        &project.root,
        "example.test/p",
        "p",
        &["normal.go", "more.go"],
    );
    regular["Module"] = json!({"Path": "example.test/p", "Main": true});
    let mut in_package = pkg(
        &project.root,
        "example.test/p [example.test/p.test]",
        "p",
        &["normal.go", "more.go", "internal_test.go"],
    );
    in_package["ForTest"] = json!("example.test/p");
    let mut outside = pkg(
        &project.root,
        "example.test/p_test [example.test/p.test]",
        "p_test",
        &["external_test.go", "extra_test.go"],
    );
    outside["ForTest"] = json!("example.test/p");
    let mut engine = Engine::new(project.root.clone());
    install(
        &mut engine,
        listing(&project, &[regular, in_package, outside], true),
        false,
    );
    let normal_items = complete(
        &mut engine,
        &normal,
        "package p\nvar Production int\nfunc f() { _ = ¦ }",
    );
    assert_labels(
        &normal_items,
        &["Production", "Sibling"],
        &["Internal", "External", "ExternalSibling"],
    );
    let internal_items = complete(
        &mut engine,
        &internal,
        "package p\nvar Internal int\nfunc f() { _ = ¦ }",
    );
    assert_labels(
        &internal_items,
        &["Production", "Sibling", "Internal"],
        &["External", "ExternalSibling"],
    );
    let external_items = complete(
        &mut engine,
        &external,
        "package p_test\nvar External int\nfunc f() { _ = ¦ }",
    );
    assert_labels(
        &external_items,
        &["External", "ExternalSibling"],
        &["Production", "Sibling", "Internal"],
    );

    let external_items = complete(
        &mut engine,
        &external,
        "package p_test\nimport \"example.test/¦\"",
    );
    let production = item(&external_items, "example.test/p");
    assert_eq!(edit(production).new_text, "example.test/p");
    assert!(
        !labels(&external_items)
            .iter()
            .any(|label| label.contains("p_test"))
    );
    let proposals = complete(&mut engine, &external, "package p_test\nvar _ = Prod¦\n");
    let production = item(&proposals, "Production");
    assert_eq!(edit(production).new_text, "p.Production");
    assert!(production.additional_text_edits.is_some());
}

#[test]
fn raw_imports_supply_aliased_qualifiers_and_dot_exports_beside_const_builtins() {
    let (project, mut engine, path) = engine();
    let dep = project.write(
        "dep/source.go",
        concat!(
            "package dependency\nvar Exported int\nvar lower int\n",
            "type PublicType int\nfunc PublicFunc() {}\n",
        ),
    );
    let dot = project.write(
        "dot/source.go",
        "package dot\nvar DotVisible int\nvar dotHidden int\n",
    );
    let ignored = project.write(
        "ignored/source.go",
        "package ignored\nvar BlankVisible int\n",
    );
    install(
        &mut engine,
        listing(
            &project,
            &[
                pkg(
                    &project.root,
                    "example.test/sample",
                    "sample",
                    &["sample.go"],
                ),
                pkg(
                    dep.parent().unwrap(),
                    "example.test/dependency",
                    "dependency",
                    &["source.go"],
                ),
                pkg(
                    dot.parent().unwrap(),
                    "example.test/dot",
                    "dot",
                    &["source.go"],
                ),
                pkg(
                    ignored.parent().unwrap(),
                    "example.test/ignored",
                    "ignored",
                    &["source.go"],
                ),
            ],
            false,
        ),
        false,
    );
    let header = concat!(
        "package sample\nimport alias \"example.test/dependency\"\n",
        "import . \"example.test/dot\"\nimport _ \"example.test/ignored\"\n"
    );
    let items = complete(&mut engine, &path, &format!("{header}const (\nA = ¦\n)"));
    assert_labels(
        &items,
        &["alias", "DotVisible", "iota"],
        &[
            "dependency",
            "dot",
            "dotHidden",
            "ignored",
            "BlankVisible",
            "init",
        ],
    );
    let items = complete(
        &mut engine,
        &path,
        &format!("{header}func f() {{ _ = alias.¦ }}"),
    );
    assert_labels(
        &items,
        &["Exported", "PublicType", "PublicFunc"],
        &["lower", "DotVisible"],
    );
    let items = complete(&mut engine, &path, &format!("{header}var y alias.¦"));
    assert_labels(&items, &["PublicType"], &["PublicFunc", "Exported"]);
    let items = complete(
        &mut engine,
        &path,
        &format!("{header}func f(alias int) {{ _ = alias.¦ }}"),
    );
    assert!(items.is_empty(), "{:?}", labels(&items));
    let items = complete(&mut engine, &path, &format!("{header}var _ = ¦"));
    assert_labels(&items, &["DotVisible"], &["iota"]);
}

#[test]
fn type_positions_filter_values_and_receivers_filter_interfaces_and_pointer_types() {
    let (_project, mut engine, path) = engine();
    let header = concat!(
        "package sample\ntype T struct{}\ntype P *T\ntype I interface{ M() }\n",
        "type Alias = T\nvar Value int\nfunc ValueFunc() {}\n",
    );
    let items = complete(&mut engine, &path, &format!("{header}func f(a ¦"));
    assert_labels(
        &items,
        &[
            "T",
            "I",
            "P",
            "Alias",
            "int",
            "struct",
            "interface",
            "map",
            "chan",
            "func",
        ],
        &["Value", "ValueFunc", "append", "iota"],
    );
    let items = complete(&mut engine, &path, &format!("{header}func (r ¦) M() {{}}"));
    assert_labels(
        &items,
        &["T"],
        &["I", "P", "Alias", "Value", "int", "struct", "interface"],
    );
    let items = complete(
        &mut engine,
        &path,
        &format!("{header}func f[G any]() {{ type Local string; LocalVal := 7; var v ¦ }}"),
    );
    assert_labels(&items, &["G", "Local", "T"], &["LocalVal", "ValueFunc"]);
    let items = complete(
        &mut engine,
        &path,
        &format!("{header}type Box[G any] struct{{}}\nvar box Box[¦]"),
    );
    assert_labels(&items, &["I", "T", "int"], &["Value", "ValueFunc"]);
}

#[test]
fn selectors_apply_generic_types_pointer_and_method_expression_rules() {
    let (_project, mut engine, path) = engine();
    let header = concat!(
        "package sample\n",
        "type Box[T any] struct { Field T }\n",
        "func (b Box[T]) Value(x T) T { return x }\n",
        "func (b *Box[T]) Pointer(x T) T { return x }\n",
        "type I interface{ Iface() }\n",
        "func fresh() Box[int] { return Box[int]{} }\n",
    );
    let cases = [
        (
            "func f(b Box[string]) { _ = b.¦ }",
            &["Field", "Value", "Pointer"][..],
            &["Iface"][..],
        ),
        (
            "func f(b *Box[string]) { _ = b.¦ }",
            &["Field", "Value", "Pointer"][..],
            &[][..],
        ),
        (
            "func f(b **Box[string]) { _ = b.¦ }",
            &[][..],
            &["Field", "Value", "Pointer"][..],
        ),
        ("func f(b *I) { _ = b.¦ }", &[][..], &["Iface"][..]),
        (
            "func f(b I) { _ = b.¦ }",
            &["Iface"][..],
            &["Field", "Value"][..],
        ),
        (
            "func f() { _ = Box[string].¦ }",
            &["Value"][..],
            &["Field", "Pointer"][..],
        ),
        (
            "func f() { _ = (*Box[string]).¦ }",
            &["Value", "Pointer"][..],
            &["Field"][..],
        ),
        (
            "func f() { _ = fresh().¦ }",
            &["Field", "Value"][..],
            &["Pointer"][..],
        ),
    ];
    for (site, present, absent) in cases {
        let result = complete(&mut engine, &path, &format!("{header}{site}"));
        assert_labels(&result, present, absent);
        if site.contains("b Box[string]") {
            assert_eq!(
                item(&result, "Field").detail.as_deref(),
                Some("field Field string")
            );
            assert_eq!(
                item(&result, "Value").detail.as_deref(),
                Some("func Value(x string) string")
            );
        }
        if site.contains("(*Box") {
            assert!(
                item(&result, "Pointer")
                    .detail
                    .as_deref()
                    .unwrap()
                    .contains("*Box[string], x string")
            );
        }
    }
}

#[test]
fn promoted_selectors_keep_the_shallowest_unambiguous_visible_member() {
    let (project, mut engine, path) = engine();
    let dep = project.write(
        "dep/lib.go",
        concat!(
            "package dep\ntype Base struct { Shared int; Exported int; hidden int }\n",
            "func (Base) Public() {}\nfunc (Base) private() {}\n",
        ),
    );
    install(
        &mut engine,
        listing(
            &project,
            &[
                pkg(
                    &project.root,
                    "example.test/sample",
                    "sample",
                    &["sample.go"],
                ),
                pkg(
                    dep.parent().unwrap(),
                    "example.test/dep",
                    "dep",
                    &["lib.go"],
                ),
            ],
            false,
        ),
        false,
    );
    let header = concat!(
        "package sample\nimport \"example.test/dep\"\n",
        "type A struct{ dep.Base }\ntype B struct{ Shared int; Ambiguous int }\n",
        "type C struct{ Ambiguous int }\n",
        "type S struct{ A; B; C; Exported string }\n",
    );
    let items = complete(
        &mut engine,
        &path,
        &format!("{header}func f(s S) {{ _ = s.¦ }}"),
    );
    assert_labels(
        &items,
        &["Shared", "Exported", "Public", "A", "B", "C"],
        &["Ambiguous", "hidden", "private"],
    );
    assert_eq!(
        item(&items, "Exported").detail.as_deref(),
        Some("field Exported string")
    );
}

#[test]
fn struct_literals_distinguish_keys_values_and_mixed_states_and_apply_legal_edits() {
    let (_project, mut engine, path) = engine();
    let header = concat!(
        "package sample\n",
        "type Inner struct{ Promoted int; Same string }\n",
        "type S struct{ First string; Fi int; Inner; Same int }\n",
        "var Value = 1\n",
    );
    for (suffix, present, absent) in [
        (
            "var _ = S{¦",
            &["First", "Fi", "Value", "Promoted"][..],
            &[][..],
        ),
        (
            "func f() {\n\t_ = S{¦\n\t_ = S{Fi: 1, First: \"\"}\n}\n",
            &["First", "Fi", "Value", "Promoted"][..],
            &[][..],
        ),
        (
            "var _ = S{First: \"\", ¦}",
            &["Fi", "Promoted", "Same"][..],
            &["First", "Value"][..],
        ),
        (
            "var _ = S{\"\", ¦}",
            &["Value"][..],
            &["Fi", "Promoted"][..],
        ),
        (
            "var _ = S{First: \"\", 1, ¦}",
            &["Fi", "Value"][..],
            &["First"][..],
        ),
        ("var _ = S{Fi: ¦}", &["Value"][..], &["Fi", "Promoted"][..]),
        (
            "var _ = S{Inner: Inner{}, ¦}",
            &["Same", "Fi"][..],
            &["Inner", "Promoted"][..],
        ),
    ] {
        let items = complete(&mut engine, &path, &format!("{header}{suffix}"));
        assert_labels(&items, present, absent);
        if suffix.contains("Inner: Inner{}") {
            assert_eq!(edit(item(&items, "Same")).new_text, "Same: ");
        }
    }

    let (text, position) = open(
        &mut engine,
        &path,
        &format!("{header}var _ = S{{Fi¦: 1}}"),
        2,
    );
    let caps = CompletionClientCapabilities {
        snippet_support: true,
        ..CompletionClientCapabilities::default()
    };
    let items = self::items(&mut engine, &path, position, &caps);
    let field = item(&items, "Fi");
    assert_eq!(edit(field).new_text, "Fi");
    assert_eq!(apply(&text, field), text);
    assert_go_syntax(&mut engine, apply(&text, field));

    let (text, position) = open(&mut engine, &path, &format!("{header}var _ = S{{Pro¦}}"), 3);
    let items = self::items(&mut engine, &path, position, &caps);
    let field = item(&items, "Promoted");
    assert_eq!(edit(field).new_text, "Inner: Inner{Promoted: $0}");
    assert_go_syntax(&mut engine, apply(&text, field).replace("$0", "1"));
}

#[test]
fn struct_literals_hide_other_packages_private_fields_and_handle_elided_elements() {
    let (project, mut engine, path) = engine();
    let dep = project.write(
        "data/lib.go",
        "package data\ntype S struct{ K int; hidden int }\n",
    );
    install(
        &mut engine,
        listing(
            &project,
            &[
                pkg(
                    &project.root,
                    "example.test/sample",
                    "sample",
                    &["sample.go"],
                ),
                pkg(
                    dep.parent().unwrap(),
                    "example.test/data",
                    "data",
                    &["lib.go"],
                ),
            ],
            false,
        ),
        false,
    );
    for suffix in ["var _ = data.S{¦}", "var _ = []data.S{{¦}}"] {
        let items = complete(
            &mut engine,
            &path,
            &format!("package sample\nimport \"example.test/data\"\n{suffix}"),
        );
        assert_labels(&items, &["K"], &["hidden"]);
    }
}

#[test]
fn keywords_labels_and_type_switch_guards_follow_go_statement_boundaries() {
    let (_project, mut engine, path) = engine();
    let header = "package sample\nfunc f(x any) {\n";
    let cases = [
        (
            "¦\n}",
            &[
                "if",
                "for",
                "switch",
                "select",
                "return",
                "go",
                "defer",
                "var",
                "const",
                "type",
                "fallthrough",
                "break",
                "continue",
                "goto",
            ][..],
            &["package", "case", "range"][..],
        ),
        ("for _, v := ¦ {}\n}", &["range"][..], &["case"][..]),
        (
            "switch x {\n¦\n}\n}",
            &["case", "default"][..],
            &["range", "cap", "int"][..],
        ),
        (
            "select {\n¦\n}\n}",
            &["case", "default"][..],
            &["range"][..],
        ),
        ("switch x.¦ {}\n}", &["(type)"][..], &[][..]),
        ("switch x.¦(int) {}\n}", &[][..], &["(type)"][..]),
        ("switch x.¦() {}\n}", &[][..], &["(type)"][..]),
        ("switch y := x.(¦) {}\n}", &["type"][..], &[][..]),
        ("switch x.(¦int) {}\n}", &["type"][..], &[][..]),
        (
            "Loop: for { Switch: switch x { case 1: break ¦ } }\n}",
            &["Loop", "Switch"][..],
            &[][..],
        ),
        (
            "Loop: for { Switch: switch x { case 1: continue ¦ } }\n}",
            &["Loop"][..],
            &["Switch"][..],
        ),
        (
            "Loop: for { Select: select { default: break ¦ } }\n}",
            &["Loop", "Select"][..],
            &[][..],
        ),
        (
            "Loop: for { _ = func(){ break ¦ }; break }\n}",
            &[][..],
            &["Loop"][..],
        ),
        (
            "{ goto ¦; Here: return }; Elsewhere: return; { Child: return }\n}",
            &["Here", "Elsewhere"][..],
            &["Child"][..],
        ),
        (
            "Outer: for { _ = func(){ goto ¦; Local: return }; break }\n}",
            &["Local"][..],
            &["Outer"][..],
        ),
    ];
    for (suffix, present, absent) in cases {
        let text = format!("{header}{suffix}");
        let items = complete(&mut engine, &path, &text);
        assert_labels(&items, present, absent);
    }
}

#[test]
fn expected_types_rank_assignment_call_return_case_and_literal_candidates() {
    let (_project, mut engine, path) = engine();
    let header = concat!(
        "package sample\nvar ChooseA int\nvar ChooseZ string\n",
        "func accept(x string){}\nfunc acceptGeneric[T any](first, second T) {}\n",
        "type S struct{ Value string }\n",
    );
    for suffix in [
        "var _ string = Cho¦",
        "func f() { var text string; text = Cho¦ }",
        "func f() string { return Cho¦ }",
        "func f() { accept(Cho¦) }",
        "func f() { acceptGeneric(\"seed\", Cho¦) }",
        "func f() { acceptGeneric[string](\"seed\", Cho¦) }",
        "var _ = S{Value: Cho¦}",
        "var _ = []S{{Cho¦}}",
        "var _ = map[int]string{1: Cho¦}",
        "func f(text string) { switch text { case Cho¦: } }",
    ] {
        let values = complete(&mut engine, &path, &format!("{header}{suffix}"));
        let a = item(&values, "ChooseA");
        let z = item(&values, "ChooseZ");
        assert!(
            z.sort_text < a.sort_text,
            "{suffix}: {:?} >= {:?}",
            z.sort_text,
            a.sort_text
        );
    }
}

#[test]
fn type_assertions_and_type_cases_weigh_matching_method_sets() {
    let (_project, mut engine, path) = engine();
    let header = concat!(
        "package sample\ntype Contract interface{ Method() int }\n",
        "type ChoiceA struct{}\nfunc (ChoiceA) Method() string { return \"\" }\n",
        "type ChoiceZ struct{}\nfunc (ChoiceZ) Method() int { return 0 }\n",
    );
    for suffix in [
        "func f(x Contract) { _ = x.(Cho¦) }",
        "func f(x Contract) { switch x.(type) { case Cho¦: } }",
    ] {
        let values = complete(&mut engine, &path, &format!("{header}{suffix}"));
        let a = item(&values, "ChoiceA");
        let z = item(&values, "ChoiceZ");
        assert!(
            z.sort_text < a.sort_text,
            "{suffix}: {:?} >= {:?}",
            z.sort_text,
            a.sort_text
        );
    }
}

#[test]
fn expected_type_adds_four_for_nil_and_conversions_and_five_for_exact_matches() {
    let (_project, mut engine, path) = engine();
    let values = complete(
        &mut engine,
        &path,
        concat!(
            "package sample\ntype Distinct int\nvar Number int\nvar Name Distinct\nvar Noon string\n",
            "var _ int = N¦",
        ),
    );
    let score = |item: &CompletionItem| {
        item.sort_text.as_ref().unwrap()[1..4]
            .parse::<i32>()
            .unwrap()
    };
    assert_eq!(
        score(item(&values, "Noon")) - score(item(&values, "Number")),
        5
    );
    assert_eq!(
        score(item(&values, "Noon")) - score(item(&values, "Name")),
        4
    );
    let values = complete(
        &mut engine,
        &path,
        "package sample\nvar nilSymbol int\nvar _ []int = ni¦",
    );
    assert_eq!(
        score(item(&values, "nilSymbol")) - score(item(&values, "nil")),
        4
    );
}

#[test]
fn rank_matches_exact_prefix_folded_and_camel_humps_before_other_weights() {
    let (_project, mut engine, path) = engine();
    let text = concat!(
        "package sample\nvar HS int\nvar HSTail int\nvar hsTail int\nvar HTTPServer int\n",
        "var _ = HS¦",
    );
    let (_, position) = open(&mut engine, &path, text, 1);
    let caps = CompletionClientCapabilities::default();
    let values = items(&mut engine, &path, position, &caps);
    assert_eq!(labels(&values), ["HS", "HSTail", "hsTail", "HTTPServer"]);
    assert!(
        values
            .windows(2)
            .all(|items| items[0].sort_text < items[1].sort_text)
    );
    let again = items(&mut engine, &path, position, &caps);
    assert_eq!(values, again);
    assert!(ranking::match_start("rece", "RequestClientCert").is_some());
    assert!(ranking::match_start("para", "ParseRawPrivateKey").is_some());
    assert!(ranking::match_start("sqr", "ELF_ARM64_RELOC_ADDEND").is_none());
    assert!(ranking::match_start("NewI", "NewUserId").is_some());
    assert!(ranking::match_start("NewI", "NewRequestWithContext").is_none());
    assert!(ranking::match_name("u", "Document").is_none());
    assert!(ranking::match_member_hump("Mar", "JoinMarshalers").is_some());
    assert!(ranking::match_member_hump("Mar", "Unmarshal").is_none());
}

#[test]
fn import_paths_apply_utf16_insert_replace_ranges_preserving_quotes_and_alias() {
    let (project, mut engine, path) = engine();
    let lib = project.write("lib/json/lib.go", "package json\n");
    install(
        &mut engine,
        listing(
            &project,
            &[
                pkg(
                    &project.root,
                    "example.test/sample",
                    "sample",
                    &["sample.go"],
                ),
                pkg(
                    lib.parent().unwrap(),
                    "example.test/lib/json",
                    "json",
                    &["lib.go"],
                ),
            ],
            false,
        ),
        false,
    );
    let (source, position) = open(
        &mut engine,
        &path,
        "package sample\r\nimport /*😀中*/ alias \"example.test/lib/js¦unrelated\"\r\n",
        1,
    );
    let caps = CompletionClientCapabilities {
        insert_replace_support: true,
        ..CompletionClientCapabilities::default()
    };
    let values = items(&mut engine, &path, position, &caps);
    let path_item = item(&values, "example.test/lib/json");
    let Some(CompletionTextEdit::InsertAndReplace(edit)) = &path_item.text_edit else {
        panic!("missing insert/replace edit");
    };
    assert_eq!(edit.insert.start, Position::new(1, 22));
    assert_eq!(edit.insert.start, edit.replace.start);
    assert_eq!(edit.insert.end, position);
    assert_eq!(edit.replace.end.character, position.character + 9);
    let applied = apply(&source, path_item);
    assert_eq!(
        applied,
        "package sample\r\nimport /*😀中*/ alias \"example.test/lib/json\"\r\n"
    );
    assert_go_syntax(&mut engine, applied);
    let values = items(
        &mut engine,
        &path,
        position,
        &CompletionClientCapabilities::default(),
    );
    assert!(matches!(
        item(&values, "example.test/lib/json").text_edit,
        Some(CompletionTextEdit::Edit(_))
    ));

    let (source, position) = open(
        &mut engine,
        &path,
        "package sample\nimport alias \"json¦",
        2,
    );
    let values = items(&mut engine, &path, position, &caps);
    let selected = item(&values, "example.test/lib/json");
    assert_eq!(selected.filter_text.as_deref(), Some("json"));
    assert_eq!(
        apply(&source, selected),
        "package sample\nimport alias \"example.test/lib/json\""
    );
    let (source, position) = open(
        &mut engine,
        &path,
        "package sample\r\nimport (\r\n\talias \"example.test/lib/js¦\r\n\t\"fmt\"\r\n)\r\n",
        3,
    );
    let values = items(&mut engine, &path, position, &caps);
    assert_eq!(
        apply(&source, item(&values, "example.test/lib/json")),
        "package sample\r\nimport (\r\n\talias \"example.test/lib/json\"\r\n\t\"fmt\"\r\n)\r\n",
    );
    for input in ["./", "../"] {
        let values = complete(
            &mut engine,
            &path,
            &format!("package sample\nimport \"{input}¦\""),
        );
        assert!(values.is_empty());
    }
}

#[test]
fn import_catalog_filters_hidden_main_internal_vendor_and_self_and_prefers_nearest_vendor() {
    let (project, mut engine, path) = engine();
    let outer = TestProject::new();
    let from = pkg(
        &project.root,
        "example.test/sample",
        "sample",
        &["sample.go"],
    );
    let dependencies = [
        (
            "internal/yes",
            "example.test/sample/internal/yes",
            "yes",
            &project,
            "Allowed",
        ),
        (
            "internal/no",
            "example.test/outside/internal/no",
            "no",
            &outer,
            "Outside",
        ),
        (
            "cmd/program",
            "example.test/program",
            "main",
            &project,
            "MainValue",
        ),
        (
            "testdata/example",
            "example.test/testdata/example",
            "example",
            &project,
            "Fixture",
        ),
        (
            "_private",
            "example.test/_private",
            "private",
            &project,
            "Hidden",
        ),
        (
            ".hidden",
            "example.test/.hidden",
            "hidden",
            &project,
            "Hidden",
        ),
        (
            "vendor/foo.test/lib",
            "foo.test/lib",
            "lib",
            &project,
            "VendorValue",
        ),
        (
            "cache/lib",
            "foo.test/lib [cache]",
            "lib",
            &outer,
            "VendorOutside",
        ),
        (
            "vendor/foo.test/private",
            "outside.test/vendor/foo.test/private",
            "private",
            &outer,
            "VendorPrivate",
        ),
    ];
    let mut packages = vec![from];
    for (dir, import, name, root, symbol) in dependencies {
        let file = root.write(
            &format!("{dir}/lib.go"),
            &format!("package {name}\nfunc {symbol}() {{}}"),
        );
        packages.push(pkg(file.parent().unwrap(), import, name, &["lib.go"]));
    }
    install(&mut engine, listing(&project, &packages, false), false);
    let values = complete(&mut engine, &path, "package sample\nimport \"¦\"");
    assert_labels(
        &values,
        &["example.test/sample/internal/yes", "foo.test/lib"],
        &[
            "example.test/outside/internal/no",
            "example.test/program",
            "example.test/testdata/example",
            "example.test/_private",
            "example.test/.hidden",
            "foo.test/private",
            "example.test/sample",
        ],
    );
    let values = complete(&mut engine, &path, "package sample\nvar _ = Vendor¦");
    assert_labels(
        &values,
        &["VendorValue"],
        &["VendorOutside", "VendorPrivate"],
    );
    assert_eq!(
        edit(item(&values, "VendorValue")).new_text,
        "lib.VendorValue()"
    );
}

#[test]
fn auto_import_uses_the_replacement_selected_by_each_workspace_folder() {
    let project = TestProject::new();
    let roots = ["left", "right"].map(|name| project.root.join(name));
    for (name, symbol) in [("left", "WitnessLeft"), ("right", "WitnessRight")] {
        project.write(&format!("{name}/go.mod"), &format!(
            "module example.test/{name}\n\ngo 1.22\n\nrequire example.test/lib v0.0.0\nreplace example.test/lib => ./lib\n"
        ));
        project.write(
            &format!("{name}/lib/go.mod"),
            "module example.test/lib\n\ngo 1.22\n",
        );
        project.write(
            &format!("{name}/lib/lib.go"),
            &format!("package lib\nconst {symbol} = 1\n"),
        );
        project.write(
            &format!("{name}/use/use.go"),
            "package use\nimport _ \"example.test/lib\"\n",
        );
        project.write(&format!("{name}/sample.go"), "package sample\n");
    }
    let mut engine = Engine::with_roots(roots.to_vec());
    install(
        &mut engine,
        Workspace::load_roots(&roots, false).unwrap(),
        false,
    );
    for (root, expected, other) in [
        (&roots[0], "WitnessLeft", "WitnessRight"),
        (&roots[1], "WitnessRight", "WitnessLeft"),
    ] {
        let path = root.join("sample.go");
        let source = "package sample\nvar _ = Witness¦";
        let items = complete(&mut engine, &path, source);
        assert_labels(&items, &[expected], &[other]);
        assert_eq!(
            apply(&marked(source).0, item(&items, expected)),
            format!("package sample\n\nimport \"example.test/lib\"\n\nvar _ = lib.{expected}")
        );
    }
}

#[test]
fn auto_imports_prioritize_existing_aliases_and_apply_nonoverlapping_crlf_edits() {
    let (project, mut engine, path) = engine();
    let old = project.write(
        "existing/lib.go",
        "package existing\nfunc Marshal() int { return 1 }",
    );
    let new = project.write(
        "json/lib.go",
        "package json\nfunc Marshal() int { return 2 }\nfunc Encode() {}\nfunc BinaryMarshaler() {}",
    );
    install(
        &mut engine,
        listing(
            &project,
            &[
                pkg(
                    &project.root,
                    "example.test/sample",
                    "sample",
                    &["sample.go"],
                ),
                pkg(
                    old.parent().unwrap(),
                    "example.test/existing",
                    "existing",
                    &["lib.go"],
                ),
                pkg(
                    new.parent().unwrap(),
                    "example.test/json",
                    "json",
                    &["lib.go"],
                ),
            ],
            false,
        ),
        false,
    );
    let (source, position) = open(
        &mut engine,
        &path,
        concat!(
            "package sample\r\nimport old \"example.test/existing\"\r\n",
            "var json, json2 int\r\nfunc f() { _, _ = \"😀中\", Mar¦ }\r\n",
        ),
        1,
    );
    let values = items(
        &mut engine,
        &path,
        position,
        &CompletionClientCapabilities::default(),
    );
    let matches = values
        .iter()
        .filter(|item| item.label == "Marshal")
        .collect::<Vec<_>>();
    assert_eq!(matches.len(), 2);
    assert_eq!(edit(matches[0]).new_text, "old.Marshal()");
    assert!(matches[0].additional_text_edits.is_none());
    assert_eq!(edit(matches[1]).new_text, "json3.Marshal()");
    assert_eq!(
        edit(matches[1]).range.start.character,
        position.character - 3
    );
    let applied = apply(&source, matches[1]);
    assert!(applied.contains("json3 \"example.test/json\""));
    assert!(applied.contains("old \"example.test/existing\""));
    assert!(applied.contains("\"😀中\", json3.Marshal()"));
    assert!(!applied.replace("\r\n", "").contains('\n'));
    assert_go_syntax(&mut engine, applied);

    let values = complete(&mut engine, &path, "package sample\nfunc f() { js¦ }");
    let encode = item(&values, "Encode");
    assert_eq!(edit(encode).new_text, "json.Encode()");
    assert_eq!(encode.filter_text.as_deref(), Some("json.Encode"));
    let values = complete(&mut engine, &path, "package sample\nfunc f() { Mar¦ }");
    assert_labels(&values, &["Marshal"], &["BinaryMarshaler"]);
    let values = complete(
        &mut engine,
        &path,
        "package sample\nfunc f() { js¦.Encode() }",
    );
    assert!(
        !values
            .iter()
            .any(|value| value.additional_text_edits.is_some())
    );

    let values = complete(
        &mut engine,
        &path,
        "package sample\nimport _ \"example.test/json\"\nvar _ = Mar¦",
    );
    let selected = values
        .iter()
        .find(|item| edit(item).new_text == "json.Marshal()")
        .unwrap();
    let (source, _) = marked("package sample\nimport _ \"example.test/json\"\nvar _ = Mar¦");
    let applied = apply(&source, selected);
    assert!(applied.contains("_ \"example.test/json\""));
    assert!(applied.contains("\n\t\"example.test/json\""));
    assert_go_syntax(&mut engine, applied);

    let values = complete(
        &mut engine,
        &path,
        "package sample\nimport . \"example.test/json\"\nvar _ = Mar¦",
    );
    assert_labels(&values, &["Marshal"], &[]);
    assert!(
        !values
            .iter()
            .any(|item| edit(item).new_text == "json.Marshal()")
    );
}

#[test]
fn completion_worker_preserves_order_and_edits_for_current_snapshots() {
    use crate::workspace::CancelToken;

    let (project, mut engine, path) = engine();
    let dep = project.write(
        "codec/lib.go",
        "package codec\n// Marshal encodes the value.\nfunc Marshal(value int) int { return value }\n",
    );
    install(
        &mut engine,
        listing(
            &project,
            &[
                pkg(
                    &project.root,
                    "example.test/sample",
                    "sample",
                    &["sample.go"],
                ),
                pkg(
                    dep.parent().unwrap(),
                    "example.test/codec",
                    "codec",
                    &["lib.go"],
                ),
            ],
            false,
        ),
        false,
    );
    let caps = CompletionClientCapabilities {
        resolve_documentation: true,
        resolve_detail: true,
        resolve_additional_text_edits: true,
        ..CompletionClientCapabilities::default()
    };
    let mut worker = CompletionWorker::default();
    for (version, input) in [
        (1, "package sample\r\nfunc f() { _ = \"😀\"; Ma¦rshal }\r\n"),
        (2, "package sample\r\nvar Sample int\r\nvar _ = Sa¦\r\n"),
    ] {
        let (text, position) = open(&mut engine, &path, input, version);
        let expected = items(&mut engine, &path, position, &caps);
        let input =
            engine.capture_completion(&path, position, caps, None, CancelToken::default(), false);
        let CompletionOutcome::Ready(value) = worker.run(input) else {
            panic!("expected completion from the current snapshot");
        };
        assert_eq!(value, serde_json::to_value(&expected).unwrap());
        let produced: Vec<CompletionItem> = serde_json::from_value(value).unwrap();
        if version == 1 {
            let resolve = |engine: &mut Engine, item: &CompletionItem| {
                let request = CompletionResolve::new(item.clone()).unwrap();
                let memo = QueryReadMemo::default();
                loop {
                    match engine.resolve_completion_item(&request, &caps, &memo) {
                        Ok(ResolveCompletionResult::Item(item)) => return item,
                        Err(error) if error.is::<super::super::SharedLoadPending>() => {
                            let _ = engine
                                .shared_disk_wake()
                                .recv_timeout(Duration::from_secs(1));
                        }
                        _ => panic!("expected resolved item"),
                    }
                }
            };
            let expected = resolve(&mut engine, item(&expected, "Marshal"));
            let produced = resolve(&mut engine, item(&produced, "Marshal"));
            assert_eq!(produced, expected);
            let accepted = apply(&text, &produced);
            assert!(accepted.contains("import \"example.test/codec\""));
            assert!(accepted.contains("codec.Marshal()"));
        } else {
            assert_labels(&produced, &["Sample"], &[]);
        }
    }
}

#[test]
fn auto_imports_rewrite_unresolved_qualifiers_and_preserve_type_position_rules() {
    let (project, mut engine, path) = engine();
    let lib = project.write("codec/lib.go", concat!(
        "package codec\nfunc Marshal() int { return 2 }\ntype Marshaler struct{}\nvar Marshaled int\n",
    ));
    install(
        &mut engine,
        listing(
            &project,
            &[
                pkg(
                    &project.root,
                    "example.test/sample",
                    "sample",
                    &["sample.go"],
                ),
                pkg(
                    lib.parent().unwrap(),
                    "example.test/go-json/v2",
                    "codec",
                    &["lib.go"],
                ),
            ],
            false,
        ),
        false,
    );
    assert_eq!(
        imports::assumed_package_name("example.test/go-json/v2"),
        "json"
    );
    assert_eq!(
        imports::assumed_package_name("example.test/json.api/v10"),
        "json_api"
    );
    for suffix in ["var _ = json.¦", "var _ = json.Mar¦"] {
        let (source, position) = open(
            &mut engine,
            &path,
            &format!("package sample\nvar codec, codec2 int\n{suffix}"),
            1,
        );
        let values = items(
            &mut engine,
            &path,
            position,
            &CompletionClientCapabilities::default(),
        );
        let marshal = item(&values, "Marshal");
        assert_eq!(edit(marshal).range.start, Position::new(2, 8));
        assert_eq!(edit(marshal).new_text, "codec3.Marshal()");
        assert_eq!(marshal.filter_text.as_deref(), Some("json.Marshal"));
        let applied = apply(&source, marshal);
        assert!(applied.contains("import codec3 \"example.test/go-json/v2\""));
        assert!(applied.ends_with("var _ = codec3.Marshal()"));
        assert_go_syntax(&mut engine, applied);
    }
    let values = complete(&mut engine, &path, "package sample\nvar _ json.Mar¦");
    assert_labels(&values, &["Marshaler"], &["Marshal", "Marshaled"]);
    assert_eq!(edit(item(&values, "Marshaler")).new_text, "codec.Marshaler");
    let values = complete(&mut engine, &path, "package sample\nvar _ = &Mar¦");
    assert_labels(&values, &["Marshaler"], &["Marshal", "Marshaled"]);
    assert_eq!(
        edit(item(&values, "Marshaler")).new_text,
        "codec.Marshaler{}"
    );
    let values = complete(
        &mut engine,
        &path,
        "package sample\nfunc f(x int) { switch x { case Mar¦: } }",
    );
    assert!(!labels(&values).contains(&"Marshal"));
}

#[test]
fn sdk_inventory_loads_only_the_qualified_packages_and_reuses_exported_symbols() {
    let (project, mut engine, path) = engine();
    install(&mut engine, project.workspace(&["sample.go"], false), true);
    let (source, position) = open(
        &mut engine,
        &path,
        "package sample\nvar _, _ = json.Mar¦",
        1,
    );
    let values = items(
        &mut engine,
        &path,
        position,
        &CompletionClientCapabilities::default(),
    );
    let marshal = item(&values, "Marshal");
    let data = serde_json::from_value::<ItemData>(marshal.data.clone().unwrap()).unwrap();
    assert!(matches!(
        data.target.unwrap().revision,
        Some(SourceRev::Disk(_))
    ));
    let applied = apply(&source, marshal);
    assert!(applied.contains("import \"encoding/json\""));
    assert!(applied.ends_with("var _, _ = json.Marshal()"));
    assert_go_syntax(&mut engine, applied);
    let files: Vec<_> = engine
        .completion_imports
        .sdk_files()
        .map(Path::to_owned)
        .collect();
    assert!(!files.is_empty());
    assert!(files.iter().all(|path| {
        let directory = path.parent().unwrap();
        directory.ends_with("encoding/json") || directory.ends_with("encoding/json/v2")
    }));
    assert_eq!(files.len(), engine.completion_imports.sdk_files().count());
    let again = items(
        &mut engine,
        &path,
        position,
        &CompletionClientCapabilities::default(),
    );
    assert_eq!(values, again);
    assert_eq!(files.len(), engine.completion_imports.sdk_files().count());
    let values = complete(
        &mut engine,
        &path,
        "package sample\nimport alias \"encoding/json\"\nvar _, _ = alias.Mar¦",
    );
    assert_eq!(edit(item(&values, "Marshal")).new_text, "Marshal()");
    assert!(item(&values, "Marshal").additional_text_edits.is_none());
    let values = complete(&mut engine, &path, "package sample\nvar _, _ = Mar¦");
    assert!(
        values
            .iter()
            .any(|item| edit(item).new_text == "json.Marshal()")
    );
    assert!(engine.completion_imports.sdk_files().count() > files.len());
    let values = complete(&mut engine, &path, "package sample\nimport \"enc¦\"");
    assert_labels(
        &values,
        &["encoding/json", "encoding/base64"],
        &["internal/abi", "cmd/go"],
    );

    let parsed_files = engine.completion_imports.sdk_files().count();
    install(&mut engine, project.workspace(&["sample.go"], false), true);
    let values = complete(&mut engine, &path, "package sample\nvar _, _ = json.Mar¦");
    assert_labels(&values, &["Marshal"], &[]);
    assert_eq!(engine.completion_imports.sdk_files().count(), parsed_files);
}

#[test]
fn native_inventory_selects_the_same_sdk_packages_and_files_as_go_list() {
    let project = TestProject::new();
    super::sdk::compare_current_sdk(&project.root).unwrap();
}

#[test]
fn lightweight_sdk_names_cover_go_declaration_groups_and_skip_nested_syntax() {
    let names = super::sdk_names::scan_names(concat!(
        "\u{feff}package sdk\n",
        "// func Comment()\n/* var Hidden int */\n",
        "func Exported[T any](value T) {}\nfunc (t T) Method() {}\n",
        "var lower, ExportedVar int\n",
        "var ( GroupA,\n GroupB = 1, 2; GroupC = `func Decoy(){}`\n )\n",
        "const ( A = '\\''; B = \"var String int\" )\n",
        "type ( Named struct { Field int }; Alias = int )\n",
        "type Étage int\n",
    ))
    .unwrap()
    .exported;
    assert_eq!(
        names,
        [
            "A",
            "Alias",
            "B",
            "Exported",
            "ExportedVar",
            "GroupA",
            "GroupB",
            "GroupC",
            "Named",
            "Étage"
        ]
    );
}

#[test]
fn closed_workspace_and_vendor_name_indexes_follow_overlay_disk_and_graph_changes() {
    let (project, mut engine, caller) = engine();
    let local = project.write("lib/lib.go", "package lib\nvar (Unrelated = 1)\n");
    let vendor = project.write(
        "vendor/example.test/v/v.go",
        "package v\ntype (Unrelated int)\n",
    );
    let packages = [
        pkg(
            &project.root,
            "example.test/sample",
            "sample",
            &["sample.go"],
        ),
        pkg(
            local.parent().unwrap(),
            "example.test/sample/lib",
            "lib",
            &["lib.go"],
        ),
        pkg(vendor.parent().unwrap(), "example.test/v", "v", &["v.go"]),
    ];
    install(&mut engine, listing(&project, &packages, false), false);
    let (_, position) = open(&mut engine, &caller, "package sample\nvar _ = Éta¦\n", 1);
    let caps = CompletionClientCapabilities::default();
    let local_key = path_key(&local);
    let vendor_key = path_key(&vendor);
    assert_labels(
        &items(&mut engine, &caller, position, &caps),
        &[],
        &["Étage"],
    );
    for key in [&local_key, &vendor_key] {
        assert!(engine.state.completion_names.contains_key(key));
        assert!(!engine.state.disk_index.contains_key(key));
    }

    engine
        .open(
            vendor.clone(),
            "package v\nvar (lower, ÉtageOverlay = 1, 2)\n".into(),
            1,
        )
        .unwrap();
    assert_labels(
        &items(&mut engine, &caller, position, &caps),
        &["ÉtageOverlay"],
        &["ÉtageDisk"],
    );
    assert!(!engine.state.disk_index.contains_key(&vendor_key));
    std::fs::write(&vendor, "package v\ntype (ÉtageDisk int)\n").unwrap();
    engine.close(&vendor);
    assert!(!engine.state.completion_names.contains_key(&vendor_key));
    assert_labels(
        &items(&mut engine, &caller, position, &caps),
        &["ÉtageDisk"],
        &["ÉtageOverlay"],
    );

    std::fs::write(&local, "package lib\nvar (lower, ÉtageSaved = 1, 2)\n").unwrap();
    engine.file_changed(&local, lsp_types::FileChangeType::CHANGED);
    assert!(!engine.state.completion_names.contains_key(&local_key));
    install(&mut engine, listing(&project, &packages, false), false);
    assert_labels(
        &items(&mut engine, &caller, position, &caps),
        &["ÉtageSaved", "ÉtageDisk"],
        &[],
    );

    engine.file_changed(&vendor, lsp_types::FileChangeType::DELETED);
    install(&mut engine, listing(&project, &packages[..2], false), false);
    assert!(!engine.state.completion_names.contains_key(&vendor_key));
    assert_labels(
        &items(&mut engine, &caller, position, &caps),
        &["ÉtageSaved"],
        &["ÉtageDisk"],
    );
}

#[test]
fn a_nonmatching_closed_sibling_still_shadows_an_auto_import_qualifier() {
    let (project, mut engine, caller) = engine();
    project.write("sibling.go", "package sample\nvar helper int\n");
    let helper = project.write("helper/lib.go", "package helper\nfunc Magic() {}\n");
    install(
        &mut engine,
        listing(
            &project,
            &[
                pkg(
                    &project.root,
                    "example.test/sample",
                    "sample",
                    &["sample.go", "sibling.go"],
                ),
                pkg(
                    helper.parent().unwrap(),
                    "example.test/helper",
                    "helper",
                    &["lib.go"],
                ),
            ],
            false,
        ),
        false,
    );
    let completion = complete(&mut engine, &caller, "package sample\nvar _ = Magi¦\n");
    let magic = item(&completion, "Magic");
    assert_eq!(edit(magic).new_text, "helper2.Magic()");
    assert!(
        magic
            .additional_text_edits
            .as_ref()
            .unwrap()
            .iter()
            .any(|edit| edit
                .new_text
                .contains("import helper2 \"example.test/helper\""))
    );
}

#[test]
fn parallel_scan_hands_off_text_and_keeps_the_graphs_first_declaration() {
    let (project, mut engine, caller) = engine();
    let first = project.write(
        "dep/Z.go",
        "package dep\nfunc MarkFirst() string { return \"\" }\n",
    );
    let second = project.write(
        "dep/a.go",
        "package dep\nfunc MarkFirst() int { return 0 }\n",
    );
    install(
        &mut engine,
        listing(
            &project,
            &[
                pkg(
                    &project.root,
                    "example.test/sample",
                    "sample",
                    &["sample.go"],
                ),
                pkg(
                    first.parent().unwrap(),
                    "example.test/dep",
                    "dep",
                    &["a.go", "Z.go"],
                ),
            ],
            false,
        ),
        false,
    );
    let (_, position) = open(&mut engine, &caller, "package sample\nvar _ = MarkF¦\n", 1);
    let caps = CompletionClientCapabilities {
        resolve_documentation: true,
        ..Default::default()
    };
    let QueryProgress::Searching(mut task) =
        engine.complete(&caller, position, None, &caps).unwrap()
    else {
        panic!("expected a batched import search");
    };
    assert!(!task.advance(&mut engine).unwrap());
    while !task.ready(&engine) {
        engine
            .completion_wake()
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap();
    }
    // Both texts belong to the completed batch. Parsing can proceed from
    // those bytes without opening the paths a second time.
    std::fs::remove_file(&first).unwrap();
    std::fs::remove_file(&second).unwrap();
    while !task.advance(&mut engine).unwrap() {}
    let CompletionResponse::Array(items) = task.finish() else {
        panic!("expected completion items");
    };
    assert_eq!(
        item(&items, "MarkFirst").detail.as_deref(),
        Some("func MarkFirst() string")
    );
    assert_eq!(
        items
            .iter()
            .filter(|item| item.label == "MarkFirst")
            .count(),
        1
    );
}

#[test]
fn declaration_parse_preserves_signatures_initializers_and_source_ranges() {
    let (_, mut engine, path) = engine();
    let text = concat!(
        "package sample\n",
        "// Doc for Étage.\n",
        "func Étage[T any](x T) struct{ Field T } {\n",
        "    _ = `foo { } // quote`\n",
        "    return struct{ Field T }{x}\n",
        "}\n",
        "var Initial = func() int { return 7 }\n",
        "func (Receiver) F(x int) { _ = x }\n",
        "type Receiver int\n",
    );
    let names = super::sdk_names::scan_names(text).unwrap();
    assert_eq!(names.bodies.len(), 2);
    assert!(text[names.bodies[0].clone()].contains("return struct"));
    assert!(!text[names.bodies[0].clone()].contains("Initial"));
    let parser = &mut engine.state.parser;
    let (indexed, elided) =
        Source::parse_declarations(parser, path.clone(), text.into(), &names.bodies).unwrap();
    let full = Source::parse(parser, path, text.into(), None).unwrap();
    assert!(elided);
    let make = |source: &Source| {
        let symbols = crate::symbols::collect(
            &source.tree,
            &source.text,
            |name| source.range(name),
            |_, _, _| {},
        );
        let mut rows = symbols
            .declarations()
            .chain(symbols.members())
            .map(|(name, sym)| {
                let desc = super::render::describe(source, sym.range, name, Kind::VARIABLE, true);
                (name.to_owned(), sym.range, desc.detail)
            })
            .collect::<Vec<_>>();
        rows.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        rows
    };
    assert_eq!(make(&indexed), make(&full));
    for bad in [
        "package sample\nvar (Foo int\n",
        "package sample\nfunc (Bad int] LooksLikeMethod() {}\n",
        "package sample\nvar Text = `unclosed\n",
        "package sample\nvar A = \"unclosed\n",
    ] {
        assert!(super::sdk_names::scan_names(bad).is_none());
    }
}

#[test]
fn a_completion_task_releases_request_sources_on_finish_cancel_or_stale() {
    let (project, mut engine, caller) = engine();
    let lib = project.write("dep/dep.go", "package dep\nfunc Export() {}\n");
    install(
        &mut engine,
        listing(
            &project,
            &[
                pkg(
                    &project.root,
                    "example.test/sample",
                    "sample",
                    &["sample.go"],
                ),
                pkg(
                    lib.parent().unwrap(),
                    "example.test/dep",
                    "dep",
                    &["dep.go"],
                ),
            ],
            false,
        ),
        false,
    );
    let (_, position) = open(&mut engine, &caller, "package sample\nvar _ = Expor¦\n", 1);
    for outcome in ["finish", "cancel", "stale"] {
        engine.state.disk_index.remove(&path_key(&lib));
        let QueryProgress::Searching(mut task) = engine
            .complete(
                &caller,
                position,
                None,
                &CompletionClientCapabilities {
                    resolve_documentation: true,
                    ..Default::default()
                },
            )
            .unwrap()
        else {
            panic!("expected an import search");
        };
        let sources = loop {
            assert!(!task.advance(&mut engine).unwrap());
            let sources = task.request.retained_declarations();
            if !sources.is_empty() {
                break sources;
            }
        };
        assert!(sources.iter().all(|source| source.upgrade().is_some()));
        match outcome {
            "finish" => {
                while !task.advance(&mut engine).unwrap() {}
                let CompletionResponse::Array(items) = task.finish() else {
                    panic!("expected completion");
                };
                assert_labels(&items, &["Export"], &[]);
            }
            "cancel" => {
                task.discard(lsp_server::ErrorCode::RequestCanceled);
                drop(task);
            }
            "stale" => {
                engine
                    .open(lib.clone(), "package dep\nfunc Updated() {}\n".into(), 2)
                    .unwrap();
                assert!(
                    task.advance(&mut engine)
                        .unwrap_err()
                        .is::<StaleCompletion>()
                );
                drop(task);
            }
            _ => unreachable!(),
        }
        assert!(sources.iter().all(|source| source.upgrade().is_none()));
    }
}

#[test]
fn a_closed_query_keeps_its_disk_snapshot_and_rejects_an_external_change() {
    let (project, mut engine, caller) = engine();
    let (text, position) = marked(concat!(
        "package sample\n",
        "// Export sends the value.\n",
        "func Export() {}\n",
        "var _ = Exp¦\n",
    ));
    project.write("sample.go", &text);
    let caps = CompletionClientCapabilities::default();
    let values = items(&mut engine, &caller, position, &caps);
    let proposal = item(&values, "Export");
    assert!(matches!(proposal.documentation.as_ref(),
        Some(Documentation::MarkupContent(docs)) if docs.value == "Export sends the value."));
    let data = serde_json::from_value::<ItemData>(proposal.data.clone().unwrap()).unwrap();
    assert!(matches!(data.document_revision, Some(SourceRev::Disk(_))));
    assert!(matches!(
        data.target.unwrap().revision,
        Some(SourceRev::Disk(_))
    ));
    let QueryProgress::Searching(mut task) =
        engine.complete(&caller, position, None, &caps).unwrap()
    else {
        panic!("expected an import search");
    };
    let retained = std::rc::Rc::downgrade(&task.request.document.as_ref().unwrap().0);
    project.write("sample.go", "package sample\n");
    assert!(
        task.advance(&mut engine)
            .unwrap_err()
            .is::<StaleCompletion>()
    );
    drop(task);
    assert!(retained.upgrade().is_none());
}

#[test]
fn completion_prefilter_observes_disk_edits_with_and_without_save() {
    let project = TestProject::new();
    let (text, position) = marked("package sample\nvar _ = NewlySaved¦\n");
    let path = project.write("sample.go", &text);
    let lib = project.write("dep/dep.go", "package dep\nfunc Other() {}\n");
    let graph = || {
        listing(
            &project,
            &[
                pkg(
                    &project.root,
                    "example.test/sample",
                    "sample",
                    &["sample.go"],
                ),
                pkg(
                    lib.parent().unwrap(),
                    "example.test/dep",
                    "dep",
                    &["dep.go"],
                ),
            ],
            false,
        )
    };
    let mut lsp = LspSession::new(&path, json!({}));
    lsp.open(&text);
    install(&mut lsp.running().engine, graph(), false);
    lsp.complete(1, position);
    assert_labels(&lsp.complete_results(1), &[], &["NewlySaved"]);
    std::fs::write(&lib, "package dep\nfunc NewlySavedOnDisk() {}\n").unwrap();
    lsp.complete(2, position);
    assert_labels(
        &lsp.complete_results(2),
        &["NewlySavedOnDisk"],
        &["NewlySaved"],
    );
    std::fs::write(&lib, "package dep\nfunc NewlySaved() {}\n").unwrap();
    lsp.notify(
        "textDocument/didSave",
        json!({
            "textDocument": {"uri": crate::protocol::path_uri(&lib).unwrap()},
        }),
    );
    install(&mut lsp.running().engine, graph(), false);
    lsp.complete(3, position);
    assert_labels(
        &lsp.complete_results(3),
        &["NewlySaved"],
        &["NewlySavedOnDisk"],
    );
}

#[test]
fn weights_workspace_usage_then_module_dependency_and_sdk_tiers() {
    let (project, mut engine, path) = engine();
    let external = TestProject::new();
    let make = |root: &TestProject, name: &str, module: Value, imports: Value, goroot: bool| {
        let file = root.write(
            &format!("{name}/lib.go"),
            &format!("package {name}\nfunc Shared() {{}}"),
        );
        let mut package = pkg(
            file.parent().unwrap(),
            &format!("example.test/{name}"),
            name,
            &["lib.go"],
        );
        package["Module"] = module;
        package["Imports"] = imports;
        package["Goroot"] = json!(goroot);
        package
    };
    let mut caller = pkg(
        &project.root,
        "example.test/sample",
        "sample",
        &["sample.go"],
    );
    caller["Module"] = json!({"Path": "example.test", "Main": true});
    let workspace = listing(
        &project,
        &[
            caller,
            make(
                &project,
                "current",
                json!({"Path": "example.test", "Main": true}),
                json!([]),
                false,
            ),
            make(
                &external,
                "direct",
                json!({"Path": "example.test/direct", "Version": "v1.0.0"}),
                json!([]),
                false,
            ),
            make(&external, "sdk", Value::Null, json!([]), true),
            make(
                &external,
                "indirect",
                json!({"Path": "example.test/indirect", "Version": "v1.0.0", "Indirect": true}),
                json!([]),
                false,
            ),
            make(&external, "popular", Value::Null, json!([]), false),
            make(
                &project,
                "another",
                json!({"Path": "example.test", "Main": true}),
                json!(["example.test/popular"]),
                false,
            ),
        ],
        false,
    );
    install(&mut engine, workspace, false);
    let values = complete(&mut engine, &path, "package sample\nvar _ = Sha¦");
    let edits: Vec<_> = values
        .iter()
        .filter(|value| value.label == "Shared")
        .map(|value| edit(value).new_text)
        .collect();
    assert_eq!(
        edits,
        [
            "popular.Shared()",
            "another.Shared()",
            "current.Shared()",
            "direct.Shared()",
            "sdk.Shared()",
            "indirect.Shared()",
        ]
    );

    let dependency = external.root.join("direct/lib.go");
    engine
        .open(
            dependency.clone(),
            "package direct\nfunc SharedOverlay() {}".into(),
            1,
        )
        .unwrap();
    let values = complete(&mut engine, &path, "package sample\nvar _ = SharedO¦");
    assert_eq!(
        edit(item(&values, "SharedOverlay")).new_text,
        "direct.SharedOverlay()"
    );
    engine.close(&dependency);
    let values = complete(&mut engine, &path, "package sample\nvar _ = SharedO¦");
    assert_labels(&values, &[], &["SharedOverlay"]);
}

#[test]
fn insertion_respects_existing_parentheses_braces_colon_and_qualifier_dot() {
    let (project, mut engine, path) = engine();
    let dep = project.write("dep/lib.go", "package dep\nfunc Function(){}\n");
    install(
        &mut engine,
        listing(
            &project,
            &[
                pkg(
                    &project.root,
                    "example.test/sample",
                    "sample",
                    &["sample.go"],
                ),
                pkg(
                    dep.parent().unwrap(),
                    "example.test/dep",
                    "dep",
                    &["lib.go"],
                ),
            ],
            false,
        ),
        false,
    );
    let caps = CompletionClientCapabilities {
        snippet_support: true,
        ..Default::default()
    };
    let header = concat!(
        "package sample\nimport \"example.test/dep\"\n",
        "func WithArg(x int){}\nfunc NoArg(){}\ntype Structure struct{ Field int }\n",
    );
    for (suffix, label, expected, snippet) in [
        ("func f() { Wi¦ }", "WithArg", "WithArg($1)$0", true),
        ("func f() { Wi¦(2) }", "WithArg", "WithArg", false),
        ("func f() { No¦ }", "NoArg", "NoArg()$0", true),
        ("var _ = Stru¦", "Structure", "Structure{$0}", true),
        ("var _ = Stru¦{}", "Structure", "Structure", false),
        ("var _ = Structure{Fi¦: 1}", "Field", "Field", false),
        ("var _ = Structure{Fi¦}", "Field", "Field: $0", true),
        ("func f() { de¦.Function() }", "dep", "dep", false),
    ] {
        let (source, position) = open(&mut engine, &path, &format!("{header}{suffix}"), 1);
        let values = items(&mut engine, &path, position, &caps);
        let value = item(&values, label);
        assert_eq!(edit(value).new_text, expected, "{suffix}");
        assert_eq!(
            value.insert_text_format,
            Some(if snippet {
                lsp_types::InsertTextFormat::SNIPPET
            } else {
                lsp_types::InsertTextFormat::PLAIN_TEXT
            })
        );
        if !snippet {
            assert_go_syntax(&mut engine, apply(&source, value));
        }
    }
    let values = complete(&mut engine, &path, &format!("{header}func f() {{ Wi¦ }}"));
    assert_eq!(edit(item(&values, "WithArg")).new_text, "WithArg()");
}

struct LspSession {
    session: TestSession,
    state: crate::State,
    uri: lsp_types::Uri,
}

impl LspSession {
    fn new(path: &Path, capabilities: Value) -> Self {
        let session = TestSession::new();
        let mut state = crate::State::starting();
        let uri = crate::protocol::path_uri(path).unwrap();
        let root = crate::protocol::path_uri(path.parent().unwrap()).unwrap();
        session.request(
            0,
            "initialize",
            json!({
                "rootUri": root, "capabilities": capabilities, "processId": null,
            }),
            |connection, request| crate::handle_request(connection, &mut state, request),
        );
        let result = session.response(0).result.unwrap();
        assert_eq!(
            result["capabilities"]["completionProvider"]["resolveProvider"],
            true
        );
        assert_eq!(
            result["capabilities"]["completionProvider"]["triggerCharacters"],
            json!(["."])
        );
        Self {
            session,
            state,
            uri,
        }
    }

    fn running(&mut self) -> &mut crate::Running {
        let crate::State::Running(running) = &mut self.state else {
            panic!("not initialized")
        };
        running
    }

    fn open(&mut self, text: &str) {
        let running = match &mut self.state {
            crate::State::Running(running) => running,
            _ => panic!("not initialized"),
        };
        self.session
            .open_go_document(&self.uri, text, |connection, notification| {
                crate::handle_notification(
                    connection,
                    running,
                    &notification.method,
                    notification.params,
                )
            });
    }

    fn notify(&mut self, method: &str, params: Value) {
        let running = match &mut self.state {
            crate::State::Running(running) => running,
            _ => panic!("not initialized"),
        };
        self.session
            .notify(method, params, |connection, notification| {
                crate::handle_notification(
                    connection,
                    running,
                    &notification.method,
                    notification.params,
                )
            });
    }

    fn flush_watches(&mut self) {
        let crate::State::Running(running) = &mut self.state else {
            panic!("not initialized");
        };
        let deadline = running.scheduler.workspace_deadline().unwrap();
        running
            .advance_watched_files(self.session.server(), deadline)
            .unwrap();
    }

    fn replace(&mut self, text: &str, version: i32) {
        let running = match &mut self.state {
            crate::State::Running(running) => running,
            _ => panic!("not initialized"),
        };
        self.session
            .replace_document(&self.uri, version, text, |connection, notification| {
                crate::handle_notification(
                    connection,
                    running,
                    &notification.method,
                    notification.params,
                )
            });
    }

    fn request(&mut self, id: i32, method: &str, params: Value) {
        self.session
            .request(id, method, params, |connection, request| {
                crate::handle_request(connection, &mut self.state, request)
            });
    }

    fn complete(&mut self, id: i32, position: Position) {
        self.request(
            id,
            "textDocument/completion",
            json!({
                "textDocument": {"uri": self.uri}, "position": position,
                "context": {"triggerKind": 1},
            }),
        );
    }

    fn advance(&mut self) {
        let crate::State::Running(running) = &mut self.state else {
            panic!("not initialized")
        };
        running.advance_search(self.session.server()).unwrap();
    }

    fn start_completion_step(&mut self) {
        let crate::State::Running(running) = &mut self.state else {
            panic!("not initialized")
        };
        running
            .scheduler
            .advance_completion(&running.engine, self.session.server())
            .unwrap();
    }

    fn step_completion(&mut self) {
        self.start_completion_step();
        let crate::State::Running(running) = &mut self.state else {
            panic!("not initialized")
        };
        let completed = running
            .scheduler
            .completion_results()
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .expect("completion worker did not finish");
        running
            .scheduler
            .complete_completion(&mut running.engine, self.session.server(), completed)
            .unwrap();
    }

    fn complete_results(&mut self, id: i32) -> Vec<CompletionItem> {
        while self.running().scheduler.has_completion() {
            assert!(!self.running().scheduler.completion_waiting_for_workspace());
            self.step_completion();
        }
        serde_json::from_value(self.session.response(id).result.unwrap()).unwrap()
    }

    fn resolve(&mut self, id: i32, item: &CompletionItem) -> lsp_server::Response {
        self.request(
            id,
            "completionItem/resolve",
            serde_json::to_value(item).unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.running().searches.is_empty() {
            let running = self.running();
            if running.scheduler.ready_search(&running.engine).is_none() {
                let _ = running
                    .engine
                    .shared_disk_wake()
                    .recv_timeout(Duration::from_millis(10));
            }
            assert!(
                Instant::now() < deadline,
                "completion resolve did not finish"
            );
            self.advance();
        }
        self.session.response(id)
    }

    fn error(&self, id: i32, code: i32) {
        assert_eq!(self.session.response(id).error.unwrap().code, code);
        self.session.assert_no_messages();
    }
}

fn feature_caps(properties: &[&str], markdown: bool) -> Value {
    json!({"textDocument": {"completion": {"completionItem": {
        "snippetSupport": true, "labelDetailsSupport": true, "deprecatedSupport": true,
        "insertReplaceSupport": true,
        "documentationFormat": [if markdown { "markdown" } else { "plaintext" }],
        "resolveSupport": {"properties": properties},
    }}}})
}

#[test]
fn lsp_completion_negotiates_capabilities_and_resolves_docs_detail_and_import_edits() {
    use lsp_types::{Documentation, MarkupKind};

    let project = TestProject::new();
    let path = project.write("sample.go", "package sample\n");
    let dep = project.write(
        "dep/lib.go",
        concat!(
            "package dep\n\n// Marshal writes the value.\n//\n// Deprecated: use Write.\n",
            "func Marshal(x int) int { return x }\n",
        ),
    );
    let workspace = || {
        listing(
            &project,
            &[
                pkg(
                    &project.root,
                    "example.test/sample",
                    "sample",
                    &["sample.go"],
                ),
                pkg(
                    dep.parent().unwrap(),
                    "example.test/dep",
                    "dep",
                    &["lib.go"],
                ),
            ],
            false,
        )
    };
    let (text, position) = marked("package sample\nvar _ = Mar¦");
    let mut rich = LspSession::new(
        &path,
        feature_caps(&["documentation", "detail", "additionalTextEdits"], true),
    );
    rich.open(&text);
    install(&mut rich.running().engine, workspace(), false);
    rich.complete(1, position);
    let proposals = rich.complete_results(1);
    let proposal = item(&proposals, "Marshal");
    assert_eq!(edit(proposal).new_text, "dep.Marshal($1)$0");
    assert!(proposal.detail.is_none());
    assert!(proposal.documentation.is_none());
    assert!(proposal.additional_text_edits.is_none());
    assert_eq!(
        proposal.tags.as_deref(),
        Some(&[lsp_types::CompletionItemTag::DEPRECATED][..])
    );
    let details = proposal.label_details.as_ref().unwrap();
    assert_eq!(details.description.as_deref(), Some("example.test/dep"));
    assert_eq!(details.detail.as_deref(), Some("(x int)"));
    let data = serde_json::from_value::<ItemData>(proposal.data.clone().unwrap()).unwrap();
    let target = data.target.unwrap();
    assert_eq!(target.file, dep);
    assert!(matches!(target.revision, Some(SourceRev::Disk(_))));
    let engine = &mut rich.running().engine;
    let dependency_text = std::fs::read_to_string(&dep).unwrap();
    let baseline = Source::parse(
        engine.snapshot().parser(),
        dep.clone(),
        dependency_text.clone(),
        None,
    )
    .unwrap();
    let baseline_docs = markup(
        &baseline,
        target.range,
        &CompletionClientCapabilities {
            markdown_docs: true,
            ..Default::default()
        },
    );
    let cache = engine.state.shared_disk.clone();
    cache.clear_full_cache();
    cache.clear_declaration_cache();
    let (full, light) = cache.projection_counts(&dep);
    let resolved: CompletionItem =
        serde_json::from_value(rich.resolve(2, proposal).result.unwrap()).unwrap();
    assert_eq!(cache.projection_counts(&dep), (full, light + 1));
    assert_eq!(resolved.documentation, baseline_docs);
    assert_eq!(resolved.text_edit, proposal.text_edit);
    assert_eq!(resolved.filter_text, proposal.filter_text);
    assert_eq!(resolved.sort_text, proposal.sort_text);
    assert_eq!(resolved.detail.as_deref(), Some("func Marshal(x int) int"));
    let Some(Documentation::MarkupContent(docs)) = resolved.documentation.as_ref() else {
        panic!("missing GoDoc")
    };
    assert_eq!(docs.kind, MarkupKind::Markdown);
    assert_eq!(
        docs.value,
        "Marshal writes the value.\n\nDeprecated: use Write."
    );
    let applied = apply(&text, &resolved).replace("$1", "1").replace("$0", "");
    assert!(applied.contains("import \"example.test/dep\""));
    assert!(applied.ends_with("var _ = dep.Marshal(1)"));
    assert_go_syntax(&mut rich.running().engine, applied);

    let mut legacy_proposal = proposal.clone();
    let legacy_data = legacy_proposal.data.as_mut().unwrap();
    legacy_data
        .as_object_mut()
        .unwrap()
        .remove("document_revision");
    legacy_data["target"]
        .as_object_mut()
        .unwrap()
        .remove("revision");
    let legacy: CompletionItem =
        serde_json::from_value(rich.resolve(4, &legacy_proposal).result.unwrap()).unwrap();
    assert!(legacy.documentation.is_none());
    assert_eq!(legacy.detail, resolved.detail);
    assert_eq!(legacy.additional_text_edits, resolved.additional_text_edits);

    std::fs::write(
        &dep,
        dependency_text.replace("writes", "encodes and writes"),
    )
    .unwrap();
    assert_eq!(rich.resolve(5, proposal).error.unwrap().code, -32801);
    std::fs::write(&dep, &dependency_text).unwrap();

    rich.replace(&text, 2);
    let stale = rich.resolve(3, proposal);
    assert_eq!(stale.error.unwrap().code, -32801);
    assert!(stale.result.is_none());

    let mut plain = LspSession::new(&path, json!({}));
    plain.open(&text);
    install(&mut plain.running().engine, workspace(), false);
    plain.complete(1, position);
    let proposals = plain.complete_results(1);
    let proposal = item(&proposals, "Marshal");
    assert_eq!(edit(proposal).new_text, "dep.Marshal()");
    assert_eq!(proposal.detail.as_deref(), Some("func Marshal(x int) int"));
    assert!(proposal.label_details.is_none());
    assert!(proposal.tags.is_none());
    assert!(proposal.deprecated.is_none());
    assert!(proposal.additional_text_edits.is_some());
    let Some(Documentation::MarkupContent(docs)) = proposal.documentation.as_ref() else {
        panic!("missing GoDoc")
    };
    assert_eq!(docs.kind, MarkupKind::PlainText);

    let mut partial = LspSession::new(&path, feature_caps(&["documentation"], false));
    partial.open(&text);
    install(&mut partial.running().engine, workspace(), false);
    partial.complete(1, position);
    let proposals = partial.complete_results(1);
    let proposal = item(&proposals, "Marshal");
    assert!(proposal.detail.is_some());
    assert!(proposal.documentation.is_none());
    assert!(proposal.additional_text_edits.is_some());
    let resolved: CompletionItem =
        serde_json::from_value(partial.resolve(2, proposal).result.unwrap()).unwrap();
    assert_eq!(
        proposal.additional_text_edits,
        resolved.additional_text_edits
    );
    assert!(matches!(resolved.documentation,
        Some(Documentation::MarkupContent(docs)) if docs.kind == MarkupKind::PlainText));
}

#[test]
fn lsp_resolve_uses_inline_member_docs_and_rejects_a_changed_overlay() {
    let project = TestProject::new();
    let path = project.write("sample.go", "package sample\n");
    let dependency = concat!(
        "package dep\n",
        "var Record struct {\n",
        "  /* 😀 */ Field int // Field is stored data.\n",
        "}\n",
    );
    let dep = project.write("dep/lib.go", dependency);
    let (text, position) = marked(concat!(
        "package sample\n",
        "import \"example.test/dep\"\n",
        "var _ = dep.Record.Fi¦\n",
    ));
    let mut lsp = LspSession::new(&path, feature_caps(&["documentation"], true));
    lsp.open(&text);
    install(
        &mut lsp.running().engine,
        listing(
            &project,
            &[
                pkg(
                    &project.root,
                    "example.test/sample",
                    "sample",
                    &["sample.go"],
                ),
                pkg(
                    dep.parent().unwrap(),
                    "example.test/dep",
                    "dep",
                    &["lib.go"],
                ),
            ],
            false,
        ),
        false,
    );
    lsp.complete(1, position);
    let proposals = lsp.complete_results(1);
    let proposal = item(&proposals, "Field");
    let data = serde_json::from_value::<ItemData>(proposal.data.clone().unwrap()).unwrap();
    let target = data.target.unwrap();
    assert_eq!(target.range.start, Position::new(2, 11));
    assert!(matches!(target.revision, Some(SourceRev::Disk(_))));
    let engine = &mut lsp.running().engine;
    let baseline = Source::parse(
        engine.snapshot().parser(),
        dep.clone(),
        dependency.into(),
        None,
    )
    .unwrap();
    let expected = markup(
        &baseline,
        target.range,
        &CompletionClientCapabilities {
            markdown_docs: true,
            ..Default::default()
        },
    );
    assert!(expected.is_some());
    let cache = engine.state.shared_disk.clone();
    cache.clear_full_cache();
    cache.clear_declaration_cache();
    let (full, light) = cache.projection_counts(&dep);
    let resolved: CompletionItem =
        serde_json::from_value(lsp.resolve(2, proposal).result.unwrap()).unwrap();
    assert_eq!(resolved.documentation, expected);
    assert_eq!(cache.projection_counts(&dep), (full, light + 1));

    lsp.running()
        .engine
        .open(dep.clone(), dependency.into(), 1)
        .unwrap();
    assert_eq!(lsp.resolve(3, proposal).error.unwrap().code, -32801);
    lsp.complete(4, position);
    let proposals = lsp.complete_results(4);
    let proposal = item(&proposals, "Field");
    let data = serde_json::from_value::<ItemData>(proposal.data.clone().unwrap()).unwrap();
    assert!(matches!(
        data.target.unwrap().revision,
        Some(SourceRev::Overlay { .. })
    ));
    lsp.running().engine.close(&dep);
    assert_eq!(lsp.resolve(5, proposal).error.unwrap().code, -32801);
}

#[test]
fn lsp_resolve_rejects_stale_document_graph_and_closed_buffers() {
    let (project, _engine, path) = engine();
    let (text, position) = marked("package sample\n// Do something.\nfunc Do() {}\nvar _ = D¦");
    let mut lsp = LspSession::new(&path, feature_caps(&["documentation"], false));
    lsp.open(&text);
    install(
        &mut lsp.running().engine,
        project.workspace(&["sample.go"], false),
        false,
    );
    lsp.complete(1, position);
    let completion = lsp.complete_results(1);
    let proposal = item(&completion, "Do");
    let _ = lsp.resolve(2, proposal).result.unwrap();
    lsp.notify(
        "textDocument/didClose",
        json!({"textDocument": {"uri": lsp.uri}}),
    );
    lsp.open(&text.replace("something", "something else"));
    assert_eq!(lsp.resolve(8, proposal).error.unwrap().code, -32801);
    lsp.replace(&text, 2);
    assert_eq!(lsp.resolve(3, proposal).error.unwrap().code, -32801);

    lsp.complete(4, position);
    let completion = lsp.complete_results(4);
    let proposal = item(&completion, "Do");
    let go_mod = crate::protocol::path_uri(&project.root.join("go.mod")).unwrap();
    lsp.notify(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": go_mod, "type": 2}]}),
    );
    lsp.flush_watches();
    assert_eq!(lsp.resolve(5, proposal).error.unwrap().code, -32801);

    install(
        &mut lsp.running().engine,
        project.workspace(&["sample.go"], false),
        false,
    );
    lsp.complete(6, position);
    let completion = lsp.complete_results(6);
    let proposal = item(&completion, "Do");
    lsp.notify(
        "textDocument/didClose",
        json!({"textDocument": {"uri": lsp.uri}}),
    );
    assert_eq!(lsp.resolve(7, proposal).error.unwrap().code, -32801);
}

#[test]
fn lsp_cancels_pending_and_batched_requests_and_rejects_outdated_batches() {
    let project = TestProject::new();
    let path = project.write("sample.go", "package sample\n");
    let mut files = vec![pkg(
        &project.root,
        "example.test/sample",
        "sample",
        &["sample.go"],
    )];
    for index in 0..24 {
        let folder = format!("dep{index}");
        let file = project.write(&format!("{folder}/lib.go"), "package dep\nfunc Export() {}");
        files.push(pkg(
            file.parent().unwrap(),
            &format!("example.test/{folder}"),
            "dep",
            &["lib.go"],
        ));
    }
    let workspace = || listing(&project, &files, false);
    let (text, position) = marked("package sample\nvar _ = Ex¦");
    let mut lsp = LspSession::new(&path, json!({}));
    lsp.open(&text);
    lsp.complete(1, position);
    lsp.step_completion();
    assert!(lsp.running().scheduler.completion_waiting_for_workspace());
    lsp.session.assert_no_messages();
    lsp.notify("$/cancelRequest", json!({"id": 1}));
    lsp.error(1, -32800);
    assert!(!lsp.running().scheduler.has_completion());

    lsp.complete(2, position);
    lsp.step_completion();
    let crate::State::Running(running) = &mut lsp.state else {
        panic!()
    };
    let load = running.engine.workspace_load();
    running
        .workspace_loaded(
            lsp.session.server(),
            crate::scheduler::WorkspaceResult::for_test(&running.engine, load, Ok(workspace())),
        )
        .unwrap();
    running.engine.completion_imports.use_empty_sdk(
        running.engine.state.completion_graph_generation,
        running.engine.state.workspace.as_ref().unwrap(),
    );
    lsp.start_completion_step();
    assert!(lsp.running().scheduler.has_completion());
    lsp.notify("$/cancelRequest", json!({"id": 2}));
    lsp.error(2, -32800);
    assert!(!lsp.running().scheduler.has_completion());
    lsp.notify("$/cancelRequest", json!({"id": 2}));
    lsp.session.assert_no_messages();
    lsp.step_completion();

    lsp.complete(3, position);
    lsp.replace(&text, 2);
    lsp.error(3, -32801);
    lsp.step_completion();

    lsp.complete(4, position);
    // A test-aware graph can replace the installed graph at the same load
    // generation. Its own completion revision still invalidates the task.
    install(&mut lsp.running().engine, workspace(), false);
    lsp.step_completion();
    lsp.error(4, -32801);

    lsp.complete(5, position);
    let items = lsp.complete_results(5);
    assert_eq!(
        items.iter().filter(|item| item.label == "Export").count(),
        24
    );
    lsp.session.assert_no_messages();
}

#[test]
fn lsp_completion_uses_unsaved_edits_close_restores_disk_and_graph_refresh_adds_packages() {
    let project = TestProject::new();
    let (text, position) = marked("package sample\nvar Disk int\nfunc f() { _ = ¦ }");
    let path = project.write("sample.go", &text);
    let mut lsp = LspSession::new(&path, json!({}));
    lsp.open(&text);
    install(
        &mut lsp.running().engine,
        project.workspace(&["sample.go"], false),
        false,
    );
    lsp.complete(1, position);
    assert_labels(&lsp.complete_results(1), &["Disk"], &["Live"]);
    lsp.replace(&text.replace("Disk", "Live"), 2);
    lsp.complete(2, position);
    assert_labels(&lsp.complete_results(2), &["Live"], &["Disk"]);
    lsp.notify(
        "textDocument/didClose",
        json!({"textDocument": {"uri": lsp.uri}}),
    );
    lsp.complete(3, position);
    assert_labels(&lsp.complete_results(3), &["Disk"], &["Live"]);

    let (text, position) = marked("package sample\nvar _ = Fres¦");
    lsp.open(&text);
    lsp.complete(4, position);
    assert_labels(&lsp.complete_results(4), &[], &["Fresh"]);
    let lib = project.write("fresh/lib.go", "package fresh\nfunc Fresh() {}");
    let uri = crate::protocol::path_uri(&lib).unwrap();
    lsp.notify(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri, "type": 1}]}),
    );
    lsp.flush_watches();
    lsp.complete(5, position);
    lsp.step_completion();
    assert!(lsp.running().scheduler.completion_waiting_for_workspace());
    let crate::State::Running(running) = &mut lsp.state else {
        panic!()
    };
    running
        .workspace_loaded(
            lsp.session.server(),
            crate::scheduler::WorkspaceResult::for_test(
                &running.engine,
                running.engine.workspace_load(),
                Ok(listing(
                    &project,
                    &[
                        pkg(
                            &project.root,
                            "example.test/sample",
                            "sample",
                            &["sample.go"],
                        ),
                        pkg(
                            lib.parent().unwrap(),
                            "example.test/fresh",
                            "fresh",
                            &["lib.go"],
                        ),
                    ],
                    false,
                )),
            ),
        )
        .unwrap();
    running.engine.completion_imports.use_empty_sdk(
        running.engine.state.completion_graph_generation,
        running.engine.state.workspace.as_ref().unwrap(),
    );
    assert_labels(&lsp.complete_results(5), &["Fresh"], &["Disk"]);
}

#[test]
fn dot_trigger_uses_incremental_utf16_buffer_edits() {
    let project = TestProject::new();
    let (text, position) = marked(concat!(
        "package sample\ntype Record struct{ Name string }\n",
        "func f(r Record) { _ = \"😀\"; _ = r¦\nnext()\n}",
    ));
    let path = project.write("sample.go", &text);
    let mut lsp = LspSession::new(&path, json!({}));
    lsp.open(&text);
    install(
        &mut lsp.running().engine,
        project.workspace(&["sample.go"], false),
        false,
    );
    lsp.notify(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": lsp.uri, "version": 2},
            "contentChanges": [{"range": lsp_types::Range::new(position, position), "text": "."}],
        }),
    );
    let position = Position::new(position.line, position.character + 1);
    lsp.request(
        1,
        "textDocument/completion",
        json!({
            "textDocument": {"uri": lsp.uri}, "position": position,
            "context": {"triggerKind": 2, "triggerCharacter": "."},
        }),
    );
    let results = lsp.complete_results(1);
    let field = item(&results, "Name");
    assert_eq!(edit(field).range, lsp_types::Range::new(position, position));
    assert_eq!(edit(field).new_text, "Name");
}

#[test]
fn applied_auto_import_alias_and_primary_edit_type_check_in_go() {
    let project = TestProject::new();
    project.write("go.mod", "module example.test/completion\n\ngo 1.20\n");
    project.write(
        "go-json/codec.go",
        "package codec\nfunc Marshal(x int) int { return x }\n",
    );
    let (text, position) = marked("package sample\nvar codec int\nvar Result = json.Mar¦(7)\n");
    let path = project.write("sample.go", &text);
    let workspace = Workspace::load(&project.root, false).unwrap();
    let mut engine = Engine::new(project.root.clone());
    engine.open(path.clone(), text.clone(), 1).unwrap();
    install(&mut engine, workspace, false);
    let values = items(
        &mut engine,
        &path,
        position,
        &CompletionClientCapabilities::default(),
    );
    let marshal = item(&values, "Marshal");
    assert_eq!(edit(marshal).new_text, "codec2.Marshal");
    let applied = apply(&text, marshal);
    assert!(applied.contains("import codec2 \"example.test/completion/go-json\""));
    project.write("sample.go", &applied);
    let output = std::process::Command::new("go")
        .args(["test", "./..."])
        .current_dir(&project.root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
