use std::collections::{HashMap, HashSet};

use lsp_types::{Range, TextEdit};

use super::*;
use crate::semantic::source::node_at_range;

pub(in crate::semantic) fn local_variable_fixes(
    document: &DocumentSnapshot<'_>,
    hints: &[(DiagnosticCode, Range)],
) -> Result<HashMap<(DiagnosticCode, Range), (DiagnosticItem, Vec<TextEdit>)>> {
    let source = document.indexed();
    let mut declarations = HashMap::<Node<'_>, HashSet<(DiagnosticCode, Range)>>::new();
    for &(code, range) in hints {
        let Some(node) = node_at_range(source, range) else {
            continue;
        };
        let mut root = node;
        while let Some(parent) = root.parent().filter(|p| p.kind() != "source_file") {
            root = parent;
        }
        if root.kind() != "source_file" && !root.has_error() {
            declarations.entry(root).or_default().insert((code, range));
        }
    }

    let mut fixes = HashMap::new();
    for (root, targets) in declarations {
        let mut locals = declarations::Locals::default();
        let mut items = Vec::new();
        declarations::LexicalAnalysis {
            source,
            locals: &mut locals,
            items: &mut items,
            cancelled: None,
        }
        .collect(root, WalkMode::Intact, true)?;

        for item in items
            .into_iter()
            .filter(|item| item.code == DiagnosticCode::ShortVarNoNewVars)
        {
            let item = item.finish(source);
            let key = (item.code, item.range);
            if targets.contains(&key)
                && let Some(operator) = node_at_range(source, item.range)
                && let Some(left) = operator
                    .parent()
                    .and_then(|p| p.child_by_field_name("left"))
                && assignable_names(left, None, source, &locals)
            {
                let edit = TextEdit::new(item.range, "=".into());
                fixes.insert(key, (item, vec![edit]));
            }
        }

        let variables: Vec<_> = locals
            .variables
            .iter()
            .copied()
            .filter(|name| {
                targets.contains(&(
                    DiagnosticCode::VarUnused,
                    source.byte_range(name.byte_range()),
                ))
            })
            .collect();
        if variables.is_empty() {
            continue;
        }
        let names: HashSet<_> = variables
            .iter()
            .map(|name| node_text(*name, &source.text))
            .collect();
        let mut referenced = HashSet::new();
        walk(root, None, WalkMode::Intact, |node, kind, _, _| {
            if declarations::identifier_kind(kind) {
                let name = node_text(node, &source.text);
                if names.contains(name)
                    && let Some(binding) = locals.lookup(node, name)
                    && binding.name != node
                {
                    // Even a write needs the declaration that this fix removes.
                    referenced.insert(binding.name.id());
                }
            }
            Ok(WalkControl::Children)
        })?;
        let declared: HashSet<_> = locals.variables.iter().map(Node::id).collect();
        for name in variables
            .into_iter()
            .filter(|name| !referenced.contains(&name.id()))
        {
            if let Some(edits) = unused_variable_edits(document, name, &locals, &declared) {
                let item = unused::unused_variable(name, source).finish(source);
                fixes.insert((item.code, item.range), (item, edits));
            }
        }
    }
    Ok(fixes)
}

fn assignable_names(
    left: Node<'_>,
    omitted: Option<Node<'_>>,
    source: &IndexedSource,
    locals: &declarations::Locals<'_>,
) -> bool {
    let mut seen = HashSet::new();
    left.named_children(&mut left.walk())
        .filter(|n| Some(*n) != omitted && !n.is_extra())
        .all(|node| {
            let name = node_text(node, &source.text);
            name == "_"
                || seen.insert(name)
                    && locals.lookup(node, name).is_some_and(|binding| {
                        matches!(
                            binding.kind,
                            "short_var_declaration"
                                | "range_clause"
                                | "receive_statement"
                                | "var_spec"
                                | "parameter_declaration"
                                | "variadic_parameter_declaration"
                                | "type_case"
                                | "default_case"
                        )
                    })
        })
}

fn unused_variable_edits(
    document: &DocumentSnapshot<'_>,
    name: Node<'_>,
    locals: &declarations::Locals<'_>,
    declared: &HashSet<usize>,
) -> Option<Vec<TextEdit>> {
    let parent = name.parent()?;
    let mut edits = vec![TextEdit::new(
        document.byte_range(name.byte_range()),
        "_".into(),
    )];
    if parent.kind() == "var_spec" {
        return Some(edits);
    }
    let statement = parent.parent()?;
    let operator = token(statement, ":=")?;
    if statement.kind() == "type_switch_statement"
        && statement.child_by_field_name("alias") == Some(parent)
    {
        let prefix = &document.text()[name.end_byte()..operator.start_byte()];
        return prefix.trim().is_empty().then(|| {
            vec![TextEdit::new(
                document.byte_range(name.start_byte()..operator.end_byte()),
                String::new(),
            )]
        });
    }
    if !matches!(
        statement.kind(),
        "short_var_declaration" | "range_clause" | "receive_statement"
    ) || statement.child_by_field_name("left") != Some(parent)
    {
        return None;
    }
    if !parent
        .named_children(&mut parent.walk())
        .any(|other| other != name && declared.contains(&other.id()))
    {
        if !assignable_names(parent, Some(name), document.indexed(), locals) {
            return None;
        }
        edits.push(TextEdit::new(
            document.byte_range(operator.byte_range()),
            "=".into(),
        ));
    }
    Some(edits)
}
