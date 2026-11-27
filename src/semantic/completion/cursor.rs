use std::ops::Range;
use std::sync::LazyLock;

use anyhow::Result;
use lsp_types::Position;
use regex::Regex;
use tree_sitter::{InputEdit, Node, Parser};

use crate::semantic::{
    snapshot::DocumentSnapshot,
    source::{Source, node_text},
};

use super::Trace;

const PLACEHOLDER: &str = "__rs_gopls_completion__";

#[derive(Clone)]
pub(super) struct Edit {
    pub(super) replace: Range<usize>,
    pub(super) offset: usize,
}

impl Edit {
    pub(super) fn prefix<'a>(&self, text: &'a str) -> &'a str {
        &text[self.replace.start..self.offset]
    }
}

pub(super) struct Probe {
    pub(super) edit: Edit,
    syntax: Option<Source>,
    early: Option<Early>,
}

enum Early {
    Suppressed,
    PackageClause,
    ImportPath(Option<char>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum LabelKind {
    Break,
    Continue,
    Goto,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum LiteralMode {
    KeysOnly,
    ValuesOnly,
    KeysAndValues,
}

#[derive(Default)]
pub(super) struct Expression {
    pub(super) is_stmt_start: bool,
    pub(super) is_const_spec: bool,
    pub(super) is_for_range_pos: bool,
    pub(super) is_case_value: bool,
    pub(super) is_address_of: bool,
}

pub(super) enum Site<'a> {
    Suppressed,
    PackageClause,
    ImportPath {
        close: Option<char>,
    },
    Label(LabelKind),
    CaseClause,
    Selector {
        receiver: Node<'a>,
        types_only: bool,
        type_switch_guard: bool,
        is_address_of: bool,
    },
    StructLiteral {
        body: Node<'a>,
        mode: LiteralMode,
    },
    TypePosition {
        is_receiver_type: bool,
        is_type_arg: bool,
        type_switch_guard: bool,
    },
    Expression(Expression),
    FileTopLevel,
}

impl Probe {
    pub(super) fn new(
        document: &DocumentSnapshot<'_>,
        parser: &mut Parser,
        position: Position,
        trace: Option<&Trace>,
    ) -> Result<Option<Self>> {
        let Some(offset) = document.offset(position) else {
            return Ok(None);
        };
        let text = document.text();
        let root = document.root_node();
        let replace = word_range(text, offset);
        let early = literal(root, text, offset)
            .map(|literal| match literal {
                Literal::Suppress => (Early::Suppressed, replace.clone()),
                Literal::Import { inner, close } => (Early::ImportPath(close), inner),
            })
            .or_else(|| {
                package_clause(root, text, replace.start)
                    .then(|| (Early::PackageClause, replace.clone()))
            });
        if let Some((early, replace)) = early {
            return Ok(Some(Self {
                edit: Edit { replace, offset },
                syntax: None,
                early: Some(early),
            }));
        }
        let after_dot = previous_nonspace(text, replace.start)
            .is_some_and(|(byte, ch)| ch == '.' && !text[byte + 1..replace.start].contains('\n'));
        let token = leaf(root, replace.start);
        let usable = !replace.is_empty()
            && token.byte_range() == replace
            && ancestors(token).all(|node| !node.is_error());
        let syntax = if usable {
            None
        } else {
            repair(
                document.indexed(),
                parser,
                replace.clone(),
                after_dot,
                trace,
            )?
        };
        Ok(Some(Self {
            edit: Edit { replace, offset },
            syntax,
            early: None,
        }))
    }

    pub(super) fn source<'a>(&'a self, document: &'a DocumentSnapshot<'_>) -> &'a Source {
        self.syntax.as_ref().unwrap_or(document.indexed())
    }

    pub(super) fn anchor<'a>(&self, source: &'a Source) -> Node<'a> {
        leaf(source.tree.root_node(), self.edit.replace.start)
    }

    pub(super) fn site<'a>(&self, source: &'a Source, original: &str) -> Site<'a> {
        match self.early {
            Some(Early::Suppressed) => return Site::Suppressed,
            Some(Early::PackageClause) => return Site::PackageClause,
            Some(Early::ImportPath(close)) => return Site::ImportPath { close },
            None => {}
        }
        let anchor = self.anchor(source);
        let left = &original[..self.edit.replace.start];
        let line_start = left.rfind('\n').map_or(0, |byte| byte + 1);
        let line_left = left[line_start..].trim_end();
        if let Some(kind) = label_kind(anchor, line_left) {
            return Site::Label(kind);
        }
        let guard = switch_header(anchor, line_left);
        let address_of = ancestors(anchor).any(|node| {
            node.kind() == "unary_expression"
                && node
                    .child_by_field_name("operator")
                    .is_some_and(|op| op.kind() == "&")
                && node
                    .child_by_field_name("operand")
                    .is_some_and(|operand| operand.end_byte() == anchor.end_byte())
        }) || line_left.ends_with('&');
        if let Some((selector, receiver, is_type)) = selector(anchor) {
            let has_assertion_parens = original[self.edit.replace.end..]
                .trim_start_matches([' ', '\t'])
                .starts_with('(');
            return Site::Selector {
                receiver,
                types_only: is_type,
                type_switch_guard: guard && !is_type && !has_assertion_parens,
                is_address_of: address_of
                    || selector.parent().is_some_and(|node| {
                        node.kind() == "unary_expression"
                            && node
                                .child_by_field_name("operator")
                                .is_some_and(|op| op.kind() == "&")
                    }),
            };
        }
        if for_range_position(anchor, line_left) {
            return Site::Expression(Expression {
                is_for_range_pos: true,
                ..Expression::default()
            });
        }
        if switch_case_position(anchor) {
            return Site::CaseClause;
        }
        if let Some(site) = type_position(anchor, line_left, guard, &source.text) {
            return site;
        }
        if let Some((body, mode)) = literal_site(anchor, &source.text) {
            return Site::StructLiteral { body, mode };
        }
        if is_file_top_level(anchor) {
            return Site::FileTopLevel;
        }
        let case_body = |node: Node<'_>| {
            matches!(
                node.kind(),
                "expression_case" | "type_case" | "communication_case"
            ) && !has_colon_before(node, anchor.start_byte())
        };
        Site::Expression(Expression {
            is_stmt_start: statement_start(anchor, line_left),
            is_const_spec: ancestors(anchor).any(|node| node.kind() == "const_spec")
                || last_statement(line_left).starts_with("const "),
            is_case_value: ancestors(anchor).any(case_body),
            is_address_of: address_of,
            ..Expression::default()
        })
    }
}

pub(super) fn ancestors(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    std::iter::successors(Some(node), |node| node.parent())
}

fn leaf(root: Node<'_>, offset: usize) -> Node<'_> {
    let end = (offset + 1).min(root.end_byte());
    root.descendant_for_byte_range(offset, end).unwrap_or(root)
}

pub(super) fn identifier_char(ch: char) -> bool {
    static LETTER_OR_DIGIT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^[\p{L}\p{Nd}]$").expect("Go identifier character"));
    ch.is_ascii_alphanumeric()
        || ch == '_'
        || (!ch.is_ascii() && LETTER_OR_DIGIT.is_match(ch.encode_utf8(&mut [0; 4])))
}

pub(super) fn identifier_start(ch: char) -> bool {
    static LETTER: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^\p{L}$").expect("Go identifier start"));
    ch.is_ascii_alphabetic()
        || ch == '_'
        || (!ch.is_ascii() && LETTER.is_match(ch.encode_utf8(&mut [0; 4])))
}

pub(super) fn is_keyword(name: &str) -> bool {
    matches!(
        name,
        "break"
            | "default"
            | "func"
            | "interface"
            | "select"
            | "case"
            | "defer"
            | "go"
            | "map"
            | "struct"
            | "chan"
            | "else"
            | "goto"
            | "package"
            | "switch"
            | "const"
            | "fallthrough"
            | "if"
            | "range"
            | "type"
            | "continue"
            | "for"
            | "import"
            | "return"
            | "var"
    )
}

fn word_range(text: &str, offset: usize) -> Range<usize> {
    let start = text[..offset].trim_end_matches(identifier_char).len();
    let end = offset
        + text[offset..]
            .find(|ch| !identifier_char(ch))
            .unwrap_or(text.len() - offset);
    start..end
}

fn previous_nonspace(text: &str, at: usize) -> Option<(usize, char)> {
    text[..at].trim_end().char_indices().next_back()
}

fn package_clause(root: Node<'_>, text: &str, start: usize) -> bool {
    let Some((byte, _)) = previous_nonspace(text, start) else {
        return false;
    };
    let keyword = leaf(root, byte);
    keyword.kind() == "package"
        && !text[keyword.end_byte()..start].contains('\n')
        && ancestors(keyword)
            .skip(1)
            .all(|node| matches!(node.kind(), "package_clause" | "source_file" | "ERROR"))
}

enum Literal {
    Suppress,
    Import {
        inner: Range<usize>,
        close: Option<char>,
    },
}

fn literal(root: Node<'_>, text: &str, offset: usize) -> Option<Literal> {
    for node in [leaf(root, offset.saturating_sub(1)), leaf(root, offset)] {
        for token in ancestors(node) {
            if offset <= token.start_byte() || offset > token.end_byte() {
                continue;
            }
            let value = node_text(token, text);
            match token.kind() {
                "comment" if offset < token.end_byte() || value.starts_with("//") => {
                    return Some(Literal::Suppress);
                }
                "int_literal" | "float_literal" | "imaginary_literal" => {
                    return Some(Literal::Suppress);
                }
                "interpreted_string_literal" | "raw_string_literal" | "rune_literal"
                    if offset < token.end_byte() =>
                {
                    let inner = token.start_byte() + 1..token.end_byte() - 1;
                    let import = token
                        .parent()
                        .is_some_and(|node| node.kind() == "import_spec");
                    if !import {
                        return Some(Literal::Suppress);
                    }
                    // Recovery can synthesize a quote, including at the end
                    // of a line in the middle of a valid import block.
                    if !token.has_error() && !text[inner.clone()].contains(['\n', '\r']) {
                        return Some(Literal::Import { inner, close: None });
                    }
                    return broken_literal(text, offset);
                }
                _ => {}
            }
        }
    }
    root.has_error()
        .then(|| broken_literal(text, offset))
        .flatten()
}

/// The parser can leave an unfinished quote or block comment as unrelated
/// tokens inside ERROR. In that case, find the lexical delimiter and import
/// group directly in the original buffer.
fn broken_literal(text: &str, offset: usize) -> Option<Literal> {
    let bytes = text.as_bytes();
    let mut at = 0;
    let mut braces = 0usize;
    let mut in_import = false;
    let mut import_group = false;
    while at < offset {
        match bytes[at] {
            b'/' if bytes.get(at + 1) == Some(&b'/') => {
                let end = text[at..].find('\n').map_or(bytes.len(), |end| at + end);
                if offset <= end {
                    return Some(Literal::Suppress);
                }
                at = end;
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                let Some(end) = text[at + 2..].find("*/").map(|end| at + 4 + end) else {
                    return Some(Literal::Suppress);
                };
                if offset < end {
                    return Some(Literal::Suppress);
                }
                at = end;
            }
            quote @ (b'"' | b'\'' | b'`') => {
                let start = at + 1;
                at = start;
                while at < bytes.len() && bytes[at] != quote {
                    match bytes[at] {
                        b'\n' | b'\r' if quote != b'`' => break,
                        b'\\'
                            if quote != b'`'
                                && bytes
                                    .get(at + 1)
                                    .is_some_and(|b| !matches!(b, b'\n' | b'\r')) =>
                        {
                            at += 2;
                        }
                        _ => at += 1,
                    }
                }
                let closed = bytes.get(at) == Some(&quote);
                if offset <= at {
                    return Some(
                        if in_import && quote != b'\'' && !text[start..at].contains(['\n', '\r']) {
                            Literal::Import {
                                inner: start..at,
                                close: (!closed).then_some(quote as char),
                            }
                        } else {
                            Literal::Suppress
                        },
                    );
                }
                at += usize::from(closed);
                in_import = import_group;
            }
            b'(' if in_import => {
                import_group = true;
                at += 1;
            }
            b')' if import_group => {
                import_group = false;
                in_import = false;
                at += 1;
            }
            b'{' | b'}' | b';' => {
                braces = match bytes[at] {
                    b'{' => braces + 1,
                    b'}' => braces.saturating_sub(1),
                    _ => braces,
                };
                in_import = import_group;
                at += 1;
            }
            b if b.is_ascii_alphabetic() || b == b'_' || !b.is_ascii() => {
                let end = text[at..].find(|ch| !identifier_char(ch));
                let length = end.unwrap_or(text.len() - at);
                if length == 0 {
                    at += text[at..].chars().next()?.len_utf8();
                    continue;
                }
                if braces == 0 && &text[at..at + length] == "import" {
                    in_import = true;
                }
                at += length;
            }
            _ => at += 1,
        }
    }
    None
}

fn repair(
    source: &Source,
    parser: &mut Parser,
    replace: Range<usize>,
    after_dot: bool,
    trace: Option<&Trace>,
) -> Result<Option<Source>> {
    let follow = &source.text[replace.end..];
    // A newline after a complete identifier terminates the statement. The
    // placeholder prevents the next line's identifier from becoming a selector.
    let separator = if after_dot
        && follow
            .trim_start_matches([' ', '\t', '\r'])
            .starts_with('\n')
        && follow
            .trim_start()
            .chars()
            .next()
            .is_some_and(identifier_start)
    {
        ";"
    } else {
        ""
    };
    let mut text = source.text.as_ref().clone();
    let marker = format!("{PLACEHOLDER}{separator}");
    text.replace_range(replace.clone(), &marker);
    let mut tree = source.tree.clone();
    let start = source.lines.point(replace.start);
    tree.edit(&InputEdit {
        start_byte: replace.start,
        old_end_byte: replace.end,
        new_end_byte: replace.start + marker.len(),
        start_position: start,
        old_end_position: source.lines.point(replace.end),
        new_end_position: tree_sitter::Point::new(start.row, start.column + marker.len()),
    });
    let mut repaired = Source::parse(parser, source.path.clone(), text, Some(&tree))?;
    let anchor = leaf(repaired.tree.root_node(), replace.start);
    if follow
        .trim_start_matches([' ', '\t', '\r'])
        .starts_with('\n')
        && let Some((brace, '{')) = previous_nonspace(&source.text, replace.start)
        && starts_assignment(follow.trim_start())
        && ancestors(anchor)
            .any(|node| node.kind() == "literal_value" && node.start_byte() == brace)
    {
        // An unclosed literal can absorb the next assignment as part of
        // another literal whose apparent type is the completion marker.
        let mut text = std::sync::Arc::unwrap_or_clone(repaired.text);
        text.insert(replace.start + marker.len(), '}');
        if let Some(trace) = trace {
            trace.parse();
        }
        repaired = Source::parse(parser, source.path.clone(), text, None)?;
    }
    let root = repaired.tree.root_node();
    if root.has_error() && ancestors(leaf(root, replace.start)).any(|node| node.is_error()) {
        let closers = missing_closers(root);
        if !closers.is_empty() {
            let mut text = std::sync::Arc::unwrap_or_clone(repaired.text);
            if root
                .named_child(root.named_child_count().saturating_sub(1))
                .is_some_and(|node| node.kind() == "comment")
            {
                text.push('\n');
            }
            text.push_str(&closers);
            if let Some(trace) = trace {
                trace.parse();
            }
            repaired = Source::parse(parser, source.path.clone(), text, None)?;
        }
    }
    Ok(Some(repaired))
}

fn starts_assignment(text: &str) -> bool {
    let Some(first) = text.chars().next().filter(|ch| identifier_start(*ch)) else {
        return false;
    };
    let end = first.len_utf8()
        + text[first.len_utf8()..]
            .chars()
            .take_while(|ch| identifier_char(*ch))
            .map(char::len_utf8)
            .sum::<usize>();
    let operator = text[end..].trim_start_matches([' ', '\t']);
    operator.starts_with(":=")
        || operator
            .strip_prefix('=')
            .is_some_and(|rest| !rest.starts_with('='))
}

fn missing_closers(root: Node<'_>) -> String {
    let mut stack = Vec::new();
    let mut walk = root.walk();
    loop {
        let node = walk.node();
        if !node.is_missing() {
            match node.kind() {
                "(" => stack.push(')'),
                "[" => stack.push(']'),
                "{" => stack.push('}'),
                ")" | "]" | "}" if stack.last().is_some_and(|ch| node.kind().starts_with(*ch)) => {
                    stack.pop();
                }
                _ => {}
            }
        }
        if walk.goto_first_child() {
            continue;
        }
        while !walk.goto_next_sibling() {
            if !walk.goto_parent() {
                return stack.into_iter().rev().collect();
            }
        }
    }
}

fn last_statement(left: &str) -> &str {
    left.rsplit([';', '{', '}']).next().unwrap_or(left).trim()
}

fn label_kind(anchor: Node<'_>, left: &str) -> Option<LabelKind> {
    let keyword = if anchor.kind() == "label_name" {
        anchor.parent()?.child(0)?.kind()
    } else {
        last_statement(left)
            .split_whitespace()
            .next()
            .filter(|word| *word == last_statement(left))?
    };
    match keyword {
        "break" => Some(LabelKind::Break),
        "continue" => Some(LabelKind::Continue),
        "goto" => Some(LabelKind::Goto),
        _ => None,
    }
}

fn selector(anchor: Node<'_>) -> Option<(Node<'_>, Node<'_>, bool)> {
    let parent = anchor.parent()?;
    let (operand, field, is_type) = match parent.kind() {
        "selector_expression" => ("operand", "field", false),
        "qualified_type" => ("package", "name", true),
        _ => return None,
    };
    (parent.child_by_field_name(field)? == anchor)
        .then(|| Some((parent, parent.child_by_field_name(operand)?, is_type)))
        .flatten()
}

fn is_type_switch(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "expression_switch_statement" | "type_switch_statement"
    )
}

fn switch_header(anchor: Node<'_>, left: &str) -> bool {
    if ancestors(anchor).any(|node| {
        is_type_switch(node)
            && node
                .children(&mut node.walk())
                .find(|child| child.kind() == "{")
                .is_none_or(|body| anchor.end_byte() <= body.start_byte())
    }) {
        return true;
    }
    left.rsplit(['{', '}'])
        .next()
        .is_some_and(|header| header.split_whitespace().any(|word| word == "switch"))
}

fn type_position<'a>(anchor: Node<'a>, left: &str, guard: bool, text: &str) -> Option<Site<'a>> {
    let mut receiver = false;
    let mut is_type_arg = false;
    let mut detected = false;
    let mut type_switch_guard = false;
    for node in ancestors(anchor) {
        let Some(parent) = node.parent() else { break };
        if node.kind() == "type_identifier"
            && !parent.child_by_field_name("name").is_some_and(|name| {
                name == node && matches!(parent.kind(), "type_spec" | "type_alias")
            })
        {
            detected = true;
        }
        match parent.kind() {
            "type_arguments" | "type_instantiation_expression"
                if parent.child_by_field_name("type") != Some(node) =>
            {
                detected = true;
                is_type_arg = true;
            }
            "parameter_list" => {
                detected = true;
                receiver = parent
                    .parent()
                    .is_some_and(|owner| owner.child_by_field_name("receiver") == Some(parent));
                break;
            }
            "argument_list" => {
                if let Some(function) = parent
                    .parent()
                    .and_then(|call| call.child_by_field_name("function"))
                {
                    let first = parent
                        .named_children(&mut parent.walk())
                        .find(|arg| !arg.is_extra());
                    if first == Some(node) && matches!(node_text(function, text), "new" | "make") {
                        detected = true;
                    }
                }
                break;
            }
            "type_assertion_expression" if parent.child_by_field_name("type") == Some(node) => {
                detected = true;
                type_switch_guard = guard;
                break;
            }
            "type_case"
                if parent
                    .children_by_field_name("type", &mut parent.walk())
                    .any(|child| child == node) =>
            {
                detected = true;
                break;
            }
            "block" | "literal_value" | "statement_list" | "source_file" => break,
            _ => {}
        }
    }
    let statement = last_statement(left);
    if !detected && !statement.contains('=') {
        let mut words = statement.split_whitespace();
        let keyword = words.next().unwrap_or("");
        detected = matches!(keyword, "var" | "type" | "const") && words.next().is_some()
            || (keyword == "func" && statement.contains('(') && !statement.contains(')'));
    }
    if statement.starts_with("func (")
        && !statement.contains(')')
        && !ancestors(anchor).any(|node| node.kind() == "func_literal")
    {
        detected = true;
        receiver = true;
    }
    if guard && left.trim_end().ends_with(".(") {
        detected = true;
        type_switch_guard = true;
    }
    detected.then_some(Site::TypePosition {
        is_receiver_type: receiver,
        is_type_arg,
        type_switch_guard,
    })
}

fn literal_site<'a>(anchor: Node<'a>, text: &str) -> Option<(Node<'a>, LiteralMode)> {
    let body = ancestors(anchor)
        .take_while(|node| !matches!(node.kind(), "block" | "argument_list" | "type_arguments"))
        .find(|node| node.kind() == "literal_value")?;
    let active = body.named_children(&mut body.walk()).find(|node| {
        node.start_byte() <= anchor.start_byte() && anchor.end_byte() <= node.end_byte()
    });
    if let Some(element) = active {
        if element.kind() == "keyed_element" {
            let key = element.child_by_field_name("key")?;
            return Some((
                body,
                if key.start_byte() <= anchor.start_byte()
                    && anchor.end_byte() <= key.end_byte()
                    && key.named_child(0) == Some(anchor)
                {
                    LiteralMode::KeysOnly
                } else {
                    LiteralMode::ValuesOnly
                },
            ));
        }
        if element.kind() == "literal_element" && element.named_child(0) != Some(anchor) {
            return Some((body, LiteralMode::ValuesOnly));
        }
    }
    let mut keyed = false;
    let mut unkeyed = false;
    for element in body
        .named_children(&mut body.walk())
        .filter(|node| Some(*node) != active)
    {
        match element.kind() {
            "keyed_element" => keyed = true,
            "literal_element" if !node_text(element, text).is_empty() => unkeyed = true,
            _ => {}
        }
    }
    let mode = match (keyed, unkeyed) {
        (true, false) => LiteralMode::KeysOnly,
        (false, true) => LiteralMode::ValuesOnly,
        _ => LiteralMode::KeysAndValues,
    };
    Some((body, mode))
}

fn is_file_top_level(anchor: Node<'_>) -> bool {
    let mut saw_expression = false;
    for node in ancestors(anchor) {
        match node.kind() {
            "block"
            | "literal_value"
            | "field_declaration_list"
            | "parameter_list"
            | "expression_case"
            | "type_case"
            | "communication_case" => return false,
            "expression_list" | "var_spec" | "const_spec" | "assignment_statement" => {
                saw_expression = true;
            }
            "ERROR"
                if node
                    .children(&mut node.walk())
                    .any(|child| child.kind() == "{") =>
            {
                return false;
            }
            _ => {}
        }
    }
    !saw_expression
}

fn statement_start(anchor: Node<'_>, left: &str) -> bool {
    let parent = anchor.parent().map(|node| node.kind());
    parent == Some("expression_statement")
        || (left.trim().is_empty() || left.ends_with([';', '{', '}', ':']))
            && parent.is_some_and(|kind| matches!(kind, "block" | "statement_list" | "ERROR"))
}

fn for_range_position(anchor: Node<'_>, left: &str) -> bool {
    let statement = last_statement(left);
    statement == "for"
        || ancestors(anchor).any(|node| {
            node.kind() == "for_statement"
                && node
                    .child_by_field_name("body")
                    .is_none_or(|body| anchor.end_byte() <= body.start_byte())
                && (statement == "for" || statement.ends_with('='))
        })
        || (statement.starts_with("for ") && statement.ends_with('='))
}

fn has_colon_before(node: Node<'_>, at: usize) -> bool {
    node.children(&mut node.walk())
        .any(|child| child.kind() == ":" && child.start_byte() < at)
}

fn switch_case_position(anchor: Node<'_>) -> bool {
    ancestors(anchor)
        .skip(1)
        .find(|node| {
            matches!(
                node.kind(),
                "block" | "expression_case" | "communication_case" | "default_case" | "type_case"
            ) || is_type_switch(*node)
                || node.kind() == "select_statement"
        })
        .filter(|node| is_type_switch(*node) || node.kind() == "select_statement")
        .is_some_and(|node| {
            node.children(&mut node.walk())
                .any(|child| child.kind() == "{" && child.end_byte() <= anchor.start_byte())
        })
}

pub(super) fn is_function(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "function_declaration" | "method_declaration" | "func_literal"
    )
}
