use std::path::PathBuf;

use lsp_types::{
    DocumentHighlight, DocumentHighlightKind, Position, Range, SemanticTokens,
    TextDocumentContentChangeEvent,
};

use super::legend;
use crate::semantic::{Engine, source::LineIndex};
use crate::test_support::TestProject;

const READ: DocumentHighlightKind = DocumentHighlightKind::READ;
const TEXT: DocumentHighlightKind = DocumentHighlightKind::TEXT;
const WRITE: DocumentHighlightKind = DocumentHighlightKind::WRITE;

fn open_fixture(source: &str) -> (TestProject, PathBuf, Engine) {
    let project = TestProject::new();
    let path = project.root.join("editor.go");
    let mut engine = Engine::new(project.root.clone());
    engine.open(path.clone(), source.into(), 1).unwrap();
    (project, path, engine)
}

fn position_at(source: &str, offset: usize) -> Position {
    let prefix = &source[..offset];
    let line = prefix.bytes().filter(|&byte| byte == b'\n').count();
    let line_prefix = prefix.rsplit('\n').next().unwrap();
    let units = line_prefix
        .strip_suffix('\r')
        .unwrap_or(line_prefix)
        .encode_utf16()
        .count();
    Position::new(line as u32, units as u32)
}

fn span(source: &str, context: &str, selected: &str) -> Range {
    let start = source
        .find(context)
        .unwrap_or_else(|| panic!("missing context {context:?}"));
    let offset = context
        .find(selected)
        .unwrap_or_else(|| panic!("missing {selected:?} in {context:?}"));
    let start = start + offset;
    Range::new(
        position_at(source, start),
        position_at(source, start + selected.len()),
    )
}

fn slice(source: &str, range: Range) -> &str {
    let lines = LineIndex::new(source);
    let start = lines.offset(source, range.start).unwrap();
    let end = lines.offset(source, range.end).unwrap();
    &source[start..end]
}

fn decode(source: &str, tokens: SemanticTokens) -> Vec<(Range, u32, u32)> {
    let mut result: Vec<(Range, u32, u32)> = Vec::new();
    let mut start = Position::default();
    for token in tokens.data {
        if token.delta_line == 0 {
            start.character += token.delta_start;
        } else {
            start.line += token.delta_line;
            start.character = token.delta_start;
        }
        let end = Position::new(start.line, start.character + token.length);
        let range = Range::new(start, end);
        let text = slice(source, range);
        assert!(start < end && !text.contains(['\r', '\n']));
        assert_eq!(text.encode_utf16().count(), token.length as usize);
        if let Some((previous, _, _)) = result.last() {
            assert!(previous.start < start && previous.end <= start);
        }
        result.push((range, token.token_type, token.token_modifiers_bitset));
    }
    result
}

fn assert_highlights(
    source: &str,
    actual: Vec<DocumentHighlight>,
    expected: &[(&str, &str, DocumentHighlightKind)],
) {
    let mut expected: Vec<_> = expected
        .iter()
        .map(|(context, selected, kind)| DocumentHighlight {
            range: span(source, context, selected),
            kind: Some(*kind),
        })
        .collect();
    expected.sort_by_key(|item| (item.range.start, item.range.end));
    assert_eq!(actual, expected);
}

#[test]
fn folds_nine_categories_and_projects_delimiters_and_regions() {
    use lsp_types::FoldingRangeKind;

    let source = concat!(
        "package p\n",
        "import \"fmt\"\nimport \"os\"\n",
        "// prior\n// region outer\nvar z = 0\n",
        "// <editor-fold>\n// inner\n// </editor-fold>\n",
        "var w = 0\n// endregion\n\n",
        "// first\n// second\n\n/* note */\n",
        "var (\n a = 1\n b = 2\n)\nconst (\n one = 1\n)\n",
        "type S struct {\n F interface {\n  M()\n }\n}\n",
        "var s = []int{\n 1,\n}\nvar raw = `first\n😀 last`\n",
        "func f(x any, c chan int) {\n",
        " switch 1 {\n case 1:\n  f(x, c)\n default:\n }\n",
        " switch x.(type) {\n case int: f(x, c)\n }\n",
        " select {\n case <-c:\n  f(x, c)\n }\n}\n",
    )
    .replace('\n', "\r\n");
    let (_project, path, mut engine) = open_fixture(&source);
    let columns = engine.folding_ranges(&path, Default::default()).unwrap();
    let starts: Vec<_> = columns
        .iter()
        .map(|fold| {
            (
                fold.start_line,
                fold.start_character.unwrap(),
                fold.kind.clone(),
            )
        })
        .collect();
    assert_eq!(
        starts,
        [
            (1, 7, Some(FoldingRangeKind::Imports)),
            (4, 0, Some(FoldingRangeKind::Region)),
            (6, 0, Some(FoldingRangeKind::Comment)),
            (12, 0, Some(FoldingRangeKind::Comment)),
            (15, 0, Some(FoldingRangeKind::Comment)),
            (16, 4, None),
            (23, 14, None),
            (24, 13, None),
            (28, 13, None),
            (31, 10, None),
            (33, 26, None),
            (34, 10, None),
            (35, 7, None),
            (39, 17, None),
            (40, 9, None),
            (42, 8, None),
            (43, 9, None),
        ]
    );
    let raw = columns.iter().find(|fold| fold.start_line == 31).unwrap();
    assert_eq!((raw.end_line, raw.end_character), (32, Some(8)));

    let lines = engine
        .folding_ranges(
            &path,
            super::EditorFoldingOptions {
                line_folding_only: true,
            },
        )
        .unwrap();
    assert!(lines.iter().all(|fold| fold.start_line < fold.end_line
        && fold.start_character.is_none()
        && fold.end_character.is_none()));
    for (start, end) in [(16, 18), (23, 26), (33, 45)] {
        assert!(
            lines
                .iter()
                .any(|fold| fold.start_line == start && fold.end_line == end)
        );
    }
}

#[test]
fn folding_uses_the_first_region_provider_in_source_order() {
    use lsp_types::FoldingRangeKind;

    let source = concat!(
        "package p\n",
        "func f() {\n",
        " // </editor-fold>\n",
        "}\n",
        "// region ignored\n",
        "var ignored = 1\n",
        "// endregion\n",
        "// <editor-fold desc=\"chosen\">\n",
        "var chosen = 2\n",
        "// </editor-fold>\n",
    );
    let (_project, path, mut engine) = open_fixture(source);
    let ranges: Vec<_> = engine
        .folding_ranges(&path, Default::default())
        .unwrap()
        .into_iter()
        .filter(|fold| fold.kind == Some(FoldingRangeKind::Region))
        .map(|fold| {
            (
                fold.start_line,
                fold.start_character,
                fold.end_line,
                fold.end_character,
            )
        })
        .collect();
    assert_eq!(ranges, [(7, Some(0), 9, Some(17))]);
}

#[test]
fn folding_compacts_single_statement_clauses_within_the_right_margin() {
    let long_value = "x".repeat(98);
    let unicode_prefix_value = "x".repeat(93);
    let source = format!(
        concat!(
            "package p\n",
            "func f(x int) {{\n",
            " switch x {{\n",
            " case len(\"🙂\"):\n",
            "  println(\"{}\")\n",
            "\tcase 1:\n",
            "  println(\"{}\")\n",
            " case 2:\n",
            "  x++\n",
            "  x--\n",
            " case 3:\n",
            " default: x++\n",
            " }}\n",
            "}}\n",
        ),
        unicode_prefix_value, long_value,
    );
    let (_project, path, mut engine) = open_fixture(&source);
    let columns: Vec<_> = engine
        .folding_ranges(&path, Default::default())
        .unwrap()
        .into_iter()
        .filter(|fold| (3..=11).contains(&fold.start_line))
        .map(|fold| {
            (
                fold.start_line,
                fold.start_character.unwrap(),
                fold.end_line,
                fold.end_character.unwrap(),
            )
        })
        .collect();
    let long_statement_end = source.lines().nth(6).unwrap().len() as u32;
    assert_eq!(
        columns,
        [
            (3, 15, 4, 2),
            (5, 8, 6, long_statement_end),
            (7, 8, 9, 5),
            (11, 8, 11, 10)
        ]
    );

    let lines: Vec<_> = engine
        .folding_ranges(
            &path,
            super::EditorFoldingOptions {
                line_folding_only: true,
            },
        )
        .unwrap()
        .into_iter()
        .filter(|fold| (3..=11).contains(&fold.start_line))
        .map(|fold| (fold.start_line, fold.end_line))
        .collect();
    assert_eq!(lines, [(3, 4), (5, 6), (7, 9)]);
}

#[test]
fn semantic_tokens_use_fixed_legend_and_resolve_roles_and_shadowing() {
    let source = concat!(
        "package sample\r\n",
        "import ns \"example.test/named\"\r\n",
        "import . \"example.test/dot\"\r\n",
        "import \"example.test/versioned/v2\"\r\n",
        "type Shape struct{ Field, Other int }\r\n",
        "type Alias = Shape\r\n",
        "type Face interface{ Do(code int) }\r\n",
        "type Regular int\r\n",
        "type Generic[T any] struct{ Item T }\r\n",
        "type Record struct{ Shape }\r\n",
        "const (K = iota; Step = K+1)\r\n",
        "var Global Alias\r\n",
        "var _ = iota\r\n",
        "func (recv *Shape) Do(code int) {}\r\n",
        "func local[T any](arg T, shape Shape, iface Face) (ret int) {\r\n",
        "  /* 😀 */ x := len(\"😀\")\r\n",
        "  x, next := x, 3\r\n",
        "  x += next\r\n",
        "  const localConst = false\r\n",
        "  _ = nil\r\n",
        "  _ = true\r\n",
        "  shape.Field = 3\r\n",
        "  _ = Shape{Other: 0}\r\n",
        "  shape.Do(2)\r\n",
        "  iface.Do(3)\r\n",
        "  _ = Alias{}\r\n",
        "  _ = ns.Unknown\r\n",
        "  _ = versioned.Unknown\r\n",
        "  ns := 1\r\n",
        "  _ = ns\r\n",
        "  Loop: for { break Loop }\r\n",
        "  _ = External\r\n",
        "  len := func(a int) int { return a }\r\n",
        "  _ = len(1)\r\n",
        "  { type int string; var shadow int; _ = shadow }\r\n",
        "  return\r\n",
        "}\r\n",
    );
    let (_project, path, mut engine) = open_fixture(source);
    let legend = legend();
    let types: Vec<_> = legend
        .token_types
        .iter()
        .map(|item| item.as_str())
        .collect();
    let modifiers: Vec<_> = legend
        .token_modifiers
        .iter()
        .map(|item| item.as_str())
        .collect();
    assert_eq!(
        types,
        [
            "namespace",
            "type",
            "struct",
            "interface",
            "typeParameter",
            "parameter",
            "variable",
            "property",
            "function",
            "method",
            "label",
        ]
    );
    assert_eq!(
        modifiers,
        ["declaration", "readonly", "defaultLibrary", "modification"]
    );
    let tokens = decode(source, engine.semantic_tokens_full(&path).unwrap());
    let token_at = |context, selected| {
        let range = span(source, context, selected);
        tokens
            .iter()
            .find(|token| token.0 == range)
            .map(|(_, kind, mods)| (types[*kind as usize], *mods))
    };
    const DECL: u32 = 1;
    const READONLY: u32 = 2;
    const DEFAULT: u32 = 4;
    const WRITE: u32 = 8;
    for (context, name, kind, mods) in [
        ("package sample", "sample", "namespace", DECL),
        ("import ns", "ns", "namespace", DECL),
        ("import .", ".", "namespace", DECL),
        ("type Shape", "Shape", "struct", DECL),
        ("type Alias", "Alias", "type", DECL),
        ("= Shape\r\n", "Shape", "struct", 0),
        ("type Face", "Face", "interface", DECL),
        ("type Regular", "Regular", "type", DECL),
        ("Generic[T any]", "T", "typeParameter", DECL),
        ("Item T", "T", "typeParameter", 0),
        ("Record struct{ Shape", "Shape", "property", DECL),
        ("Field, Other", "Field", "property", DECL),
        ("Other int", "Other", "property", DECL),
        ("K = iota", "K", "variable", DECL | READONLY),
        ("K = iota", "iota", "variable", DEFAULT | READONLY),
        ("Step = K+1", "K", "variable", READONLY),
        ("Global Alias", "Global", "variable", DECL),
        ("Global Alias", "Alias", "type", 0),
        ("recv *Shape", "recv", "parameter", DECL),
        ("Do(code int) {}", "Do", "method", DECL),
        ("Do(code int) }", "Do", "method", DECL),
        ("func local", "local", "function", DECL),
        ("arg T", "arg", "parameter", DECL),
        ("ret int", "ret", "parameter", DECL),
        ("ret int", "int", "type", DEFAULT),
        ("/* 😀 */ x", "x", "variable", DECL),
        ("len(\"😀\")", "len", "function", DEFAULT),
        ("x, next :=", "x", "variable", WRITE),
        ("x, next :=", "next", "variable", DECL),
        ("x += next", "x", "variable", WRITE),
        (
            "localConst = false",
            "false",
            "variable",
            DEFAULT | READONLY,
        ),
        ("_ = nil", "nil", "variable", DEFAULT),
        ("_ = true", "true", "variable", DEFAULT | READONLY),
        ("shape.Field =", "Field", "property", WRITE),
        ("Shape{Other: 0}", "Other", "property", WRITE),
        ("shape.Do(2)", "Do", "method", 0),
        ("iface.Do(3)", "Do", "method", 0),
        ("Alias{}", "Alias", "type", 0),
        ("ns.Unknown", "ns", "namespace", 0),
        ("versioned.Unknown", "versioned", "namespace", 0),
        ("ns := 1", "ns", "variable", DECL),
        ("_ = ns\r\n", "ns", "variable", 0),
        ("Loop: for", "Loop", "label", DECL),
        ("break Loop", "Loop", "label", 0),
        ("len(1)", "len", "variable", 0),
        ("var shadow int", "int", "type", 0),
    ] {
        assert_eq!(
            token_at(context, name),
            Some((kind, mods)),
            "{context:?} {name:?}"
        );
    }
    for (context, unresolved) in [
        ("var _ = iota", "iota"),
        ("ns.Unknown", "Unknown"),
        ("versioned.Unknown", "Unknown"),
        ("_ = External", "External"),
    ] {
        assert_eq!(token_at(context, unresolved), None, "{context:?}");
    }
    let utf16 = span(source, "/* 😀 */ x", "x");
    assert_eq!(utf16.start.character, 11);
    assert_eq!(token_at("/* 😀 */ x", "x"), Some(("variable", DECL)));
}

#[test]
fn fields_bind_in_implicit_literals_and_inferred_selector_chains() {
    let source = concat!(
        "package p\n",
        "type Leaf struct{ Value int }\n",
        "type Pocket struct{ Leaf Leaf }\n",
        "var _ = []Leaf{{Value: 1}}\n",
        "var _ = [1]Leaf{{Value: 2}}\n",
        "var _ = map[int]*Leaf{0: {Value: 3}}\n",
        "var _ = Pocket{Leaf: {Value: 4}}\n",
        "var _ = Pocket{{Value: 5}}\n",
        "func newLeaf() *Leaf { return &Leaf{} }\n",
        "func f() {\n",
        "  leaves := []Leaf{{Value: 6}}\n",
        "  leaves[0].Value++\n",
        "  field := newLeaf()\n",
        "  _ = field.Value\n",
        "}\n",
    );
    let (_project, path, mut engine) = open_fixture(source);
    let result = engine
        .document_highlight(&path, span(source, "field.Value", "Value").start)
        .unwrap();
    assert_highlights(
        source,
        result,
        &[
            ("Value int", "Value", READ),
            ("Value: 1", "Value", WRITE),
            ("Value: 2", "Value", WRITE),
            ("Value: 3", "Value", WRITE),
            ("Value: 4", "Value", WRITE),
            ("Value: 5", "Value", WRITE),
            ("Value: 6", "Value", WRITE),
            ("leaves[0].Value++", "Value", WRITE),
            ("field.Value", "Value", READ),
        ],
    );
}

#[test]
fn promoted_members_require_a_unique_single_file_path() {
    let source = concat!(
        "package p\nimport ext \"example.test/elsewhere\"\n",
        "type Inner struct{ F int }\n",
        "func (Inner) Hit(amount int) {}\n",
        "type Via struct{ Inner }\n",
        "type Another struct{ Inner }\n",
        "type Deep struct{ Via }\n",
        "type Ambiguous struct{ Via; Another }\n",
        "type Unproven struct{ Via; ext.T }\n",
        "func f(d Deep, a Ambiguous, u Unproven) {\n",
        "    d.F++; d.Hit(7); _ = a.F; _ = u.F\n}",
    );
    let (_project, path, mut engine) = open_fixture(source);
    let highlighted = engine
        .document_highlight(&path, span(source, "d.F++", "F").start)
        .unwrap();
    assert_highlights(
        source,
        highlighted,
        &[("F int", "F", READ), ("d.F++", "F", WRITE)],
    );
    for context in ["a.F", "u.F"] {
        let position = span(source, context, "F").start;
        assert!(
            engine
                .document_highlight(&path, position)
                .unwrap()
                .is_empty()
        );
    }
    let argument = span(source, "d.Hit(7)", "7");
    let hints = engine.inlay_hints(&path, argument).unwrap();
    assert_eq!(hints.len(), 1);
    assert!(
        matches!(&hints[0].label, lsp_types::InlayHintLabel::String(label) if label == "amount:")
    );
}

#[test]
fn labelled_short_declarations_reuse_bindings_from_the_same_block() {
    let source = concat!(
        "package p\nimport . \"example.test/dot\"\n",
        "func f(arg int) {\n",
        "  Again: arg, extra := arg, 1\n",
        "  _ = extra + arg\n",
        "  goto Again\n",
        "}\n",
    );
    let (_project, path, mut engine) = open_fixture(source);
    let reused = span(source, "Again: arg, extra", "arg");
    let tokens = decode(source, engine.semantic_tokens_full(&path).unwrap());
    assert!(tokens.contains(&(reused, 5, 1 << 3)));
    let highlighted = engine.document_highlight(&path, reused.start).unwrap();
    assert_highlights(
        source,
        highlighted,
        &[
            ("arg int", "arg", READ),
            ("Again: arg, extra", "arg", WRITE),
            (":= arg, 1", "arg", READ),
            ("extra + arg", "arg", READ),
        ],
    );
    let dot = span(source, "import .", ".");
    assert!(
        engine
            .document_highlight(&path, dot.start)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn document_highlights_classify_reads_and_writes_on_precise_operands() {
    let source = concat!(
        "package p\n",
        "type Box struct{ Field int }\n",
        "func (recv *Box) update(arg int, p *int, a []int, ch chan int) (out int) {\n",
        "  var x = arg\n",
        "  const key = 0\n",
        "  x, y := x, arg\n",
        "  (x) = y\n",
        "  x += y\n",
        "  x++\n",
        "  *p = y\n",
        "  a[key] = y\n",
        "  _ = map[int]int{key: 1}\n",
        "  _ = Box{Field: y}\n",
        "  recv.Field = y\n",
        "  _ = recv.Field\n",
        "  consume(&x)\n",
        "  ch <- y\n",
        "  _ = <-ch\n",
        "  for x, y = range a { x-- }\n",
        "  x = <-ch\n",
        "  select { case x = <-ch: case item, ok := <-ch: _, _ = item, ok }\n",
        "  { x := arg; _ = x }\n",
        "  out = x\n",
        "  return\n",
        "}\n",
    );
    let (_project, path, mut engine) = open_fixture(source);
    let mut check = |context, selected, expected: &[(&str, &str, DocumentHighlightKind)]| {
        let actual = engine
            .document_highlight(&path, span(source, context, selected).start)
            .unwrap();
        assert_highlights(source, actual, expected);
    };
    check(
        "(x) = y",
        "x",
        &[
            ("var x = arg", "x", WRITE),
            ("x, y :=", "x", WRITE),
            (":= x, arg", "x", READ),
            ("(x) = y", "x", WRITE),
            ("x += y", "x", WRITE),
            ("x++", "x", WRITE),
            ("consume(&x)", "x", READ),
            ("for x, y", "x", WRITE),
            ("x--", "x", WRITE),
            ("  x = <-ch", "x", WRITE),
            ("case x = <-ch", "x", WRITE),
            ("out = x", "x", READ),
        ],
    );
    check(
        "key: 1",
        "key",
        &[
            ("const key", "key", WRITE),
            ("a[key]", "key", READ),
            ("key: 1", "key", READ),
        ],
    );
    check(
        "Field: y",
        "Field",
        &[
            ("Field int", "Field", READ),
            ("Field: y", "Field", WRITE),
            ("recv.Field = y", "Field", WRITE),
            ("_ = recv.Field", "Field", READ),
        ],
    );
    check(
        "recv.Field = y",
        "recv",
        &[
            ("recv *Box", "recv", READ),
            ("recv.Field = y", "recv", READ),
            ("_ = recv.Field", "recv", READ),
        ],
    );
    check(
        "*p = y",
        "p",
        &[("p *int", "p", READ), ("*p = y", "p", WRITE)],
    );
    check(
        "a[key]",
        "a",
        &[
            ("a []int", "a", READ),
            ("a[key]", "a", READ),
            ("a { x--", "a", READ),
        ],
    );
    check(
        "ch <- y",
        "ch",
        &[
            ("ch chan int", "ch", READ),
            ("ch <- y", "ch", WRITE),
            ("_ = <-ch", "ch", READ),
            ("  x = <-ch", "ch", READ),
            ("case x = <-ch", "ch", READ),
            ("case item, ok := <-ch", "ch", READ),
        ],
    );
    check(
        "case item, ok",
        "item",
        &[
            ("case item, ok", "item", WRITE),
            ("= item, ok", "item", READ),
        ],
    );
    check(
        "out int",
        "out",
        &[("out int", "out", READ), ("out = x", "out", WRITE)],
    );
}

#[test]
fn document_highlights_do_not_merge_unresolved_or_shadowed_names() {
    let source = concat!(
        "package p\n",
        "import lib \"example.test/library\"\n",
        "type T struct {}\n",
        "func (t T) Method() {}\n",
        "func f(t T, arg int) {\n",
        "  arg, added := arg, 1\n",
        "  _ = added\n",
        "  { arg := 2; _ = arg }\n",
        "  t.Method()\n",
        "  _ = lib.Unresolved\n",
        "  lib := t\n",
        "  lib.Method()\n",
        "  discard(Unresolved, Unresolved)\n",
        "}\n",
    );
    let (project, path, mut engine) = open_fixture(source);
    project.write("editor.go", source);
    project.write("sibling.go", "package p\nconst Unresolved = 1\n");
    let token_result = engine.semantic_tokens_full(&path).unwrap();
    let mut check = |context, selected, expected: &[(&str, &str, DocumentHighlightKind)]| {
        let actual = engine
            .document_highlight(&path, span(source, context, selected).start)
            .unwrap();
        assert_highlights(source, actual, expected);
    };
    check(
        "arg, added :=",
        "arg",
        &[
            ("arg int", "arg", READ),
            ("arg, added :=", "arg", WRITE),
            (":= arg, 1", "arg", READ),
        ],
    );
    check(
        "arg := 2",
        "arg",
        &[("arg := 2", "arg", WRITE), ("_ = arg }", "arg", READ)],
    );
    check(
        "lib.Unresolved",
        "lib",
        &[("import lib", "lib", TEXT), ("lib.Unresolved", "lib", TEXT)],
    );
    check(
        "lib := t",
        "lib",
        &[("lib := t", "lib", WRITE), ("lib.Method()", "lib", READ)],
    );
    check(
        "t.Method()",
        "Method",
        &[
            ("Method() {}", "Method", TEXT),
            ("t.Method()", "Method", TEXT),
            ("lib.Method()", "Method", TEXT),
        ],
    );
    check("lib.Unresolved", "Unresolved", &[]);
    check("discard(Unresolved", "Unresolved", &[]);
    check("Unresolved)", "Unresolved", &[]);
    assert!(engine.install_workspace(
        engine.workspace_load(),
        project.workspace(&["editor.go", "sibling.go"], false)
    ));
    assert_eq!(token_result, engine.semantic_tokens_full(&path).unwrap());
    let cursor = span(source, "discard(Unresolved", "Unresolved").start;
    assert!(engine.document_highlight(&path, cursor).unwrap().is_empty());
}

#[test]
fn highlights_lexical_initializers_case_bindings_and_closure_captures() {
    let source = concat!(
        "package p\n",
        "const step = 9\n",
        "type Box[T any] struct { field T }\n",
        "func (recv Box[R]) f(ch chan int, value any) {\n",
        "  var member R\n",
        "  _ = recv.field\n",
        "  _ = member\n",
        "  const captured = 100\n",
        "  _ = func() { _ = captured + 1; _ = recv.field }\n",
        "  { const captured = 200; _ = captured + 2 }\n",
        "  if step := 1; step < 3 { step++ } else { _ = step + 4 }\n",
        "  for step := 5; step < 6; step++ { _ = step + 7 }\n",
        "  switch step := 8; step { case 8: _ = step + 9 }\n",
        "  for index := range ch { _ = index + 10 }\n",
        "  select {\n",
        "  case item := <-ch:\n",
        "    item, count := item + 11, 1\n",
        "    _, _ = func() int { return item + 12 }, count\n",
        "  case item := <-ch:\n",
        "    _ = item + 13\n",
        "  }\n",
        "  switch bound := value.(type) {\n",
        "  case int: _ = bound + 14\n",
        "  default: _ = func() { _ = bound }\n",
        "  }\n",
        "  _ = step + 15\n",
        "}\n",
    );
    let (_project, path, mut engine) = open_fixture(source);
    let mut check = |context, selected, expected: &[(&str, &str, DocumentHighlightKind)]| {
        let result = engine
            .document_highlight(&path, span(source, context, selected).start)
            .unwrap();
        assert_highlights(source, result, expected);
    };
    check(
        "captured + 1",
        "captured",
        &[
            ("const captured = 100", "captured", WRITE),
            ("captured + 1", "captured", READ),
        ],
    );
    check(
        "Box[R]",
        "R",
        &[("Box[R]", "R", TEXT), ("member R", "R", TEXT)],
    );
    check(
        "recv Box",
        "recv",
        &[
            ("recv Box", "recv", READ),
            ("_ = recv.field\n", "recv", READ),
            ("_ = recv.field }", "recv", READ),
        ],
    );
    check(
        "field T",
        "T",
        &[("Box[T any]", "T", TEXT), ("field T", "T", TEXT)],
    );
    check(
        "step + 4",
        "step",
        &[
            ("step := 1", "step", WRITE),
            ("step < 3", "step", READ),
            ("{ step++ }", "step", WRITE),
            ("step + 4", "step", READ),
        ],
    );
    check(
        "step < 6",
        "step",
        &[
            ("step := 5", "step", WRITE),
            ("step < 6", "step", READ),
            ("step++ {", "step", WRITE),
            ("step + 7", "step", READ),
        ],
    );
    check(
        "step + 9",
        "step",
        &[
            ("step := 8", "step", WRITE),
            ("; step {", "step", READ),
            ("step + 9", "step", READ),
        ],
    );
    check(
        "index + 10",
        "index",
        &[
            ("index := range", "index", WRITE),
            ("index + 10", "index", READ),
        ],
    );
    check(
        "item + 11",
        "item",
        &[
            ("item := <-ch:", "item", WRITE),
            ("item, count :=", "item", WRITE),
            ("item + 11", "item", READ),
            ("item + 12", "item", READ),
        ],
    );
    check(
        "bound + 14",
        "bound",
        &[
            ("bound :=", "bound", WRITE),
            ("bound + 14", "bound", READ),
            ("_ = bound }", "bound", READ),
        ],
    );
    check(
        "step + 15",
        "step",
        &[("const step", "step", WRITE), ("step + 15", "step", READ)],
    );
}

#[test]
fn highlights_predeclared_constants_through_the_same_name_index() {
    let source = concat!(
        "package p\n",
        "const (a = iota; b = iota + 1)\n",
        "var _ = []any{nil, true, false}\n",
        "func f() {\n",
        "  _, _, _ = true, false, nil\n",
        "  { true := 1; _ = true + 2 }\n",
        "  _ = iota\n",
        "}\n",
    );
    let (_project, path, mut engine) = open_fixture(source);
    let mut check = |context, selected, expected: &[(&str, &str, DocumentHighlightKind)]| {
        let result = engine
            .document_highlight(&path, span(source, context, selected).start)
            .unwrap();
        assert_highlights(source, result, expected);
    };
    check(
        "a = iota",
        "iota",
        &[("a = iota", "iota", READ), ("b = iota", "iota", READ)],
    );
    for name in ["nil", "true", "false"] {
        check(
            "{nil, true, false}",
            name,
            &[
                ("{nil, true, false}", name, READ),
                ("= true, false, nil", name, READ),
            ],
        );
    }
    check("  _ = iota", "iota", &[]);
}

#[test]
fn a_cancelled_highlight_computation_produces_no_result() {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::{Editor, document_highlight};

    let source = "package p\nconst target = 42\nvar _ = target\n";
    let (_project, path, mut engine) = open_fixture(source);
    let mut snapshot = engine.snapshot();
    let document = snapshot.document(&path).unwrap().unwrap();
    let editor = Editor::new(&document);
    let target =
        document_highlight::prepare(&editor, span(source, "const target", "target").start).unwrap();
    let cancelled = AtomicBool::new(true);
    assert!(document_highlight::compute(&editor, target, Some(&cancelled)).is_none());
    cancelled.store(false, Ordering::Relaxed);
    let (result, candidates) =
        document_highlight::compute(&editor, target, Some(&cancelled)).unwrap();
    assert_eq!(candidates, 2);
    assert_highlights(
        source,
        result,
        &[
            ("const target", "target", WRITE),
            ("_ = target", "target", READ),
        ],
    );
}

#[test]
fn document_highlights_include_the_implicit_import_path() {
    let source = concat!(
        "package p\r\n",
        "import (\r\n",
        "  \"strings\"\r\n",
        "  strconv \"strconv\"\r\n",
        ")\r\n",
        "func f() {\r\n",
        "  _ = \"\u{1f642}\"; _ = strings.ToUpper(\"name\")\r\n",
        "  _ = strings.ToLower(\"name\")\r\n",
        "  _ = strconv.Itoa(1)\r\n",
        "  { strings := \"\"; _ = strings }\r\n",
        "}\r\n",
    );
    let (_project, path, mut engine) = open_fixture(source);
    let mut check = |context, selected, expected: &[(&str, &str, DocumentHighlightKind)]| {
        let actual = engine
            .document_highlight(&path, span(source, context, selected).start)
            .unwrap();
        assert_highlights(source, actual, expected);
    };
    check(
        "strings.ToUpper",
        "strings",
        &[
            ("\"strings\"", "strings", TEXT),
            ("strings.ToUpper", "strings", TEXT),
            ("strings.ToLower", "strings", TEXT),
        ],
    );
    check(
        "strconv.Itoa",
        "strconv",
        &[
            ("strconv \"strconv\"", "strconv", TEXT),
            ("strconv.Itoa", "strconv", TEXT),
        ],
    );
    check(
        "_ = strings }",
        "strings",
        &[
            ("strings :=", "strings", WRITE),
            ("_ = strings }", "strings", READ),
        ],
    );
}

#[test]
fn highlights_utf8_names_across_cold_offset_scan_boundaries() {
    let prefix = "package p\n//";
    let declaration = "\nconst ";
    let padding = "a".repeat(65_535 - prefix.len() - declaration.len());
    let source = format!(
        "{prefix}{padding}{declaration}界名 = 1\nvar _ = 界名\nvar _ = \"界名\"\nvar 界名字 = 2\n"
    );
    let (_project, path, mut engine) = open_fixture(&source);
    let usage = span(&source, "_ = 界名", "界名");
    assert_highlights(
        &source,
        engine.document_highlight(&path, usage.start).unwrap(),
        &[("const 界名", "界名", WRITE), ("_ = 界名", "界名", READ)],
    );
}

#[test]
fn exit_highlights_use_nearest_function_and_prune_return_expressions() {
    let source = concat!(
        "package p\n",
        "func run() int {\n",
        "  defer panic(11)\n",
        "  go panic(12)\n",
        "  if true { panic(13); return 14 }\n",
        "  { panic := func(v int) { return }; panic(15) }\n",
        "  _ = func() int { panic(16); return 17 }\n",
        "  return pack(panic(18))\n",
        "}\n",
    );
    let (_project, path, mut engine) = open_fixture(source);
    let outer = [
        ("func run", "func", TEXT),
        ("defer panic(11)", "panic(11)", TEXT),
        ("go panic(12)", "panic(12)", TEXT),
        ("panic(13)", "panic(13)", TEXT),
        ("return 14", "return 14", TEXT),
        ("return pack(panic(18))", "return pack(panic(18))", TEXT),
    ];
    for cursor in [
        span(source, "func run", "func").end,
        span(source, "panic(11)", "panic").start,
        span(source, "return 14", "return").end,
    ] {
        assert_highlights(
            source,
            engine.document_highlight(&path, cursor).unwrap(),
            &outer,
        );
    }
    assert_highlights(
        source,
        engine
            .document_highlight(&path, span(source, "panic(16)", "panic").start)
            .unwrap(),
        &[
            ("func() int { panic(16)", "func", TEXT),
            ("panic(16)", "panic(16)", TEXT),
            ("return 17", "return 17", TEXT),
        ],
    );
    assert_highlights(
        source,
        engine
            .document_highlight(&path, span(source, "panic(15)", "panic").start)
            .unwrap(),
        &[
            ("panic :=", "panic", DocumentHighlightKind::WRITE),
            ("panic(15)", "panic", DocumentHighlightKind::READ),
        ],
    );
}

#[test]
fn jump_highlights_separate_labels_and_naked_targets_and_stop_at_functions() {
    let source = concat!(
        "package p\n",
        "func f(arg any, ch chan int) {\n",
        "  Top: for {\n",
        "    switch arg { case 0: break; case 1: continue }\n",
        "    switch arg.(type) { case int: break }\n",
        "    select { case <-ch: break; default: break Top }\n",
        "    if true { continue Top }\n",
        "    _ = func() { for { break }; goto Top }\n",
        "    break\n",
        "  }\n",
        "  First: Next: _ = arg\n",
        "  goto First\n",
        "  goto Next\n",
        "  _ = func() { goto First }\n",
        "  _: for { break _ }\n",
        "}\n",
    );
    let (_project, path, mut engine) = open_fixture(source);
    let mut check = |context, selected, expected: &[(&str, &str, DocumentHighlightKind)]| {
        let actual = engine
            .document_highlight(&path, span(source, context, selected).start)
            .unwrap();
        assert_highlights(source, actual, expected);
    };
    check(
        "Top: for",
        "for",
        &[
            ("Top: for", "for", TEXT),
            ("case 1: continue", "continue", TEXT),
            ("    break\n", "break", TEXT),
        ],
    );
    check(
        "case 1: continue",
        "continue",
        &[
            ("Top: for", "for", TEXT),
            ("case 1: continue", "continue", TEXT),
            ("    break\n", "break", TEXT),
        ],
    );
    check(
        "break Top",
        "break",
        &[
            ("Top: for", "Top", TEXT),
            ("break Top", "break Top", TEXT),
            ("continue Top", "continue Top", TEXT),
        ],
    );
    check(
        "switch arg {",
        "switch",
        &[
            ("switch arg {", "switch", TEXT),
            ("case 0: break", "break", TEXT),
        ],
    );
    check(
        "switch arg.(type)",
        "switch",
        &[
            ("switch arg.(type)", "switch", TEXT),
            ("case int: break", "break", TEXT),
        ],
    );
    check(
        "select {",
        "select",
        &[("select {", "select", TEXT), ("<-ch: break", "break", TEXT)],
    );
    check(
        "break Top",
        "Top",
        &[
            ("Top: for", "Top", TEXT),
            ("break Top", "Top", TEXT),
            ("continue Top", "Top", TEXT),
        ],
    );
    check(
        "goto First\n",
        "First",
        &[
            ("First: Next:", "First", TEXT),
            ("goto First\n", "First", TEXT),
        ],
    );
    check(
        "goto Next",
        "Next",
        &[("Next: _", "Next", TEXT), ("goto Next", "Next", TEXT)],
    );
    check("goto First }", "First", &[]);
    check("break _", "break", &[]);
}

#[test]
fn return_slot_highlights_follow_result_positions_and_overlay_named_results() {
    let source = concat!(
        "package p\n",
        "func pair() (int, string) { return 7, \"fresh\" }\n",
        "func named(x int) (first int, last string) {\n",
        "  first = x\n",
        "  last = \"v\"\n",
        "  _ = func() string { return \"nested\" }\n",
        "  if x > 1 { return 10, \"outer\" }\n",
        "  if x > 2 { return pair() }\n",
        "  if x > 3 { return x + 1, pick(\"arg\") }\n",
        "  return\n",
        "}\n",
        "func anon() []int { return []int{30} }\n",
        "func bodyless() []int\n",
    );
    let (_project, path, mut engine) = open_fixture(source);
    let mut check = |context, selected, expected: &[(&str, &str, DocumentHighlightKind)]| {
        let actual = engine
            .document_highlight(&path, span(source, context, selected).start)
            .unwrap();
        assert_highlights(source, actual, expected);
    };
    check(
        "last string",
        "last",
        &[
            ("last string", "last", READ),
            ("last = \"v\"", "last", WRITE),
            ("return 10, \"outer\"", "\"outer\"", TEXT),
            ("return pair()", "pair()", TEXT),
            ("pick(\"arg\")", "pick(\"arg\")", TEXT),
        ],
    );
    check(
        "last = \"v\"",
        "last",
        &[
            ("last string", "last", READ),
            ("last = \"v\"", "last", WRITE),
        ],
    );
    check(
        "return 10",
        "10",
        &[
            ("first int", "int", TEXT),
            ("return 10", "10", TEXT),
            ("return pair()", "pair()", TEXT),
            ("x + 1", "x + 1", TEXT),
        ],
    );
    check("pick(\"arg\")", "arg", &[]);
    check(
        "anon() []int",
        "[",
        &[
            ("anon() []int", "[]int", TEXT),
            ("[]int{30}", "[]int{30}", TEXT),
        ],
    );
    check("[]int{30}", "30", &[]);
    check("bodyless() []int", "[", &[]);
}

#[test]
fn utf16_highlights_handle_comments_eof_and_recovered_overlays() {
    let disk = "package p\r\nvar Disk int\r\n";
    let source = concat!(
        "package p\r\n",
        "var Current int\r\n",
        "func unfinished() {\r\n",
        "  /* 😀 Current */ _ = Current",
    );
    let (project, path, mut engine) = open_fixture(source);
    project.write("editor.go", disk);
    let comment_position = span(source, "/* 😀 Current", "Current").start;
    assert!(
        engine
            .document_highlight(&path, comment_position)
            .unwrap()
            .is_empty()
    );
    let reference = span(source, "_ = Current", "Current");
    assert_eq!(
        reference,
        Range::new(Position::new(3, 23), Position::new(3, 30))
    );
    assert_highlights(
        source,
        engine.document_highlight(&path, reference.end).unwrap(),
        &[
            ("var Current", "Current", WRITE),
            ("_ = Current", "Current", READ),
        ],
    );
    assert!(
        decode(source, engine.semantic_tokens_full(&path).unwrap())
            .iter()
            .any(|token| token.0 == reference)
    );

    let update = source.replace("Current", "Edited");
    engine
        .edit(
            &path,
            &[TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: update.clone(),
            }],
            2,
        )
        .unwrap();
    let edited = engine
        .document_highlight(&path, span(&update, "_ = Edited", "Edited").end)
        .unwrap();
    assert_highlights(
        &update,
        edited,
        &[
            ("var Edited", "Edited", WRITE),
            ("_ = Edited", "Edited", READ),
        ],
    );
    engine.close(&path);
    let restored = engine
        .document_highlight(&path, span(disk, "var Disk", "Disk").end)
        .unwrap();
    assert_highlights(disk, restored, &[("var Disk", "Disk", WRITE)]);
    let tokens = decode(disk, engine.semantic_tokens_full(&path).unwrap());
    assert_eq!(
        tokens
            .iter()
            .map(|token| slice(disk, token.0))
            .collect::<Vec<_>>(),
        ["p", "Disk", "int"]
    );
}

mod inlays {
    use lsp_types::{InlayHint, InlayHintKind, InlayHintLabel, Position, Range};

    use super::{open_fixture, position_at};

    fn position(source: &str, marker: &str) -> Position {
        let (before, after) = marker.split_once('¦').expect("missing anchor");
        let offset = source
            .find(&format!("{before}{after}"))
            .unwrap_or_else(|| panic!("missing {marker:?}"));
        position_at(source, offset + before.len())
    }

    fn all() -> Range {
        Range::new(Position::new(0, 0), Position::new(1000, 0))
    }

    fn summary(hints: &[InlayHint]) -> Vec<(Position, String)> {
        hints
            .iter()
            .map(|hint| {
                let InlayHintLabel::String(label) = &hint.label else {
                    panic!("expected a plain label")
                };
                if label.starts_with("= ") {
                    assert_eq!(hint.kind, None);
                    assert_eq!(hint.padding_left, Some(true));
                    assert_eq!(hint.padding_right, None);
                } else {
                    assert_eq!(hint.kind, Some(InlayHintKind::PARAMETER));
                    assert_eq!(hint.padding_left, None);
                    assert_eq!(hint.padding_right, Some(true));
                }
                (hint.position, label.clone())
            })
            .collect()
    }

    fn expected(source: &str, hints: &[(&str, &str)]) -> Vec<(Position, String)> {
        let mut hints: Vec<_> = hints
            .iter()
            .map(|(marker, label)| (position(source, marker), (*label).to_owned()))
            .collect();
        hints.sort();
        hints
    }

    #[test]
    fn call_parameters_follow_signatures_direct_children_and_half_open_range() {
        let source = r#"package p
type Record struct { Field int }
func group(first, second int, _ int, tail ...int) {}
func unique(...int) {}
func unnamed(int) {}
func pair(note string, value int) {}
func (record Record) use(times int) {}
func (record Record) sum(first int, more ...int) {}
func test(x int, r Record, dst []byte) {
    group(1 + 2, x + 3, 4, 5, 6)
    group((x + 7), 8, 9, 10)
    group(-x, -(x), 0)
    f := func(when int) {}
    f(11)
    var g func(elapsed int)
    g(12)
    (func(instant int) {})(13)
    r.use(14)
    m := r.use
    m(15)
    Record.use(r, 16)
    Record.use(Record{}, 32)
    Record.sum(r, 33, 34, 35)
    unbound := Record.use
    unbound(r, 36)
    pair("😀", 17)
    unknown(18)
    copy(dst, "abc")
    _ = append(dst, 19, 20)
    _ = min(21, 22, 23)
    panic(nil)
    _ = len("xyz")
    len := func(local string) int { return 0 }
    _ = len("shadow")
    _ = new(int)
    _ = make([]int, 27)
    unique(29, 30)
    unnamed(31)
}
"#;
        let source = source.replace('\n', "\r\n");
        let (_project, path, mut engine) = open_fixture(&source);
        let hints = engine.inlay_hints(&path, all()).unwrap();
        assert_eq!(
            summary(&hints),
            expected(
                &source,
                &[
                    ("group(¦1 + 2", "first:"),
                    ("4, ¦5, 6)", "tail...:"),
                    ("group(¦(x + 7)", "first:"),
                    ("+ 7), ¦8, 9", "second:"),
                    ("8, 9, ¦10)", "tail...:"),
                    ("-x, ¦-(x)", "second:"),
                    ("f(¦11)", "when:"),
                    ("g(¦12)", "elapsed:"),
                    ("{})(¦13)", "instant:"),
                    ("r.use(¦14)", "times:"),
                    ("m(¦15)", "times:"),
                    ("33, ¦34, 35)", "...:"),
                    ("pair(¦\"😀\"", "note:"),
                    ("\"😀\", ¦17)", "value:"),
                    ("copy(dst, ¦\"abc\"", "src:"),
                    ("append(dst, ¦19, 20)", "elems...:"),
                    ("min(¦21, 22", "x:"),
                    ("21, ¦22, 23", "y...:"),
                    ("panic(¦nil)", "v:"),
                    ("len(¦\"xyz\"", "v:"),
                    ("len(¦\"shadow\"", "local:"),
                    ("unique(¦29, 30)", "...:"),
                ]
            )
        );

        let start = position(&source, "pair(¦\"😀\"");
        let end = position(&source, "\"😀\", ¦17)");
        assert_eq!(end.character - start.character, 6);
        let restricted = engine.inlay_hints(&path, Range::new(start, end)).unwrap();
        assert_eq!(summary(&restricted), [(start, "note:".to_owned())]);
        assert_eq!(
            summary(&engine.inlay_hints(&path, all()).unwrap()),
            summary(&hints)
        );
    }

    #[test]
    fn unkeyed_struct_fields_and_named_returns_use_their_own_filters() {
        let source = r#"package p
type Embedded struct{ X int }
type Holder struct {
    Embedded
    Left, Right int
    Tail string
}
func run(value int) (count int, _ int, text string) {
    _ = Holder{Embedded{X: 1}, value, 2, "a"}
    _ = Holder{Embedded{}, 3, 4, "b", 5}
    _ = Holder{Left: 6}
    _ = []Holder{{Embedded{}, 7, 8, "c"}}
    _ = map[string]Holder{"key": {Embedded{}, 9, 10, "d"}}
    _ = struct { Nested Holder }{{Embedded{}, 11, 12, "e"}}
    _ = Holder{Embedded{}, runInt(), value, "f"}
    if value == 1 { return ((value + 11)), 8, "g" }
    if value == 2 { return value + 12, 9, "h" }
    if value == 3 { return }
    func() (inner int) { return 14 }()
    return 1 + 2, 15, "i"
}
func runInt() int { return 0 }
"#;
        let (_project, path, mut engine) = open_fixture(source);
        assert_eq!(
            summary(&engine.inlay_hints(&path, all()).unwrap()),
            expected(
                source,
                &[
                    ("Embedded{X: 1}, ¦value", "Left:"),
                    ("value, ¦2, \"a\"", "Right:"),
                    ("2, ¦\"a\"}", "Tail:"),
                    ("[]Holder{{Embedded{}, ¦7", "Left:"),
                    ("7, ¦8, \"c\"", "Right:"),
                    ("8, ¦\"c\"}}", "Tail:"),
                    ("\"key\": {Embedded{}, ¦9", "Left:"),
                    ("9, ¦10, \"d\"", "Right:"),
                    ("10, ¦\"d\"}}", "Tail:"),
                    ("Nested Holder }{¦{Embedded{}", "Nested:"),
                    ("Embedded{}, ¦11, 12", "Left:"),
                    ("11, ¦12, \"e\"", "Right:"),
                    ("12, ¦\"e\"}}", "Tail:"),
                    ("Embedded{}, ¦runInt()", "Left:"),
                    ("runInt(), ¦value", "Right:"),
                    ("value, ¦\"f\"}", "Tail:"),
                    ("return ¦((value + 11))", "count:"),
                    ("8, ¦\"g\" }", "text:"),
                    ("9, ¦\"h\" }", "text:"),
                    ("return ¦14 }", "inner:"),
                    ("return ¦1 + 2", "count:"),
                    ("15, ¦\"i\"", "text:"),
                ]
            )
        );
    }

    #[test]
    fn constants_evaluate_visible_dependencies_inherited_rows_and_go_bytes() {
        let source = r#"package p
const (
    A, B = iota, iota + 10
    C, D
    Plain = 9
    MismatchA, MismatchB
    Hex = 0x10
    Inherit
)
const Ref = B + D
const Float = 0x1.8p2 / 4
const ByteLen = len("\xff")
const UnicodeLen = len("😀")
const Rune = '😀'
const Signed = ^3
const BitOps = ((1 << 7) | 7) &^ 2
const Logical = ("ab" + "cd") == "abcd" && !false && (2.5 > 2)
const Tab = "a" + "\t"
const Escaped = "\u0058"
const Raw = "unchanged"
const Illegal = 1 / 0
const Wrong uint8 = 1 << 8
const Cycle = Loop + 1
const Loop = Cycle + 1
const Unknown = fromElsewhere + 1
const BEL = "x" + "\a"
const BS = "x" + "\b"
const FF = "x" + "\f"
const LF = "x" + "\n"
const CR = "x" + "\r"
const VT = "x" + "\v"
const 𐐀 = 0b100
type U uint8
const Typed U = 7
const Inverse = ^Typed
func a(iota int) {
    const Shadow = iota
    const Local = Ref * 2
}
func b() {
    const iota = 12
    const Local = iota + 1
}
"#;
        let source = source.replace('\n', "\r\n");
        let (_project, path, mut engine) = open_fixture(&source);
        let hints = engine.inlay_hints(&path, all()).unwrap();
        let values: Vec<_> = hints
            .into_iter()
            .filter(|hint| hint.kind.is_none())
            .collect();
        assert_eq!(
            summary(&values),
            expected(
                &source,
                &[
                    ("A¦, B = iota", "= 0"),
                    ("B¦ = iota", "= 10"),
                    ("C¦, D", "= 1"),
                    ("C, D¦", "= 11"),
                    ("Hex¦ =", "= 16"),
                    ("Inherit¦", "= 16"),
                    ("Ref¦ =", "= 21"),
                    ("Float¦ =", "= 1.5"),
                    ("ByteLen¦ =", "= 1"),
                    ("UnicodeLen¦ =", "= 4"),
                    ("Rune¦ =", "= 128512"),
                    ("Signed¦ =", "= -4"),
                    ("BitOps¦ =", "= 133"),
                    ("Logical¦ =", "= true"),
                    ("Tab¦ =", "= \"a\t\""),
                    ("Escaped¦ =", "= \"X\""),
                    ("𐐀¦ =", "= 4"),
                    ("Inverse¦ =", "= 248"),
                    ("Local¦ = Ref", "= 42"),
                    ("Local¦ = iota", "= 13"),
                ]
            )
        );

        let start = position(&source, "const ¦𐐀");
        let end = position(&source, "const 𐐀¦ ");
        assert_eq!(end.character - start.character, 2);
    }
}
