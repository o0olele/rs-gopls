use super::*;
use crate::semantic::resolver::{
    MAX_TYPE_DEPTH, TypeIdentity, TypeKind, is_exported, is_predeclared, is_predeclared_type,
    materialize_type,
};
use crate::symbols::OutlineKind;

pub(super) fn inspect_bare(
    analysis: &mut Analysis<'_, '_, '_>,
    walk_mode: WalkMode<'_>,
    known_file_or_package_name: impl Fn(&str) -> bool,
    verify: &mut HashSet<(usize, usize)>,
) -> Result<()> {
    walk(
        analysis.source.tree.root_node(),
        analysis.cancelled,
        walk_mode,
        |node, kind, parents, _| {
            if kind == "identifier"
                && unused::value_name(analysis, node, kind, parents)
                && reliable_bare_use(node, parents)
                && !analysis.locals.may_have_recovered_binding(node)
            {
                let name = analysis.text(node);
                if name == "_"
                    || is_predeclared(name)
                    || analysis.locals.lookup(node, name).is_some()
                    || known_file_or_package_name(name)
                {
                    verify.remove(&(node.start_byte(), node.end_byte()));
                } else if !analysis.imports.possibly_binds_bare(name) {
                    analysis.error(
                        DiagnosticCode::NameUnresolved,
                        node,
                        format!("Undefined {name}"),
                    );
                }
            }
            Ok(WalkControl::Children)
        },
    )
}

fn reliable_bare_use(node: Node<'_>, parents: Parents<'_, '_>) -> bool {
    if node.has_error() || imports::qualifier(node, parents) {
        return false;
    }
    let mut statement = true;
    for parent in parents.iter().rev() {
        match parent.kind() {
            "statement_list" | "block" | "source_file" => statement = false,
            "qualified_type" | "selector_expression" => return false,
            "keyed_element"
                if parent.child_by_field_name("key").is_some_and(|key| {
                    key.start_byte() <= node.start_byte() && node.end_byte() <= key.end_byte()
                }) =>
            {
                return false;
            }
            _ if statement && parent.has_error() => return false,
            _ => {}
        }
    }
    true
}

fn type_annotation(node: Node<'_>, parents: Parents<'_, '_>) -> bool {
    parents.at(1).is_some_and(|parent| match parent.kind() {
        "var_spec"
        | "const_spec"
        | "type_spec"
        | "type_alias"
        | "parameter_declaration"
        | "variadic_parameter_declaration"
        | "type_assertion_expression"
        | "composite_literal" => parent.child_by_field_name("type") == Some(node),
        "field_declaration" => true,
        "function_declaration"
        | "method_declaration"
        | "func_literal"
        | "function_type"
        | "method_elem" => parent.child_by_field_name("result") == Some(node),
        _ => false,
    })
}

pub(super) fn check(
    analysis: &mut Analysis<'_, '_, '_>,
    node: Node<'_>,
    parents: Parents<'_, '_>,
    value_name: bool,
    local: Option<declarations::Local<'_>>,
    qualifier: bool,
) -> Result<()> {
    if matches!(node.kind(), "qualified_type" | "selector_expression") {
        return selector(analysis, node);
    }
    if !value_name {
        return Ok(());
    }
    let name = analysis.text(node);
    if node.kind() == "type_identifier"
        && type_annotation(node, parents)
        && !analysis.locals.incomplete_package
    {
        let actual_type = if let Some(local) = local {
            Some(local.is_type())
        } else if let Some(kind) = analysis.locals.globals.get(name) {
            Some(matches!(
                kind,
                OutlineKind::Struct | OutlineKind::Interface | OutlineKind::Class
            ))
        } else {
            is_predeclared(name).then(|| is_predeclared_type(name))
        };
        if actual_type == Some(false) {
            analysis.error(
                DiagnosticCode::TypeUnresolved,
                node,
                format!("{name} is not a type"),
            );
            return Ok(());
        }
    }
    if is_predeclared(name)
        || name == "_"
        || analysis.locals.globals.contains_key(name)
        || analysis.locals.incomplete_package
        || local.is_some()
    {
        return Ok(());
    }
    if types::literal_key(parents).is_some() && !types::is_container_key(analysis, parents)? {
        return Ok(());
    }
    if analysis
        .imports
        .can_bind(&mut analysis.resolver, name, qualifier)?
    {
        return Ok(());
    }
    let is_type = node.kind() == "type_identifier" && !qualifier;
    let code = if is_type {
        DiagnosticCode::TypeUnresolved
    } else {
        DiagnosticCode::NameUnresolved
    };
    let item = analysis.error(
        code,
        node,
        format!("Undefined {}{name}", if is_type { "type " } else { "" }),
    );
    if qualifier {
        item.fix = Fix::ImportPackage(name.to_owned());
    }
    Ok(())
}

fn selector(analysis: &mut Analysis<'_, '_, '_>, node: Node<'_>) -> Result<()> {
    let qualified = node.kind() == "qualified_type";
    let (Some(receiver), Some(field)) = (
        node.child_by_field_name(if qualified { "package" } else { "operand" }),
        node.child_by_field_name(if qualified { "name" } else { "field" }),
    ) else {
        return Ok(());
    };
    let name = analysis.text(field);
    if declarations::is_identifier(receiver)
        && analysis
            .locals
            .lookup(receiver, analysis.text(receiver))
            .is_none()
        && let Some(package) = analysis.imports.package(analysis.text(receiver))
    {
        if package.load_error {
            return Ok(());
        }
        let symbol = analysis.resolver.find_symbol(package, name, None)?;
        if !is_exported(name)
            && symbol.is_some()
            && package.directory_and_name() != analysis.package.directory_and_name()
        {
            analysis.error(
                DiagnosticCode::NameUnexported,
                field,
                format!("{name} is not exported by {}", package.name),
            );
        } else if symbol.is_none() {
            analysis.error(
                if qualified {
                    DiagnosticCode::TypeUnresolved
                } else {
                    DiagnosticCode::NameUnresolved
                },
                field,
                format!("Undefined {}.{name}", analysis.text(receiver)),
            );
        }
        return Ok(());
    }
    if qualified {
        return Ok(());
    }
    let Some(ty) = analysis.inferred(receiver)? else {
        return Ok(());
    };
    if analysis
        .resolver
        .member(analysis.package, &ty, name, true)?
        .is_some()
    {
        return Ok(());
    }
    if !is_exported(name)
        && let Some(owner) = analysis.resolver.resolve_alias(ty.clone())?
        && owner.package.directory_and_name() != analysis.package.directory_and_name()
        && analysis
            .resolver
            .member(owner.package, &owner, name, true)?
            .is_some()
    {
        analysis.error(
            DiagnosticCode::NameUnexported,
            field,
            format!("Cannot refer to unexported member {name}"),
        );
    } else if definitely_missing(&mut analysis.resolver, &ty, name)? {
        analysis.error(
            DiagnosticCode::FieldOrMethodUnresolved,
            field,
            format!("No field or method named {name}"),
        );
    }
    Ok(())
}

// A failed navigation lookup can also mean an incomplete embedding or ambiguous promotion.
pub(super) fn definitely_missing<'w>(
    resolver: &mut Resolver<'w, '_>,
    ty: &TypeIdentity<'w>,
    name: &str,
) -> Result<bool> {
    let mut pending = vec![(ty.clone(), 0)];
    let mut visited = 0;
    while let Some((ty, depth)) = pending.pop() {
        visited += 1;
        if visited > MAX_TYPE_DEPTH * MAX_TYPE_DEPTH
            || depth >= MAX_TYPE_DEPTH
            || ty.package.load_error
            || ty.pointers > 1
            || resolver.direct_member(&ty, name, false)?.is_some()
        {
            return Ok(false);
        }
        let Some((file, info, substitutions)) = resolver.type_context(&ty)? else {
            match &ty.kind {
                TypeKind::Named { name: base, .. }
                    if !is_predeclared_type(base)
                        || matches!(base.as_str(), "any" | "comparable")
                        || (base == "error" && name == "Error") =>
                {
                    return Ok(false);
                }
                TypeKind::Parameter(_) => return Ok(false),
                _ => continue,
            }
        };
        if info.has_type_terms
            || matches!(&ty.kind, TypeKind::Inline { shape, .. } if shape.has_type_terms)
        {
            return Ok(false);
        }
        for base in info
            .alias
            .into_iter()
            .chain(info.underlying)
            .chain(info.embedded)
        {
            let imports = &resolver.indexes.file_index(&file)?.imports;
            let Some(base) = materialize_type(
                resolver.workspace,
                ty.package,
                &file,
                imports,
                base,
                &substitutions,
            ) else {
                return Ok(false);
            };
            pending.push((base, depth + 1));
        }
    }
    Ok(true)
}
