mod cache;
mod declarations;
mod imports;
mod model;
mod package_errors;
mod syntax;
mod types;
mod unresolved;
mod unused;

#[cfg(test)]
mod tests;

use anyhow::Result;
use tree_sitter::Node;

use super::QueryProgress;
use super::index::IndexedSource;
use super::resolver::Resolver;
use super::snapshot::{DocumentSnapshot, EngineSnapshot};
use super::source::{is_test_source, node_text};
use crate::workspace::{Package, Workspace};

pub(crate) use cache::DiagnosticsCache;
pub(crate) use model::{
    DiagnosticCode, DiagnosticFixData, DiagnosticItem, DiagnosticReport, DiagnosticSeverity,
    DiagnosticSnapshotKey, DiagnosticTag,
};

fn snapshot_key(
    snapshot: &EngineSnapshot<'_>,
    document: &DocumentSnapshot<'_>,
) -> DiagnosticSnapshotKey {
    DiagnosticSnapshotKey::new(
        document,
        snapshot.load_for(false).generation,
        snapshot.workspace().is_some_and(Workspace::includes_tests),
    )
}

#[cfg(test)]
pub(crate) fn analyze_document(
    snapshot: &mut EngineSnapshot<'_>,
    document: &DocumentSnapshot<'_>,
) -> Result<DiagnosticReport> {
    let key = snapshot_key(snapshot, document);
    analyze(snapshot, document, key)
}

fn analyze(
    snapshot: &mut EngineSnapshot<'_>,
    document: &DocumentSnapshot<'_>,
    key: DiagnosticSnapshotKey,
) -> Result<DiagnosticReport> {
    let mut items = syntax::collect_syntax_diagnostics(document);
    let requires_tests = is_test_source(document.path());
    let QueryProgress::Ready(graph) = snapshot.diagnostics_graph(requires_tests) else {
        return Ok(DiagnosticReport::new(key, items, true));
    };
    let package = match snapshot.package_for(document, requires_tests) {
        QueryProgress::Ready(package) => package,
        _ => None,
    };
    let group_id = graph
        .group_for_path(document.path())
        .or_else(|| package.map(Package::group_id));
    let package_errors =
        package_errors::collect_package_errors(graph, document, package, group_id, &items);
    if syntax::has_reliable_declarations(document.root_node())
        && let Some(package) = package
        && document.outline().package.as_deref() == Some(&package.name)
    {
        let mut analysis = Analysis {
            source: document.indexed(),
            package,
            group_id: group_id
                .filter(|group| graph.package_in_group(package, *group))
                .unwrap_or_else(|| package.group_id()),
            resolver: snapshot.resolver(graph),
            locals: declarations::Locals::default(),
            items,
        };
        declarations::collect(&mut analysis)?;
        imports::collect(&mut analysis)?;
        unused::collect(&mut analysis)?;
        walk(document.root_node(), |node| {
            unresolved::check(&mut analysis, node)?;
            types::check(&mut analysis, node)
        })?;
        items = analysis.items;
    }
    items.extend(package_errors);
    Ok(DiagnosticReport::new(key, items, false))
}

pub(crate) fn get_or_compute<'c>(
    cache: &'c mut DiagnosticsCache,
    snapshot: &mut EngineSnapshot<'_>,
    document: &DocumentSnapshot<'_>,
) -> Result<&'c DiagnosticReport> {
    let key = snapshot_key(snapshot, document);
    let partial = !matches!(
        snapshot.diagnostics_graph(is_test_source(document.path())),
        QueryProgress::Ready(_)
    );
    if !cache
        .report(&key.path_key)
        .is_some_and(|r| r.snapshot == key && r.is_partial == partial)
    {
        let report = analyze(snapshot, document, key.clone())?;
        cache.insert(report);
    }
    Ok(cache
        .report(&key.path_key)
        .expect("report for the current snapshot"))
}

struct Analysis<'s, 'w, 'i> {
    source: &'s IndexedSource,
    package: &'w Package,
    group_id: usize,
    resolver: Resolver<'w, 'i>,
    locals: declarations::Locals<'s>,
    items: Vec<DiagnosticItem>,
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
    ) -> &mut DiagnosticItem {
        self.items.push(DiagnosticItem::new(
            code,
            DiagnosticSeverity::Error,
            self.source.range(node),
            message,
        ));
        self.items.last_mut().unwrap()
    }
}

fn walk<'t>(root: Node<'t>, mut visit: impl FnMut(Node<'t>) -> Result<()>) -> Result<()> {
    let mut cursor = root.walk();
    loop {
        let node = cursor.node();
        if node.is_named() && !node.is_extra() && (!node.has_error() || node == root) {
            visit(node)?;
            if cursor.goto_first_child() {
                continue;
            }
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return Ok(());
            }
        }
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
