mod cache;
mod declarations;
mod imports;
mod inputs;
mod model;
mod package_errors;
mod syntax;
mod types;
mod unresolved;
mod unused;
mod worker;

#[cfg(test)]
mod tests;

use anyhow::Result;
use std::collections::HashSet;
use std::ops::Range as Bytes;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::{Duration, Instant};
use tree_sitter::Node;

use super::index::IndexedSource;
use super::resolver::Resolver;
use super::snapshot::{DocumentSnapshot, EngineSnapshot};
use super::source::{is_test_source, node_text};
use crate::workspace::{Package, Workspace};

pub(crate) use worker::{DiagnosticInput, DiagnosticsPreparation};

pub(crate) use cache::{DiagnosticKeys, DiagnosticsCache};
use inputs::diagnostics_view;
pub(crate) use inputs::snapshot_key_for_path;
pub(crate) use model::{
    DiagnosticCode, DiagnosticFixData, DiagnosticItem, DiagnosticReport, DiagnosticSeverity,
    DiagnosticSnapshotKey, DiagnosticTag,
};

pub(crate) fn snapshot_key(
    snapshot: &EngineSnapshot<'_>,
    document: &DocumentSnapshot<'_>,
) -> std::io::Result<DiagnosticSnapshotKey> {
    snapshot_key_for_path(snapshot, document.path())
}

#[cfg(test)]
pub(crate) fn analyze_document(
    snapshot: &mut EngineSnapshot<'_>,
    document: &DocumentSnapshot<'_>,
) -> Result<DiagnosticReport> {
    let key = snapshot_key(snapshot, document)?;
    analyze(snapshot, document, key, None)
}

pub(super) fn analyze(
    snapshot: &mut EngineSnapshot<'_>,
    document: &DocumentSnapshot<'_>,
    key: DiagnosticSnapshotKey,
    cancelled: Option<&AtomicBool>,
) -> Result<DiagnosticReport> {
    checkpoint(cancelled)?;
    let mut stages = crate::telemetry::enabled().then(Stages::default);
    let mut items = timed(&mut stages, Stage::Syntax, || {
        syntax::collect_syntax_diagnostics(document, cancelled)
    })?;
    let Some(view) = diagnostics_view(snapshot, document) else {
        log_stages(document, stages, true);
        return Ok(DiagnosticReport::new(key, items, true));
    };
    let graph = view.graph;
    let group_id = view.group_id;
    let package_errors = timed(&mut stages, Stage::PackageErrors, || {
        package_errors::collect_package_errors(graph, document, view.package, group_id, &items)
    });
    if syntax::has_reliable_declarations(document.root_node())
        && let Some(package) = view.package
        && super::completion::render::package_name(document.indexed()) == Some(&package.name)
    {
        let source = document.indexed();
        let mut analysis = Analysis {
            source,
            package,
            group_id: group_id
                .filter(|group| graph.package_in_group(package, *group))
                .unwrap_or_else(|| package.group_id()),
            resolver: snapshot.diagnostic_resolver(graph, cancelled),
            locals: declarations::Locals::default(),
            imports: timed(&mut stages, Stage::Imports, || {
                imports::Bindings::new(source, graph, package)
            }),
            items: Vec::new(),
            cancelled,
        };
        timed(&mut stages, Stage::Declarations, || {
            declarations::collect(&mut analysis)
        })?;
        inspect(&mut analysis, &mut stages)?;
        for (i, item) in analysis.items.into_iter().enumerate() {
            if i & 1023 == 0 {
                checkpoint(cancelled)?;
            }
            items.push(item.finish(source));
        }
    }
    items.extend(package_errors);
    checkpoint(cancelled)?;
    log_stages(document, stages, false);
    Ok(DiagnosticReport::new(key, items, false))
}

#[cfg(test)]
pub(crate) fn get_or_compute<'c>(
    cache: &'c mut DiagnosticsCache,
    snapshot: &mut EngineSnapshot<'_>,
    document: &DocumentSnapshot<'_>,
) -> Result<&'c DiagnosticReport> {
    let key = snapshot_key(snapshot, document)?;
    if cache.get(&key).is_none() {
        let report = analyze(snapshot, document, key.clone(), None)?;
        cache.insert(report);
    }
    Ok(cache.get(&key).expect("report for the current snapshot"))
}

struct Analysis<'s, 'w, 'i> {
    source: &'s IndexedSource,
    package: &'w Package,
    group_id: usize,
    resolver: Resolver<'w, 'i>,
    locals: declarations::Locals<'s>,
    imports: imports::Bindings<'w>,
    items: Vec<PendingItem>,
    cancelled: Option<&'i AtomicBool>,
}

impl<'s> Analysis<'s, '_, '_> {
    fn text(&self, node: Node<'_>) -> &'s str {
        node_text(node, &self.source.text)
    }

    fn error(
        &mut self,
        code: DiagnosticCode,
        node: Node<'_>,
        message: impl Into<String>,
    ) -> &mut PendingItem {
        self.error_bytes(code, node.byte_range(), message)
    }

    fn error_bytes(
        &mut self,
        code: DiagnosticCode,
        range: Bytes<usize>,
        message: impl Into<String>,
    ) -> &mut PendingItem {
        self.items.push(PendingItem {
            code,
            range,
            message: message.into(),
            fix: Fix::None,
        });
        self.items.last_mut().unwrap()
    }
}

struct PendingItem {
    code: DiagnosticCode,
    range: Bytes<usize>,
    message: String,
    fix: Fix,
}

enum Fix {
    None,
    Assign,
    ReplaceWithBlank(String),
    RemoveImport(String, Bytes<usize>),
    ImportPackage(String),
}

impl PendingItem {
    fn finish(self, source: &IndexedSource) -> DiagnosticItem {
        let range = source.byte_range(self.range);
        let mut item =
            DiagnosticItem::new(self.code, DiagnosticSeverity::Error, range, self.message);
        item.fix_data = match self.fix {
            Fix::None => DiagnosticFixData::None,
            Fix::Assign => DiagnosticFixData::ChangeShortDeclToAssign {
                operator_range: range,
            },
            Fix::ReplaceWithBlank(name) => {
                item.tags.push(DiagnosticTag::Unnecessary);
                DiagnosticFixData::ReplaceUnusedVarWithBlank {
                    name,
                    ident_range: range,
                }
            }
            Fix::RemoveImport(import_path, bytes) => {
                item.tags.push(DiagnosticTag::Unnecessary);
                DiagnosticFixData::RemoveUnusedImport {
                    import_path,
                    spec_range: source.byte_range(bytes),
                }
            }
            Fix::ImportPackage(qualifier) => DiagnosticFixData::ImportMissingPackage {
                qualifier,
                ident_range: range,
            },
        };
        item
    }
}

fn inspect(analysis: &mut Analysis<'_, '_, '_>, stages: &mut Option<Stages>) -> Result<()> {
    if let Some(stages) = stages {
        inspect_with::<true>(analysis, stages)
    } else {
        inspect_with::<false>(analysis, &mut Stages::default())
    }
}

fn inspect_with<const RECORD: bool>(
    analysis: &mut Analysis<'_, '_, '_>,
    stages: &mut Stages,
) -> Result<()> {
    let root = analysis.source.tree.root_node();
    let track_variables = !analysis.locals.variables.is_empty();
    let mut variables = HashSet::new();
    let mut imports = HashSet::new();
    let mut returns = Vec::new();
    walk(root, analysis.cancelled, |node, kind, parents, entering| {
        if !entering {
            returns.pop();
            return Ok(WalkControl::Children);
        }
        let function = is_function(kind);
        if function {
            returns.push(types::ReturnSlots::new(node));
        }
        if RECORD {
            stages.inspected_nodes += 1;
        }
        let value_name = declarations::identifier_kind(kind)
            && unused::value_name(analysis, node, kind, parents);
        let local = value_name
            .then(|| analysis.locals.lookup(node, analysis.text(node)))
            .flatten();
        let qualifier = value_name && imports::qualifier(node, parents);
        if qualifier && local.is_none() {
            imports.insert(analysis.text(node));
        }
        if track_variables && let Some(binding) = local {
            stages.measure::<RECORD, _>(Stage::Unused, || {
                unused::mark_read(analysis, parents, binding, &mut variables)
            })?;
        }
        if value_name || matches!(kind, "qualified_type" | "selector_expression") {
            stages.measure::<RECORD, _>(Stage::Unresolved, || {
                unresolved::check(analysis, node, parents, value_name, local, qualifier)
            })?;
        }
        if types::requires_check(kind, value_name && analysis.text(node) == "_") {
            stages.measure::<RECORD, _>(Stage::Types, || {
                declarations::check_assignable(analysis, node)?;
                types::check(analysis, node, parents, value_name, returns.last())
            })?;
        }
        Ok(if function {
            WalkControl::Scoped
        } else {
            WalkControl::Children
        })
    })?;
    stages.measure::<RECORD, _>(Stage::Imports, || imports::collect(analysis, &imports))?;
    stages.measure::<RECORD, _>(Stage::Unused, || unused::collect(analysis, &variables))
}

#[derive(Clone, Copy)]
struct Parents<'t, 'p>(&'p [(Node<'t>, Option<&'static str>)]);

impl<'t, 'p> Parents<'t, 'p> {
    fn at(self, level: usize) -> Option<Node<'t>> {
        self.0.len().checked_sub(level).map(|index| self.0[index].0)
    }

    fn iter(self) -> impl DoubleEndedIterator<Item = Node<'t>> + 'p {
        self.0.iter().map(|(node, _)| *node)
    }
}

fn is_function(kind: &str) -> bool {
    matches!(
        kind,
        "function_declaration" | "method_declaration" | "func_literal"
    )
}

fn checkpoint(cancelled: Option<&AtomicBool>) -> Result<()> {
    anyhow::ensure!(
        cancelled.is_none_or(|flag| !flag.load(Relaxed)),
        "diagnostics cancelled"
    );
    Ok(())
}

#[derive(PartialEq, Eq)]
enum WalkControl {
    Skip,
    Children,
    Scoped,
}

fn walk<'t>(
    root: Node<'t>,
    cancelled: Option<&AtomicBool>,
    mut visit: impl FnMut(Node<'t>, &'static str, Parents<'t, '_>, bool) -> Result<WalkControl>,
) -> Result<()> {
    let mut cursor = root.walk();
    let mut parents = Vec::new();
    let mut visited = 0_u32;
    let has_errors = root.has_error();
    loop {
        if visited & 1023 == 0 {
            checkpoint(cancelled)?;
        }
        visited = visited.wrapping_add(1);
        let node = cursor.node();
        if node.is_named() && !node.is_extra() && (!has_errors || !node.has_error() || node == root)
        {
            let kind = node.kind();
            let control = visit(node, kind, Parents(&parents), true)?;
            let on_exit = (control == WalkControl::Scoped).then_some(kind);
            if control != WalkControl::Skip && cursor.goto_first_child() {
                parents.push((node, on_exit));
                continue;
            }
            if let Some(kind) = on_exit {
                visit(node, kind, Parents(&parents), false)?;
            }
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return Ok(());
            }
            let (parent, on_exit) = parents.pop().expect("cursor parent");
            if let Some(kind) = on_exit {
                visit(parent, kind, Parents(&parents), false)?;
            }
        }
    }
}

enum Stage {
    Syntax,
    Declarations,
    Imports,
    Unused,
    Unresolved,
    Types,
    PackageErrors,
}

#[derive(Default)]
struct Stages {
    times: [Duration; 7],
    inspected_nodes: usize,
}

impl Stages {
    fn measure<const RECORD: bool, T>(&mut self, stage: Stage, run: impl FnOnce() -> T) -> T {
        if RECORD {
            let start = Instant::now();
            let result = run();
            self.times[stage as usize] += start.elapsed();
            result
        } else {
            run()
        }
    }
}

fn timed<T>(stages: &mut Option<Stages>, stage: Stage, run: impl FnOnce() -> T) -> T {
    if let Some(stages) = stages {
        stages.measure::<true, _>(stage, run)
    } else {
        run()
    }
}

fn log_stages(document: &DocumentSnapshot<'_>, stages: Option<Stages>, partial: bool) {
    if let Some(stages) = stages {
        let millis = |stage: Stage| crate::telemetry::millis(stages.times[stage as usize]);
        crate::telemetry::event!("diagnostic_stages", {
            "path": crate::telemetry::path(document.path()),
            "partial": partial,
            "syntax_ms": millis(Stage::Syntax),
            "declarations_ms": millis(Stage::Declarations),
            "imports_ms": millis(Stage::Imports),
            "unused_ms": millis(Stage::Unused),
            "unresolved_ms": millis(Stage::Unresolved),
            "types_ms": millis(Stage::Types),
            "package_errors_ms": millis(Stage::PackageErrors),
            "inspected_nodes": stages.inspected_nodes,
        });
    }
}

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    node.named_children(&mut node.walk())
        .filter(|n| !n.is_extra())
        .collect()
}

fn field_children<'t>(node: Node<'t>, field: &str) -> Vec<Node<'t>> {
    node.children_by_field_name(field, &mut node.walk())
        .filter(|n| n.is_named() && !n.is_extra())
        .collect()
}

fn token<'t>(node: Node<'t>, kind: &str) -> Option<Node<'t>> {
    node.children(&mut node.walk()).find(|n| n.kind() == kind)
}

fn unparen(mut node: Node<'_>) -> Node<'_> {
    while matches!(
        node.kind(),
        "parenthesized_expression" | "parenthesized_type"
    ) {
        let Some(inner) = node
            .named_children(&mut node.walk())
            .find(|n| !n.is_extra())
        else {
            break;
        };
        node = inner;
    }
    node
}
