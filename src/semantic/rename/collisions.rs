use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use lsp_types::Range;
use tree_sitter::Node;

use crate::scope::{declaration_binding, receiver_type_parameters, reference_binding};
use crate::semantic::resolver::{Resolver, TypeIdentity, is_exported};
use crate::semantic::source::{is_declaration_name, is_type_declaration, node_text};
use crate::semantic::{Engine, Source};
use crate::symbols::SymbolKind;
use crate::workspace::{Package, path_key};

pub(super) fn check_collisions(
    engine: &mut Engine,
    family: &[(PathBuf, Range)],
    new_name: &str,
) -> Result<()> {
    for (path, range) in family {
        let mut snapshot = engine.snapshot();
        let document = snapshot
            .document(path)?
            .context("missing rename declaration")?;
        let source = document.indexed();
        let import = source.imports.iter().any(|import| {
            import.binding == *range
                || (range.start == range.end && import.binding.start == range.start)
        });
        let workspace = snapshot
            .workspace()
            .context("checking names requires a package graph")?;
        let package = snapshot
            .active_package(workspace, source)
            .context("declaration is outside the active package")?;
        let mut member_owner = None;
        let mut field_name = None;
        if import {
            ensure!(new_name != "init", "an import cannot be named init");
        } else {
            let name = source
                .reference_at(range.start)
                .map(|query| query.name())
                .filter(|name| source.range(*name) == *range)
                .context("cannot identify declaration for collision checking")?;
            let old = node_text(name, &source.text);
            let parent = name.parent().context("missing declaration syntax")?;
            if parent.kind() == "method_declaration"
                && parent.child_by_field_name("name") == Some(name)
            {
                member_owner =
                    Some(method_owner(source, name).context("cannot identify method receiver")?);
            } else if let Some(container) = member_container(name) {
                let mut duplicate = false;
                source.for_each_name(container, new_name, |other| {
                    duplicate |= other != name && member_container(other) == Some(container)
                });
                ensure!(!duplicate, "member {new_name} already exists in this type");
                let Some(spec) = container.parent().filter(|n| is_type_declaration(*n)) else {
                    continue;
                };
                let owner = spec
                    .child_by_field_name("name")
                    .context("type name is missing")?;
                if declaration_scope(owner).is_none_or(|s| s.kind() != "source_file") {
                    continue;
                }
                member_owner = Some(node_text(owner, &source.text));
                if parent.kind() != "method_elem" && old != new_name {
                    field_name = Some(old);
                }
            } else {
                let scope = declaration_scope(name).context("cannot identify declaration scope")?;
                if scope.kind() != "source_file" {
                    check_local(source, name, scope, new_name)?;
                    continue;
                }
                ensure!(
                    old != "init" && new_name != "init",
                    "renaming a package declaration to or from init changes initialization"
                );
                ensure!(
                    package.name != "main" || (old != "main" && new_name != "main"),
                    "renaming main would change the program entry point"
                );
            }
        }
        let mut resolver = snapshot.resolver(workspace);
        if let Some(old) = field_name {
            check_derived_receivers(
                &mut resolver,
                package,
                &(path.clone(), *range),
                old,
                new_name,
            )?;
        } else if let Some(owner) = member_owner
            && let Some(ty) = resolver.underlying_type(TypeIdentity::named(package, owner))?
            && let Some(field) = resolver.direct_member(&ty, new_name, false)?
            && !matches!(field.symbol.kind, SymbolKind::Function(_))
            && (is_exported(new_name)
                || field.package.directory_and_name() == package.directory_and_name())
        {
            ensure!(
                path_key(&field.file) == path_key(path) && field.symbol.range == *range,
                "{owner} already has a field named {new_name}"
            );
        }
        let files: BTreeSet<_> = workspace
            .same_package_variants(package)
            .filter(|variant| variant.name == package.name)
            .flat_map(|variant| resolver.indexes.files(variant).into_owned())
            .collect();
        for file in files {
            let index = resolver.indexes.file_index(&file)?;
            let same_file = path_key(&file) == path_key(path);
            if let Some(owner) = member_owner {
                if let Some(symbol) = index.symbols.member(owner, new_name) {
                    ensure!(
                        same_file
                            && symbol.range == *range
                            && !other_declaration(source, *range, new_name, Some(owner)),
                        "member {owner}.{new_name} already exists in {}",
                        file.display()
                    );
                }
                continue;
            }
            if let Some(symbol) = index.symbols.declaration(new_name) {
                ensure!(
                    same_file
                        && symbol.range == *range
                        && !other_declaration(source, *range, new_name, None),
                    "{new_name} is already declared in package {}",
                    package.name
                );
            }
            if import && !same_file {
                continue;
            }
            // Imports share a file block, and Go additionally forbids a package
            // declaration from duplicating any identifier in a file block.
            for existing in index.imports.clone().iter() {
                if import && existing.binding.start == range.start {
                    continue;
                }
                let dependency = workspace.resolve_import(package, &existing.path);
                let taken = match existing.alias.as_deref() {
                    Some(".") if is_exported(new_name) => {
                        let dependency = dependency
                            .context("cannot check names from an unresolved dot import")?;
                        resolver
                            .indexes
                            .find(dependency, new_name, |idx| {
                                idx.symbols.declaration(new_name).map(|s| s.range)
                            })?
                            .is_some()
                    }
                    Some(alias) => alias == new_name,
                    None => {
                        dependency
                            .context("cannot check the name of an unresolved import")?
                            .name
                            == new_name
                    }
                };
                ensure!(
                    !taken,
                    "{new_name} conflicts with an import in {}",
                    file.display()
                );
            }
        }
    }
    Ok(())
}

// A type definition retains the underlying struct fields. A method declared on
// that new type (or an alias used as a receiver) cannot take the same field name.
fn check_derived_receivers(
    resolver: &mut Resolver<'_, '_>,
    package: &Package,
    field: &(PathBuf, Range),
    old: &str,
    new_name: &str,
) -> Result<()> {
    let workspace = resolver.workspace;
    for candidate in workspace
        .navigation_packages()
        .filter(|p| workspace.is_project_package(p))
    {
        if !is_exported(new_name) && candidate.directory_and_name() != package.directory_and_name()
        {
            continue;
        }
        let mut receivers = BTreeSet::new();
        for file in resolver.indexes.files(candidate).into_owned() {
            receivers.extend(
                resolver
                    .indexes
                    .file_index(&file)?
                    .symbols
                    .member_owners(new_name)
                    .map(str::to_owned),
            );
        }
        for receiver in receivers {
            let ty = TypeIdentity::named(candidate, &receiver);
            let Some(underlying) = resolver.underlying_type(ty)? else {
                continue;
            };
            if let Some(inherited) = resolver.direct_member(&underlying, old, false)? {
                ensure!(
                    inherited.symbol.range != field.1
                        || path_key(&inherited.file) != path_key(&field.0),
                    "field {new_name} would conflict with method {receiver}.{new_name}"
                );
            }
        }
    }
    Ok(())
}

fn ancestors(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    std::iter::successors(Some(node), |n| n.parent())
}

fn method_owner<'a>(source: &'a Source, name: Node<'_>) -> Option<&'a str> {
    let method = name.parent().filter(|m| {
        m.kind() == "method_declaration" && m.child_by_field_name("name") == Some(name)
    })?;
    let parameter = method.child_by_field_name("receiver")?.named_child(0)?;
    let mut ty = parameter.child_by_field_name("type")?;
    loop {
        ty = match ty.kind() {
            "type_identifier" => return Some(node_text(ty, &source.text)),
            "pointer_type" | "parenthesized_type" => ty.named_child(0)?,
            "generic_type" => ty.child_by_field_name("type")?,
            _ => return None,
        };
    }
}

// FileSymbols retains the first declaration of each name. Inspect the syntax
// when that first result is the target itself and the input might be ambiguous.
fn other_declaration(source: &Source, target: Range, spelling: &str, owner: Option<&str>) -> bool {
    let mut found = false;
    source.for_each_name(source.tree.root_node(), spelling, |other| {
        if source.range(other) == target {
            return;
        }
        if let Some(owner) = owner {
            let field_owner = member_container(other)
                .and_then(|n| n.parent())
                .filter(|n| is_type_declaration(*n))
                .and_then(|n| n.child_by_field_name("name"))
                .filter(|n| declaration_scope(*n).is_some_and(|s| s.kind() == "source_file"));
            found |= method_owner(source, other) == Some(owner)
                || field_owner.is_some_and(|n| node_text(n, &source.text) == owner);
        } else {
            found |= other.parent().is_some_and(|parent| {
                matches!(
                    parent.kind(),
                    "function_declaration" | "type_spec" | "type_alias" | "var_spec" | "const_spec"
                )
            }) && is_declaration_name(other)
                && declaration_scope(other).is_some_and(|s| s.kind() == "source_file");
        }
    });
    found
}

fn member_container(name: Node<'_>) -> Option<Node<'_>> {
    let mut node = name;
    while let Some(parent) = node.parent() {
        match parent.kind() {
            "field_declaration" | "method_elem" => {
                let mut cursor = parent.walk();
                let mut names = parent.children_by_field_name("name", &mut cursor);
                let direct = match names.next() {
                    Some(first) => first == name || names.any(|n| n == name),
                    None => parent.child_by_field_name("type") == Some(node),
                };
                return direct
                    .then(|| {
                        ancestors(parent)
                            .find(|n| matches!(n.kind(), "struct_type" | "interface_type"))
                    })
                    .flatten();
            }
            "generic_type" | "pointer_type" | "parenthesized_type" => {
                if parent.named_child(0) != Some(node) {
                    return None;
                }
            }
            "qualified_type" if parent.child_by_field_name("name") == Some(node) => {}
            _ => return None,
        }
        node = parent;
    }
    None
}

fn declaration_scope(name: Node<'_>) -> Option<Node<'_>> {
    for node in ancestors(name).skip(1) {
        match node.kind() {
            "expression_list"
                if node.parent().is_some_and(|switch| {
                    switch.kind() == "type_switch_statement"
                        && switch.child_by_field_name("alias") == Some(node)
                }) =>
            {
                return Some(node);
            }
            "block" => {
                let owner = node.parent()?;
                return Some(
                    if matches!(
                        owner.kind(),
                        "function_declaration" | "method_declaration" | "func_literal"
                    ) && owner.child_by_field_name("body") == Some(node)
                    {
                        owner
                    } else {
                        node
                    },
                );
            }
            "function_declaration"
            | "method_declaration"
            | "method_elem"
            | "func_literal"
            | "function_type"
                if node.child_by_field_name("name") != Some(name) =>
            {
                return Some(node);
            }
            "type_spec" | "type_alias"
                if node
                    .child_by_field_name("type_parameters")
                    .is_some_and(|args| {
                        args.start_byte() <= name.start_byte() && name.end_byte() <= args.end_byte()
                    }) =>
            {
                return Some(node);
            }
            "if_statement"
            | "for_statement"
            | "expression_switch_statement"
            | "type_switch_statement"
            | "expression_case"
            | "type_case"
            | "default_case"
            | "communication_case"
            | "source_file" => return Some(node),
            _ => {}
        }
    }
    None
}

fn receiver_parameter(name: Node<'_>) -> bool {
    if name.parent().is_none_or(|p| p.kind() != "type_elem") {
        return false;
    }
    ancestors(name)
        .find(|n| n.kind() == "method_declaration")
        .and_then(receiver_type_parameters)
        .is_some_and(|(_, args)| name.parent().and_then(|p| p.parent()) == Some(args))
}

fn guard_in_case(guard: Node<'_>, scope: Node<'_>) -> bool {
    guard
        .parent()
        .and_then(|list| list.parent())
        .is_some_and(|switch| {
            switch.kind() == "type_switch_statement"
                && switch.child_by_field_name("alias") == guard.parent()
                && scope.parent() == Some(switch)
        })
}

fn check_local(source: &Source, name: Node<'_>, scope: Node<'_>, new_name: &str) -> Result<()> {
    let mut duplicate = false;
    // A type-switch guard declares one implicit variable in every case.
    source.for_each_name(source.tree.root_node(), new_name, |other| {
        if other == name
            || member_container(other).is_some()
            || (!is_declaration_name(other)
                && declaration_binding(other, &source.text).is_none()
                && !receiver_parameter(other))
            || reference_binding(other, &source.text) == Some(name)
        {
            return;
        }
        if let Some(candidate_scope) = declaration_scope(other) {
            duplicate |= scope == candidate_scope
                || guard_in_case(name, candidate_scope)
                || guard_in_case(other, scope);
        }
    });
    ensure!(!duplicate, "{new_name} is already declared in this block");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestProject;

    fn check(first: &str, second: &str, target: &str, new_name: &str) -> Result<()> {
        let project = TestProject::new();
        let path = project.write("a.go", first);
        project.write("b.go", second);
        let mut engine = Engine::new(project.root.clone());
        engine.install_workspace(
            engine.workspace_load(),
            project.workspace(&["a.go", "b.go"], true),
        );
        let start = first.find(target).unwrap();
        let name_len = target
            .split(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
            .next()
            .unwrap()
            .len();
        let span = engine
            .snapshot()
            .document(&path)?
            .unwrap()
            .byte_range(start..start + name_len);
        check_collisions(&mut engine, &[(path, span)], new_name)
    }

    #[test]
    fn preserves_fields_of_defined_types_and_alias_receivers() {
        let types = "package sample\ntype Shape struct { Size int }; type Alias = Shape; type Derived Shape\n";
        let methods = "package sample\nfunc (Alias) Count() {}; func (*Derived) Total() {}\n";
        for reserved in ["Count", "Total"] {
            assert!(
                check(types, methods, "Size int", reserved).is_err(),
                "{reserved}"
            );
        }
        for method in ["Count()", "Total()"] {
            assert!(check(methods, types, method, "Size").is_err(), "{method}");
        }
        assert!(check(methods, types, "Total()", "Count").is_ok());
    }

    #[test]
    fn places_type_switch_aliases_in_the_individual_case_blocks() {
        let other = "package sample\n";
        let code = "package sample\nfunc f(x any) { switch initv := 1; val := x.(type) { case int: var same int; _, _, _ = val, same, initv } }";
        assert!(check(code, other, "val :=", "same").is_err());
        assert!(check(code, other, "same int", "val").is_err());
        assert!(check(code, other, "val :=", "initv").is_ok());
        let reused = "package sample\nfunc f(x any) { switch val := x.(type) { case int: val, next := 1, 2; _, _ = val, next } }";
        assert!(check(reused, other, "val :=", "val").is_ok());
    }

    #[test]
    fn rejects_ambiguous_old_names_without_counting_short_redeclarations() {
        let b = "package sample\n";
        for a in [
            "package sample\nvar Old int\nconst Old = 2",
            "package sample\nfunc f(Old int, Old string) {}",
            "package sample\ntype T struct { Old int; Old string }",
            "package sample\ntype T int\nfunc (T) Old() {}\nfunc (*T) Old() {}",
        ] {
            assert!(check(a, b, "Old", "Old").is_err(), "{a}");
        }
        assert!(
            check(
                "package sample\nvar Old int",
                "package sample\nfunc Old() {}",
                "Old",
                "Old"
            )
            .is_err()
        );
        let short = "package sample\nfunc f(Old int) { Old, next := Old, 2; _, _ = Old, next }";
        assert!(check(short, b, "Old int", "Old").is_ok());
        let named = "package sample\ntype T struct { Old int }; type U struct { Old int }";
        assert!(check(named, b, "Old int", "Old").is_ok());
    }

    #[test]
    fn separates_blocks_and_combines_function_parameters_results_and_body() {
        let other = "package sample\n";
        let code = "package sample\ntype T int\nfunc (recv T) call(arg int) (out int) { x := 1; { nested := x; _ = nested }; _ = recv; return }\n";
        for target in ["arg int", "recv T", "out int", "x :="] {
            for conflict in ["arg", "recv", "out", "x"] {
                if !target.starts_with(conflict) {
                    assert!(
                        check(code, other, target, conflict).is_err(),
                        "{target} -> {conflict}"
                    );
                }
            }
            assert!(
                check(code, other, target, "nested").is_ok(),
                "{target} -> nested"
            );
        }
        let branches = "package sample\nfunc f() { if old := 1; old > 0 { value := 2; _ = value }; if value := 2; value > 0 {} }";
        assert!(check(branches, other, "old :=", "value").is_ok());
    }

    #[test]
    fn checks_package_and_file_scope_in_both_directions() {
        let a = "package sample\nimport alias \"elsewhere\"\nvar Old int\n";
        let b = "package sample\nimport taken \"other\"\nconst Occupied = 1\n";
        for name in ["alias", "taken", "Occupied", "init"] {
            assert!(check(a, b, "Old int", name).is_err(), "{name}");
        }
        assert!(check(a, b, "alias \"", "Occupied").is_err());
        assert!(check(a, b, "alias \"", "taken").is_ok());
        assert!(check(a, b, "alias \"", "init").is_err());
        assert!(check(a, b, "Old int", "main").is_ok());
        assert!(
            check(
                "package sample\nfunc f(local int) {}",
                b,
                "local int",
                "init"
            )
            .is_ok()
        );
    }

    #[test]
    fn checks_fields_and_methods_of_the_same_receiver_across_files() {
        let a = "package sample\ntype T struct { Field int; Taken int }\ntype I interface { First(); Second() }\nfunc (T) Old() {}\n";
        let b =
            "package sample\nfunc (*T) Method() {}\ntype Other struct{}\nfunc (Other) Free() {}\n";
        for (target, new_name) in [
            ("Field int", "Taken"),
            ("Field int", "Method"),
            ("Old()", "Field"),
            ("Old()", "Method"),
            ("First()", "Second"),
        ] {
            assert!(
                check(a, b, target, new_name).is_err(),
                "{target} -> {new_name}"
            );
        }
        assert!(check(a, b, "Old()", "Free").is_ok());
    }
}
