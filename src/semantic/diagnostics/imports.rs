use std::collections::{HashMap, HashSet};

use super::package_errors::tested_package;
use super::*;
use crate::semantic::resolver::is_exported;
use crate::semantic::source::DiagnosticImport;
use crate::workspace::{PackageErrorKind, PackageLoadError};

pub(super) fn qualifier(node: Node<'_>, parents: Parents<'_, '_>) -> bool {
    parents.at(1).is_some_and(|p| match p.kind() {
        "qualified_type" => p.child_by_field_name("package") == Some(node),
        "selector_expression" => p.child_by_field_name("operand") == Some(node),
        _ => false,
    })
}

#[derive(Default)]
pub(super) struct Bindings<'w> {
    packages: HashMap<String, &'w Package>,
    explicit: HashSet<String>,
    escaped: HashSet<String>,
    dots: Vec<&'w Package>,
    unknown_qualifier: bool,
    unknown_dot: bool,
}

impl<'w> Bindings<'w> {
    pub(super) fn new(source: &IndexedSource, graph: &'w Workspace, package: &Package) -> Self {
        let mut bindings = Self::default();
        for import in source.diagnostic_imports.iter() {
            let alias = import.alias.as_deref();
            if alias == Some("_") {
                continue;
            }
            let target = import
                .path
                .as_ref()
                .and_then(|path| graph.resolve_import(package, path));
            match (import.path.as_deref(), alias, target) {
                (_, Some("."), Some(target)) if !target.load_error => bindings.dots.push(target),
                (_, Some("."), _) => bindings.unknown_dot = true,
                (_, name, Some(target)) => {
                    bindings
                        .packages
                        .entry(name.unwrap_or(&target.name).to_owned())
                        .or_insert(target);
                }
                (_, None, None) => bindings.unknown_qualifier = true,
                (Some(_), Some(name), None) => {
                    bindings.explicit.insert(name.to_owned());
                }
                (None, Some(name), None) => {
                    bindings.escaped.insert(name.to_owned());
                }
            }
        }
        bindings
    }

    pub(super) fn package(&self, name: &str) -> Option<&'w Package> {
        self.packages.get(name).copied()
    }

    pub(super) fn possibly_binds_bare(&self, name: &str) -> bool {
        self.packages.contains_key(name)
            || self.explicit.contains(name)
            || self.escaped.contains(name)
            || self.unknown_qualifier
            || (is_exported(name) && (self.unknown_dot || !self.dots.is_empty()))
    }

    pub(super) fn can_bind(
        &self,
        resolver: &mut Resolver<'w, '_>,
        name: &str,
        qualifier: bool,
    ) -> Result<bool> {
        if self.packages.contains_key(name)
            || self.explicit.contains(name)
            || (qualifier && (self.unknown_qualifier || self.escaped.contains(name)))
        {
            return Ok(true);
        }
        if is_exported(name) {
            if self.unknown_dot {
                return Ok(true);
            }
            for package in &self.dots {
                if resolver.find_symbol(package, name, None)?.is_some() {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}

pub(super) fn collect(analysis: &mut Analysis<'_, '_, '_>, used: &HashSet<&str>) -> Result<()> {
    let root = analysis.source.tree.root_node();
    if analysis.source.diagnostic_imports.is_empty() {
        return Ok(());
    }
    let graph = analysis.resolver.workspace;
    let ancestors = import_ancestors(
        graph,
        analysis.package,
        analysis.group_id,
        analysis.cancelled,
    )?;
    let mut aliases = HashSet::new();
    for spec in analysis.source.diagnostic_imports.iter() {
        checkpoint(analysis.cancelled)?;
        let Some(path) = spec.path.as_deref() else {
            continue;
        };
        if path == "C" {
            continue;
        }
        let target = graph.resolve_import_in_group(analysis.package, analysis.group_id, path);
        let alias = spec
            .alias
            .as_deref()
            .or_else(|| target.map(|p| p.name.as_str()));
        if let Some(name) = alias.filter(|name| !matches!(*name, "_" | "."))
            && !aliases.insert(name)
        {
            analysis.error_bytes(
                DiagnosticCode::ImportDuplicateAlias,
                spec.binding.clone(),
                format!("Import name {name} is already bound in this file"),
            );
        }
        if target
            .is_some_and(|pkg| pkg.directory_and_name() == analysis.package.directory_and_name())
        {
            analysis.error_bytes(
                DiagnosticCode::ImportSelf,
                spec.literal.clone(),
                format!("Package imports itself as {path:?}"),
            );
            continue;
        }
        if target.is_some_and(|pkg| ancestors.contains(pkg.id.as_str())) {
            analysis.error_bytes(
                DiagnosticCode::ImportCycle,
                spec.literal.clone(),
                format!("Import of {path:?} creates an import cycle"),
            );
            continue;
        }
        if target.is_none() {
            // go list indexes dependencies. A new import in an overlay may be outside that closure.
            if analysis.source.document_version.is_none()
                || graph.package_errors().iter().any(|error| {
                    error.group_id == analysis.group_id
                        && import_error_evidence(error, analysis.package, path)
                })
            {
                analysis.error_bytes(
                    DiagnosticCode::ImportInvalid,
                    spec.literal.clone(),
                    format!("Cannot resolve import {path:?}"),
                );
            }
            continue;
        }
        if !root.has_error()
            && let Some(item) = unused_import(spec, path, alias, used)
        {
            analysis.items.push(item);
        }
    }
    Ok(())
}

fn unused_import(
    spec: &DiagnosticImport,
    path: &str,
    alias: Option<&str>,
    used: &HashSet<&str>,
) -> Option<PendingItem> {
    let name = alias.filter(|name| !matches!(*name, "_" | ".") && !used.contains(name))?;
    Some(PendingItem {
        code: DiagnosticCode::ImportUnused,
        range: spec.binding.clone(),
        message: format!("{path:?} imported as {name} and unused"),
        fix: Fix::RemoveImport(path.to_owned(), spec.spec.clone()),
    })
}

pub(super) fn import_ancestors<'w>(
    graph: &'w Workspace,
    package: &Package,
    group_id: usize,
    cancelled: Option<&AtomicBool>,
) -> Result<HashSet<&'w str>> {
    // An overlay's import can create a cycle through any transitive importer.
    let mut ancestors = HashSet::new();
    let mut stack: Vec<_> = graph.direct_importers(package).collect();
    while let Some(pkg) = stack.pop() {
        checkpoint(cancelled)?;
        if graph.package_in_group(pkg, group_id) && ancestors.insert(pkg.id.as_str()) {
            stack.extend(graph.direct_importers(pkg));
        }
    }
    Ok(ancestors)
}

#[derive(Default)]
pub(in crate::semantic) struct LocalImports<'s, 'w> {
    pub(in crate::semantic) unused: Vec<DiagnosticItem>,
    pub(in crate::semantic) package_names: HashMap<&'s str, &'w str>,
}

pub(in crate::semantic) fn local_imports<'s, 'w>(
    snapshot: &EngineSnapshot<'w>,
    document: &'s DocumentSnapshot<'_>,
    check_usage: bool,
) -> Result<LocalImports<'s, 'w>> {
    let mut result = LocalImports::default();
    let source = document.indexed();
    if source.diagnostic_imports.is_empty() {
        return Ok(result);
    }
    let Some(view) = diagnostics_view(snapshot, document) else {
        return Ok(result);
    };
    let Some(package) = view.package else {
        return Ok(result);
    };
    if super::super::completion::render::package_name(source) != Some(&package.name) {
        return Ok(result);
    }
    let graph = view.graph;
    let group = view.package_group(package);
    let candidates: Vec<_> = source
        .diagnostic_imports
        .iter()
        .filter_map(|spec| {
            let path = spec.path.as_deref()?;
            if path == "C" || matches!(spec.alias.as_deref(), Some("_" | ".")) {
                return None;
            }
            let target = graph.resolve_import_in_group(package, group, path)?;
            Some((spec, path, target))
        })
        .collect();
    let root = document.root_node();
    if candidates.is_empty() {
        return Ok(result);
    }
    let mut used = HashSet::new();
    let check_usage = check_usage && !root.has_error();
    if check_usage
        && !import_usage_from_spelling(
            root,
            &source.text,
            source.diagnostic_imports.iter().map(|spec| spec.spec.end),
            candidates
                .iter()
                .map(|(spec, _, target)| spec.alias.as_deref().unwrap_or(&target.name)),
            &mut used,
        )
    {
        used.clear();
        let mut locals = declarations::Locals::default();
        declarations::LexicalAnalysis {
            source,
            locals: &mut locals,
            items: &mut Vec::new(),
            cancelled: None,
        }
        .collect(root, WalkMode::Intact, false)?;
        walk(root, None, WalkMode::Intact, |node, kind, parents, _| {
            if declarations::identifier_kind(kind)
                && qualifier(node, parents)
                && unused::value_name(&locals, node, kind, parents)
            {
                let name = node_text(node, &source.text);
                if locals.lookup(node, name).is_none() {
                    used.insert(name);
                }
            }
            Ok(WalkControl::Children)
        })?;
    }
    let ancestors = if check_usage {
        import_ancestors(graph, package, group, None)?
    } else {
        HashSet::new()
    };
    for (spec, path, target) in candidates {
        if !target.load_error {
            result.package_names.insert(path, target.name.as_str());
        }
        if check_usage
            && target.directory_and_name() != package.directory_and_name()
            && !ancestors.contains(target.id.as_str())
            && let Some(item) = unused_import(
                spec,
                path,
                spec.alias.as_deref().or(Some(&target.name)),
                &used,
            )
        {
            result.unused.push(item.finish(source));
        }
    }
    Ok(result)
}

fn import_usage_from_spelling<'a>(
    root: Node<'_>,
    text: &str,
    import_ends: impl Iterator<Item = usize>,
    names: impl Iterator<Item = &'a str>,
    used: &mut HashSet<&'a str>,
) -> bool {
    let header_end = root
        .named_children(&mut root.walk())
        .take_while(|node| {
            matches!(
                node.kind(),
                "package_clause" | "import_declaration" | "comment"
            )
        })
        .last()
        .map_or(0, |node| node.end_byte());
    if import_ends.max().is_some_and(|end| end > header_end) {
        return false;
    }
    for name in names {
        if let Some(offset) = text[header_end..].find(name) {
            if !first_import_call(root, header_end + offset, name.len()) {
                return false;
            }
            used.insert(name);
        }
    }
    true
}

// The first spelling after the imports cannot have an earlier local binding.
// Restrict that proof to a direct call on the right of := in ordinary nested
// function blocks. Other scopes and ambiguous spellings use lexical analysis.
fn first_import_call(root: Node<'_>, start: usize, length: usize) -> bool {
    let Some(node) = root.named_descendant_for_byte_range(start, start + length) else {
        return false;
    };
    if node.kind() != "identifier" || node.byte_range() != (start..start + length) {
        return false;
    }
    let Some(selector) = node.parent().filter(|parent| {
        parent.kind() == "selector_expression"
            && parent.child_by_field_name("operand") == Some(node)
    }) else {
        return false;
    };
    let Some(call) = selector.parent().filter(|parent| {
        parent.kind() == "call_expression"
            && parent.child_by_field_name("function") == Some(selector)
    }) else {
        return false;
    };
    let Some(list) = call.parent() else {
        return false;
    };
    let Some(statement) = list.parent().filter(|parent| {
        parent.kind() == "short_var_declaration"
            && parent.child_by_field_name("right") == Some(list)
    }) else {
        return false;
    };
    let mut child = statement;
    while let Some(parent) = child.parent() {
        match parent.kind() {
            "block" | "statement_list" => child = parent,
            "function_declaration" | "method_declaration" => {
                return parent.child_by_field_name("body") == Some(child)
                    && parent.parent() == Some(root);
            }
            _ => return false,
        }
    }
    false
}

pub(super) fn import_error_evidence(
    error: &PackageLoadError,
    package: &Package,
    path: &str,
) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_import_call_requires_a_provably_unshadowed_spelling() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
            .unwrap();
        let header = "package use\nimport (\n\t\"strings\"\n\t\"fmt\"\n)\n";
        for (body, expected) in [
            ("func Use() { { n := fmt.Sprint(1); _ = n } }\n", true),
            (
                "func Use(fmt interface{ Sprint(int) string }) { n := fmt.Sprint(1); _ = n }\n",
                false,
            ),
        ] {
            let source = format!("{header}{body}");
            let tree = parser.parse(&source, None).unwrap();
            let root = tree.root_node();
            assert!(!root.has_error());
            let mut used = HashSet::new();
            assert_eq!(
                import_usage_from_spelling(
                    root,
                    &source,
                    [header.len() - 1].into_iter(),
                    ["strings", "fmt"].into_iter(),
                    &mut used
                ),
                expected,
            );
            assert_eq!(used.contains("fmt"), expected);
            assert!(!used.contains("strings"));
        }
    }
}
