use std::cmp::Reverse;
use std::sync::atomic::{AtomicBool, Ordering};

use lsp_types::{DocumentHighlight, DocumentHighlightKind, Position, Range};
use tree_sitter::Node;

use super::{
    Editor, Occurrence, SymbolId, SymbolKind, is_breakable, is_function, named_children,
    parameter_slots,
};
use crate::scope::initializer_at;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum HighlightKey {
    Exits(usize),
    Jump {
        target: usize,
        labeled: bool,
    },
    Name {
        binding: Option<HighlightBinding>,
        slot: Option<ReturnSlotKey>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum HighlightBinding {
    Declaration(usize),
    Builtin(Box<str>),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct ReturnSlotKey {
    owner: usize,
    index: usize,
    signature: Option<std::ops::Range<usize>>,
}

#[derive(Clone, Copy)]
pub(super) enum Target<'tree> {
    Exits(Node<'tree>),
    Jump(JumpTarget<'tree>),
    Name {
        symbol: Option<NameTarget<'tree>>,
        slot: Option<ReturnTarget<'tree>>,
    },
}

#[derive(Clone, Copy)]
pub(super) struct NameTarget<'tree> {
    symbol: Occurrence<'tree>,
    spelling: &'tree str,
    scope: Option<Node<'tree>>,
}

#[derive(Clone, Copy)]
pub(super) struct ReturnTarget<'tree> {
    body: Node<'tree>,
    index: usize,
    signature: Option<Node<'tree>>,
}

impl Target<'_> {
    pub(super) fn key(&self) -> HighlightKey {
        match *self {
            Self::Exits(owner) => HighlightKey::Exits(owner.start_byte()),
            Self::Jump(target) => HighlightKey::Jump {
                target: match target {
                    JumpTarget::Statement(node) | JumpTarget::Label(node) => node.start_byte(),
                },
                labeled: matches!(target, JumpTarget::Label(_)),
            },
            Self::Name { symbol, slot } => HighlightKey::Name {
                binding: symbol.map(|target| match target.symbol.id {
                    SymbolId::Declaration(offset) => HighlightBinding::Declaration(offset),
                    SymbolId::Builtin(name) => HighlightBinding::Builtin(name.into()),
                }),
                slot: slot.map(|target| ReturnSlotKey {
                    owner: target.body.start_byte(),
                    index: target.index,
                    signature: target.signature.map(|node| node.byte_range()),
                }),
            },
        }
    }

    pub(super) fn path(&self) -> &'static str {
        match self {
            Self::Exits(_) => "function_exit",
            Self::Jump(_) => "jump",
            Self::Name { slot: Some(_), .. } => "return_slot",
            Self::Name {
                symbol: Some(NameTarget { scope: Some(_), .. }),
                ..
            } => "lexical",
            Self::Name { .. } => "name_offsets",
        }
    }
}

pub(super) fn prepare<'tree>(editor: &Editor<'tree>, position: Position) -> Option<Target<'tree>> {
    let leaf = caret_leaf(editor, position)?;
    if matches!(leaf.kind(), "func" | "return") || is_panic_callee(editor, leaf) {
        return editor.function_owner(leaf).map(Target::Exits);
    }
    if matches!(
        leaf.kind(),
        "for" | "switch" | "select" | "break" | "continue"
    ) {
        return jump_target_at(editor, leaf).map(Target::Jump);
    }

    let slot = editor.function_owner(leaf).and_then(|owner| {
        let body = owner.child_by_field_name("body")?;
        let slots = result_slots(owner);
        if slots.is_empty() {
            return None;
        }
        let (index, signature) = return_slot_at(editor, leaf, owner, &slots)?;
        Some(ReturnTarget {
            body,
            index,
            signature,
        })
    });
    let symbol = (leaf.kind() != "dot")
        .then(|| editor.symbol(leaf))
        .flatten()
        .map(|symbol| NameTarget {
            spelling: editor.text(leaf),
            scope: lexical_scope(editor, leaf, symbol),
            symbol,
        });
    (symbol.is_some() || slot.is_some()).then_some(Target::Name { symbol, slot })
}

fn lexical_scope<'tree>(
    editor: &Editor<'tree>,
    leaf: Node<'tree>,
    symbol: Occurrence<'tree>,
) -> Option<Node<'tree>> {
    let SymbolId::Declaration(offset) = symbol.id else {
        return None;
    };
    if matches!(
        symbol.kind,
        SymbolKind::Namespace | SymbolKind::Property | SymbolKind::Method
    ) {
        return None;
    }
    let declaration = if leaf.start_byte() == offset {
        leaf
    } else {
        editor.node_for_bytes(offset, offset + 1)?
    };
    let scope = match symbol.kind {
        SymbolKind::Label => editor.function_owner(declaration),
        SymbolKind::Parameter => editor.ancestors(declaration).find(|node| {
            is_function(*node) || matches!(node.kind(), "function_type" | "method_elem")
        }),
        SymbolKind::TypeParameter => editor
            .ancestors(declaration)
            .find(|node| is_function(*node) || matches!(node.kind(), "type_spec" | "type_alias")),
        _ => local_declaration_scope(editor, declaration),
    }?;

    let inside = scope.start_byte() <= leaf.start_byte() && leaf.end_byte() <= scope.end_byte();
    // Scope recovery can join a detached body to its preceding signature.
    // Include every same-name candidate in an uncertain recovery scope.
    let recovered = scope.has_error()
        || editor
            .ancestors(scope)
            .take_while(|node| node.kind() != "source_file")
            .any(|node| node.is_error() || node.is_missing())
        || (is_function(scope)
            && scope.child_by_field_name("body").is_none()
            && editor.root().has_error());
    (inside && !recovered).then_some(scope)
}

fn local_declaration_scope<'tree>(
    editor: &Editor<'tree>,
    declaration: Node<'tree>,
) -> Option<Node<'tree>> {
    let parent = editor.parent(declaration)?;
    if parent.kind() == "expression_list" {
        let statement = editor.parent(parent)?;
        let owner = editor.parent(statement)?;
        match statement.kind() {
            "type_switch_statement" => return Some(statement),
            "range_clause" if owner.kind() == "for_statement" => return Some(owner),
            "receive_statement" if owner.kind() == "communication_case" => return Some(owner),
            "short_var_declaration" => match owner.kind() {
                "if_statement" | "expression_switch_statement" | "type_switch_statement" => {
                    return Some(owner);
                }
                "for_clause" => {
                    return editor
                        .parent(owner)
                        .filter(|node| node.kind() == "for_statement");
                }
                _ => {}
            },
            _ => return None,
        }
    }
    editor
        .ancestors(parent)
        .take_while(|node| !node.is_error() && !is_function(*node))
        .find(|node| node.kind() == "statement_list")
}

pub(super) fn compute<'tree>(
    editor: &Editor<'tree>,
    target: Target<'tree>,
    cancelled: Option<&AtomicBool>,
) -> Option<(Vec<DocumentHighlight>, usize)> {
    check_cancelled(cancelled)?;
    let mut result = Vec::new();
    let mut count = 0;
    let mut add = |node, kind| {
        let range = editor.range(node);
        if range.start < range.end {
            result.push(DocumentHighlight {
                range,
                kind: Some(kind),
            });
        }
    };

    match target {
        Target::Exits(owner) => {
            function_exits(editor, owner, cancelled, |node| {
                count += 1;
                add(node, DocumentHighlightKind::TEXT);
            })?;
        }
        Target::Jump(target) => {
            jump_occurrences(editor, target, cancelled, |node| {
                count += 1;
                add(node, DocumentHighlightKind::TEXT);
            })?;
        }
        Target::Name { symbol, slot } => {
            if let Some(slot) = slot {
                return_occurrences(slot, cancelled, |node| {
                    count += 1;
                    add(node, DocumentHighlightKind::TEXT);
                })?;
            }
            if let Some(target) = symbol {
                count += name_occurrences(editor, target, cancelled, add)?;
                result.extend(implicit_import_highlight(
                    editor,
                    target.spelling,
                    target.symbol,
                ));
            }
        }
    }

    result.sort_by_key(|item| {
        let priority = match item.kind {
            Some(DocumentHighlightKind::WRITE) => 2,
            Some(DocumentHighlightKind::READ) => 1,
            _ => 0,
        };
        (item.range.start, item.range.end, Reverse(priority))
    });
    result.dedup_by_key(|item| item.range);
    check_cancelled(cancelled)?;
    Some((result, count))
}

fn name_occurrences<'tree>(
    editor: &Editor<'tree>,
    target: NameTarget<'tree>,
    cancelled: Option<&AtomicBool>,
    mut add: impl FnMut(Node<'tree>, DocumentHighlightKind),
) -> Option<usize> {
    let source = editor.document.indexed();
    let mut count = 0;
    let mut candidate = |node| {
        count += 1;
        if let Some(candidate) = editor.symbol(node)
            && candidate.id == target.symbol.id
        {
            add(node, access(editor, node, &candidate));
        }
    };
    let includes_special_tokens = target.symbol.kind == SymbolKind::Label
        || matches!(target.spelling, "true" | "false" | "nil" | "iota");
    let ready = if includes_special_tokens {
        None
    } else {
        source.ready_name_offsets(target.spelling)
    };
    if let Some(scope) = target.scope
        && ready.is_none()
    {
        walk(scope, cancelled, |node| {
            if node.child_count() == 0 && editor.text(node) == target.spelling {
                candidate(node);
            }
            true
        })?;
    } else {
        let cold_offsets;
        let offsets = if let Some(offsets) = ready {
            offsets
        } else {
            cold_offsets = source.highlight_name_offsets(target.spelling, cancelled)?;
            &*cold_offsets
        };
        let offsets = if let Some(scope) = target.scope {
            let first = offsets.partition_point(|&offset| offset < scope.start_byte());
            let last = offsets.partition_point(|&offset| offset < scope.end_byte());
            &offsets[first..last]
        } else {
            offsets
        };
        for &offset in offsets {
            check_cancelled(cancelled)?;
            let end = offset + target.spelling.len();
            let node = match target.scope {
                Some(scope) => scope.descendant_for_byte_range(offset, end),
                None => editor.node_for_bytes(offset, end),
            };
            if let Some(node) = node
                && node.byte_range() == (offset..end)
                && !node.is_extra()
                && node.child_count() == 0
            {
                candidate(node);
            }
        }
    }
    Some(count)
}

fn check_cancelled(cancelled: Option<&AtomicBool>) -> Option<()> {
    (!cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed))).then_some(())
}

fn implicit_import_highlight(
    editor: &Editor<'_>,
    spelling: &str,
    symbol: Occurrence<'_>,
) -> Option<DocumentHighlight> {
    if symbol.kind != SymbolKind::Namespace {
        return None;
    }
    let binding = editor.import_binding(spelling)?;
    if symbol.id != SymbolId::Declaration(binding.start_byte())
        || editor.parent(binding)?.child_by_field_name("path") != Some(binding)
    {
        return None;
    }
    // The path text inside the quotes is the declaration range for an implicit import.
    let range = Range::new(
        editor.position(binding.start_byte() + 1),
        editor.position(binding.end_byte() - 1),
    );
    Some(DocumentHighlight {
        range,
        kind: Some(DocumentHighlightKind::TEXT),
    })
}

fn caret_leaf<'tree>(editor: &Editor<'tree>, position: Position) -> Option<Node<'tree>> {
    let offset = editor.offset(position)?;
    let text = editor.source();
    let char_at = |offset| text.get(offset..)?.chars().next();
    let locate = |offset| {
        let end = offset + char_at(offset)?.len_utf8();
        editor.node_for_bytes(offset, end)
    };

    let current = locate(offset);
    if current.is_some_and(in_comment) {
        return None;
    }
    if !char_at(offset).is_some_and(caret_character)
        && let Some((previous, _)) = text.get(..offset)?.char_indices().next_back()
        && char_at(previous).is_some_and(caret_character)
    {
        return locate(previous).filter(|node| !in_comment(*node));
    }
    current
}

fn caret_character(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '_' | '\'' | '"' | '`')
}

fn in_comment(node: Node<'_>) -> bool {
    node.kind() == "comment"
}

pub(super) fn access<'tree>(
    editor: &Editor<'tree>,
    node: Node<'tree>,
    symbol: &Occurrence<'_>,
) -> DocumentHighlightKind {
    if !matches!(
        symbol.kind,
        SymbolKind::Variable | SymbolKind::Parameter | SymbolKind::Property
    ) {
        return DocumentHighlightKind::TEXT;
    }

    let parent = editor.parent(node);
    let variable_definition = symbol.declaration
        && parent.is_some_and(|parent| matches!(parent.kind(), "var_spec" | "const_spec"));
    let struct_key = symbol.kind == SymbolKind::Property
        && parent
            .filter(|parent| parent.kind() == "literal_element")
            .is_some_and(|key| {
                editor.parent(key).is_some_and(|element| {
                    element.kind() == "keyed_element"
                        && element.child_by_field_name("key") == Some(key)
                })
            });
    if variable_definition || struct_key || is_write_target(editor, node, parent) {
        DocumentHighlightKind::WRITE
    } else {
        DocumentHighlightKind::READ
    }
}

fn is_write_target<'tree>(
    editor: &Editor<'tree>,
    mut expression: Node<'tree>,
    mut enclosing: Option<Node<'tree>>,
) -> bool {
    while let Some(parent) = enclosing {
        match parent.kind() {
            "parenthesized_expression" => {}
            "unary_expression"
                if parent
                    .child_by_field_name("operator")
                    .is_some_and(|operator| matches!(operator.kind(), "*" | "&" | "<-")) => {}
            "selector_expression" if parent.child_by_field_name("field") == Some(expression) => {}
            "inc_statement" | "dec_statement" => return true,
            "send_statement" => return parent.child_by_field_name("channel") == Some(expression),
            "expression_list" => {
                return editor
                    .parent(parent)
                    .is_some_and(|statement| match statement.kind() {
                        "assignment_statement"
                        | "short_var_declaration"
                        | "range_clause"
                        | "receive_statement" => {
                            statement.child_by_field_name("left") == Some(parent)
                        }
                        "type_switch_statement" => {
                            statement.child_by_field_name("alias") == Some(parent)
                        }
                        _ => false,
                    });
            }
            _ => return false,
        }
        expression = parent;
        enclosing = editor.parent(parent);
    }
    false
}

fn is_panic_callee<'tree>(editor: &Editor<'tree>, callee: Node<'tree>) -> bool {
    callee.kind() == "identifier"
        && editor.text(callee) == "panic"
        && editor.parent(callee).is_some_and(|call| {
            call.kind() == "call_expression" && call.child_by_field_name("function") == Some(callee)
        })
        && editor
            .symbol(callee)
            .is_some_and(|symbol| symbol.default_library)
}

fn function_exits<'tree>(
    editor: &Editor<'tree>,
    owner: Node<'tree>,
    cancelled: Option<&AtomicBool>,
    mut add: impl FnMut(Node<'tree>),
) -> Option<()> {
    if let Some(keyword) = keyword(owner, "func") {
        add(keyword);
    }
    let Some(body) = owner.child_by_field_name("body") else {
        return Some(());
    };
    walk_function_body(body, cancelled, |node| match node.kind() {
        "return_statement" => {
            add(node);
            false
        }
        "call_expression"
            if node
                .child_by_field_name("function")
                .is_some_and(|callee| is_panic_callee(editor, callee)) =>
        {
            add(node);
            true
        }
        _ => true,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum JumpTarget<'tree> {
    Statement(Node<'tree>),
    Label(Node<'tree>),
}

impl<'tree> JumpTarget<'tree> {
    fn scope(self, editor: &Editor<'tree>) -> Node<'tree> {
        match self {
            Self::Statement(statement) => statement,
            Self::Label(label) => editor
                .function_owner(label)
                .and_then(|owner| owner.child_by_field_name("body"))
                .or_else(|| editor.parent(label))
                .unwrap_or(label),
        }
    }
}

fn jump_target_at<'tree>(editor: &Editor<'tree>, leaf: Node<'tree>) -> Option<JumpTarget<'tree>> {
    let parent = editor.parent(leaf)?;
    if is_breakable(parent) {
        Some(JumpTarget::Statement(parent))
    } else {
        jump_target(editor, parent)
    }
}

fn jump_target<'tree>(editor: &Editor<'tree>, jump: Node<'tree>) -> Option<JumpTarget<'tree>> {
    if !matches!(jump.kind(), "break_statement" | "continue_statement") {
        return None;
    }
    if let Some(label) = named_children(jump).find(|node| node.kind() == "label_name") {
        return editor.label_definition(label).map(JumpTarget::Label);
    }
    let allowed = |node: Node<'_>| {
        if jump.kind() == "continue_statement" {
            node.kind() == "for_statement"
        } else {
            is_breakable(node)
        }
    };
    editor
        .ancestors(jump)
        .skip(1)
        .take_while(|node| !is_function(*node))
        .find(|node| allowed(*node))
        .map(JumpTarget::Statement)
}

fn jump_occurrences<'tree>(
    editor: &Editor<'tree>,
    target: JumpTarget<'tree>,
    cancelled: Option<&AtomicBool>,
    mut add: impl FnMut(Node<'tree>),
) -> Option<()> {
    match target {
        JumpTarget::Statement(statement) => {
            let keyword_name = match statement.kind() {
                "for_statement" => "for",
                "select_statement" => "select",
                _ => "switch",
            };
            if let Some(keyword) = keyword(statement, keyword_name) {
                add(keyword);
            }
        }
        JumpTarget::Label(label) => add(label),
    }
    walk_function_body(target.scope(editor), cancelled, |node| {
        if jump_target(editor, node) == Some(target) {
            add(node);
        }
        true
    })
}

struct ResultSlot<'tree> {
    name: Option<Node<'tree>>,
    ty: Option<Node<'tree>>,
}

fn result_slots(owner: Node<'_>) -> Vec<ResultSlot<'_>> {
    let Some(result) = owner.child_by_field_name("result") else {
        return Vec::new();
    };
    if result.kind() != "parameter_list" {
        return vec![ResultSlot {
            name: None,
            ty: Some(result),
        }];
    }
    parameter_slots(result)
        .map(|(parameter, name)| ResultSlot {
            name,
            ty: parameter.child_by_field_name("type"),
        })
        .collect()
}

fn return_occurrences<'tree>(
    target: ReturnTarget<'tree>,
    cancelled: Option<&AtomicBool>,
    mut add: impl FnMut(Node<'tree>),
) -> Option<()> {
    if let Some(signature) = target.signature {
        add(signature);
    }
    walk_function_body(target.body, cancelled, |node| {
        if node.kind() != "return_statement" {
            return true;
        }
        let list = named_children(node).find(|part| part.kind() == "expression_list");
        if let (Some(expression), _) = initializer_at(list, target.index) {
            add(expression);
        }
        false
    })
}

fn return_slot_at<'tree>(
    editor: &Editor<'tree>,
    leaf: Node<'tree>,
    owner: Node<'tree>,
    slots: &[ResultSlot<'tree>],
) -> Option<(usize, Option<Node<'tree>>)> {
    let mut node = leaf;
    while node != owner {
        if let Some(index) = slots
            .iter()
            .position(|slot| slot.name.or(slot.ty) == Some(node))
        {
            return Some((index, Some(node)));
        }
        if matches!(
            node.kind(),
            "block" | "parameter_list" | "argument_list" | "literal_value"
        ) {
            return None;
        }
        let parent = editor.parent(node)?;
        if parent.kind() == "expression_list"
            && editor
                .parent(parent)
                .is_some_and(|statement| statement.kind() == "return_statement")
        {
            let index = named_children(parent).position(|expression| expression == node)?;
            let slot = slots.get(index)?;
            return Some((index, slot.ty.or(slot.name)));
        }
        node = parent;
    }
    None
}

fn keyword<'tree>(node: Node<'tree>, spelling: &str) -> Option<Node<'tree>> {
    node.children(&mut node.walk())
        .find(|child| child.kind() == spelling && !child.is_missing())
}

fn walk_function_body<'tree>(
    root: Node<'tree>,
    cancelled: Option<&AtomicBool>,
    mut visit: impl FnMut(Node<'tree>) -> bool,
) -> Option<()> {
    walk(root, cancelled, |node| !is_function(node) && visit(node))
}

fn walk<'tree>(
    root: Node<'tree>,
    cancelled: Option<&AtomicBool>,
    mut visit: impl FnMut(Node<'tree>) -> bool,
) -> Option<()> {
    let mut cursor = root.walk();
    loop {
        check_cancelled(cancelled)?;
        let node = cursor.node();
        if node.is_named()
            && !node.is_extra()
            && !node.is_missing()
            && visit(node)
            && cursor.goto_first_child()
        {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return Some(());
            }
        }
    }
}
