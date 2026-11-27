use std::collections::HashSet;

use super::package_errors::tested_package;
use super::*;
use crate::semantic::source::import_path;
use crate::workspace::{PackageErrorKind, PackageLoadError};

pub(super) fn qualifier(node: Node<'_>) -> bool {
    node.parent().is_some_and(|p| match p.kind() {
        "qualified_type" => p.child_by_field_name("package") == Some(node),
        "selector_expression" => p.child_by_field_name("operand") == Some(node),
        _ => false,
    })
}

pub(super) fn specs(root: Node<'_>) -> Vec<Node<'_>> {
    children(root)
        .into_iter()
        .filter(|n| n.kind() == "import_declaration")
        .flat_map(children)
        .flat_map(|node| {
            if node.kind() == "import_spec_list" {
                children(node)
            } else {
                vec![node]
            }
        })
        .filter(|n| n.kind() == "import_spec")
        .collect()
}

pub(super) fn collect(analysis: &mut Analysis<'_, '_, '_>) -> Result<()> {
    let root = analysis.source.tree.root_node();
    let specs = specs(root);
    if specs.is_empty() {
        return Ok(());
    }
    let mut used = HashSet::new();
    walk(root, |node| {
        if declarations::is_identifier(node)
            && qualifier(node)
            && analysis.locals.lookup(node, analysis.text(node)).is_none()
        {
            used.insert(analysis.text(node));
        }
        Ok(())
    })?;
    let graph = analysis.resolver.workspace;
    // Reverse edges give all packages with an import path back to this package.
    let mut ancestors = HashSet::new();
    let mut stack: Vec<_> = graph.direct_importers(analysis.package).collect();
    while let Some(pkg) = stack.pop() {
        if graph.package_in_group(pkg, analysis.group_id) && ancestors.insert(&pkg.id) {
            stack.extend(graph.direct_importers(pkg));
        }
    }
    let mut aliases = HashSet::new();
    for spec in specs {
        let Some(literal) = spec.child_by_field_name("path") else {
            continue;
        };
        let Some(path) = import_path(analysis.text(literal)) else {
            continue;
        };
        if path == "C" {
            continue;
        }
        let target = graph.resolve_import_in_group(analysis.package, analysis.group_id, &path);
        let binding = spec.child_by_field_name("name").unwrap_or(literal);
        let alias = spec
            .child_by_field_name("name")
            .map(|n| analysis.text(n))
            .or_else(|| target.map(|p| p.name.as_str()));
        if let Some(name) = alias.filter(|name| !matches!(*name, "_" | "."))
            && !aliases.insert(name)
        {
            analysis.error(
                DiagnosticCode::ImportDuplicateAlias,
                binding,
                format!("Import name {name} is already bound in this file"),
            );
        }
        if target
            .is_some_and(|pkg| pkg.directory_and_name() == analysis.package.directory_and_name())
        {
            analysis.error(
                DiagnosticCode::ImportSelf,
                literal,
                format!("Package imports itself as {path:?}"),
            );
            continue;
        }
        if target.is_some_and(|pkg| ancestors.contains(&pkg.id)) {
            analysis.error(
                DiagnosticCode::ImportCycle,
                literal,
                format!("Import of {path:?} creates an import cycle"),
            );
            continue;
        }
        if target.is_none() {
            // go list indexes dependencies. A new import in an overlay may be outside that closure.
            if analysis.source.document_version.is_none()
                || graph.package_errors().iter().any(|error| {
                    error.group_id == analysis.group_id
                        && import_error_evidence(error, analysis.package, &path)
                })
            {
                analysis.error(
                    DiagnosticCode::ImportInvalid,
                    literal,
                    format!("Cannot resolve import {path:?}"),
                );
            }
            continue;
        }
        if !root.has_error()
            && let Some(name) =
                alias.filter(|name| !matches!(*name, "_" | ".") && !used.contains(name))
        {
            let range = analysis.source.range(spec);
            let item = analysis.error(
                DiagnosticCode::ImportUnused,
                binding,
                format!("{path:?} imported as {name} and unused"),
            );
            item.tags.push(DiagnosticTag::Unnecessary);
            item.fix_data = DiagnosticFixData::RemoveUnusedImport {
                import_path: path,
                spec_range: range,
            };
        }
    }
    Ok(())
}

fn import_error_evidence(error: &PackageLoadError, package: &Package, path: &str) -> bool {
    error.kind != PackageErrorKind::Group
        && tested_package(error).is_none_or(|tested| package.test_of() == Some(tested))
        && (!package.is_production() || !error.path.as_deref().is_some_and(is_test_source))
        && (error.import_path == path
            || error.import_stack.last().is_some_and(|entry| entry == path)
            || error
                .message
                .split_whitespace()
                .any(|word| word.trim_matches([':', '"']) == path))
}
