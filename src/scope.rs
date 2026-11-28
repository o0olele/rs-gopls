use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use tree_sitter::Node;

#[derive(Clone, Copy)]
pub(crate) struct Binding<'tree> {
    pub(crate) name: Node<'tree>,
    pub(crate) scope: Option<Node<'tree>>,
    pub(crate) explicit_type: Option<Node<'tree>>,
    pub(crate) initializer: Option<Node<'tree>>,
    pub(crate) result_index: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code, reason = "consumed by completion in Wave 2")]
pub(crate) enum VisibleBindingKind {
    Var,
    Const,
    Type,
    Parameter,
    Receiver,
    Result,
    TypeParameter,
    ShortVar,
    Range,
    TypeSwitch,
    Receive,
}

#[allow(dead_code, reason = "consumed by completion in Wave 2")]
pub(crate) struct VisibleBinding<'tree> {
    pub(crate) name: String,
    pub(crate) binding: Binding<'tree>,
    pub(crate) kind: VisibleBindingKind,
    pub(crate) scope_depth: usize,
}

/// Enumerates lexical bindings from the innermost scope out. Package scope
/// and imports are provided by the declaration index and document snapshot.
#[allow(dead_code, reason = "consumed by completion in Wave 2")]
pub(crate) fn visible_bindings<'tree>(
    root: Node<'tree>,
    offset: usize,
    text: &str,
) -> Vec<VisibleBinding<'tree>> {
    visible_bindings_cancellable(root, offset, text, None).expect("uncancelled lexical bindings")
}

pub(crate) fn visible_bindings_cancellable<'tree>(
    root: Node<'tree>,
    offset: usize,
    text: &str,
    cancelled: Option<&AtomicBool>,
) -> Result<Vec<VisibleBinding<'tree>>> {
    let mut collector = VisibleBindings {
        offset,
        text,
        cancelled,
        depth: 0,
        seen: HashSet::new(),
        bindings: Vec::new(),
    };
    let mut anchor = root
        .named_descendant_for_byte_range(offset, offset)
        .unwrap_or(root);
    let previous = text
        .get(..offset)
        .and_then(|prefix| prefix.trim_end().len().checked_sub(1));
    if let Some((body, function)) =
        previous.and_then(|byte| recovered_function_body(root, byte, offset, text))
    {
        collector.recovered_body(body, function);
        return collector.finish();
    }
    if anchor == root
        && let Some(byte) = previous
        && let Some(tail) = root.named_descendant_for_byte_range(byte, byte + 1)
    {
        // Trailing whitespace can fall outside the range of a block with a
        // missing closing brace. Recover only a block that is still open.
        let mut node = Some(tail);
        while let Some(scope) = node {
            if collector.cancelled() {
                return collector.finish();
            }
            if scope.kind() == "block" && collector.in_braces(scope) {
                anchor = scope;
                break;
            }
            node = scope.parent();
        }
    }
    let mut current = Some(anchor);
    while let Some(scope) = current {
        if collector.cancelled() {
            break;
        }
        collector.scope(scope);
        current = scope.parent();
    }
    collector.finish()
}

struct VisibleBindings<'tree, 'text> {
    offset: usize,
    text: &'text str,
    cancelled: Option<&'text AtomicBool>,
    depth: usize,
    seen: HashSet<&'text str>,
    bindings: Vec<VisibleBinding<'tree>>,
}

impl<'tree> VisibleBindings<'tree, '_> {
    fn cancelled(&self) -> bool {
        is_cancelled(self.cancelled)
    }

    fn finish(self) -> Result<Vec<VisibleBinding<'tree>>> {
        anyhow::ensure!(!self.cancelled(), "lexical binding collection cancelled");
        Ok(self.bindings)
    }

    fn add(&mut self, binding: Binding<'tree>, kind: VisibleBindingKind) {
        let name = &self.text[binding.name.byte_range()];
        if name == "_" || name.is_empty() || !self.seen.insert(name) {
            return;
        }
        self.bindings.push(VisibleBinding {
            name: name.to_owned(),
            binding,
            kind,
            scope_depth: self.depth,
        });
    }

    fn scope(&mut self, scope: Node<'tree>) {
        use VisibleBindingKind::*;

        match scope.kind() {
            "block" if self.in_braces(scope) => {
                if let Some(function) = scope.parent().filter(|node| is_function(*node)) {
                    self.signature(function);
                    self.type_parameters(function);
                }
                self.statement_list(scope);
                self.depth += 1;
            }
            "if_statement" => {
                self.initializer(scope);
                self.depth += 1;
            }
            "for_statement" => {
                let mut cursor = scope.walk();
                for clause in scope.named_children(&mut cursor) {
                    if self.cancelled() {
                        return;
                    }
                    match clause.kind() {
                        "for_clause" => self.initializer(clause),
                        "range_clause"
                            if has_short_operator(clause)
                                && scope
                                    .child_by_field_name("body")
                                    .is_some_and(|body| self.in_braces(body)) =>
                        {
                            self.left_bindings(clause, Range);
                        }
                        _ => {}
                    }
                }
                self.depth += 1;
            }
            "expression_switch_statement" | "type_switch_statement" | "select_statement" => {
                self.switch_case(scope);
                if scope.kind() != "select_statement" {
                    self.initializer(scope);
                    self.depth += 1;
                }
            }
            "function_declaration" | "method_declaration" | "type_spec" | "type_alias" => {
                self.type_parameters(scope);
            }
            _ => {}
        }
    }

    fn recovered_body(&mut self, body: Node<'tree>, function: Node<'tree>) {
        // An unfinished selector at EOF can detach the body from its
        // declaration. The remaining brace tokens still separate scopes.
        let mut blocks = Vec::new();
        let offset = self.offset;
        for child in body
            .children(&mut body.walk())
            .take_while(|child| child.start_byte() < offset)
        {
            if self.cancelled() {
                return;
            }
            match child.kind() {
                "{" => blocks.push(Vec::new()),
                "}" => {
                    blocks.pop();
                }
                _ if child.is_named() => {
                    if let Some(block) = blocks.last_mut() {
                        block.push(child);
                    }
                }
                _ => {}
            }
        }
        for (index, block) in blocks.into_iter().enumerate().rev() {
            if index == 0 {
                self.signature(function);
                self.type_parameters(function);
            }
            for statement in block {
                if self.cancelled() {
                    return;
                }
                self.statement(statement);
            }
            self.depth += 1;
        }
    }

    fn in_braces(&self, node: Node<'_>) -> bool {
        let mut cursor = node.walk();
        let offset = self.offset;
        node.children(&mut cursor)
            .take_while(|_| !self.cancelled())
            .any(|child| child.kind() == "{" && child.end_byte() <= offset)
            && node
                .children(&mut cursor)
                .take_while(|_| !self.cancelled())
                .find(|child| child.kind() == "}" && !child.is_missing())
                .is_none_or(|close| offset <= close.start_byte())
    }

    fn initializer(&mut self, scope: Node<'tree>) {
        if let Some(initializer) = scope.child_by_field_name("initializer") {
            self.statement(initializer);
        }
    }

    fn statement_list(&mut self, scope: Node<'tree>) {
        if let Some(statements) = scope
            .named_children(&mut scope.walk())
            .find(|child| child.kind() == "statement_list")
        {
            self.statements(statements);
        }
    }

    fn statements(&mut self, list: Node<'tree>) {
        let offset = self.offset;
        for node in list
            .named_children(&mut list.walk())
            .take_while(|node| node.start_byte() < offset)
        {
            if self.cancelled() {
                return;
            }
            self.statement(node);
        }
    }

    fn statement(&mut self, node: Node<'tree>) {
        use VisibleBindingKind::*;

        match node.kind() {
            "var_declaration" | "var_spec_list" | "const_declaration" | "type_declaration"
            | "labeled_statement" | "statement_list" => self.statements(node),
            "var_spec" if node.end_byte() < self.offset => self.named_bindings(node, Var),
            "const_spec" if node.end_byte() < self.offset => self.named_bindings(node, Const),
            "short_var_declaration" if node.end_byte() < self.offset => {
                self.left_bindings(node, ShortVar);
            }
            "type_spec" | "type_alias" => {
                if let Some(name) = node.child_by_field_name("name")
                    && name.end_byte() <= self.offset
                {
                    self.add(explicit_binding(node, name, 0), Type);
                }
            }
            _ => {}
        }
    }

    fn named_bindings(&mut self, node: Node<'tree>, kind: VisibleBindingKind) {
        for (index, name) in node
            .children_by_field_name("name", &mut node.walk())
            .filter(Node::is_named)
            .enumerate()
        {
            if self.cancelled() {
                return;
            }
            self.add(explicit_binding(node, name, index), kind);
        }
    }

    fn left_bindings(&mut self, node: Node<'tree>, kind: VisibleBindingKind) {
        if let Some(left) = node.child_by_field_name("left") {
            for (index, name) in identifiers(left) {
                if self.cancelled() {
                    return;
                }
                let binding = if kind == VisibleBindingKind::ShortVar {
                    short_binding(node, name, index)
                } else {
                    assignment_binding(node, name, index)
                };
                self.add(binding, kind);
            }
        }
    }

    fn signature(&mut self, function: Node<'tree>) {
        use VisibleBindingKind::*;

        for (field, kind) in [
            ("receiver", Receiver),
            ("parameters", Parameter),
            ("result", Result),
        ] {
            let Some(list) = function
                .child_by_field_name(field)
                .filter(|list| list.kind() == "parameter_list")
            else {
                continue;
            };
            for parameter in list.named_children(&mut list.walk()) {
                if self.cancelled() {
                    return;
                }
                self.named_bindings(parameter, kind);
            }
        }
    }

    fn type_parameters(&mut self, declaration: Node<'tree>) {
        if let Some(list) = declaration
            .child_by_field_name("type_parameters")
            .filter(|list| list.start_byte() <= self.offset)
        {
            for parameter in list.named_children(&mut list.walk()) {
                if self.cancelled() {
                    return;
                }
                self.named_bindings(parameter, VisibleBindingKind::TypeParameter);
            }
        }
        if declaration.kind() == "method_declaration"
            && declaration
                .child_by_field_name("receiver")
                .is_some_and(|receiver| receiver.end_byte() <= self.offset)
            && let Some((_, arguments)) = receiver_type_parameters(declaration)
        {
            for name in receiver_parameter_names(arguments) {
                if self.cancelled() {
                    return;
                }
                self.add(
                    binding(name, None, None, 0),
                    VisibleBindingKind::TypeParameter,
                );
            }
        }
    }

    fn switch_case(&mut self, switch: Node<'tree>) {
        if !self.in_braces(switch) {
            return;
        }
        let offset = self.offset;
        let case = switch
            .named_children(&mut switch.walk())
            .take_while(|node| node.start_byte() <= offset && !self.cancelled())
            .filter(|node| {
                matches!(
                    node.kind(),
                    "expression_case" | "type_case" | "default_case" | "communication_case"
                )
            })
            .last();
        let Some(case) = case.filter(|case| {
            case.children(&mut case.walk())
                .any(|child| child.kind() == ":" && child.end_byte() <= offset)
        }) else {
            return;
        };
        if switch.kind() == "type_switch_statement"
            && let Some(alias) = switch.child_by_field_name("alias")
        {
            for (_, name) in identifiers(alias) {
                if self.cancelled() {
                    return;
                }
                self.add(
                    case_alias_binding(switch, case, name, self.text),
                    VisibleBindingKind::TypeSwitch,
                );
            }
        }
        if let Some(receive) = case
            .child_by_field_name("communication")
            .filter(|node| node.kind() == "receive_statement" && has_short_operator(*node))
        {
            self.left_bindings(receive, VisibleBindingKind::Receive);
        }
        self.statement_list(case);
        self.depth += 1;
    }
}

fn is_function(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "function_declaration" | "method_declaration" | "func_literal"
    )
}

fn recovered_function_body<'tree>(
    root: Node<'tree>,
    byte: usize,
    offset: usize,
    text: &str,
) -> Option<(Node<'tree>, Node<'tree>)> {
    let node = root.first_named_child_for_byte(byte)?;
    let function = node.prev_named_sibling()?;
    (node.is_error()
        && node.start_byte() <= byte
        && node
            .next_named_sibling()
            .is_none_or(|next| offset < next.start_byte())
        && is_function(function)
        && function.child_by_field_name("body").is_none()
        && node.child(0).is_some_and(|child| child.kind() == "{")
        && text
            .get(function.end_byte()..node.start_byte())?
            .trim()
            .is_empty())
    .then_some((node, function))
}

pub(crate) fn lexical_binding<'tree>(
    reference: Node<'tree>,
    name: &str,
    text: &str,
) -> Option<Binding<'tree>> {
    lexical_binding_cancellable(reference, name, text, None)
}

pub(crate) fn lexical_binding_cancellable<'tree>(
    reference: Node<'tree>,
    name: &str,
    text: &str,
    cancelled: Option<&AtomicBool>,
) -> Option<Binding<'tree>> {
    lexical_binding_with_cancellation(reference, name, text, &|node| node.parent(), cancelled)
}

pub(crate) fn lexical_binding_with<'tree>(
    reference: Node<'tree>,
    name: &str,
    text: &str,
    parent_of: &impl Fn(Node<'tree>) -> Option<Node<'tree>>,
) -> Option<Binding<'tree>> {
    lexical_binding_with_cancellation(reference, name, text, parent_of, None)
}

fn is_cancelled(cancelled: Option<&AtomicBool>) -> bool {
    cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed))
}

fn lexical_binding_with_cancellation<'tree>(
    reference: Node<'tree>,
    name: &str,
    text: &str,
    parent_of: &impl Fn(Node<'tree>) -> Option<Node<'tree>>,
    cancelled: Option<&AtomicBool>,
) -> Option<Binding<'tree>> {
    if name == "_" {
        return None;
    }

    let offset = reference.start_byte();
    let initializer = |scope: Node<'tree>| {
        scope
            .child_by_field_name("initializer")
            .filter(|node| node.end_byte() <= offset)
            .and_then(|node| declaration_cancellable(node, name, text, cancelled))
    };

    let mut child = reference;
    while let Some(scope) = parent_of(child) {
        if is_cancelled(cancelled) {
            return None;
        }
        let binding = match scope.kind() {
            "statement_list" | "var_spec_list" | "const_declaration" | "type_declaration" => {
                preceding_binding(child, scope, name, text, parent_of, cancelled)
            }
            "if_statement" | "expression_switch_statement" | "for_clause" => initializer(scope),
            "for_statement" if child.kind() == "block" => {
                let mut cursor = scope.walk();
                scope
                    .named_children(&mut cursor)
                    .find_map(|clause| match clause.kind() {
                        "for_clause" => initializer(clause),
                        "range_clause" if has_short_operator(clause) => {
                            left_binding(clause, name, text)
                        }
                        _ => None,
                    })
            }
            "type_switch_statement" => {
                type_case_binding(scope, child, offset, name, text).or_else(|| initializer(scope))
            }
            "communication_case" => case_entry_binding(child, name, text, parent_of),
            "function_declaration" | "method_declaration" | "func_literal"
                if child.kind() == "block" =>
            {
                function_binding(scope, name, text, cancelled)
            }
            "function_declaration" => type_parameter_binding(scope, offset, name, text, cancelled),
            "method_declaration" => receiver_type_parameter_binding(scope, name, text),
            "type_spec" | "type_alias" => {
                type_parameter_binding(scope, offset, name, text, cancelled).or_else(|| {
                    let statement = parent_of(parent_of(scope)?)?;
                    (statement.kind() != "source_file")
                        .then(|| declaration_cancellable(scope, name, text, cancelled))
                        .flatten()
                })
            }
            _ => None,
        };
        if let Some(mut binding) = binding {
            binding.scope.get_or_insert(scope);
            return Some(binding);
        }
        child = scope;
    }
    None
}

fn preceding_binding<'tree>(
    child: Node<'tree>,
    statement_list: Node<'tree>,
    name: &str,
    text: &str,
    parent_of: &impl Fn(Node<'tree>) -> Option<Node<'tree>>,
    cancelled: Option<&AtomicBool>,
) -> Option<Binding<'tree>> {
    let mut binding = None;
    for node in previous_named_siblings(child, statement_list) {
        if is_cancelled(cancelled) {
            return None;
        }
        if let Some(found) = declaration_cancellable(node, name, text, cancelled) {
            if parent_of(found.name)
                .and_then(parent_of)
                .is_none_or(|node| node.kind() != "short_var_declaration")
            {
                return Some(found);
            }
            // A short declaration can reuse an existing variable from the same block.
            binding = Some(found);
        }
    }

    let binding = binding?;
    if let Some(initial) = case_entry_binding(statement_list, name, text, parent_of) {
        return Some(initial);
    }
    let block = parent_of(statement_list)?;
    let function = parent_of(block)?;
    if statement_list.kind() == "statement_list"
        && block.kind() == "block"
        && matches!(
            function.kind(),
            "function_declaration" | "method_declaration" | "func_literal"
        )
    {
        return function_binding(function, name, text, cancelled).or(Some(binding));
    }
    Some(binding)
}

fn previous_named_siblings<'tree>(
    child: Node<'tree>,
    parent: Node<'tree>,
) -> impl Iterator<Item = Node<'tree>> {
    let mut cursor = parent.walk();
    let at_child = !parent.has_error()
        && cursor
            .goto_first_child_for_byte(child.start_byte())
            .is_some()
        && cursor.node() == child;
    let mut previous = Some(child);
    std::iter::from_fn(move || {
        if !at_child {
            // Preserve the Node lookup in recovery trees with zero-width siblings.
            previous = previous.and_then(|node| node.prev_named_sibling());
            return previous;
        }
        while cursor.goto_previous_sibling() {
            let node = cursor.node();
            if node.is_named() {
                return Some(node);
            }
        }
        None
    })
}

// The implicit block of a select or type-switch case already contains its
// receive variables or guard variable before the first statement is reached.
fn case_entry_binding<'tree>(
    statements: Node<'tree>,
    name: &str,
    text: &str,
    parent_of: &impl Fn(Node<'tree>) -> Option<Node<'tree>>,
) -> Option<Binding<'tree>> {
    if statements.kind() != "statement_list" {
        return None;
    }
    let case = parent_of(statements)?;
    match case.kind() {
        "communication_case" => case
            .child_by_field_name("communication")
            .filter(|n| n.kind() == "receive_statement" && has_short_operator(*n))
            .and_then(|receive| left_binding(receive, name, text)),
        "type_case" | "default_case" => {
            let switch = parent_of(case).filter(|s| s.kind() == "type_switch_statement")?;
            type_case_binding(switch, case, statements.start_byte(), name, text)
        }
        _ => None,
    }
}

fn function_binding<'tree>(
    function: Node<'tree>,
    name: &str,
    text: &str,
    cancelled: Option<&AtomicBool>,
) -> Option<Binding<'tree>> {
    ["parameters", "receiver", "result", "type_parameters"]
        .into_iter()
        .filter_map(|field| function.child_by_field_name(field))
        .find_map(|node| declaration_cancellable(node, name, text, cancelled))
        .or_else(|| receiver_type_parameter_binding(function, name, text))
}

pub(crate) fn receiver_type_parameters(method: Node<'_>) -> Option<(Node<'_>, Node<'_>)> {
    let receiver = method.child_by_field_name("receiver")?;
    let mut ty = receiver.named_child(0)?.child_by_field_name("type")?;
    if ty.kind() == "pointer_type" {
        ty = ty.named_child(0)?;
    }
    (ty.kind() == "generic_type").then(|| {
        Some((
            ty.child_by_field_name("type")?,
            ty.child_by_field_name("type_arguments")?,
        ))
    })?
}

fn receiver_type_parameter_binding<'tree>(
    method: Node<'tree>,
    name: &str,
    text: &str,
) -> Option<Binding<'tree>> {
    let (_, arguments) = receiver_type_parameters(method)?;
    let binding_name = receiver_parameter_names(arguments)
        .find(|node| text.get(node.byte_range()) == Some(name))?;
    Some(binding(binding_name, None, None, 0))
}

fn receiver_parameter_names(arguments: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    std::iter::successors(arguments.named_child(0), Node::next_named_sibling)
        .filter(|node| node.kind() == "type_elem")
        .filter_map(|node| node.named_child(0))
        .filter(|node| node.kind() == "type_identifier")
}

fn type_parameter_binding<'tree>(
    declaration_node: Node<'tree>,
    offset: usize,
    name: &str,
    text: &str,
    cancelled: Option<&AtomicBool>,
) -> Option<Binding<'tree>> {
    declaration_node
        .child_by_field_name("type_parameters")
        .filter(|list| list.start_byte() <= offset)
        .and_then(|list| declaration_cancellable(list, name, text, cancelled))
}

fn type_case_binding<'tree>(
    switch: Node<'tree>,
    case: Node<'tree>,
    offset: usize,
    name: &str,
    text: &str,
) -> Option<Binding<'tree>> {
    if !matches!(case.kind(), "type_case" | "default_case") {
        return None;
    }
    let mut cursor = case.walk();
    if !case
        .named_children(&mut cursor)
        .any(|node| node.kind() == "statement_list" && node.start_byte() <= offset)
    {
        return None;
    }

    let alias = switch.child_by_field_name("alias")?;
    let (_, name) = identifier_in_list(alias, name, text)?;
    Some(case_alias_binding(switch, case, name, text))
}

fn case_alias_binding<'tree>(
    switch: Node<'tree>,
    case: Node<'tree>,
    name: Node<'tree>,
    text: &str,
) -> Binding<'tree> {
    let mut cursor = case.walk();
    let mut types = case
        .children_by_field_name("type", &mut cursor)
        .filter(Node::is_named);
    let explicit_type = types
        .next()
        .filter(|node| text.get(node.byte_range()) != Some("nil") && types.next().is_none());
    let initializer = if explicit_type.is_none() {
        switch.child_by_field_name("value")
    } else {
        None
    };
    Binding {
        scope: Some(case),
        ..binding(name, explicit_type, initializer, 0)
    }
}

pub(crate) fn reference_binding<'tree>(node: Node<'tree>, text: &str) -> Option<Node<'tree>> {
    reference_binding_with(node, text, &|node| node.parent())
}

pub(crate) fn reference_binding_with<'tree>(
    node: Node<'tree>,
    text: &str,
    parent_of: &impl Fn(Node<'tree>) -> Option<Node<'tree>>,
) -> Option<Node<'tree>> {
    let spelling = text.get(node.byte_range())?;
    if spelling == "_" {
        return None;
    }
    let previous =
        lexical_binding_with(node, spelling, text, parent_of).map(|binding| binding.name);
    if declaration_binding_with(node, text, parent_of).is_none() {
        return previous;
    }
    let parent = parent_of(node)?;
    if parent.kind() == "expression_list"
        && let Some(statement) =
            parent_of(parent).filter(|node| node.kind() == "short_var_declaration")
        && let Some(block) = parent_of(statement).filter(|node| node.kind() == "statement_list")
        && let Some(previous) = previous
        && same_short_declaration_scope(previous, block, spelling, text, parent_of)
    {
        return Some(previous);
    }
    Some(node)
}

fn same_short_declaration_scope<'tree>(
    previous: Node<'tree>,
    block: Node<'tree>,
    spelling: &str,
    text: &str,
    parent_of: &impl Fn(Node<'tree>) -> Option<Node<'tree>>,
) -> bool {
    if case_entry_binding(block, spelling, text, parent_of)
        .is_some_and(|binding| binding.name == previous)
    {
        return true;
    }
    let Some(body) = parent_of(block) else {
        return false;
    };
    let mut cursor = parent_of(previous);
    while let Some(node) = cursor {
        match node.kind() {
            "statement_list" => return node == block,
            "function_declaration" | "method_declaration" | "func_literal" => {
                return node.child_by_field_name("body") == Some(body);
            }
            "source_file" => return false,
            _ => cursor = parent_of(node),
        }
    }
    false
}

pub(crate) fn declaration_binding<'tree>(name: Node<'tree>, text: &str) -> Option<Binding<'tree>> {
    declaration_binding_with(name, text, &|node| node.parent())
}

pub(crate) fn declaration_binding_with<'tree>(
    name: Node<'tree>,
    text: &str,
    parent_of: &impl Fn(Node<'tree>) -> Option<Node<'tree>>,
) -> Option<Binding<'tree>> {
    let parent = parent_of(name)?;
    let owner = if parent.kind() == "expression_list" {
        parent_of(parent)?
    } else {
        parent
    };
    let spelling = text.get(name.byte_range())?;
    let binding = match owner.kind() {
        "range_clause" | "receive_statement" if has_short_operator(owner) => {
            left_binding(owner, spelling, text)
        }
        "type_switch_statement" if parent.kind() == "expression_list" => {
            let (_, name) =
                identifier_in_list(owner.child_by_field_name("alias")?, spelling, text)?;
            Some(binding(name, None, owner.child_by_field_name("value"), 0))
        }
        _ => declaration(owner, spelling, text),
    }?;
    (binding.name == name).then_some(binding)
}

fn declaration<'tree>(node: Node<'tree>, name: &str, text: &str) -> Option<Binding<'tree>> {
    declaration_cancellable(node, name, text, None)
}

fn declaration_cancellable<'tree>(
    node: Node<'tree>,
    name: &str,
    text: &str,
    cancelled: Option<&AtomicBool>,
) -> Option<Binding<'tree>> {
    let mut cursor = node.walk();
    match node.kind() {
        "var_declaration"
        | "const_declaration"
        | "type_declaration"
        | "var_spec_list"
        | "parameter_list"
        | "type_parameter_list"
        | "labeled_statement" => node
            .named_children(&mut cursor)
            .take_while(|_| !is_cancelled(cancelled))
            .find_map(|child| declaration_cancellable(child, name, text, cancelled)),
        "short_var_declaration" => {
            let names = node.child_by_field_name("left")?;
            let (index, name) = identifiers(names)
                .take_while(|_| !is_cancelled(cancelled))
                .find(|(_, node)| text.get(node.byte_range()) == Some(name))?;
            Some(short_binding(node, name, index))
        }
        "type_spec"
        | "type_alias"
        | "var_spec"
        | "const_spec"
        | "parameter_declaration"
        | "variadic_parameter_declaration"
        | "type_parameter_declaration"
        | "field_declaration" => {
            let (index, name) = node
                .children_by_field_name("name", &mut cursor)
                .take_while(|_| !is_cancelled(cancelled))
                .filter(Node::is_named)
                .enumerate()
                .find(|(_, candidate)| text.get(candidate.byte_range()) == Some(name))?;
            Some(explicit_binding(node, name, index))
        }
        _ => None,
    }
}

fn explicit_binding<'tree>(node: Node<'tree>, name: Node<'tree>, index: usize) -> Binding<'tree> {
    let (initializer, result_index) = initializer_at(node.child_by_field_name("value"), index);
    binding(
        name,
        node.child_by_field_name("type"),
        initializer,
        result_index,
    )
}

fn short_binding<'tree>(node: Node<'tree>, name: Node<'tree>, index: usize) -> Binding<'tree> {
    let (initializer, result_index) = initializer_at(node.child_by_field_name("right"), index);
    binding(name, None, initializer, result_index)
}

pub(crate) fn initializer_at(list: Option<Node<'_>>, index: usize) -> (Option<Node<'_>>, usize) {
    let Some(list) = list else {
        return (None, 0);
    };
    let mut cursor = list.walk();
    let mut values = list
        .named_children(&mut cursor)
        .filter(|node| !node.is_extra());
    let first = values.next();
    let Some(second) = values.next() else {
        return (first, index);
    };
    (
        match index {
            0 => first,
            1 => Some(second),
            _ => values.nth(index - 2),
        },
        0,
    )
}

fn left_binding<'tree>(node: Node<'tree>, name: &str, text: &str) -> Option<Binding<'tree>> {
    let (index, name) = identifier_in_list(node.child_by_field_name("left")?, name, text)?;
    Some(assignment_binding(node, name, index))
}

fn assignment_binding<'tree>(node: Node<'tree>, name: Node<'tree>, index: usize) -> Binding<'tree> {
    let initializer = if node.kind() == "range_clause" {
        Some(node)
    } else {
        node.child_by_field_name("right")
    };
    binding(name, None, initializer, index)
}

fn identifier_in_list<'tree>(
    list: Node<'tree>,
    name: &str,
    text: &str,
) -> Option<(usize, Node<'tree>)> {
    identifiers(list).find(|(_, node)| text.get(node.byte_range()) == Some(name))
}

fn identifiers(list: Node<'_>) -> impl Iterator<Item = (usize, Node<'_>)> {
    std::iter::successors(list.named_child(0), Node::next_named_sibling)
        .filter(|node| !node.is_extra())
        .enumerate()
        .filter(|(_, node)| {
            matches!(
                node.kind(),
                "identifier" | "nil" | "true" | "false" | "iota"
            )
        })
}

fn has_short_operator(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|child| child.kind() == ":=")
}

fn binding<'tree>(
    name: Node<'tree>,
    explicit_type: Option<Node<'tree>>,
    initializer: Option<Node<'tree>>,
    result_index: usize,
) -> Binding<'tree> {
    Binding {
        name,
        scope: None,
        explicit_type,
        initializer,
        result_index,
    }
}

#[cfg(test)]
mod tests {
    use super::VisibleBindingKind::*;
    use super::*;

    #[test]
    fn case_scope_short_declarations_reuse_the_guard_or_receive_variable() {
        for (text, first_case, second_case) in [
            (
                "package p\nfunc f(a any) { switch value := a.(type) { case int: ptr := &value; value, n := value+1, 2; _, _, _ = ptr, value, n; case string: _ = value } }",
                "ptr :=",
                "case string",
            ),
            (
                "package p\nfunc f(c chan int) { select { case value := <-c: ptr := &value; value, n := value+1, 2; _, _, _ = ptr, value, n; case <-c: value := 8; _ = value } }",
                "ptr :=",
                "case <-c",
            ),
        ] {
            let mut parser = tree_sitter::Parser::new();
            parser
                .set_language(&tree_sitter_go::LANGUAGE.into())
                .unwrap();
            let tree = parser.parse(text, None).unwrap();
            assert!(!tree.root_node().has_error());
            let guard = text.find("value :=").unwrap();
            let start = text.find(first_case).unwrap();
            let end = text.find(second_case).unwrap();
            let uses: Vec<_> = text[start..end]
                .match_indices("value")
                .map(|(index, _)| start + index)
                .collect();
            assert_eq!(uses.len(), 4);
            for at in uses {
                let node = tree
                    .root_node()
                    .named_descendant_for_byte_range(at, at + 1)
                    .unwrap();
                assert_eq!(
                    reference_binding(node, text).unwrap().start_byte(),
                    guard,
                    "use at byte {at} in {text}"
                );
            }
            let last = text.rfind("value").unwrap();
            let node = tree
                .root_node()
                .named_descendant_for_byte_range(last, last + 1)
                .unwrap();
            let expected = if second_case == "case string" {
                guard
            } else {
                text.rfind("value :=").unwrap()
            };
            assert_eq!(
                reference_binding(node, text).unwrap().start_byte(),
                expected
            );
        }
    }

    fn at_cursor(text: &str, check: impl FnOnce(&str, &[VisibleBinding<'_>])) {
        let offset = text.find('|').unwrap();
        let text = text.replacen('|', "", 1);
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(&text, None).unwrap();
        let bindings = visible_bindings(tree.root_node(), offset, &text);
        check(&text, &bindings);
    }

    fn get<'a, 'tree>(
        bindings: &'a [VisibleBinding<'tree>],
        name: &str,
    ) -> &'a VisibleBinding<'tree> {
        bindings
            .iter()
            .find(|binding| binding.name == name)
            .unwrap_or_else(|| panic!("missing {name}"))
    }

    fn absent(bindings: &[VisibleBinding<'_>], names: &[&str]) {
        for name in names {
            assert!(
                bindings.iter().all(|binding| binding.name != *name),
                "unexpected {name}"
            );
        }
    }

    #[test]
    fn groups_parameters_with_body_declarations_and_prefers_inner_bindings() {
        at_cursor(
            concat!(
                "package sample\nvar Outside int\n",
                "func f[T any](arg T) (result int) {\n",
                "\targ, second := pair()\n",
                "\tvar item T\n\tconst count = 2\n\ttype Local struct{}\n",
                "\t{\n\t\titem := arg\n\t\tvar _ = count\n\t\t|\n\t\tlater := 1\n\t}\n}\n",
            ),
            |text, bindings| {
                let item = get(bindings, "item");
                assert_eq!((item.kind, item.scope_depth), (ShortVar, 0));
                let initializer = item.binding.initializer.unwrap();
                assert_eq!(&text[initializer.byte_range()], "arg");
                for (name, kind) in [
                    ("arg", Parameter),
                    ("result", Result),
                    ("T", TypeParameter),
                    ("second", ShortVar),
                    ("count", Const),
                    ("Local", Type),
                ] {
                    let binding = get(bindings, name);
                    assert_eq!((binding.kind, binding.scope_depth), (kind, 1), "{name}");
                }
                assert_eq!(get(bindings, "second").binding.result_index, 1);
                let arg = get(bindings, "arg").binding.explicit_type.unwrap();
                assert_eq!(&text[arg.byte_range()], "T");
                absent(bindings, &["Outside", "later", "_"]);
                assert_eq!(bindings.len(), 7);
            },
        );
    }

    #[test]
    fn scopes_grouped_declarations_per_spec_and_excludes_own_initializer() {
        at_cursor(
            "package p\nfunc f() {\nvar (earlier = 1\nown = earlier|\nlater = 3\n)\n}\n",
            |_, bindings| {
                assert_eq!(get(bindings, "earlier").kind, Var);
                absent(bindings, &["own", "later"]);
            },
        );
        at_cursor(
            "package p\nfunc f() {\nconst (first = 1\nnext = first|\n)\n}\n",
            |_, bindings| {
                assert_eq!(get(bindings, "first").kind, Const);
                absent(bindings, &["next"]);
            },
        );
        at_cursor(
            "package p\nfunc f(base int) {\nif fresh := base|; fresh > 0 {}\n}\n",
            |_, bindings| {
                assert_eq!(get(bindings, "base").kind, Parameter);
                absent(bindings, &["fresh"]);
            },
        );
        at_cursor(
            "package p\nfunc f(x int) { { x := x| } }\n",
            |_, bindings| assert_eq!(get(bindings, "x").kind, Parameter),
        );
        at_cursor(
            "package p\nfunc f() {\ntype Node struct{ Child *Node| }\n}\n",
            |_, bindings| assert_eq!(get(bindings, "Node").kind, Type),
        );
    }

    #[test]
    fn handles_receivers_and_type_parameters_in_signatures() {
        for (text, in_body) in [
            ("package p\nfunc (r *Box[T]) F(arg T|) (out T) {}\n", false),
            ("package p\nfunc (r *Box[T]) F(arg T) (out T) { | }\n", true),
        ] {
            at_cursor(text, |_, bindings| {
                assert_eq!(get(bindings, "T").kind, TypeParameter);
                if in_body {
                    assert_eq!(get(bindings, "r").kind, Receiver);
                    assert_eq!(get(bindings, "arg").kind, Parameter);
                    assert_eq!(get(bindings, "out").kind, Result);
                } else {
                    absent(bindings, &["r", "arg", "out"]);
                }
            });
        }
    }

    #[test]
    fn scopes_range_and_for_init_bindings_to_the_loop() {
        for (text, in_body) in [
            ("for i, v := range rows| { _ = v }", false),
            ("for i, v := range rows { | }", true),
        ] {
            at_cursor(
                &format!("package p\nfunc f(rows []int) {{\n{text}\n}}\n"),
                |_, bindings| {
                    assert_eq!(get(bindings, "rows").kind, Parameter);
                    if in_body {
                        let value = get(bindings, "v");
                        assert_eq!(value.kind, Range);
                        assert_eq!(value.binding.initializer.unwrap().kind(), "range_clause");
                        assert_eq!(value.binding.result_index, 1);
                        assert_eq!(get(bindings, "i").scope_depth, 1);
                    } else {
                        absent(bindings, &["i", "v"]);
                    }
                },
            );
        }
        at_cursor(
            "package p\nfunc f() { for i := 0; i < 1; i++ { | } }\n",
            |_, bindings| {
                assert_eq!(
                    (get(bindings, "i").kind, get(bindings, "i").scope_depth),
                    (ShortVar, 1)
                );
            },
        );
    }

    #[test]
    fn scopes_type_switch_and_select_names_to_the_case_body() {
        let prefix = "package p\nfunc f(value any) {\nswitch init := 1; narrowed := value";
        at_cursor(
            &format!("{prefix}|.(type) {{ case int: }}\n}}\n"),
            |_, bindings| {
                assert_eq!(get(bindings, "init").kind, ShortVar);
                absent(bindings, &["narrowed"]);
            },
        );
        at_cursor(
            &format!("{prefix}.(type) {{ case int: | \ncase string: var later bool\n}}\n}}\n"),
            |text, bindings| {
                let narrowed = get(bindings, "narrowed");
                assert_eq!((narrowed.kind, narrowed.scope_depth), (TypeSwitch, 0));
                assert_eq!(
                    &text[narrowed.binding.explicit_type.unwrap().byte_range()],
                    "int"
                );
                assert_eq!(get(bindings, "init").scope_depth, 1);
                absent(bindings, &["later"]);
            },
        );
        at_cursor(
            &format!("{prefix}.(type) {{ case int, string: | }}\n}}\n"),
            |text, bindings| {
                let narrowed = get(bindings, "narrowed").binding;
                assert!(narrowed.explicit_type.is_none());
                assert_eq!(&text[narrowed.initializer.unwrap().byte_range()], "value");
            },
        );
        at_cursor(
            "package p\nfunc f(ch chan int) { select { case item, ok := <-ch: | } }\n",
            |_, bindings| {
                assert_eq!(get(bindings, "item").kind, Receive);
                assert_eq!(get(bindings, "ok").binding.result_index, 1);
            },
        );
    }

    #[test]
    fn finds_bindings_in_trailing_whitespace_and_an_unfinished_selector() {
        for suffix in ["\n\t|\n}\n", "\n\t|", "\n\tearly.|", "\n\tearly.|  "] {
            at_cursor(
                &format!("package p\nfunc f(arg int) {{\n\tearly := arg{suffix}"),
                |_, bindings| {
                    assert_eq!(get(bindings, "arg").kind, Parameter);
                    assert_eq!(get(bindings, "early").kind, ShortVar);
                },
            );
        }
        at_cursor(
            "package p\nfunc f(a int) { var x int\n}\n|\n",
            |_, bindings| {
                assert!(bindings.is_empty());
            },
        );
        at_cursor(
            "package p\nfunc f(a int) { a := 1\n{\na := 2\na.|",
            |text, bindings| {
                let a = get(bindings, "a");
                assert_eq!((a.kind, a.scope_depth), (ShortVar, 0));
                assert_eq!(&text[a.binding.initializer.unwrap().byte_range()], "2");
            },
        );
    }
}
