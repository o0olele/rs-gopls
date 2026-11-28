use std::fmt::Write;
use std::time::Instant;

use lsp_types::{CodeActionKind, FileChangeType};

use super::*;
use crate::protocol::path_uri;
use crate::semantic::actions::code_actions;
use crate::semantic::inference_memo::{with_memo_stats, without_memo};
use crate::semantic::package_index::without_index;
use crate::semantic::query::QueryProgress;

fn quick_fixes(engine: &mut Engine, path: &Path, report: &DiagnosticReport) -> serde_json::Value {
    let mut snapshot = engine.snapshot();
    let document = snapshot.document(path).unwrap().unwrap();
    let range = document.byte_range(0..document.text().len());
    let actions = code_actions(
        &mut snapshot,
        &document,
        &path_uri(path).unwrap(),
        range,
        Some(&[CodeActionKind::QUICKFIX]),
        Some(report),
        &[],
    )
    .unwrap();
    serde_json::to_value(actions).unwrap()
}

fn compare_indexed(engine: &mut Engine, path: &Path) -> DiagnosticReport {
    let actual = analyze_file(engine, path);
    let expected = without_index(|| analyze_file(engine, path));
    assert_eq!(actual, expected, "{}", path.display());
    assert_eq!(
        quick_fixes(engine, path, &actual),
        without_index(|| quick_fixes(engine, path, &expected)),
    );
    actual
}

fn add_siblings(project: &TestProject, count: usize) -> Vec<String> {
    (0..count)
        .map(|i| {
            let name = format!("peer{i:03}.go");
            let mut text = String::from("package sample\n");
            for j in 0..8 {
                writeln!(text, "var Peer{i:03}_{j} int").unwrap();
            }
            project.write(&name, &text);
            name
        })
        .collect()
}

#[test]
fn generic_imported_call_respects_lexical_shadowing() {
    let project = TestProject::new();
    project.write(
        "dep/dep.go",
        "package dep\nfunc Identity[T any](v T) T { return v }\ntype Outer struct{ Value int }\n",
    );
    let text = r#"package sample
import "dep"
func use() {
    outer := dep.Identity(dep.Outer{})
    var _ string = outer.Value
    {
        dep := struct{ Identity func(string) string }{}
        value := dep.Identity("text")
        var _ int = value
    }
}
"#;
    let path = project.write("sample.go", text);
    let listing = format!(
        "{}\n{}",
        json!({
            "Dir": project.root, "ImportPath": "sample", "Name": "sample",
            "GoFiles": ["sample.go"], "Imports": ["dep"],
        }),
        json!({
            "Dir": project.root.join("dep"), "ImportPath": "dep", "Name": "dep",
            "GoFiles": ["dep.go"],
        }),
    );
    let mut engine = Engine::new(project.root.clone());
    assert!(engine.install_workspace(
        engine.workspace_load(),
        Workspace::from_test_listing(std::slice::from_ref(&project.root), &listing, false),
    ));
    engine.open(path.clone(), text.into(), 1).unwrap();

    let report = analyze_file(&mut engine, &path);
    item_at(&report, text, TypeMismatch, "= outer.Value", "outer.Value");
    item_at(&report, text, TypeMismatch, "= value", "value");
    assert_eq!(report.items.len(), 2);
}

#[test]
fn package_index_matches_linear_diagnostics_and_quick_fixes_through_edits() {
    let project = TestProject::new();
    let mut siblings = add_siblings(&project, 72);
    let tagged = project.root.join("peer071.go");
    let tagged_text = fs::read_to_string(&tagged).unwrap();
    project.write("peer071.go", &format!("//go:build custom\n\n{tagged_text}"));
    let owner_text = r#"package sample
import alias "dep"
func Body() { _ = "😀" }
var Owned = alias.Value()
var Duplicate int
type Inner struct{ Embedded int }
func (Inner) Run(n int) int { return n }
type Outer struct{ Inner }
type Alias = Outer
"#;
    let target_text = r#"package sample
var Duplicate int
var _ int = Owned
var _ int = Peer071_0
func (Inner) Embedded() {}
func use(a Alias) {
    _ = "😀"; unused := a.Run(1)
    n := a.Run(2)
    n := n
    _ = n
    _ = a.Missing
    _ = TestOnly
}
"#;
    let target = project.write("target.go", target_text);
    let owner = project.write("owner.go", owner_text);
    let test = project.write("owner_test.go", "package sample\nvar TestOnly = Owned\n");
    project.write("dep/dep.go", "package dep\nfunc Value() int { return 1 }\n");
    project.write(
        "alternative/dep.go",
        "package alternative\nfunc Value() string { return \"s\" }\n",
    );
    siblings.extend(["owner.go".into(), "target.go".into()]);
    let workspace = |files: &[String], tests: bool| {
        let package = json!({"Dir": project.root, "ImportPath": "sample", "Name": "sample", "GoFiles": files, "Imports": ["dep", "alternative"]});
        let dependencies = [
            json!({"Dir": project.root.join("dep"), "ImportPath": "dep", "Name": "dep", "GoFiles": ["dep.go"]}),
            json!({"Dir": project.root.join("alternative"), "ImportPath": "alternative", "Name": "alternative", "GoFiles": ["dep.go"]}),
        ];
        let mut listing = format!("{package}\n{}\n{}", dependencies[0], dependencies[1]);
        if tests {
            let mut files = files.to_vec();
            files.push("owner_test.go".into());
            let variant = json!({"Dir": project.root, "ImportPath": "sample [sample.test]", "ForTest": "sample", "Name": "sample", "GoFiles": files, "Imports": ["dep", "alternative"]});
            writeln!(listing, "\n{variant}").unwrap();
        }
        Workspace::from_test_listing(std::slice::from_ref(&project.root), &listing, tests)
    };
    let mut engine = Engine::new(project.root.clone());
    assert!(engine.install_workspace(engine.workspace_load(), workspace(&siblings, false)));
    engine.open(target.clone(), target_text.into(), 1).unwrap();

    let original = compare_indexed(&mut engine, &target);
    assert_eq!(compare_indexed(&mut engine, &target), original);
    item_at(
        &original,
        target_text,
        NameRedeclared,
        "var Duplicate",
        "Duplicate",
    );
    item_at(
        &original,
        target_text,
        NameRedeclared,
        "(Inner) Embedded",
        "Embedded",
    );
    item_at(&original, target_text, VarUnused, "unused :=", "unused");
    item_at(&original, target_text, ShortVarNoNewVars, "n := n", ":=");
    item_at(
        &original,
        target_text,
        NameUnresolved,
        "_ = TestOnly",
        "TestOnly",
    );
    assert_eq!(
        quick_fixes(&mut engine, &target, &original)
            .as_array()
            .unwrap()
            .len(),
        2
    );

    engine.open(owner.clone(), owner_text.into(), 1).unwrap();
    assert_eq!(compare_indexed(&mut engine, &target).items, original.items);
    let resolve_owned = |engine: &mut Engine| {
        let mut snapshot = engine.snapshot();
        let document = snapshot.document(&target).unwrap().unwrap();
        let position = occurrence(target_text, "= Owned", "Owned").start;
        let QueryProgress::Ready(Some(location)) =
            snapshot.resolve_name(&document, position).unwrap()
        else {
            panic!("Owned must resolve to the owner file");
        };
        location
    };
    assert_eq!(
        resolve_owned(&mut engine),
        (owner.clone(), occurrence(owner_text, "var Owned", "Owned"))
    );
    let moved = owner_text.replace("_ = \"😀\"", "_ = \"😀\"\n    _ = 2\n");
    replace(&mut engine, &owner, &moved, 2);
    assert_eq!(compare_indexed(&mut engine, &target).items, original.items);
    assert_eq!(
        resolve_owned(&mut engine),
        (owner.clone(), occurrence(&moved, "var Owned", "Owned"))
    );

    let moved_target = target_text.replace("var Duplicate", "\n\nvar Duplicate");
    replace(&mut engine, &target, &moved_target, 2);
    let report = compare_indexed(&mut engine, &target);
    item_at(
        &report,
        &moved_target,
        NameRedeclared,
        "var Duplicate",
        "Duplicate",
    );
    replace(&mut engine, &target, target_text, 3);

    let signature = owner_text
        .replace("Run(n int)", "Run(n string)")
        .replace("return n", "return 0");
    replace(&mut engine, &owner, &signature, 3);
    item_at(
        &compare_indexed(&mut engine, &target),
        target_text,
        TypeMismatch,
        "a.Run(1)",
        "1",
    );
    let changed_import = owner_text.replace("\"dep\"", "\"alternative\"");
    replace(&mut engine, &owner, &changed_import, 4);
    item_at(
        &compare_indexed(&mut engine, &target),
        target_text,
        TypeMismatch,
        "= Owned",
        "Owned",
    );
    replace(
        &mut engine,
        &owner,
        &format!("{owner_text}var Broken = )\n"),
        5,
    );
    compare_indexed(&mut engine, &target);
    engine.close(&owner);
    assert_eq!(compare_indexed(&mut engine, &target).items, original.items);
    project.write("owner.go", &moved);
    engine.file_changed(&owner, FileChangeType::CHANGED);
    assert_eq!(compare_indexed(&mut engine, &target).items, original.items);
    assert_eq!(
        resolve_owned(&mut engine),
        (owner.clone(), occurrence(&moved, "var Owned", "Owned"))
    );

    let shortened: Vec<_> = siblings
        .iter()
        .filter(|file| *file != "peer071.go")
        .cloned()
        .collect();
    assert!(engine.install_workspace(engine.workspace_load(), workspace(&shortened, false)));
    item_at(
        &compare_indexed(&mut engine, &target),
        target_text,
        NameUnresolved,
        "= Peer071_0",
        "Peer071_0",
    );
    assert!(engine.install_workspace(engine.workspace_load(), workspace(&siblings, true)));
    assert_eq!(compare_indexed(&mut engine, &target).items, original.items);
    assert!(compare_indexed(&mut engine, &test).items.is_empty());
    let variant_only = project.write(
        "owner_test.go",
        "package sample\nvar TestOnly int = Owned\nvar _ = TestOnly\n",
    );
    engine.file_changed(&variant_only, FileChangeType::CHANGED);
    assert!(compare_indexed(&mut engine, &test).items.is_empty());
    assert_eq!(compare_indexed(&mut engine, &target).items, original.items);
}

#[test]
fn inference_memo_matches_full_diagnostics_and_fixes_with_shadowing_and_recovery() {
    let project = TestProject::new();
    let library = r#"package sample
type Cell[T any] struct { Value T }
func (c Cell[T]) Get() T { return c.Value }
func Box[T any](v T) Cell[T] { return Cell[T]{Value: v} }
type Embedded struct { Cell[int] }
type Alias = Embedded
type Text struct { Word string }
type Number struct { Count int }
var recursive = recursive
"#;
    let text = r#"package sample
func use(v any, a Alias) {
    one := Box(1)
    word := Box("word")
    var _ int = one.Get()
    var _ string = word.Get()
    var _ int = word.Get()
    var _ int = a.Get()
    var _ int = a.Value
    {
        one := Box("shadowed")
        var _ string = one.Get()
        var _ int = one.Get()
    }
    switch v := v.(type) {
    case Text:
        var _ string = v.Word
        _ = v.Count
    case Number:
        var _ int = v.Count
        _ = v.Word
    case nil, bool:
        _ = v
    default:
        _ = v
    }
    _ = "😀"; unused := one.Get()
    var _ int = recursive
}
func recovery() { _ = 1 + ); _ = unknownInRecovery }
"#;
    project.write("library.go", library);
    let path = project.write("sample.go", text);
    let mut engine = Engine::new(project.root.clone());
    assert!(engine.install_workspace(
        engine.workspace_load(),
        project.workspace(&["library.go", "sample.go"], false),
    ));
    engine.open(path.clone(), text.into(), 1).unwrap();

    let compare = |engine: &mut Engine| {
        let (actual, stats) = with_memo_stats(|| analyze_file(engine, &path));
        let expected = without_memo(|| analyze_file(engine, &path));
        assert_eq!(actual, expected);
        assert!(stats.expr_hits > 0, "{stats:?}");
        assert!(stats.local_hits > 0, "{stats:?}");
        assert!(stats.ready_writes > 0, "{stats:?}");
        assert!(stats.transient_count > 0, "{stats:?}");
        assert_eq!(
            quick_fixes(engine, &path, &actual),
            quick_fixes(engine, &path, &expected)
        );
        actual
    };
    let report = compare(&mut engine);
    item_at(
        &report,
        text,
        TypeMismatch,
        "var _ int = word.Get()",
        "word.Get()",
    );
    item_at(
        &report,
        text,
        TypeMismatch,
        "var _ int = one.Get()\n    }",
        "one.Get()",
    );
    item_at(
        &report,
        text,
        FieldOrMethodUnresolved,
        "_ = v.Count",
        "Count",
    );
    item_at(&report, text, FieldOrMethodUnresolved, "_ = v.Word", "Word");
    item_at(&report, text, SyntaxError, "1 + )", ")");
    item_at(
        &report,
        text,
        NameUnresolved,
        "_ = unknownInRecovery",
        "unknownInRecovery",
    );
    assert_eq!(
        quick_fixes(&mut engine, &path, &report)
            .as_array()
            .unwrap()
            .len(),
        1
    );

    {
        let mut snapshot = engine.snapshot();
        let document = snapshot.document(&path).unwrap().unwrap();
        let key = snapshot_key(&snapshot, &document).unwrap();
        assert!(
            analyze(
                &mut snapshot,
                &document,
                key,
                Some(&AtomicBool::new(true)),
                &mut None
            )
            .is_err()
        );
    }
    assert_eq!(compare(&mut engine), report);
    let repaired = text
        .replace("_ = 1 + )", "_ = 1")
        .replace("unknownInRecovery", "0");
    replace(&mut engine, &path, &repaired, 2);
    let repaired = compare(&mut engine);
    assert!(
        !repaired
            .items
            .iter()
            .any(|item| item.code == SyntaxError || item.code == NameUnresolved)
    );
}

#[test]
#[ignore = "Stage 2 timing comparison"]
fn stage2_cache_timings_by_ast_size() {
    const SAMPLES: usize = 30;
    for functions in [45, 223] {
        let project = TestProject::new();
        let mut siblings = add_siblings(&project, 96);
        siblings.extend(["library.go".into(), "sample.go".into()]);
        let files: Vec<_> = siblings.iter().map(String::as_str).collect();
        project.write(
            "library.go",
            r#"package sample
type Data[T any] struct { Value T }
func (d Data[T]) Get() T { return d.Value }
func Wrap[T any](t T) Data[T] { return Data[T]{Value: t} }
type Embedded struct { Data[int] }
type Alias = Embedded
func consume(a, b, c int) { }
"#,
        );
        let mut text = String::from("package sample\n");
        for i in 0..functions {
            writeln!(text, "func f{i}(e Alias) {{").unwrap();
            text.push_str(
                r#"    x := Wrap(e.Get())
    y := Wrap(x.Get())
    z := Wrap(y.Get())
    a := z
    b := a
    c := b
    consume(x.Get(), b.Get(), c.Get())
    consume(e.Value, a.Value, c.Value)
    var _ int = c.Get()
    var _ string = c.Get()
    _ = Peer095_7
"#,
            );
            writeln!(text, "    _ = c.Missing{i}\n}}").unwrap();
        }
        let path = project.write("sample.go", &text);
        let mut engine = Engine::new(project.root.clone());
        assert!(
            engine.install_workspace(engine.workspace_load(), project.workspace(&files, false))
        );
        engine.open(path.clone(), text, 1).unwrap();
        let mut snapshot = engine.snapshot();
        let document = snapshot.document(&path).unwrap().unwrap();
        let key = snapshot_key(&snapshot, &document).unwrap();
        let mut run = || {
            let mut stages = Some(Stages::default());
            let start = Instant::now();
            let report = analyze(&mut snapshot, &document, key.clone(), None, &mut stages).unwrap();
            let total = start.elapsed();
            let stages = stages.unwrap();
            let metrics = [
                total,
                stages.times[Stage::Declarations as usize],
                stages.times[Stage::Unresolved as usize],
                stages.times[Stage::Types as usize],
            ];
            (
                report,
                stages.inspected_nodes,
                metrics.map(|time| time.as_secs_f64() * 1000.0),
            )
        };
        let (baseline, nodes, _) = without_index(|| without_memo(&mut run));
        assert_eq!(without_memo(&mut run).0, baseline);
        assert_eq!(run().0, baseline);
        let mut samples = [
            [Vec::new(), Vec::new(), Vec::new(), Vec::new()],
            [Vec::new(), Vec::new(), Vec::new(), Vec::new()],
            [Vec::new(), Vec::new(), Vec::new(), Vec::new()],
        ];
        for i in 0..SAMPLES {
            for offset in 0..samples.len() {
                let mode = (i + offset) % samples.len();
                let (report, _, metrics) = match mode {
                    0 => without_index(|| without_memo(&mut run)),
                    1 => without_memo(&mut run),
                    _ => run(),
                };
                assert_eq!(report, baseline);
                for (name, metric) in metrics.into_iter().enumerate() {
                    samples[mode][name].push(metric);
                }
            }
        }
        for (mode, values) in ["linear", "package index", "package index + memo"]
            .into_iter()
            .zip(samples)
        {
            for (metric, mut values) in ["Stage 2", "declarations", "unresolved", "types"]
                .into_iter()
                .zip(values)
            {
                values.sort_by(f64::total_cmp);
                let percentile = |rank: f64| {
                    let position = rank * (values.len() - 1) as f64;
                    let lower = position.floor() as usize;
                    let upper = position.ceil() as usize;
                    values[lower] + (values[upper] - values[lower]) * position.fract()
                };
                eprintln!(
                    "nodes={nodes} mode={mode} metric={metric} n={SAMPLES} p50={:.3} p95={:.3} max={:.3} ms",
                    percentile(0.50),
                    percentile(0.95),
                    percentile(1.0)
                );
            }
        }
    }
}
