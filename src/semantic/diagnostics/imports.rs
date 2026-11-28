use std::collections::{HashMap, HashSet};

use super::package_errors::tested_package;
use super::*;
use crate::semantic::resolver::is_exported;
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
    // Reverse edges give all packages with an import path back to this package.
    let mut ancestors = HashSet::new();
    let mut stack: Vec<_> = graph.direct_importers(analysis.package).collect();
    while let Some(pkg) = stack.pop() {
        checkpoint(analysis.cancelled)?;
        if graph.package_in_group(pkg, analysis.group_id) && ancestors.insert(&pkg.id) {
            stack.extend(graph.direct_importers(pkg));
        }
    }
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
        if target.is_some_and(|pkg| ancestors.contains(&pkg.id)) {
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
            && let Some(name) =
                alias.filter(|name| !matches!(*name, "_" | ".") && !used.contains(name))
        {
            let item = analysis.error_bytes(
                DiagnosticCode::ImportUnused,
                spec.binding.clone(),
                format!("{path:?} imported as {name} and unused"),
            );
            item.fix = Fix::RemoveImport(path.to_owned(), spec.spec.clone());
        }
    }
    Ok(())
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
