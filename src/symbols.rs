use std::collections::{HashMap, HashSet};
use std::ops;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use lsp_types::Range;
use tree_sitter::{Node, Tree};

use crate::scope::{initializer_at, lexical_binding};

#[derive(Clone, Debug)]
pub(crate) struct TypeRef {
    pub(crate) pointers: u8,
    pub(crate) kind: TypeRefKind,
}

#[derive(Clone, Debug)]
pub(crate) enum TypeRefKind {
    Named(NamedType),
    Container(Box<Container<TypeRef>>, Range),
    Inline(Arc<InlineType>),
    Function(Arc<FunctionSignature>),
}

#[derive(Clone, Debug)]
pub(crate) struct NamedType {
    pub(crate) qualifier: Option<String>,
    pub(crate) name: String,
    pub(crate) arguments: Vec<Option<TypeRef>>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum Container<T> {
    Slice(T),
    Array {
        element: T,
        length: Option<String>,
    },
    Map {
        key: T,
        value: T,
    },
    Channel {
        element: T,
        receive: bool,
        send: bool,
    },
}

#[derive(Clone, Debug, Default)]
pub(crate) struct InlineType {
    pub(crate) origin: Range,
    pub(crate) interface: bool,
    pub(crate) members: HashMap<String, Symbol>,
    pub(crate) embedded: Vec<TypeRef>,
    pub(crate) fields: Vec<InlineField>,
    pub(crate) has_type_terms: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct InlineField {
    pub(crate) name: String,
    pub(crate) embedded: bool,
    pub(crate) tag: Option<String>,
    pub(crate) ty: Option<TypeRef>,
}

#[derive(Clone, Debug)]
pub(crate) struct Symbol {
    pub(crate) range: Range,
    pub(crate) kind: SymbolKind,
    pub(crate) pointer_receiver: bool,
    pub(crate) receiver_parameters: Vec<Option<String>>,
    pub(crate) addressable: bool,
}

impl Symbol {
    fn new(range: Range, kind: SymbolKind, addressable: bool) -> Self {
        Self {
            range,
            kind,
            pointer_receiver: false,
            receiver_parameters: Vec::new(),
            addressable,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum SymbolKind {
    Value(Option<TypeRef>),
    InferredValue {
        initializer: ops::Range<usize>,
        result_index: usize,
    },
    Type,
    Function(Arc<FunctionSignature>),
}

#[derive(Clone, Debug)]
pub(crate) struct FunctionSignature {
    pub(crate) origin: FunctionOrigin,
    pub(crate) result_tuple: Option<Range>,
    pub(crate) results: Arc<[Option<TypeRef>]>,
    pub(crate) parameters: Vec<Option<TypeRef>>,
    pub(crate) parameters_known: bool,
    pub(crate) variadic: bool,
    pub(crate) type_parameters: Vec<GenericParameter>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum FunctionOrigin {
    Name(Range),
    FuncKeyword(Range),
}

impl FunctionOrigin {
    pub(crate) fn range(self) -> Range {
        match self {
            Self::Name(range) | Self::FuncKeyword(range) => range,
        }
    }

    pub(crate) fn function_type(self) -> Option<Range> {
        match self {
            Self::FuncKeyword(range) => Some(range),
            Self::Name(_) => None,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct GenericParameter {
    pub(crate) name: String,
    pub(crate) name_range: Range,
    pub(crate) constraint: TypeConstraint,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TypeConstraint {
    pub(crate) methods: Option<TypeRef>,
    pub(crate) approximation: Option<TypeRef>,
    pub(crate) terms: Vec<ConstraintTerm>,
}

#[derive(Clone, Debug)]
pub(crate) struct ConstraintTerm {
    pub(crate) reference: TypeRef,
    pub(crate) approximate: bool,
}

#[derive(Clone, Default)]
pub(crate) struct TypeDetails {
    pub(crate) alias: Option<TypeRef>,
    pub(crate) underlying: Option<TypeRef>,
    pub(crate) interface: bool,
    pub(crate) structure: bool,
    pub(crate) method_names: Vec<String>,
    pub(crate) embedded: Vec<TypeRef>,
    pub(crate) parameters: Vec<GenericParameter>,
    pub(crate) has_type_terms: bool,
}

#[derive(Default)]
pub(crate) struct FileSymbols {
    declarations: HashMap<String, Symbol>,
    members: HashMap<String, HashMap<String, Symbol>>,
    types: HashMap<String, TypeDetails>,
}

impl FileSymbols {
    pub(crate) fn declaration(&self, name: &str) -> Option<&Symbol> {
        self.declarations.get(name)
    }

    pub(crate) fn declarations(&self) -> impl Iterator<Item = (&str, &Symbol)> {
        self.declarations
            .iter()
            .map(|(name, symbol)| (name.as_str(), symbol))
    }

    pub(crate) fn member(&self, ty: &str, name: &str) -> Option<&Symbol> {
        self.members.get(ty)?.get(name)
    }

    pub(crate) fn members_of(&self, ty: &str) -> impl Iterator<Item = (&str, &Symbol)> {
        self.members
            .get(ty)
            .into_iter()
            .flatten()
            .map(|(name, member)| (name.as_str(), member))
    }

    #[cfg(test)]
    pub(crate) fn members(&self) -> impl Iterator<Item = (&str, &Symbol)> {
        self.members
            .values()
            .flatten()
            .map(|(name, member)| (name.as_str(), member))
    }

    pub(crate) fn type_details(&self, ty: &str) -> Option<&TypeDetails> {
        self.types.get(ty)
    }

    pub(crate) fn type_declarations(&self) -> impl Iterator<Item = (&str, &TypeDetails)> {
        self.types
            .iter()
            .map(|(name, details)| (name.as_str(), details))
    }

    pub(crate) fn member_owners<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> {
        self.members.iter().filter_map(move |(owner, members)| {
            members.get(name).and_then(|member| {
                matches!(member.kind, SymbolKind::Function(_)).then_some(owner.as_str())
            })
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OutlineKind {
    Constant,
    Variable,
    Class,
    Struct,
    Interface,
    EmbeddedInterface,
    Field,
    Function,
    Method,
}

pub(crate) struct OutlineReceiver {
    pub(crate) base: String,
    pub(crate) spelling: String,
}

pub(crate) struct OutlineSymbol {
    pub(crate) name: String,
    pub(crate) kind: OutlineKind,
    pub(crate) detail: Option<String>,
    pub(crate) range: ops::Range<usize>,
    pub(crate) selection_range: ops::Range<usize>,
    pub(crate) receiver: Option<OutlineReceiver>,
    children: Option<Rc<[OutlineSymbol]>>,
}

impl OutlineSymbol {
    pub(crate) fn children(&self) -> &[OutlineSymbol] {
        self.children.as_deref().unwrap_or_default()
    }
}

pub(crate) struct FileOutline {
    pub(crate) package: Option<String>,
    pub(crate) symbols: Vec<OutlineSymbol>,
}

pub(crate) fn collect_outline(tree: &Tree, text: &str) -> FileOutline {
    collect_outline_with(tree, text, true, None, None, || {})
}

pub(crate) type MethodNames = HashMap<String, HashSet<String>>;

pub(crate) fn diagnostic_method_names(
    tree: &Tree,
    text: &str,
    cancelled: Option<&AtomicBool>,
) -> anyhow::Result<MethodNames> {
    let root = tree.root_node();
    let mut cursor = root.walk();
    let collector = OutlineCollector {
        text,
        details: false,
        methods: None,
        cancelled,
    };
    let mut methods = MethodNames::new();
    for node in root.named_children(&mut cursor) {
        if is_cancelled(cancelled) {
            break;
        }
        if node.kind() == "method_declaration"
            && let Some(symbol) = collector.callable(node)
            && let Some(receiver) = symbol.receiver
        {
            methods
                .entry(receiver.base)
                .or_default()
                .insert(symbol.name);
        }
    }
    anyhow::ensure!(!is_cancelled(cancelled), "diagnostics cancelled");
    Ok(methods)
}

pub(crate) fn collect_diagnostic_outline(
    tree: &Tree,
    text: &str,
    methods: &MethodNames,
    cancelled: Option<&AtomicBool>,
) -> anyhow::Result<FileOutline> {
    let outline = collect_outline_with(tree, text, false, Some(methods), cancelled, || {});
    anyhow::ensure!(!is_cancelled(cancelled), "diagnostics cancelled");
    Ok(outline)
}

fn collect_outline_with(
    tree: &Tree,
    text: &str,
    details: bool,
    methods: Option<&MethodNames>,
    cancelled: Option<&AtomicBool>,
    mut before_declaration: impl FnMut(),
) -> FileOutline {
    let mut outline = FileOutline {
        package: None,
        symbols: Vec::new(),
    };
    let root = tree.root_node();
    let mut cursor = root.walk();
    let collector = OutlineCollector {
        text,
        details,
        methods,
        cancelled,
    };
    for node in root.named_children(&mut cursor) {
        before_declaration();
        if is_cancelled(cancelled) {
            break;
        }
        collector.top_level(node, &mut outline);
    }
    outline
}

#[cfg(test)]
#[test]
fn diagnostic_declaration_collection_honors_cancellation_before_the_next_declaration() {
    use std::time::{Duration, Instant};

    let declarations = (0..1024)
        .map(|i| format!("var value{i} int\n"))
        .collect::<String>();
    let text = format!("package sample\n{declarations}");
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_go::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(&text, None).unwrap();
    let cancelled = AtomicBool::new(false);
    let mut steps = 0;
    let mut signalled = None;
    let outline = collect_outline_with(&tree, &text, false, None, Some(&cancelled), || {
        steps += 1;
        if steps == 5 {
            signalled = Some(Instant::now());
            cancelled.store(true, Relaxed);
        }
    });
    assert_eq!(steps, 5);
    assert_eq!(outline.symbols.len(), 3);
    assert!(signalled.unwrap().elapsed() <= Duration::from_millis(50));
}

struct OutlineCollector<'a> {
    text: &'a str,
    details: bool,
    methods: Option<&'a MethodNames>,
    cancelled: Option<&'a AtomicBool>,
}

impl<'a> OutlineCollector<'a> {
    fn top_level(&self, node: Node<'_>, outline: &mut FileOutline) {
        match node.kind() {
            "package_clause" => {
                if outline.package.is_none() {
                    outline.package = only_named_child(node)
                        .and_then(|name| self.name(name))
                        .map(str::to_owned);
                }
            }
            "const_declaration" | "var_declaration" | "type_declaration" => {
                let grouped = node
                    .children(&mut node.walk())
                    .any(|child| matches!(child.kind(), "(" | "var_spec_list"));
                self.specs(
                    node,
                    (!grouped).then_some(node),
                    &mut None,
                    &mut outline.symbols,
                );
            }
            "function_declaration" | "method_declaration" => {
                outline.symbols.extend(self.callable(node));
            }
            "ERROR" => {
                // A wrapper may expose raw brace tokens around a partial body.
                // Collect recovered siblings at declaration scope.
                let mut depth = 0usize;
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    match child.kind() {
                        "{" => depth += 1,
                        "}" => depth = depth.saturating_sub(1),
                        _ if depth == 0 && child.is_named() => self.top_level(child, outline),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn specs<'tree>(
        &self,
        list: Node<'tree>,
        declaration: Option<Node<'tree>>,
        previous_const: &mut Option<Node<'tree>>,
        symbols: &mut Vec<OutlineSymbol>,
    ) {
        let mut cursor = list.walk();
        for spec in list.named_children(&mut cursor) {
            if is_cancelled(self.cancelled) {
                break;
            }
            let range = declaration.unwrap_or(spec);
            match spec.kind() {
                "const_spec" => {
                    if spec.child_by_field_name("value").is_some() {
                        *previous_const = Some(spec);
                    }
                    self.values(
                        spec,
                        range,
                        previous_const.unwrap_or(spec),
                        OutlineKind::Constant,
                        symbols,
                    );
                }
                "var_spec" => self.values(spec, range, spec, OutlineKind::Variable, symbols),
                "type_spec" | "type_alias" => symbols.extend(self.ty(spec, range)),
                "var_spec_list" | "ERROR" => self.specs(spec, declaration, previous_const, symbols),
                _ => {}
            }
        }
    }

    fn values(
        &self,
        spec: Node<'_>,
        declaration: Node<'_>,
        initializer: Node<'_>,
        kind: OutlineKind,
        symbols: &mut Vec<OutlineSymbol>,
    ) {
        let mut cursor = spec.walk();
        let names = spec
            .children_by_field_name("name", &mut cursor)
            .filter(|node| node.kind() == "identifier")
            .take_while(|_| !is_cancelled(self.cancelled));
        if !self.details {
            symbols.extend(names.filter_map(|name| self.symbol(name, declaration, kind)));
            return;
        }
        let ty = initializer.child_by_field_name("type");
        let values = initializer.child_by_field_name("value");
        let mut value_cursor = initializer.walk();
        let mut expressions = values
            .map(|list| list.named_children(&mut value_cursor))
            .into_iter()
            .flatten()
            .filter(|node| !node.is_extra())
            .peekable();
        let first = expressions.next();
        let shared = expressions.peek().is_none();
        let mut ordered_values = first.into_iter().chain(expressions);
        for name in names {
            let value = if shared { first } else { ordered_values.next() };
            let Some(mut symbol) = self.symbol(name, declaration, kind) else {
                continue;
            };
            symbol.detail = match (ty, value) {
                (Some(ty), Some(value)) => {
                    Some(format!("{} = {}", self.text(ty), self.text(value)))
                }
                (Some(ty), None) => Some(self.text(ty).to_owned()),
                (None, Some(value)) => Some(format!("= {}", self.text(value))),
                (None, None) => None,
            };
            symbols.push(symbol);
        }
    }

    fn ty(&self, spec: Node<'_>, declaration: Node<'_>) -> Option<OutlineSymbol> {
        let name = spec.child_by_field_name("name")?;
        let ty = spec.child_by_field_name("type");
        let inline = ty.and_then(outline_inline_type);
        let alias = spec.kind() == "type_alias";
        let kind = match (alias, inline.map(|node| node.kind())) {
            (false, Some("struct_type")) => OutlineKind::Struct,
            (false, Some("interface_type")) => OutlineKind::Interface,
            _ => OutlineKind::Class,
        };
        let mut symbol = self.symbol(name, declaration, kind)?;
        if self.details
            && let Some(ty) = ty
        {
            let detail = self.type_detail(ty, inline);
            symbol.detail = Some(if alias {
                format!("= {detail}")
            } else {
                detail.to_owned()
            });
        }
        if let Some(inline) = inline {
            if self.details {
                symbol.children = self.members(inline, None);
            } else if kind == OutlineKind::Struct
                && let Some(names) = self.methods.and_then(|methods| methods.get(&symbol.name))
            {
                symbol.children = self.members(inline, Some(names));
            }
        }
        Some(symbol)
    }

    fn members(
        &self,
        inline: Node<'_>,
        filter: Option<&HashSet<String>>,
    ) -> Option<Rc<[OutlineSymbol]>> {
        let mut members = Vec::new();
        self.collect_members(inline, filter, &mut members);
        (!members.is_empty()).then(|| members.into())
    }

    fn collect_members(
        &self,
        list: Node<'_>,
        filter: Option<&HashSet<String>>,
        members: &mut Vec<OutlineSymbol>,
    ) {
        let mut cursor = list.walk();
        let mut names = list.walk();
        for member in list.named_children(&mut cursor) {
            if is_cancelled(self.cancelled) {
                break;
            }
            match member.kind() {
                "field_declaration_list" | "ERROR" => self.collect_members(member, filter, members),
                "field_declaration" => self.fields(member, &mut names, filter, members),
                "method_elem" => members.extend(self.callable(member)),
                "type_elem" => members.extend(self.embedded_interface(member)),
                _ => {}
            }
        }
    }

    fn fields<'t>(
        &self,
        field: Node<'t>,
        cursor: &mut tree_sitter::TreeCursor<'t>,
        filter: Option<&HashSet<String>>,
        members: &mut Vec<OutlineSymbol>,
    ) {
        let Some(ty) = field.child_by_field_name("type") else {
            return;
        };
        let matches = |name: Node<'_>| filter.is_none_or(|names| names.contains(self.text(name)));
        let mut names = field.children_by_field_name("name", cursor).peekable();
        if names.peek().is_none() {
            if let Some(syntax) = named_type_syntax(ty)
                && matches(syntax.name)
                && let Some(mut symbol) = self.symbol(syntax.name, field, OutlineKind::Field)
            {
                if self.details {
                    let spelling = self.text(field);
                    symbol.detail = (spelling != symbol.name).then(|| spelling.to_owned());
                }
                members.push(symbol);
            }
            return;
        }

        if !self.details {
            members.extend(
                names
                    .take_while(|_| !is_cancelled(self.cancelled))
                    .filter(|name| matches(*name))
                    .filter_map(|name| self.symbol(name, field, OutlineKind::Field)),
            );
            return;
        }

        let inline = unparenthesized_type(ty)
            .and_then(|node| {
                if node.kind() == "pointer_type" {
                    only_named_child(node)
                } else {
                    Some(node)
                }
            })
            .and_then(outline_inline_type);
        let children = inline.and_then(|inline| self.members(inline, None));
        let ty_detail = self.type_detail(ty, inline);
        let detail = match field.child_by_field_name("tag") {
            Some(tag) => format!("{ty_detail} {}", self.text(tag)),
            None => ty_detail.to_owned(),
        };
        for name in names {
            if let Some(mut symbol) = self.symbol(name, field, OutlineKind::Field) {
                symbol.detail = Some(detail.clone());
                symbol.children = children.clone();
                members.push(symbol);
            }
        }
    }

    fn embedded_interface(&self, member: Node<'_>) -> Option<OutlineSymbol> {
        let term = only_named_child(member)?;
        if member
            .children(&mut member.walk())
            .any(|child| child.kind() == "|")
        {
            return None;
        }
        let syntax = named_type_syntax(term)?;
        let mut symbol = self.symbol(syntax.name, member, OutlineKind::EmbeddedInterface)?;
        let spelling = self.text(term);
        symbol.detail = (spelling != symbol.name).then(|| spelling.to_owned());
        Some(symbol)
    }

    fn callable(&self, declaration: Node<'_>) -> Option<OutlineSymbol> {
        let name = declaration.child_by_field_name("name")?;
        let kind = if declaration.kind() == "function_declaration" {
            OutlineKind::Function
        } else {
            OutlineKind::Method
        };
        let mut symbol = self.symbol(name, declaration, kind)?;
        if declaration.kind() == "method_declaration" {
            let receiver = declaration.child_by_field_name("receiver")?;
            let parameter = only_named_child(receiver)?;
            let ty = parameter.child_by_field_name("type")?;
            let base = unparenthesized_type(ty)?;
            let base = if base.kind() == "pointer_type" {
                only_named_child(base)?
            } else {
                base
            };
            let named = named_type_syntax(base)?;
            if named.qualifier.is_some() {
                return None;
            }
            symbol.receiver = Some(OutlineReceiver {
                base: self.name(named.name)?.to_owned(),
                spelling: if self.details {
                    self.text(ty).to_owned()
                } else {
                    String::new()
                },
            });
        }
        if self.details
            && let Some(parameters) = declaration.child_by_field_name("parameters")
        {
            let start = declaration
                .child_by_field_name("type_parameters")
                .unwrap_or(parameters);
            let end = declaration
                .child_by_field_name("result")
                .unwrap_or(parameters);
            symbol.detail = self
                .text
                .get(start.start_byte()..end.end_byte())
                .map(str::to_owned);
        }
        Some(symbol)
    }

    fn symbol(
        &self,
        name: Node<'_>,
        declaration: Node<'_>,
        kind: OutlineKind,
    ) -> Option<OutlineSymbol> {
        let spelling = self.name(name)?;
        let mut range = declaration.byte_range();
        let selection_range = name.byte_range();
        if selection_range.start < range.start
            || selection_range.end > range.end
            || self.text.get(range.clone()).is_none()
        {
            return None;
        }
        if self.details
            && range == selection_range
            && let Some(parent) = declaration.parent()
        {
            // Bare embeddings and implicit const specs can be a single token.
            // Include the adjacent delimiter or trivia within the enclosing node.
            let bytes = self.text.as_bytes();
            let after = &bytes[range.end..parent.end_byte()];
            let before = &bytes[parent.start_byte()..range.start];
            if after.starts_with(b"\r\n") {
                range.end += 2;
            } else if after
                .first()
                .is_some_and(|byte| byte.is_ascii_whitespace() || b";})".contains(byte))
            {
                range.end += 1;
            } else if before.ends_with(b"\r\n") {
                range.start -= 2;
            } else if before
                .last()
                .is_some_and(|byte| byte.is_ascii_whitespace() || b";{(".contains(byte))
            {
                range.start -= 1;
            }
        }
        Some(OutlineSymbol {
            name: spelling.to_owned(),
            kind,
            detail: None,
            range,
            selection_range,
            receiver: None,
            children: None,
        })
    }

    fn name(&self, node: Node<'_>) -> Option<&'a str> {
        let name = self.text(node);
        (!node.is_missing() && !matches!(name, "" | "_")).then_some(name)
    }

    fn text(&self, node: Node<'_>) -> &'a str {
        self.text.get(node.byte_range()).unwrap_or_default()
    }

    fn type_detail(&self, ty: Node<'_>, inline: Option<Node<'_>>) -> &'a str {
        let Some(inline) = inline else {
            return self.text(ty);
        };
        let pointer = unparenthesized_type(ty).is_some_and(|node| node.kind() == "pointer_type");
        match (inline.kind(), pointer) {
            ("struct_type", false) => "struct",
            ("struct_type", true) => "*struct",
            ("interface_type", false) => "interface",
            ("interface_type", true) => "*interface",
            _ => self.text(ty),
        }
    }
}

fn outline_inline_type(node: Node<'_>) -> Option<Node<'_>> {
    let node = unparenthesized_type(node)?;
    matches!(node.kind(), "struct_type" | "interface_type").then_some(node)
}

pub(crate) fn collect(
    tree: &Tree,
    text: &str,
    range: impl Fn(Node<'_>) -> Range,
    visit: impl FnMut(Node<'_>, &str, &Symbol),
) -> FileSymbols {
    collect_symbols(tree, text, range, visit, None)
}

pub(crate) fn collect_cancellable(
    tree: &Tree,
    text: &str,
    range: impl Fn(Node<'_>) -> Range,
    cancelled: &AtomicBool,
) -> anyhow::Result<FileSymbols> {
    let symbols = collect_symbols(tree, text, range, |_, _, _| {}, Some(cancelled));
    anyhow::ensure!(!cancelled.load(Relaxed), "diagnostics cancelled");
    Ok(symbols)
}

fn collect_symbols(
    tree: &Tree,
    text: &str,
    range: impl Fn(Node<'_>) -> Range,
    mut visit: impl FnMut(Node<'_>, &str, &Symbol),
    cancelled: Option<&AtomicBool>,
) -> FileSymbols {
    let mut indexer = Indexer {
        text,
        range: &range,
        visit: &mut visit,
        symbols: FileSymbols::default(),
        cancelled,
    };
    let root = tree.root_node();
    let mut cursor = root.walk();
    for declaration in root.named_children(&mut cursor) {
        if is_cancelled(cancelled) {
            break;
        }
        indexer.declaration(declaration);
    }
    indexer.symbols
}

struct Indexer<'a> {
    text: &'a str,
    range: &'a dyn Fn(Node<'_>) -> Range,
    visit: &'a mut dyn FnMut(Node<'_>, &str, &Symbol),
    symbols: FileSymbols,
    cancelled: Option<&'a AtomicBool>,
}

impl Indexer<'_> {
    fn declaration(&mut self, declaration: Node<'_>) {
        let mut cursor = declaration.walk();
        match declaration.kind() {
            "var_declaration" | "var_spec_list" | "const_declaration" | "type_declaration" => {
                for spec in declaration.named_children(&mut cursor) {
                    if is_cancelled(self.cancelled) {
                        break;
                    }
                    self.declaration(spec);
                }
            }
            "var_spec" | "const_spec" => self.value_declaration(declaration),
            "type_spec" | "type_alias" => self.type_declaration(declaration),
            "function_declaration" => {
                if let Some(name) = declaration.child_by_field_name("name") {
                    let results = function_signature(declaration, self.text, self.range);
                    self.insert_declaration(name, SymbolKind::Function(results), false);
                }
            }
            "method_declaration" => self.method_declaration(declaration),
            _ => {}
        }
    }

    fn value_declaration(&mut self, declaration: Node<'_>) {
        let mut cursor = declaration.walk();
        let names = declaration
            .children_by_field_name("name", &mut cursor)
            .filter(|node| node.kind() == "identifier");
        let ty = declaration.child_by_field_name("type");
        let explicit_type =
            ty.and_then(|node| type_ref_inner(node, self.text, self.range, self.cancelled));
        let values = declaration.child_by_field_name("value");
        let variable = declaration.kind() == "var_spec";
        for (index, name) in names.enumerate() {
            if is_cancelled(self.cancelled) {
                break;
            }
            let kind = if variable
                && ty.is_none()
                && let (Some(initializer), result_index) = initializer_at(values, index)
            {
                match composite_value_type(initializer, self.text, self.range) {
                    Some(ty) if result_index == 0 => SymbolKind::Value(Some(ty)),
                    _ => SymbolKind::InferredValue {
                        initializer: initializer.byte_range(),
                        result_index,
                    },
                }
            } else {
                SymbolKind::Value(explicit_type.clone())
            };
            self.insert_declaration(name, kind, variable);
        }
    }

    fn insert_declaration(&mut self, name: Node<'_>, kind: SymbolKind, addressable: bool) {
        let key = node_text(name, self.text);
        if key.is_empty() || key == "_" {
            return;
        }
        if let std::collections::hash_map::Entry::Vacant(entry) =
            self.symbols.declarations.entry(key.to_owned())
        {
            let symbol = Symbol::new((self.range)(name), kind, addressable);
            (self.visit)(name, key, &symbol);
            entry.insert(symbol);
        }
    }

    fn type_declaration(&mut self, declaration: Node<'_>) {
        let (Some(name), Some(ty)) = (
            declaration.child_by_field_name("name"),
            declaration.child_by_field_name("type"),
        ) else {
            return;
        };
        self.insert_declaration(name, SymbolKind::Type, false);

        let owner = node_text(name, self.text);
        let mut details = TypeDetails {
            parameters: type_parameters(declaration, self.text, self.range),
            ..TypeDetails::default()
        };
        if declaration.kind() == "type_alias" {
            details.alias = type_ref_inner(ty, self.text, self.range, self.cancelled);
        } else if let Some(node) = outline_inline_type(ty) {
            // Named declarations store members separately. Structural fields
            // are needed only for inline and alias type references.
            let inline = inline_type(node, self.text, self.range, self.cancelled, false);
            details.interface = inline.interface;
            details.structure = !inline.interface;
            details.has_type_terms = inline.has_type_terms;
            if inline.interface {
                details.method_names = inline.members.keys().cloned().collect();
            }
            details.embedded = inline.embedded;
            let members = self.symbols.members.entry(owner.to_owned()).or_default();
            if members.is_empty() {
                *members = inline.members;
            } else {
                for (name, symbol) in inline.members {
                    if is_cancelled(self.cancelled) {
                        return;
                    }
                    members.entry(name).or_insert(symbol);
                }
            }
        } else {
            details.underlying = type_ref_inner(ty, self.text, self.range, self.cancelled);
        }
        self.symbols
            .types
            .entry(owner.to_owned())
            .or_insert(details);
    }

    fn method_declaration(&mut self, declaration: Node<'_>) {
        let (Some(receiver), Some(name)) = (
            declaration.child_by_field_name("receiver"),
            declaration.child_by_field_name("name"),
        ) else {
            return;
        };
        let mut cursor = receiver.walk();
        let mut parameters = receiver
            .named_children(&mut cursor)
            .filter(|node| node.kind() == "parameter_declaration");
        let Some(parameter) = parameters.next() else {
            return;
        };
        if parameters.next().is_some() {
            return;
        }
        let Some(owner) = parameter
            .child_by_field_name("type")
            .and_then(|ty| type_ref(ty, self.text, self.range))
        else {
            return;
        };
        let TypeRefKind::Named(named) = owner.kind else {
            return;
        };
        if named.qualifier.is_some() || owner.pointers > 1 {
            return;
        }
        let results = function_signature(declaration, self.text, self.range);
        let mut symbol = Symbol::new((self.range)(name), SymbolKind::Function(results), false);
        symbol.pointer_receiver = owner.pointers == 1;
        symbol.receiver_parameters = named
            .arguments
            .into_iter()
            .map(|argument| {
                let TypeRef {
                    pointers: 0,
                    kind: TypeRefKind::Named(parameter),
                } = argument?
                else {
                    return None;
                };
                (parameter.qualifier.is_none() && parameter.arguments.is_empty())
                    .then_some(parameter.name)
            })
            .collect();
        self.insert_member(&named.name, name, symbol);
    }

    fn insert_member(&mut self, owner: &str, name: Node<'_>, symbol: Symbol) {
        let key = node_text(name, self.text);
        if key.is_empty() || key == "_" {
            return;
        }
        if let std::collections::hash_map::Entry::Vacant(entry) = self
            .symbols
            .members
            .entry(owner.to_owned())
            .or_default()
            .entry(key.to_owned())
        {
            (self.visit)(name, key, &symbol);
            entry.insert(symbol);
        }
    }
}

pub(crate) fn type_ref(
    node: Node<'_>,
    text: &str,
    range: &dyn Fn(Node<'_>) -> Range,
) -> Option<TypeRef> {
    type_ref_inner(node, text, range, None)
}

fn is_cancelled(cancelled: Option<&AtomicBool>) -> bool {
    cancelled.is_some_and(|flag| flag.load(Relaxed))
}

fn type_ref_inner(
    node: Node<'_>,
    text: &str,
    range: &dyn Fn(Node<'_>) -> Range,
    cancelled: Option<&AtomicBool>,
) -> Option<TypeRef> {
    if is_cancelled(cancelled) {
        return None;
    }
    let node = unparenthesized_type(node)?;
    let parse = |field| type_ref_inner(node.child_by_field_name(field)?, text, range, cancelled);
    let container = |container| {
        let token = node.child(0)?;
        Some(TypeRefKind::Container(Box::new(container), range(token)))
    };
    let kind = match node.kind() {
        "pointer_type" | "unary_expression"
            if node.kind() == "pointer_type"
                || node
                    .child_by_field_name("operator")
                    .is_some_and(|op| op.kind() == "*") =>
        {
            let mut inner = type_ref_inner(only_named_child(node)?, text, range, cancelled)?;
            inner.pointers = inner.pointers.checked_add(1)?;
            return Some(inner);
        }
        "slice_type" => container(Container::Slice(parse("element")?))?,
        "array_type" | "implicit_length_array_type" => container(Container::Array {
            element: parse("element")?,
            length: node
                .child_by_field_name("length")
                .map(|length| node_text(length, text).to_owned()),
        })?,
        "map_type" => container(Container::Map {
            key: parse("key")?,
            value: parse("value")?,
        })?,
        "channel_type" => container(Container::Channel {
            element: parse("value")?,
            receive: node
                .children(&mut node.walk())
                .filter(|child| !child.is_extra())
                .nth(1)
                .is_some_and(|second| second.kind() != "<-"),
            send: node.child(0).is_some_and(|first| first.kind() != "<-"),
        })?,
        "struct_type" | "interface_type" => {
            TypeRefKind::Inline(Arc::new(inline_type(node, text, range, cancelled, true)))
        }
        "function_type"
        | "func_literal"
        | "function_declaration"
        | "method_declaration"
        | "method_elem" => TypeRefKind::Function(function_signature(node, text, range)),
        _ => TypeRefKind::Named(named_type(node, text, range)?),
    };
    Some(TypeRef { pointers: 0, kind })
}

fn named_type(node: Node<'_>, text: &str, range: &dyn Fn(Node<'_>) -> Range) -> Option<NamedType> {
    let syntax = named_type_syntax(node)?;
    let name = node_text(syntax.name, text);
    let qualifier = syntax.qualifier.map(|qualifier| node_text(qualifier, text));
    if name.is_empty() || name == "_" || qualifier.is_some_and(|qualifier| qualifier.is_empty()) {
        return None;
    }
    let arguments = syntax
        .arguments
        .map(|list| {
            list.named_children(&mut list.walk())
                .filter(|node| node.kind() == "type_elem")
                .map(|node| only_named_child(node).and_then(|ty| type_ref(ty, text, range)))
                .collect()
        })
        .unwrap_or_default();
    Some(NamedType {
        qualifier: qualifier.map(str::to_owned),
        name: name.to_owned(),
        arguments,
    })
}

fn inline_type(
    node: Node<'_>,
    text: &str,
    range: &dyn Fn(Node<'_>) -> Range,
    cancelled: Option<&AtomicBool>,
    retain_fields: bool,
) -> InlineType {
    let mut inline = InlineType {
        origin: range(node.child(0).unwrap_or(node)),
        interface: node.kind() == "interface_type",
        ..InlineType::default()
    };
    let mut cursor = node.walk();
    if inline.interface {
        for element in node.named_children(&mut cursor) {
            if is_cancelled(cancelled) {
                break;
            }
            match element.kind() {
                "method_elem" => {
                    if let Some(name) = element.child_by_field_name("name") {
                        let results = function_signature(element, text, range);
                        let symbol = Symbol::new(range(name), SymbolKind::Function(results), false);
                        inline
                            .members
                            .entry(node_text(name, text).to_owned())
                            .or_insert(symbol);
                    }
                }
                "type_elem" => {
                    if let Some(term) = only_named_child(element)
                        && let Some(reference) = type_ref_inner(term, text, range, cancelled)
                        && matches!(reference.kind, TypeRefKind::Named(_))
                    {
                        inline.embedded.push(reference);
                    } else {
                        inline.has_type_terms = true;
                    }
                }
                _ => {}
            }
        }
        return inline;
    }
    let Some(list) = node
        .named_children(&mut cursor)
        .find(|node| node.kind() == "field_declaration_list")
    else {
        return inline;
    };
    inline.members.reserve(list.named_child_count());
    let mut name_cursor = list.walk();
    for field in list
        .named_children(&mut list.walk())
        .filter(|node| node.kind() == "field_declaration")
    {
        if is_cancelled(cancelled) {
            break;
        }
        let field_type = field.child_by_field_name("type");
        let mut ty = field_type.and_then(|node| type_ref_inner(node, text, range, cancelled));
        let tag = if retain_fields {
            field
                .child_by_field_name("tag")
                .map(|tag| node_text(tag, text).to_owned())
        } else {
            None
        };
        let mut names = field
            .children_by_field_name("name", &mut name_cursor)
            .filter(|node| node.kind() == "field_identifier")
            .peekable();
        if names.peek().is_some() {
            while let Some(name) = names.next() {
                if is_cancelled(cancelled) {
                    return inline;
                }
                let key = node_text(name, text);
                if retain_fields {
                    inline.fields.push(InlineField {
                        name: key.to_owned(),
                        embedded: false,
                        tag: tag.clone(),
                        ty: ty.clone(),
                    });
                }
                if !key.is_empty() && key != "_" {
                    let field_type = if names.peek().is_some() {
                        ty.clone()
                    } else {
                        ty.take()
                    };
                    let symbol = Symbol::new(range(name), SymbolKind::Value(field_type), true);
                    inline.members.entry(key.to_owned()).or_insert(symbol);
                }
            }
        } else if let (Some(mut ty), Some(syntax)) = (ty, field_type.and_then(named_type_syntax)) {
            // The optional star belongs to the embedded field, not its named type child.
            if field
                .children(&mut field.walk())
                .any(|child| child.kind() == "*")
            {
                ty.pointers += 1;
            }
            let name = syntax.name;
            if retain_fields {
                inline.fields.push(InlineField {
                    name: node_text(name, text).to_owned(),
                    embedded: true,
                    tag,
                    ty: Some(ty.clone()),
                });
            }
            let symbol = Symbol::new(range(name), SymbolKind::Value(Some(ty.clone())), true);
            inline
                .members
                .entry(node_text(name, text).to_owned())
                .or_insert(symbol);
            inline.embedded.push(ty);
        }
    }
    inline
}

struct NamedTypeSyntax<'tree> {
    qualifier: Option<Node<'tree>>,
    name: Node<'tree>,
    arguments: Option<Node<'tree>>,
}

fn named_type_syntax(mut node: Node<'_>) -> Option<NamedTypeSyntax<'_>> {
    let mut arguments = None;
    loop {
        node = match node.kind() {
            "type_identifier" | "identifier" => {
                return Some(NamedTypeSyntax {
                    qualifier: None,
                    name: node,
                    arguments,
                });
            }
            "selector_expression" => {
                let qualifier = node.child_by_field_name("operand")?;
                if qualifier.kind() != "identifier" {
                    return None;
                }
                return Some(NamedTypeSyntax {
                    qualifier: Some(qualifier),
                    name: node.child_by_field_name("field")?,
                    arguments,
                });
            }
            "qualified_type" => {
                return Some(NamedTypeSyntax {
                    qualifier: Some(node.child_by_field_name("package")?),
                    name: node.child_by_field_name("name")?,
                    arguments,
                });
            }
            "parenthesized_type" => only_named_child(node)?,
            "generic_type" => {
                arguments = Some(node.child_by_field_name("type_arguments")?);
                node.child_by_field_name("type")?
            }
            _ => return None,
        };
    }
}

fn type_parameters(
    declaration: Node<'_>,
    text: &str,
    range: &dyn Fn(Node<'_>) -> Range,
) -> Vec<GenericParameter> {
    let mut parameters = Vec::new();
    if let Some(list) = declaration.child_by_field_name("type_parameters") {
        let mut cursor = list.walk();
        for parameter in list.named_children(&mut cursor) {
            let constraint = parameter
                .child_by_field_name("type")
                .map(|node| type_constraint(node, text, range))
                .unwrap_or_default();
            let mut cursor = parameter.walk();
            let names = parameter
                .children_by_field_name("name", &mut cursor)
                .filter(|node| node.kind() == "identifier");
            parameters.extend(names.map(|name| GenericParameter {
                name: node_text(name, text).to_owned(),
                name_range: range(name),
                constraint: constraint.clone(),
            }));
        }
    }
    parameters
}

pub(crate) fn type_constraint(
    constraint: Node<'_>,
    text: &str,
    range: &dyn Fn(Node<'_>) -> Range,
) -> TypeConstraint {
    let Some(ty) = only_named_child(constraint) else {
        return TypeConstraint {
            terms: constraint_terms(constraint, text, range),
            ..TypeConstraint::default()
        };
    };
    let methods = match ty.kind() {
        "interface_type" | "qualified_type" | "generic_type" => type_ref(ty, text, range),
        "type_identifier" if !matches!(node_text(ty, text), "any" | "comparable") => {
            type_ref(ty, text, range)
        }
        _ => None,
    };
    let (term, terms) = if ty.kind() == "interface_type" {
        let mut cursor = ty.walk();
        let mut terms = ty
            .named_children(&mut cursor)
            .filter(|node| node.kind() == "type_elem");
        let single = terms.next().filter(|_| terms.next().is_none());
        (
            single.and_then(only_named_child),
            single.map_or_else(Vec::new, |term| constraint_terms(term, text, range)),
        )
    } else {
        (Some(ty), constraint_terms(constraint, text, range))
    };
    let approximation = term
        .filter(|term| term.kind() == "negated_type")
        .and_then(only_named_child)
        .and_then(|term| type_ref(term, text, range));
    TypeConstraint {
        methods,
        approximation,
        terms,
    }
}

fn constraint_terms(
    element: Node<'_>,
    text: &str,
    range: &dyn Fn(Node<'_>) -> Range,
) -> Vec<ConstraintTerm> {
    element
        .named_children(&mut element.walk())
        .filter(|term| !term.is_extra())
        .map(|term| {
            let approximate = term.kind() == "negated_type";
            let ty = if approximate {
                only_named_child(term)?
            } else {
                term
            };
            Some(ConstraintTerm {
                reference: type_ref(ty, text, range)?,
                approximate,
            })
        })
        .collect::<Option<_>>()
        .unwrap_or_default()
}

fn unparenthesized_type(mut node: Node<'_>) -> Option<Node<'_>> {
    while node.kind() == "parenthesized_type" {
        node = only_named_child(node)?;
    }
    Some(node)
}

fn only_named_child(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    let mut children = node
        .named_children(&mut cursor)
        .filter(|node| !node.is_extra());
    let child = children.next()?;
    children.next().is_none().then_some(child)
}

fn composite_value_type(
    mut value: Node<'_>,
    text: &str,
    range: &dyn Fn(Node<'_>) -> Range,
) -> Option<TypeRef> {
    let mut pointers = 0_u8;
    loop {
        value = match value.kind() {
            "composite_literal" => {
                let mut ty = type_ref(value.child_by_field_name("type")?, text, range)?;
                ty.pointers = ty.pointers.checked_add(pointers)?;
                return Some(ty);
            }
            "parenthesized_expression" => only_named_child(value)?,
            "unary_expression"
                if value
                    .child_by_field_name("operator")
                    .is_some_and(|op| op.kind() == "&") =>
            {
                pointers = pointers.checked_add(1)?;
                value.child_by_field_name("operand")?
            }
            _ => return None,
        };
    }
}

fn function_signature(
    function: Node<'_>,
    text: &str,
    range: &dyn Fn(Node<'_>) -> Range,
) -> Arc<FunctionSignature> {
    let type_parameters = type_parameters(function, text, range);
    let parameters = function.child_by_field_name("parameters");
    let parameters_known = parameters.is_some_and(|list| !list.has_error());
    let variadic = parameters.is_some_and(|list| {
        list.named_children(&mut list.walk())
            .any(|node| node.kind() == "variadic_parameter_declaration")
    });
    let parameters = parameters.map_or_else(Vec::new, |list| {
        parameter_types(list, |node| type_ref(node, text, range))
    });
    let origin = if matches!(function.kind(), "function_type" | "func_literal") {
        FunctionOrigin::FuncKeyword(range(function.child(0).unwrap_or(function)))
    } else {
        FunctionOrigin::Name(range(
            function.child_by_field_name("name").unwrap_or(function),
        ))
    };
    let result_tuple = function
        .child_by_field_name("result")
        .filter(|node| node.kind() == "parameter_list")
        .and_then(|node| node.child(0))
        .map(range);
    Arc::new(FunctionSignature {
        origin,
        result_tuple,
        results: function_results(function, text, range),
        parameters,
        parameters_known,
        variadic,
        type_parameters,
    })
}

fn function_results(
    function: Node<'_>,
    text: &str,
    range: &dyn Fn(Node<'_>) -> Range,
) -> Arc<[Option<TypeRef>]> {
    let Some(result) = function.child_by_field_name("result") else {
        return Arc::from([]);
    };
    // Go's named functions and methods are declared at package scope.
    let local = !matches!(
        function.kind(),
        "function_declaration" | "method_declaration"
    ) && std::iter::successors(function.parent(), |node| node.parent())
        .any(|owner| owner.kind() == "block");
    let parse = |node| {
        if local && has_shadowed_result_type(node, text) {
            return None;
        }
        type_ref(node, text, range)
    };
    if result.kind() != "parameter_list" {
        return Arc::from([parse(result)]);
    }
    parameter_types(result, parse).into()
}

pub(crate) fn parameter_types<'tree>(
    list: Node<'tree>,
    parse: impl Fn(Node<'tree>) -> Option<TypeRef>,
) -> Vec<Option<TypeRef>> {
    let mut result = Vec::new();
    let mut cursor = list.walk();
    for parameter in list.named_children(&mut cursor).filter(|node| {
        matches!(
            node.kind(),
            "parameter_declaration" | "variadic_parameter_declaration"
        )
    }) {
        let ty = parameter.child_by_field_name("type").and_then(&parse);
        let mut name_cursor = parameter.walk();
        let count = parameter
            .children_by_field_name("name", &mut name_cursor)
            .filter(|node| node.kind() == "identifier")
            .count()
            .max(1);
        result.extend(std::iter::repeat_n(ty, count));
    }
    result
}

// Package-indexed result types cannot stand in for similarly named local declarations.
fn has_shadowed_result_type(ty: Node<'_>, text: &str) -> bool {
    match ty.kind() {
        "qualified_type" => false,
        "type_identifier" => {
            lexical_binding(ty, node_text(ty, text), text).is_some_and(|binding| {
                binding
                    .name
                    .parent()
                    .is_some_and(|owner| owner.kind() != "type_parameter_declaration")
            })
        }
        _ => ty
            .named_children(&mut ty.walk())
            .any(|child| has_shadowed_result_type(child, text)),
    }
}

fn node_text<'text>(node: Node<'_>, text: &'text str) -> &'text str {
    &text[node.byte_range()]
}

#[cfg(test)]
mod outline_tests {
    use super::*;
    use OutlineKind::*;

    fn parse(text: &str) -> Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
            .unwrap();
        parser.parse(text, None).unwrap()
    }

    #[test]
    fn enumerates_only_indexed_package_declarations() {
        let text = concat!(
            "package p\nimport \"fmt\"\n",
            "type Record struct{ Field int }\n",
            "var Variable Record\nconst Constant = 1\nvar _ = fmt.Print\n",
            "func Function() { var local int }\n",
            "func (Record) Method() {}\n",
        );
        let symbols = collect(
            &parse(text),
            text,
            |node| {
                let position = |point: tree_sitter::Point| {
                    lsp_types::Position::new(point.row as u32, point.column as u32)
                };
                Range::new(
                    position(node.start_position()),
                    position(node.end_position()),
                )
            },
            |_, _, _| {},
        );
        let mut declarations: Vec<_> = symbols.declarations().collect();
        declarations.sort_unstable_by_key(|(name, _)| *name);
        assert_eq!(
            declarations
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>(),
            ["Constant", "Function", "Record", "Variable"]
        );
        for (name, symbol) in declarations {
            assert!(std::ptr::eq(symbol, symbols.declaration(name).unwrap()));
            let line = text.lines().nth(symbol.range.start.line as usize).unwrap();
            assert_eq!(
                &line[symbol.range.start.character as usize..symbol.range.end.character as usize],
                name
            );
        }
    }

    fn labels(symbols: &[OutlineSymbol]) -> Vec<(&str, OutlineKind)> {
        symbols
            .iter()
            .map(|symbol| (symbol.name.as_str(), symbol.kind))
            .collect()
    }

    fn assert_ranges(text: &str, symbols: &[OutlineSymbol], parent: ops::Range<usize>) {
        for symbol in symbols {
            let range = &symbol.range;
            let selection = &symbol.selection_range;
            assert!(
                parent.start <= range.start && range.end <= parent.end,
                "{}",
                symbol.name
            );
            assert!(
                range.start <= selection.start && selection.end <= range.end,
                "{}",
                symbol.name
            );
            assert_eq!(&text[selection.clone()], symbol.name);
            assert_ranges(text, symbol.children(), range.clone());
        }
    }

    #[test]
    fn retains_value_specs_and_const_initializers_in_source_order() {
        let text = concat!(
            "package p\nimport ignored \"fmt\"\n",
            "const (\nFirst, Second uint = iota, iota + 3\nThird, Fourth\nFifth = iota\nSixth\n)\n",
            "var (_, AfterBlank = 0, 1\nLeft, Right = pair()\nLabel string\n)\n",
            "var Single = 3\nvar _ = 9\n",
        );
        let outline = collect_outline(&parse(text), text);
        assert_eq!(outline.package.as_deref(), Some("p"));
        assert_eq!(
            labels(&outline.symbols),
            [
                ("First", Constant),
                ("Second", Constant),
                ("Third", Constant),
                ("Fourth", Constant),
                ("Fifth", Constant),
                ("Sixth", Constant),
                ("AfterBlank", Variable),
                ("Left", Variable),
                ("Right", Variable),
                ("Label", Variable),
                ("Single", Variable),
            ]
        );
        assert_eq!(
            outline
                .symbols
                .iter()
                .map(|symbol| symbol.detail.as_deref())
                .collect::<Vec<_>>(),
            [
                Some("uint = iota"),
                Some("uint = iota + 3"),
                Some("uint = iota"),
                Some("uint = iota + 3"),
                Some("= iota"),
                Some("= iota"),
                Some("= 1"),
                Some("= pair()"),
                Some("= pair()"),
                Some("string"),
                Some("= 3"),
            ]
        );
        assert_eq!(outline.symbols[0].range, outline.symbols[1].range);
        assert_eq!(&text[outline.symbols[2].range.clone()], "Third, Fourth");
        assert_eq!(&text[outline.symbols[10].range.clone()], "var Single = 3");
        assert_ranges(text, &outline.symbols, 0..text.len());
    }

    #[test]
    fn describes_types_fields_constraints_and_receiver_signatures() {
        let text = concat!(
            "package p\n",
            "type (\nCount int\nAlias = io.Reader\nEmpty = struct{ Field string }\n)\n",
            "type Box[T any] struct {\n",
            "Left, Right int `json:\"coordinate\"`\n*pkg.T\nEmbed[int]\nPlain\n",
            "Inner, Also struct { Nested int }\n",
            "Pointer *(struct { Value int })\n",
            "Port interface { Open(string) error; io.Reader; Embedded[T]; Plain; ~int; A | B }\n",
            "_ int\n}\n",
            "type Reader interface { Read([]byte) (int, error) }\n",
            "func Make[T any](arg T, rest ...T) (T, error) { var Local int; _ = Local; return arg, nil }\n",
            "func (r *Box[T]) Start(n int) error { return nil }\n",
            "func (Box[T]) Stop() {}\nfunc init() {}\nfunc init() {}\n",
        );
        let tree = parse(text);
        assert!(
            !tree.root_node().has_error(),
            "{}",
            tree.root_node().to_sexp()
        );
        let outline = collect_outline(&tree, text);
        let symbols = &outline.symbols;
        assert_eq!(
            labels(symbols),
            [
                ("Count", Class),
                ("Alias", Class),
                ("Empty", Class),
                ("Box", Struct),
                ("Reader", Interface),
                ("Make", Function),
                ("Start", Method),
                ("Stop", Method),
                ("init", Function),
                ("init", Function),
            ]
        );
        assert_eq!(symbols[0].detail.as_deref(), Some("int"));
        assert_eq!(symbols[1].detail.as_deref(), Some("= io.Reader"));
        assert_eq!(labels(symbols[2].children()), [("Field", Field)]);
        let fields = symbols[3].children();
        assert_eq!(
            labels(fields),
            [
                ("Left", Field),
                ("Right", Field),
                ("T", Field),
                ("Embed", Field),
                ("Plain", Field),
                ("Inner", Field),
                ("Also", Field),
                ("Pointer", Field),
                ("Port", Field),
            ]
        );
        assert_eq!(
            fields[0].detail.as_deref(),
            Some("int `json:\"coordinate\"`")
        );
        assert_eq!(fields[2].detail.as_deref(), Some("*pkg.T"));
        assert_eq!(fields[3].detail.as_deref(), Some("Embed[int]"));
        assert_eq!(&text[fields[4].range.clone()], "Plain\n");
        assert_eq!(&text[fields[4].selection_range.clone()], "Plain");
        assert_eq!(labels(fields[5].children()), [("Nested", Field)]);
        assert_eq!(labels(fields[6].children()), [("Nested", Field)]);
        assert_eq!(fields[7].detail.as_deref(), Some("*struct"));
        assert_eq!(labels(fields[7].children()), [("Value", Field)]);
        let methods = fields[8].children();
        assert_eq!(
            labels(methods),
            [
                ("Open", Method),
                ("Reader", EmbeddedInterface),
                ("Embedded", EmbeddedInterface),
                ("Plain", EmbeddedInterface)
            ]
        );
        assert_eq!(methods[0].detail.as_deref(), Some("(string) error"));
        assert_eq!(methods[1].detail.as_deref(), Some("io.Reader"));
        assert_eq!(methods[2].detail.as_deref(), Some("Embedded[T]"));
        assert_eq!(&text[methods[3].range.clone()], "Plain;");
        assert_eq!(&text[methods[3].selection_range.clone()], "Plain");
        assert_eq!(
            symbols[5].detail.as_deref(),
            Some("[T any](arg T, rest ...T) (T, error)")
        );
        assert_eq!(symbols[6].detail.as_deref(), Some("(n int) error"));
        for (method, spelling) in [(&symbols[6], "*Box[T]"), (&symbols[7], "Box[T]")] {
            let receiver = method.receiver.as_ref().unwrap();
            assert_eq!(receiver.base, "Box");
            assert_eq!(receiver.spelling, spelling);
        }
        assert_ranges(text, symbols, 0..text.len());
    }

    #[test]
    fn keeps_recognized_declarations_in_broken_files() {
        for tail in [
            "func Broken(\n",
            "func Broken() {\nvar Local int\n",
            "type Broken struct {\nField int\n",
            "var Broken = @\n",
        ] {
            let text = format!("package p\nvar Before int\n@\nfunc AfterError() {{}}\n{tail}");
            let tree = parse(&text);
            assert!(tree.root_node().has_error());
            let outline = collect_outline(&tree, &text);
            let labels = labels(&outline.symbols);
            assert!(
                labels.contains(&("Before", Variable)),
                "{}",
                tree.root_node().to_sexp()
            );
            assert!(
                labels.contains(&("AfterError", Function)),
                "{}",
                tree.root_node().to_sexp()
            );
            assert!(
                !labels.contains(&("Local", Variable)),
                "{}",
                tree.root_node().to_sexp()
            );
            assert_ranges(&text, &outline.symbols, 0..text.len());
        }
    }

    #[test]
    fn retains_later_declarations_when_parser_resynchronizes() {
        for broken in ["func Broken(\n", "type Broken struct {\nField int\n"] {
            let text = format!("package p\n{broken}func AfterError() {{}}\nvar Tail int\n");
            let tree = parse(&text);
            assert!(tree.root_node().has_error());
            let outline = collect_outline(&tree, &text);
            let labels = labels(&outline.symbols);
            assert!(
                labels.contains(&("Tail", Variable)),
                "{}: {labels:?}",
                tree.root_node().to_sexp()
            );
            assert_ranges(&text, &outline.symbols, 0..text.len());
        }
    }

    #[test]
    fn distinguishes_an_empty_parameter_list_from_a_recovered_signature() {
        let text = "package p\nfunc Getter() int { return 1 }\nfunc Broken(a int, @) {}\n";
        let tree = parse(text);
        let root = tree.root_node();
        let mut cursor = root.walk();
        let mut functions = root
            .named_children(&mut cursor)
            .filter(|node| node.kind() == "function_declaration");
        let getter = function_signature(functions.next().unwrap(), text, &|_| Range::default());
        assert!(getter.parameters_known && getter.parameters.is_empty());
        let broken = functions
            .next()
            .unwrap_or_else(|| panic!("{}", root.to_sexp()));
        assert!(!function_signature(broken, text, &|_| Range::default()).parameters_known);
    }
}
