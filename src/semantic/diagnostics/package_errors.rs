use lsp_types::Position;

use super::syntax::visible_range;
use super::{
    DiagnosticCode, DiagnosticItem, DiagnosticSeverity, DocumentSnapshot, Package, Workspace,
    is_test_source,
};
use crate::workspace::{PackageErrorKind, PackageLoadError, path_key};

pub(super) fn collect_package_errors(
    graph: &Workspace,
    document: &DocumentSnapshot<'_>,
    package: Option<&Package>,
    group_id: Option<usize>,
    syntax: &[DiagnosticItem],
) -> Vec<DiagnosticItem> {
    let key = path_key(document.path());
    let tests = is_test_source(document.path());
    let group_scope = graph.group_scope_for_path(document.path());
    let root = document.root_node();
    let fallback = root
        .named_children(&mut root.walk())
        .find(|n| n.kind() == "package_clause")
        .map_or(0..0, |n| n.byte_range());
    graph
        .package_errors()
        .iter()
        .filter_map(|error| {
            let positioned = error
                .path
                .as_ref()
                .is_some_and(|path| path_key(path) == key);
            let same_context = if error.kind == PackageErrorKind::Group {
                group_scope == Some(error.group_id)
            } else {
                // A broken dependency outside the roots might have a Go
                // source position without a package node or registered folder.
                group_id.map_or(positioned, |group| group == error.group_id)
            };
            let tested = tested_package(error);
            if !same_context || !tests && tested.is_some() {
                return None;
            }
            if error.kind != PackageErrorKind::Group
                && document.version().is_some()
                && syntax.is_empty()
                && is_parse_error(&error.message)
            {
                return None;
            }
            // A test compilation can fail in a shared source file. Report that
            // failure at its test package's clause when visiting a test file.
            if !positioned {
                if error
                    .path
                    .as_deref()
                    .is_some_and(|path| tested.is_none() || is_test_source(path))
                {
                    return None;
                }
                let owner_matches = match unpositioned_package(graph, error) {
                    Some(owner) => {
                        tests != owner.is_production()
                            && package.is_some_and(|package| package.id == owner.id)
                    }
                    None => {
                        tests == tested.is_some()
                            && error
                                .package_dir
                                .as_deref()
                                .is_some_and(|dir| key.parent() == Some(path_key(dir).as_path()))
                            && (error.package_name.is_empty()
                                || document.outline().package.as_deref()
                                    == Some(&error.package_name))
                            && package.is_none_or(|package| {
                                let raw = error
                                    .import_path
                                    .split_once(" [")
                                    .map_or(error.import_path.as_str(), |(raw, _)| raw);
                                package.import_path() == raw && package.test_of() == tested
                            })
                    }
                };
                if error.kind != PackageErrorKind::Group && !owner_matches {
                    return None;
                }
            }
            let line = if positioned { error.line } else { None };
            let bytes = line
                .and_then(|line| {
                    let line = line.checked_sub(1)?;
                    let start = document.offset(Position::new(line, 0))?;
                    let end = document.offset(Position::new(line, u32::MAX))?;
                    let column = error.column.unwrap_or(1).saturating_sub(1) as usize;
                    let offset = document
                        .text()
                        .floor_char_boundary(start.saturating_add(column).min(end));
                    Some(offset..offset)
                })
                .unwrap_or_else(|| fallback.clone());
            Some(DiagnosticItem::new(
                DiagnosticCode::PackageLoad,
                DiagnosticSeverity::Error,
                visible_range(document, bytes),
                error.message.clone(),
            ))
        })
        .collect()
}

fn unpositioned_package<'w>(graph: &'w Workspace, error: &PackageLoadError) -> Option<&'w Package> {
    let owner = error.package_id.as_deref().and_then(|id| graph.package(id));
    if error.kind == PackageErrorKind::Direct && owner.is_some() {
        return owner;
    }
    stack_package(graph, error, owner).or(owner)
}

fn stack_package<'w>(
    graph: &'w Workspace,
    error: &PackageLoadError,
    owner: Option<&Package>,
) -> Option<&'w Package> {
    let raw = error.import_stack.last()?.as_str();
    let path = raw.strip_suffix(" (test)").unwrap_or(raw);
    if let Some(tested) = tested_package(error) {
        let path = path.split_once(" [").map_or(path, |(path, _)| path);
        return graph
            .package_in_group_by_import(error.group_id, &format!("{path} [{tested}.test]"));
    }
    // A generated test binary has an import map pointing to test variants,
    // even though go list leaves its own ForTest field empty.
    owner
        .and_then(|package| graph.resolve_import_in_group(package, error.group_id, path))
        .or_else(|| graph.package_in_group_by_import(error.group_id, path))
}

pub(super) fn tested_package(error: &PackageLoadError) -> Option<&str> {
    if error.for_test.is_empty() {
        error.import_stack.last()?.strip_suffix(" (test)")
    } else {
        Some(&error.for_test)
    }
}

fn is_parse_error(message: &str) -> bool {
    message.lines().any(|line| {
        let detail = line.rsplit_once(": ").map_or(line, |(_, detail)| detail);
        [
            "expected ",
            "unexpected ",
            "invalid character",
            "illegal character",
            "illegal UTF-8",
            "string literal not terminated",
            "raw string literal not terminated",
            "rune literal not terminated",
            "comment not terminated",
            "missing import path",
        ]
        .iter()
        .any(|prefix| detail.starts_with(prefix))
            || line.contains("syntax error")
    })
}
