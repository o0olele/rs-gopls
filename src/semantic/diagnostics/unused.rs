use std::collections::HashSet;

use super::*;
use crate::semantic::source::is_declaration_name_in;

pub(super) fn assignment_target<'t>(parents: Parents<'t, '_>) -> Option<Node<'t>> {
    let mut ancestors = parents.iter().rev().peekable();
    while ancestors
        .next_if(|p| p.kind() == "parenthesized_expression")
        .is_some()
    {}
    let list = ancestors.next()?;
    let stmt = ancestors.next()?;
    (list.kind() == "expression_list" && stmt.child_by_field_name("left") == Some(list))
        .then_some(stmt)
}

pub(super) fn value_name(
    locals: &declarations::Locals<'_>,
    node: Node<'_>,
    kind: &str,
    parents: Parents<'_, '_>,
) -> bool {
    !locals.declarations.contains(&node.id())
        && !parents.at(1).is_some_and(|p| match p.kind() {
            "package_clause" | "import_spec" => true,
            "qualified_type" => p.child_by_field_name("name") == Some(node),
            "field_declaration" if kind == "type_identifier" => false,
            _ => is_declaration_name_in(node, p),
        })
}

pub(super) fn mark_read(
    analysis: &mut Analysis<'_, '_, '_>,
    parents: Parents<'_, '_>,
    binding: declarations::Local<'_>,
    used: &mut HashSet<usize>,
) -> Result<()> {
    if !assignment_target(parents)
        .is_some_and(|stmt| stmt.kind() != "assignment_statement" || token(stmt, "=").is_some())
        && !types::is_struct_key(analysis, parents)?
    {
        used.insert(binding.name.id());
    }
    Ok(())
}

pub(super) fn collect(analysis: &mut Analysis<'_, '_, '_>, used: &HashSet<usize>) -> Result<()> {
    for node in std::mem::take(&mut analysis.locals.variables) {
        checkpoint(analysis.cancelled)?;
        if !used.contains(&node.id()) {
            analysis.items.push(unused_variable(node, analysis.source));
        }
    }
    Ok(())
}

pub(super) fn unused_variable(node: Node<'_>, source: &IndexedSource) -> PendingItem {
    let name = node_text(node, &source.text);
    PendingItem {
        code: DiagnosticCode::VarUnused,
        range: node.byte_range(),
        message: format!("{name} declared and unused"),
        fix: Fix::ReplaceWithBlank(name.to_owned()),
    }
}
