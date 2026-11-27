use std::cell::RefCell;
use std::collections::VecDeque;

use lsp_types::{InlayHint, Position, Range, SemanticTokens};
use tree_sitter::Node;

use crate::scope::{self, Binding};

use super::presentation::builtin_function;
use super::resolver::is_predeclared_type;
use super::snapshot::DocumentSnapshot;
use super::source::{is_declaration_name_in, node_at_range};

mod document_highlight;
mod folding_range;
mod highlight_request;
mod inlay_hint;
mod semantic_tokens;

#[cfg(test)]
mod tests;

pub(super) use folding_range::folding_ranges;
pub(super) use highlight_request::HighlightCache;
pub(crate) use highlight_request::{HighlightInput, HighlightOutput, HighlightPreparation};
pub(crate) use semantic_tokens::legend;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct EditorFoldingOptions {
    pub line_folding_only: bool,
}

pub(super) fn semantic_tokens_full(document: &DocumentSnapshot<'_>) -> SemanticTokens {
    semantic_tokens::tokens(&Editor::new(document))
}

pub(super) fn inlay_hints(document: &DocumentSnapshot<'_>, range: Range) -> Vec<InlayHint> {
    inlay_hint::hints(&Editor::new(document), range)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SymbolId<'tree> {
    Declaration(usize),
    Builtin(&'tree str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SymbolKind {
    Namespace,
    Type,
    Struct,
    Interface,
    TypeParameter,
    Parameter,
    Variable,
    Property,
    Function,
    Method,
    Label,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Occurrence<'tree> {
    id: SymbolId<'tree>,
    kind: SymbolKind,
    declaration: bool,
    readonly: bool,
    default_library: bool,
}

struct Editor<'tree> {
    document: &'tree DocumentSnapshot<'tree>,
    anchors: RefCell<VecDeque<Node<'tree>>>,
}

impl<'tree> Editor<'tree> {
    fn new(document: &'tree DocumentSnapshot<'tree>) -> Self {
        Self {
            document,
            anchors: RefCell::default(),
        }
    }

    fn root(&self) -> Node<'tree> {
        self.document.root_node()
    }

    fn remember_anchor(&self, anchor: Node<'tree>) {
        let mut anchors = self.anchors.borrow_mut();
        const RECENT_DECLARATIONS: usize = 8;
        if anchors.len() == RECENT_DECLARATIONS {
            anchors.pop_front();
        }
        anchors.push_back(anchor);
    }

    fn anchor(&self, start: usize, end: usize) -> Option<Node<'tree>> {
        self.anchors.borrow().iter().rev().copied().find(|anchor| {
            anchor.start_byte() <= start && start < anchor.end_byte() && end <= anchor.end_byte()
        })
    }

    fn node_for_bytes(&self, start: usize, end: usize) -> Option<Node<'tree>> {
        if let Some(anchor) = self.anchor(start, end) {
            return anchor.descendant_for_byte_range(start, end);
        }
        let root = self.root();
        if let Some(anchor) = root.first_child_for_byte(start)
            && anchor.start_byte() <= start
            && end <= anchor.end_byte()
        {
            self.remember_anchor(anchor);
            return anchor.descendant_for_byte_range(start, end);
        }
        root.descendant_for_byte_range(start, end)
    }

    fn parent(&self, node: Node<'tree>) -> Option<Node<'tree>> {
        let root = self.root();
        if node == root {
            return None;
        }
        let anchor = self.anchor(node.start_byte(), node.end_byte()).or_else(|| {
            let anchor = root.child_with_descendant(node)?;
            self.remember_anchor(anchor);
            Some(anchor)
        });
        if let Some(mut ancestor) = anchor {
            if ancestor == node {
                return Some(root);
            }
            // Tree-sitter's Node::parent starts at the file root on each call.
            // Descend only through the enclosing declaration after the first lookup.
            while let Some(child) = ancestor.child_with_descendant(node) {
                if child == node {
                    return Some(ancestor);
                }
                if child == ancestor {
                    break;
                }
                ancestor = child;
            }
        }
        node.parent()
    }

    fn ancestors(&self, node: Node<'tree>) -> impl Iterator<Item = Node<'tree>> {
        std::iter::successors(Some(node), |node| self.parent(*node))
    }

    fn function_owner(&self, node: Node<'tree>) -> Option<Node<'tree>> {
        self.ancestors(node).find(|node| is_function(*node))
    }

    fn declaration_kind(&self, node: Node<'tree>) -> Option<SymbolKind> {
        use SymbolKind::*;

        if self.embedded_field(node).is_some() {
            return Some(Property);
        }
        let parent = self.parent(node)?;
        Some(match parent.kind() {
            "function_declaration" => Function,
            "method_declaration" | "method_elem" => Method,
            "type_alias" => Type,
            "type_spec" => match parent
                .child_by_field_name("type")
                .map(unwrap_parens)
                .map(|ty| ty.kind())
            {
                Some("struct_type") => Struct,
                Some("interface_type") => Interface,
                _ => Type,
            },
            "parameter_declaration" | "variadic_parameter_declaration" => Parameter,
            "type_parameter_declaration" | "type_elem" => TypeParameter,
            "var_spec" | "const_spec" | "expression_list" => Variable,
            "field_declaration" => Property,
            _ => return None,
        })
    }

    fn embedded_field(&self, name: Node<'tree>) -> Option<Node<'tree>> {
        let field = self
            .ancestors(name)
            .find(|node| node.kind() == "field_declaration")?;
        (field.child_by_field_name("name").is_none()
            && field
                .child_by_field_name("type")
                .and_then(terminal_type_name)
                == Some(name))
        .then_some(field)
    }

    fn in_file_scope(&self, node: Node<'tree>) -> bool {
        self.ancestors(node)
            .skip(1)
            .skip_while(|parent| {
                matches!(
                    parent.kind(),
                    "var_spec"
                        | "var_spec_list"
                        | "var_declaration"
                        | "const_spec"
                        | "const_declaration"
                        | "type_spec"
                        | "type_alias"
                        | "type_declaration"
                        | "function_declaration"
                )
            })
            .next()
            .is_some_and(|scope| scope.kind() == "source_file")
    }

    fn literal_key_owner(&self, node: Node<'tree>) -> Option<Node<'tree>> {
        let element = self
            .parent(node)
            .filter(|parent| parent.kind() == "literal_element")?;
        let keyed = self
            .parent(element)
            .filter(|parent| parent.kind() == "keyed_element")?;
        (keyed.child_by_field_name("key") == Some(element)).then(|| self.parent(keyed))?
    }

    fn source(&self) -> &'tree str {
        self.document.text()
    }

    fn text(&self, node: Node<'_>) -> &'tree str {
        &self.source()[node.byte_range()]
    }

    fn offset(&self, position: Position) -> Option<usize> {
        self.document.offset(position)
    }

    fn position(&self, offset: usize) -> Position {
        self.document
            .indexed()
            .lines
            .position(self.source(), offset)
    }

    fn range(&self, node: Node<'_>) -> Range {
        self.document.byte_range(node.byte_range())
    }

    fn file_name(&self, name: &str) -> Option<Node<'tree>> {
        let range = self.document.indexed().editor_names.declaration(name)?;
        self.node_for_bytes(range.start, range.end)
    }

    fn ambiguous_file_name(&self, name: &str) -> bool {
        self.document.indexed().editor_names.ambiguous(name)
    }

    fn import_binding(&self, name: &str) -> Option<Node<'tree>> {
        let range = self
            .document
            .indexed()
            .editor_names
            .imports
            .get(name)?
            .as_ref()?;
        self.node_for_bytes(range.start, range.end)
    }

    fn lexical_name(&self, node: Node<'tree>) -> Option<Node<'tree>> {
        scope::reference_binding_with(node, self.source(), &|node| self.parent(node))
            .map(|target| {
                if target == node {
                    self.labelled_short_reuse(node).unwrap_or(target)
                } else {
                    target
                }
            })
            .filter(|target| {
                !self.ambiguous_file_name(self.text(*target)) || !self.in_file_scope(*target)
            })
            .or_else(|| {
                // A detached, unfinished function body can still expose its
                // preceding signature and locals through scope's recovery path.
                if !self
                    .ancestors(node)
                    .any(|parent| parent.is_error() && self.parent(parent) == Some(self.root()))
                {
                    return None;
                }
                let name = self.text(node);
                scope::visible_bindings(self.root(), node.start_byte(), self.source())
                    .into_iter()
                    .find(|visible| visible.name == name)
                    .map(|visible| visible.binding.name)
            })
    }

    fn labelled_short_reuse(&self, node: Node<'tree>) -> Option<Node<'tree>> {
        let statement = self.parent(self.parent(node)?)?;
        if statement.kind() != "short_var_declaration"
            || self.parent(statement)?.kind() != "labeled_statement"
        {
            return None;
        }
        let list = self
            .ancestors(statement)
            .skip(1)
            .find(|parent| parent.kind() != "labeled_statement")?;
        let previous =
            scope::lexical_binding_with(node, self.text(node), self.source(), &|node| {
                self.parent(node)
            })?
            .name;
        let scope = self.ancestors(previous).skip(1).find(|parent| {
            parent.kind() == "statement_list"
                || is_function(*parent)
                || parent.kind() == "source_file"
        })?;
        (scope == list || scope.child_by_field_name("body") == self.parent(list))
            .then_some(previous)
    }

    fn declaration(&self, node: Node<'tree>) -> Option<Node<'tree>> {
        let SymbolId::Declaration(offset) = self.symbol(node)?.id else {
            return None;
        };
        self.node_for_bytes(offset, offset + 1)
    }

    fn symbol(&self, node: Node<'tree>) -> Option<Occurrence<'tree>> {
        use SymbolKind::*;

        if node.is_missing() || node.byte_range().is_empty() || self.text(node) == "_" {
            return None;
        }
        if !matches!(
            node.kind(),
            "identifier"
                | "type_identifier"
                | "field_identifier"
                | "package_identifier"
                | "label_name"
                | "true"
                | "false"
                | "nil"
                | "iota"
                | "dot"
        ) {
            return None;
        }
        let parent = self.parent(node)?;
        let occurrence = |target: Node<'tree>, kind, readonly| Occurrence {
            id: SymbolId::Declaration(target.start_byte()),
            kind,
            declaration: target == node,
            readonly,
            default_library: false,
        };
        if self.embedded_field(node).is_some() {
            return Some(occurrence(node, Property, false));
        }
        match parent.kind() {
            "package_clause" | "import_spec" => return Some(occurrence(node, Namespace, false)),
            "labeled_statement" if node.kind() == "label_name" => {
                return Some(occurrence(node, Label, false));
            }
            "break_statement" | "continue_statement" | "goto_statement"
                if node.kind() == "label_name" =>
            {
                return Some(occurrence(self.label_definition(node)?, Label, false));
            }
            "selector_expression" if parent.child_by_field_name("field") == Some(node) => {
                let member = self.selector_member(parent, 0)?;
                return Some(occurrence(member, self.declaration_kind(member)?, false));
            }
            "qualified_type" if parent.child_by_field_name("name") == Some(node) => {
                // The name belongs to another package.
                return None;
            }
            _ => {}
        }
        if let Some(literal) = self.literal_key_owner(node) {
            let shape = self.literal_shape(literal, 0)?;
            if let LocalType::Syntax(ty) = shape
                && ty.kind() == "struct_type"
            {
                let field = unique(
                    direct_fields(ty)
                        .into_iter()
                        .filter(|field| self.text(*field) == self.text(node)),
                )?;
                return Some(occurrence(field, Property, false));
            }
        }
        if is_declaration_name_in(node, parent) || self.is_receiver_type_parameter(node) {
            return Some(occurrence(
                node,
                self.declaration_kind(node)?,
                parent.kind() == "const_spec",
            ));
        }
        if node.kind() == "field_identifier" {
            return None;
        }
        let name = self.text(node);
        if let Some(target) = self.lexical_name(node).or_else(|| self.file_name(name)) {
            return Some(occurrence(
                target,
                self.declaration_kind(target)?,
                self.parent(target)
                    .is_some_and(|parent| parent.kind() == "const_spec"),
            ));
        }
        if self.ambiguous_file_name(name) {
            return None;
        }
        if self
            .document
            .indexed()
            .editor_names
            .imports
            .contains_key(name)
        {
            return Some(occurrence(self.import_binding(name)?, Namespace, false));
        }
        let (kind, readonly) = if is_predeclared_type(name) {
            (Type, false)
        } else if builtin_function(name).is_some() {
            (Function, false)
        } else {
            match name {
                "nil" => (Variable, false),
                "true" | "false" => (Variable, true),
                "iota"
                    if self
                        .ancestors(node)
                        .any(|parent| parent.kind() == "const_spec") =>
                {
                    (Variable, true)
                }
                _ => return None,
            }
        };
        // Built-in identities are shared only within a document request.
        Some(Occurrence {
            id: SymbolId::Builtin(name),
            kind,
            declaration: false,
            readonly,
            default_library: true,
        })
    }

    fn is_receiver_type_parameter(&self, node: Node<'tree>) -> bool {
        if node.kind() != "type_identifier" {
            return false;
        }
        let Some(element) = self
            .parent(node)
            .filter(|parent| parent.kind() == "type_elem")
        else {
            return false;
        };
        let Some((_, arguments)) = self
            .function_owner(node)
            .and_then(scope::receiver_type_parameters)
        else {
            return false;
        };
        self.parent(element) == Some(arguments)
    }

    fn label_definition(&self, reference: Node<'tree>) -> Option<Node<'tree>> {
        let name = self.text(reference);
        if name == "_" {
            return None;
        }
        let statement = self.parent(reference)?;
        for scope in self
            .ancestors(statement)
            .skip(1)
            .take_while(|scope| !is_function(*scope))
        {
            if statement.kind() == "break_statement" {
                if is_breakable(scope)
                    && let Some(label) = self.parent(scope).and_then(label_name)
                    && self.text(label) == name
                {
                    return Some(label);
                }
            } else if scope.kind() == "statement_list" {
                for child in named_children(scope) {
                    let mut current = Some(child);
                    while let Some(labelled) =
                        current.filter(|node| node.kind() == "labeled_statement")
                    {
                        if let Some(label) =
                            label_name(labelled).filter(|label| self.text(*label) == name)
                        {
                            return Some(label);
                        }
                        current = named_children(labelled).find(|node| node.kind() != "label_name");
                    }
                }
            }
        }
        None
    }

    fn call_parameters(&self, call: Node<'tree>) -> Option<Vec<Parameter<'tree>>> {
        let callee = unwrap_parens(call.child_by_field_name("function")?);
        if self.is_type_expression(callee) {
            return None;
        }
        let callable = self.callable(callee, 0)?;
        let mut result = self.parameters(callable.node.child_by_field_name("parameters")?);
        if callable.explicit_receiver {
            // A method expression's function signature keeps its receiver and variadic
            // slots, but GoLand's synthesized signature drops the parameter names.
            for parameter in &mut result {
                parameter.name = None;
            }
            result.insert(
                0,
                Parameter {
                    name: None,
                    variadic: false,
                },
            );
        }
        Some(result)
    }

    fn parameters(&self, list: Node<'tree>) -> Vec<Parameter<'tree>> {
        parameter_slots(list)
            .map(|(parameter, name)| Parameter {
                name: name.map(|name| self.text(name)),
                variadic: parameter.kind() == "variadic_parameter_declaration",
            })
            .collect()
    }

    fn callable(&self, expression: Node<'tree>, depth: usize) -> Option<Callable<'tree>> {
        match self.shape(self.expression_type(expression, 0, depth + 1)?, depth + 1)? {
            LocalType::Callable(callable) => Some(callable),
            LocalType::Syntax(node) if node.kind() == "function_type" => Some(Callable::new(node)),
            _ => None,
        }
    }

    fn is_type_expression(&self, node: Node<'tree>) -> bool {
        let node = unwrap_parens(node);
        match node.kind() {
            "identifier" | "type_identifier" => self.symbol(node).is_some_and(|symbol| {
                matches!(
                    symbol.kind,
                    SymbolKind::Type
                        | SymbolKind::Struct
                        | SymbolKind::Interface
                        | SymbolKind::TypeParameter
                )
            }),
            "index_expression" | "type_instantiation_expression" | "generic_type" => {
                let base = node
                    .child_by_field_name("operand")
                    .or_else(|| node.child_by_field_name("type"));
                base.is_some_and(|base| self.is_type_expression(base))
            }
            "unary_expression"
                if node
                    .child_by_field_name("operator")
                    .is_some_and(|op| op.kind() == "*") =>
            {
                node.child_by_field_name("operand")
                    .is_some_and(|operand| self.is_type_expression(operand))
            }
            _ => false,
        }
    }

    fn selector_member(&self, selector: Node<'tree>, depth: usize) -> Option<Node<'tree>> {
        let operand = selector.child_by_field_name("operand")?;
        let name = self.text(selector.child_by_field_name("field")?);
        let ty = self.expression_type(operand, 0, depth + 1)?;
        let member = self.member(ty, name, depth + 1)?;
        if self.is_type_expression(operand)
            && self.declaration_kind(member) != Some(SymbolKind::Method)
        {
            return None;
        }
        Some(member)
    }

    fn member(&self, ty: LocalType<'tree>, name: &str, depth: usize) -> Option<Node<'tree>> {
        let mut frontier = vec![ty];
        let mut remaining = 256;
        for level in depth..MAX_LOCAL_DEPTH {
            if frontier.is_empty() {
                break;
            }
            let mut next = Vec::new();
            let mut found = None;
            for ty in frontier {
                if remaining == 0 {
                    return None;
                }
                remaining -= 1;
                if let Some(member) = self.direct_member(ty, name, level, &mut next)?
                    && found.replace(member).is_some()
                {
                    return None;
                }
            }
            if found.is_some() {
                return found;
            }
            frontier = next;
        }
        None
    }

    // An unknown embedded shape prevents proving a unique promoted member at
    // that depth. The inner Option represents a known type with no direct match.
    fn direct_member(
        &self,
        mut ty: LocalType<'tree>,
        name: &str,
        depth: usize,
        embedded: &mut Vec<LocalType<'tree>>,
    ) -> Option<Option<Node<'tree>>> {
        // Follow aliases for method identity. A new defined type owns its
        // methods, while members of its underlying aggregate retain their IDs.
        for _ in depth..MAX_LOCAL_DEPTH {
            let node = match ty {
                LocalType::Pointer(pointee) => {
                    ty = *pointee;
                    continue;
                }
                LocalType::Syntax(node) => unwrap_parens(node),
                _ => return Some(None),
            };
            if node.kind() == "pointer_type" {
                ty = LocalType::Syntax(named_children(node).next()?);
                continue;
            }
            let Some(owner) = self.type_owner(node) else {
                break;
            };
            let owner_name = owner.child_by_field_name("name")?;
            if self.document.indexed().editor_names.methods.contains(name)
                && self.file_name(self.text(owner_name)) == Some(owner_name)
                && let Some(member) = self
                    .document
                    .declarations()
                    .member(self.text(owner_name), name)
                && let Some(member_node) = node_at_range(self.document.indexed(), member.range)
                && self
                    .parent(member_node)
                    .is_some_and(|parent| parent.kind() == "method_declaration")
            {
                return Some(Some(member_node));
            }
            if owner.kind() != "type_alias" {
                break;
            }
            ty = LocalType::Syntax(owner.child_by_field_name("type")?);
        }
        let shape = self.shape(ty, depth)?;
        let shape = match shape {
            LocalType::Pointer(pointee) => self.shape(*pointee, depth)?,
            _ => shape,
        };
        let LocalType::Syntax(shape) = shape else {
            return Some(None);
        };
        match shape.kind() {
            "struct_type" => {
                let fields = direct_fields(shape);
                let mut matches = fields
                    .iter()
                    .copied()
                    .filter(|field| self.text(*field) == name);
                if let Some(field) = matches.next() {
                    return matches.next().is_none().then_some(Some(field));
                }
                embedded.extend(
                    fields
                        .into_iter()
                        .filter_map(|field| self.embedded_field(field))
                        .filter_map(field_type),
                );
            }
            "interface_type" => {
                let mut matches = named_children(shape)
                    .filter(|element| element.kind() == "method_elem")
                    .filter_map(|method| method.child_by_field_name("name"))
                    .filter(|method| self.text(*method) == name);
                if let Some(method) = matches.next() {
                    return matches.next().is_none().then_some(Some(method));
                }
                embedded.extend(
                    named_children(shape)
                        .filter(|element| element.kind() == "type_elem")
                        .filter_map(|element| unique(named_children(element)))
                        .map(LocalType::Syntax),
                );
            }
            "qualified_type" => return None,
            _ => {}
        }
        Some(None)
    }

    fn type_owner(&self, node: Node<'tree>) -> Option<Node<'tree>> {
        let name = match node.kind() {
            "generic_type" | "type_instantiation_expression" => node.child_by_field_name("type")?,
            "index_expression" => node.child_by_field_name("operand")?,
            "type_identifier" | "identifier" => node,
            _ => return None,
        };
        if !matches!(name.kind(), "type_identifier" | "identifier") {
            return None;
        }
        let target = self
            .lexical_name(name)
            .or_else(|| self.file_name(self.text(name)))?;
        self.parent(target)
            .filter(|owner| matches!(owner.kind(), "type_spec" | "type_alias"))
    }

    fn shape(&self, ty: LocalType<'tree>, depth: usize) -> Option<LocalType<'tree>> {
        let mut ty = ty;
        for _ in depth..MAX_LOCAL_DEPTH {
            let LocalType::Syntax(node) = ty else {
                return Some(ty);
            };
            let node = unwrap_parens(node);
            ty = match node.kind() {
                "type_identifier"
                | "identifier"
                | "generic_type"
                | "index_expression"
                | "type_instantiation_expression" => {
                    if let Some(owner) = self.type_owner(node) {
                        LocalType::Syntax(owner.child_by_field_name("type")?)
                    } else if is_predeclared_type(self.text(node))
                        && self.lexical_name(node).is_none()
                        && self.file_name(self.text(node)).is_none()
                        && !self.ambiguous_file_name(self.text(node))
                    {
                        return Some(LocalType::Syntax(node));
                    } else {
                        return None;
                    }
                }
                "pointer_type" => {
                    LocalType::Pointer(Box::new(LocalType::Syntax(named_children(node).next()?)))
                }
                _ => return Some(LocalType::Syntax(node)),
            };
        }
        None
    }

    fn expression_type(
        &self,
        node: Node<'tree>,
        index: usize,
        depth: usize,
    ) -> Option<LocalType<'tree>> {
        if depth >= MAX_LOCAL_DEPTH {
            return None;
        }
        let node = unwrap_parens(node);
        let next = depth + 1;
        let syntax = |field| node.child_by_field_name(field).map(LocalType::Syntax);
        match node.kind() {
            "identifier" | "type_identifier" | "nil" | "true" | "false" | "iota" if index == 0 => {
                let name = self
                    .lexical_name(node)
                    .or_else(|| self.file_name(self.text(node)))?;
                let parent = self.parent(name)?;
                match parent.kind() {
                    "function_declaration" => Some(LocalType::Callable(Callable::new(parent))),
                    "type_spec" | "type_alias" => Some(LocalType::Syntax(node)),
                    _ => {
                        let parent = |node| self.parent(node);
                        let binding = if name == node {
                            scope::declaration_binding_with(name, self.source(), &parent)
                        } else {
                            scope::lexical_binding_with(
                                node,
                                self.text(node),
                                self.source(),
                                &parent,
                            )
                            .filter(|binding| binding.name == name)
                            .or_else(|| {
                                scope::declaration_binding_with(name, self.source(), &parent)
                            })
                        }?;
                        self.binding_type(binding, next)
                    }
                }
            }
            "selector_expression" if index == 0 => {
                let member = self.selector_member(node, next)?;
                let parent = self.parent(member)?;
                match parent.kind() {
                    "field_declaration" => field_type(parent),
                    "method_declaration" | "method_elem" => Some(LocalType::Callable(Callable {
                        node: parent,
                        explicit_receiver: self
                            .is_type_expression(node.child_by_field_name("operand")?),
                    })),
                    _ if self.declaration_kind(member) == Some(SymbolKind::Property) => {
                        let field = self
                            .ancestors(member)
                            .find(|node| node.kind() == "field_declaration")?;
                        field_type(field)
                    }
                    _ => None,
                }
            }
            "composite_literal" | "type_conversion_expression" if index == 0 => syntax("type"),
            "type_assertion_expression" if index == 0 => syntax("type"),
            "func_literal" if index == 0 => Some(LocalType::Callable(Callable::new(node))),
            "call_expression" => {
                let callee = node.child_by_field_name("function")?;
                if index == 0 && self.is_type_expression(callee) {
                    return Some(LocalType::Syntax(callee));
                }
                if index == 0
                    && let Some(builtin) = self
                        .symbol(unwrap_parens(callee))
                        .filter(|symbol| symbol.default_library)
                {
                    let argument = named_children(node.child_by_field_name("arguments")?).next();
                    return match builtin.id {
                        SymbolId::Builtin("make") => argument.map(LocalType::Syntax),
                        SymbolId::Builtin("new") => {
                            argument.map(|ty| LocalType::Pointer(Box::new(LocalType::Syntax(ty))))
                        }
                        SymbolId::Builtin("append") => self.expression_type(argument?, 0, next),
                        _ => None,
                    };
                }
                let result = self
                    .callable(callee, next)?
                    .node
                    .child_by_field_name("result")?;
                if result.kind() == "parameter_list" {
                    let (parameter, _) = parameter_slots(result).nth(index)?;
                    parameter.child_by_field_name("type").map(LocalType::Syntax)
                } else {
                    (index == 0).then_some(LocalType::Syntax(result))
                }
            }
            "type_instantiation_expression" => {
                if index == 0 && self.is_type_expression(node) {
                    Some(LocalType::Syntax(node))
                } else {
                    self.expression_type(node.child_by_field_name("type")?, index, next)
                }
            }
            "index_expression" | "slice_expression" if index == 0 => {
                let operand = node.child_by_field_name("operand")?;
                let ty = self.expression_type(operand, 0, next)?;
                if node.kind() == "index_expression" && self.is_type_expression(operand) {
                    return Some(LocalType::Syntax(node));
                }
                if matches!(ty, LocalType::Callable(_)) && node.kind() == "index_expression" {
                    return Some(ty);
                }
                let element = self.element_type(ty, next)?;
                Some(if node.kind() == "slice_expression" {
                    LocalType::Slice(Box::new(element))
                } else {
                    element
                })
            }
            "unary_expression" if index == 0 => {
                let ty = self.expression_type(node.child_by_field_name("operand")?, 0, next)?;
                match node.child_by_field_name("operator")?.kind() {
                    "&" => Some(LocalType::Pointer(Box::new(ty))),
                    "*" if self.is_type_expression(node) => Some(LocalType::Pointer(Box::new(ty))),
                    "*" => match self.shape(ty, next)? {
                        LocalType::Pointer(pointee) => Some(*pointee),
                        _ => None,
                    },
                    "<-" => self.channel_value(ty, next),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn binding_type(&self, binding: Binding<'tree>, depth: usize) -> Option<LocalType<'tree>> {
        if let Some(ty) = binding.explicit_type {
            let ty = LocalType::Syntax(ty);
            return Some(
                if self
                    .parent(binding.name)
                    .is_some_and(|parent| parent.kind() == "variadic_parameter_declaration")
                {
                    LocalType::Slice(Box::new(ty))
                } else {
                    ty
                },
            );
        }
        let initializer = binding.initializer?;
        if initializer.kind() == "range_clause" {
            let ty = self.expression_type(initializer.child_by_field_name("right")?, 0, depth)?;
            return self.range_binding_type(ty, binding.result_index, depth);
        }
        self.expression_type(initializer, binding.result_index, depth)
    }

    fn range_binding_type(
        &self,
        ty: LocalType<'tree>,
        index: usize,
        depth: usize,
    ) -> Option<LocalType<'tree>> {
        match (self.shape(ty.clone(), depth)?, index) {
            (LocalType::Syntax(node), 0) if node.kind() == "map_type" => {
                node.child_by_field_name("key").map(LocalType::Syntax)
            }
            (LocalType::Syntax(node), 0) if node.kind() == "channel_type" => {
                self.channel_value(ty, depth)
            }
            (_, 1) => self.element_type(ty, depth),
            _ => None,
        }
    }

    fn channel_value(&self, ty: LocalType<'tree>, depth: usize) -> Option<LocalType<'tree>> {
        let LocalType::Syntax(channel) = self.shape(ty, depth)? else {
            return None;
        };
        (channel.kind() == "channel_type")
            .then(|| channel.child_by_field_name("value").map(LocalType::Syntax))?
    }

    fn element_type(&self, ty: LocalType<'tree>, depth: usize) -> Option<LocalType<'tree>> {
        match self.shape(ty, depth)? {
            LocalType::Pointer(ty) => self.element_type(*ty, depth + 1),
            LocalType::Slice(element) => Some(*element),
            LocalType::Syntax(node) => match node.kind() {
                "array_type" | "implicit_length_array_type" | "slice_type" => {
                    node.child_by_field_name("element").map(LocalType::Syntax)
                }
                "map_type" => node.child_by_field_name("value").map(LocalType::Syntax),
                _ => None,
            },
            _ => None,
        }
    }

    fn literal_type(&self, literal: Node<'tree>, depth: usize) -> Option<LocalType<'tree>> {
        if depth >= MAX_LOCAL_DEPTH {
            return None;
        }
        let element = self.parent(literal)?;
        if element.kind() == "composite_literal" {
            return element.child_by_field_name("type").map(LocalType::Syntax);
        }
        let parent = self.parent(element)?;
        let keyed = (parent.kind() == "keyed_element").then_some(parent);
        let outer = match keyed {
            Some(keyed) => self.parent(keyed)?,
            None => parent,
        };
        let mut ty = self.shape(self.literal_type(outer, depth + 1)?, depth + 1)?;
        if let LocalType::Pointer(pointee) = ty {
            ty = self.shape(*pointee, depth + 1)?;
        }
        if let LocalType::Syntax(shape) = ty {
            if shape.kind() == "struct_type" {
                let fields = direct_fields(shape);
                let field = if let Some(keyed) = keyed {
                    let key = named_children(keyed.child_by_field_name("key")?).next()?;
                    unique(
                        fields
                            .into_iter()
                            .filter(|field| self.text(*field) == self.text(key)),
                    )?
                } else {
                    fields
                        .get(named_children(outer).position(|child| child == element)?)
                        .copied()?
                };
                return field_type(
                    self.ancestors(field)
                        .find(|node| node.kind() == "field_declaration")?,
                );
            }
            if shape.kind() == "map_type"
                && keyed.is_some_and(|keyed| keyed.child_by_field_name("key") == Some(element))
            {
                return shape.child_by_field_name("key").map(LocalType::Syntax);
            }
        }
        self.element_type(ty, depth + 1)
    }

    fn literal_shape(&self, literal: Node<'tree>, depth: usize) -> Option<LocalType<'tree>> {
        match self.shape(self.literal_type(literal, depth)?, depth)? {
            LocalType::Pointer(pointee) => self.shape(*pointee, depth + 1),
            ty => Some(ty),
        }
    }

    fn literal_fields(&self, literal: Node<'tree>) -> Option<Vec<Node<'tree>>> {
        let LocalType::Syntax(ty) = self.literal_shape(literal, 0)? else {
            return None;
        };
        (ty.kind() == "struct_type").then(|| direct_fields(ty))
    }
}

const MAX_LOCAL_DEPTH: usize = 32;

#[derive(Clone)]
enum LocalType<'tree> {
    Syntax(Node<'tree>),
    Callable(Callable<'tree>),
    Pointer(Box<Self>),
    Slice(Box<Self>),
}

#[derive(Clone, Copy)]
struct Callable<'tree> {
    node: Node<'tree>,
    explicit_receiver: bool,
}

impl<'tree> Callable<'tree> {
    fn new(node: Node<'tree>) -> Self {
        Self {
            node,
            explicit_receiver: false,
        }
    }
}

struct Parameter<'tree> {
    name: Option<&'tree str>,
    variadic: bool,
}

fn parameter_slots(list: Node<'_>) -> impl Iterator<Item = (Node<'_>, Option<Node<'_>>)> {
    named_children(list)
        .filter(|node| {
            matches!(
                node.kind(),
                "parameter_declaration" | "variadic_parameter_declaration"
            )
        })
        .flat_map(|parameter| {
            let mut names = named_children(parameter)
                .filter(|node| node.kind() == "identifier")
                .peekable();
            let unnamed = names.peek().is_none();
            unnamed
                .then_some((parameter, None))
                .into_iter()
                .chain(names.map(move |name| (parameter, Some(name))))
        })
}

fn field_type(field: Node<'_>) -> Option<LocalType<'_>> {
    let ty = LocalType::Syntax(field.child_by_field_name("type")?);
    Some(
        if field
            .children(&mut field.walk())
            .any(|node| node.kind() == "*")
        {
            LocalType::Pointer(Box::new(ty))
        } else {
            ty
        },
    )
}

fn direct_fields(ty: Node<'_>) -> Vec<Node<'_>> {
    let Some(list) = named_children(ty).find(|node| node.kind() == "field_declaration_list") else {
        return Vec::new();
    };
    let mut fields = Vec::new();
    for field in named_children(list).filter(|node| node.kind() == "field_declaration") {
        let start = fields.len();
        fields.extend(
            field
                .children_by_field_name("name", &mut field.walk())
                .filter(Node::is_named),
        );
        if fields.len() == start {
            fields.extend(
                field
                    .child_by_field_name("type")
                    .and_then(terminal_type_name),
            );
        }
    }
    fields
}

fn terminal_type_name(mut node: Node<'_>) -> Option<Node<'_>> {
    loop {
        node = match node.kind() {
            "identifier" | "type_identifier" => return Some(node),
            "qualified_type" => node.child_by_field_name("name")?,
            "generic_type" | "type_instantiation_expression" => node.child_by_field_name("type")?,
            "pointer_type" | "parenthesized_type" => named_children(node).next()?,
            _ => return None,
        };
    }
}

fn unwrap_parens(mut node: Node<'_>) -> Node<'_> {
    while matches!(
        node.kind(),
        "parenthesized_expression" | "parenthesized_type"
    ) {
        let Some(child) = named_children(node).next() else {
            break;
        };
        node = child;
    }
    node
}

fn is_function(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "function_declaration" | "method_declaration" | "func_literal"
    )
}

fn is_breakable(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "for_statement"
            | "expression_switch_statement"
            | "type_switch_statement"
            | "select_statement"
    )
}

fn label_name(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("label")
        .filter(|label| label.kind() == "label_name")
}

fn named_children(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    std::iter::successors(node.named_child(0), Node::next_named_sibling)
        .filter(|node| !node.is_extra() && !node.is_missing())
}

fn descendants(root: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    let mut cursor = root.walk();
    let mut visit_root = true;
    let mut descend = true;
    std::iter::from_fn(move || {
        if std::mem::take(&mut visit_root) {
            return Some(root);
        }
        loop {
            if !descend || !cursor.goto_first_child() {
                while !cursor.goto_next_sibling() {
                    if !cursor.goto_parent() {
                        return None;
                    }
                }
            }
            let node = cursor.node();
            descend = node.is_named() && !node.is_extra() && !node.is_missing();
            if descend {
                return Some(node);
            }
        }
    })
}

fn unique<T>(mut items: impl Iterator<Item = T>) -> Option<T> {
    let first = items.next()?;
    items.next().is_none().then_some(first)
}
