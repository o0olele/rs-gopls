mod cache;
mod declarations;
mod imports;
mod inputs;
mod model;
mod package_errors;
mod retention;
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

pub(super) use syntax::has_reliable_package_scope;

pub(crate) use cache::{DiagnosticKeys, DiagnosticsCache};
use inputs::diagnostics_view;
pub(crate) use inputs::snapshot_key_for_path;
pub(crate) use model::{
    ContextCompleteness, DiagnosticCode, DiagnosticFixData, DiagnosticItem, DiagnosticProvenance,
    DiagnosticReport, DiagnosticSeverity, DiagnosticSnapshotKey, DiagnosticStage, DiagnosticTag,
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
    analyze(snapshot, document, key, None, &mut None)
}

pub(super) fn analyze(
    snapshot: &mut EngineSnapshot<'_>,
    document: &DocumentSnapshot<'_>,
    key: DiagnosticSnapshotKey,
    cancelled: Option<&AtomicBool>,
    stages: &mut Option<Stages>,
) -> Result<DiagnosticReport> {
    checkpoint(cancelled)?;
    let mut items = timed(stages, Stage::Syntax, || {
        syntax::collect_syntax_diagnostics(document, cancelled)
    })?;
    let Some(view) = diagnostics_view(snapshot, document) else {
        return Ok(DiagnosticReport::new(key, items, true));
    };
    let graph = view.graph;
    let group_id = view.group_id;
    let package_errors = timed(stages, Stage::PackageErrors, || {
        package_errors::collect_package_errors(
            graph,
            document,
            view.package,
            group_id,
            &items,
            DiagnosticStage::Stage2,
        )
    });
    if syntax::has_reliable_declarations(document.root_node(), cancelled)?
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
            imports: timed(stages, Stage::Imports, || {
                imports::Bindings::new(source, graph, package)
            }),
            items: Vec::new(),
            cancelled,
        };
        timed(stages, Stage::Declarations, || {
            declarations::collect(&mut analysis)
        })?;
        inspect(&mut analysis, stages)?;
        if document.root_node().has_error() && !analysis.locals.incomplete_package {
            let globals = std::mem::take(&mut analysis.locals.globals);
            analysis.locals = declarations::Locals::default();
            let filter = WalkMode::Recover {
                dirty: None,
                damaged_only: true,
            };
            declarations::collect_locals(&mut analysis, filter)?;
            unresolved::inspect_bare(
                &mut analysis,
                filter,
                |name| globals.contains_key(name),
                &mut HashSet::new(),
            )?;
        }
        for (i, item) in analysis.items.into_iter().enumerate() {
            if i & 255 == 0 {
                checkpoint(cancelled)?;
            }
            items.push(item.finish(source));
        }
    }
    items.extend(package_errors);
    checkpoint(cancelled)?;
    let degraded = graph.total_load_failure().is_some();
    Ok(DiagnosticReport::at_stage(
        key,
        items,
        DiagnosticStage::Stage2,
        if degraded {
            ContextCompleteness::Partial
        } else {
            ContextCompleteness::Complete
        },
    ))
}

pub(super) fn analyze_stage1(
    snapshot: &mut EngineSnapshot<'_>,
    document: &DocumentSnapshot<'_>,
    key: DiagnosticSnapshotKey,
    dirty: Option<Bytes<usize>>,
    verify: &mut HashSet<(usize, usize)>,
    cancelled: Option<&AtomicBool>,
    stages: &mut Option<Stages>,
) -> Result<DiagnosticReport> {
    checkpoint(cancelled)?;
    let mut items = timed(stages, Stage::Syntax, || {
        syntax::collect_syntax_diagnostics(document, cancelled)
    })?;
    let mut context = ContextCompleteness::Partial;
    let view = diagnostics_view(snapshot, document);
    if let Some(view) = &view {
        let package_errors = timed(stages, Stage::PackageErrors, || {
            package_errors::collect_package_errors(
                view.graph,
                document,
                view.package,
                view.group_id,
                &items,
                DiagnosticStage::Stage1,
            )
        });
        items.extend(package_errors);
    }
    if let Some(view) = view
        && let Some(package) = view.package
        && let Some(summaries) = timed(stages, Stage::Declarations, || {
            declarations::stage1_summaries(snapshot, document, &view, cancelled)
        })?
    {
        let source = document.indexed();
        let mut names = HashSet::new();
        for summary in &summaries {
            for (i, name) in summary.file_declarations().enumerate() {
                if i & 255 == 0 {
                    checkpoint(cancelled)?;
                }
                names.insert(name);
            }
        }
        let filter = WalkMode::Recover {
            dirty: dirty.as_ref(),
            damaged_only: false,
        };
        let mut analysis = Analysis {
            source,
            package,
            group_id: package.group_id(),
            resolver: snapshot.diagnostic_resolver(view.graph, cancelled),
            locals: declarations::Locals::default(),
            imports: timed(stages, Stage::Imports, || {
                imports::Bindings::new(source, view.graph, package)
            }),
            items: Vec::new(),
            cancelled,
        };
        timed(stages, Stage::Declarations, || {
            declarations::collect_locals(&mut analysis, filter)
        })?;
        timed(stages, Stage::Unresolved, || {
            unresolved::inspect_bare(&mut analysis, filter, |name| names.contains(name), verify)
        })?;
        for (i, item) in analysis.items.into_iter().enumerate() {
            if i & 255 == 0 {
                checkpoint(cancelled)?;
            }
            items.push(item.finish(source));
        }
        context = ContextCompleteness::Complete;
    }
    checkpoint(cancelled)?;
    Ok(DiagnosticReport::at_stage(
        key,
        items,
        DiagnosticStage::Stage1,
        context,
    ))
}

#[cfg(test)]
pub(crate) fn get_or_compute<'c>(
    cache: &'c mut DiagnosticsCache,
    snapshot: &mut EngineSnapshot<'_>,
    document: &DocumentSnapshot<'_>,
) -> Result<&'c DiagnosticReport> {
    let key = snapshot_key(snapshot, document)?;
    if cache.get(&key).is_none() {
        let report = analyze(snapshot, document, key.clone(), None, &mut None)?;
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
    walk(
        root,
        analysis.cancelled,
        WalkMode::Intact,
        |node, kind, parents, entering| {
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
        },
    )?;
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

#[derive(Clone, Copy)]
enum WalkMode<'a> {
    Intact,
    Recover {
        dirty: Option<&'a Bytes<usize>>,
        damaged_only: bool,
    },
}

impl WalkMode<'_> {
    fn includes(self, node: Node<'_>, has_errors: bool, depth: usize) -> bool {
        match self {
            Self::Intact => !has_errors || depth == 0 || !node.has_error(),
            Self::Recover {
                dirty,
                damaged_only,
            } => {
                !node.is_missing()
                    && (depth != 1
                        || ((!damaged_only || node.has_error())
                            && dirty.is_none_or(|dirty| {
                                node.start_byte() <= dirty.end && dirty.start <= node.end_byte()
                            })))
            }
        }
    }

    fn recovers(self) -> bool {
        matches!(self, Self::Recover { .. })
    }
}

fn walk<'t>(
    root: Node<'t>,
    cancelled: Option<&AtomicBool>,
    mode: WalkMode<'_>,
    mut visit: impl FnMut(Node<'t>, &'static str, Parents<'t, '_>, bool) -> Result<WalkControl>,
) -> Result<()> {
    let mut cursor = root.walk();
    let mut parents = Vec::new();
    let mut visited = 0_u32;
    let has_errors = root.has_error();
    loop {
        let node = cursor.node();
        if visited & 255 == 0 || parents.len() == 1 {
            checkpoint(cancelled)?;
        }
        visited = visited.wrapping_add(1);
        if node.is_named()
            && (!node.is_extra() || mode.recovers() && node.is_error())
            && mode.includes(node, has_errors, parents.len())
        {
            let kind = node.kind();
            if is_function(kind) {
                checkpoint(cancelled)?;
            }
            let control = visit(node, kind, Parents(&parents), true)?;
            let on_exit = (control == WalkControl::Scoped).then_some(kind);
            if control != WalkControl::Skip && !node.is_error() && cursor.goto_first_child() {
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
pub(crate) struct Stages {
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

    pub(crate) fn log(self, mut fields: serde_json::Value) {
        let millis = |stage: Stage| crate::telemetry::millis(self.times[stage as usize]);
        fields["syntax_ms"] = millis(Stage::Syntax).into();
        fields["declarations_ms"] = millis(Stage::Declarations).into();
        fields["imports_ms"] = millis(Stage::Imports).into();
        fields["unused_ms"] = millis(Stage::Unused).into();
        fields["unresolved_ms"] = millis(Stage::Unresolved).into();
        fields["types_ms"] = millis(Stage::Types).into();
        fields["package_errors_ms"] = millis(Stage::PackageErrors).into();
        fields["inspected_nodes"] = self.inspected_nodes.into();
        crate::telemetry::emit("diagnostic_stages", fields);
    }
}

fn timed<T>(stages: &mut Option<Stages>, stage: Stage, run: impl FnOnce() -> T) -> T {
    if let Some(stages) = stages {
        stages.measure::<true, _>(stage, run)
    } else {
        run()
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
