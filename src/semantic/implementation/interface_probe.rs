use std::collections::{HashMap, HashSet};

use tree_sitter::Node;

use super::super::{
    presentation::syntax_text,
    source::{Source, decoded_import_path},
};

pub(super) type Arity = (usize, usize, bool);

#[derive(Default, Eq, PartialEq)]
pub(super) struct InterfaceSpellings {
    pub(super) receiver_methods: Vec<ReceiverMethod>,
    pub(super) interface_methods: Vec<ReceiverMethod>,
    pub(super) type_names: HashSet<String>,
    pub(super) uncertain_types: HashSet<String>,
    pub(super) type_references: Vec<TypeReference>,
    pub(super) has_local_types: bool,
    pub(super) default_imports: HashSet<String>,
    pub(super) alias_imports: HashMap<String, String>,
    pub(super) dot_imports: HashSet<String>,
    local_declarations: Vec<String>,
}

#[derive(Eq, PartialEq)]
pub(super) struct ReceiverMethod {
    pub(super) receiver: String,
    pub(super) name: String,
    pub(super) arity: Option<Arity>,
}

#[derive(Eq, PartialEq)]
pub(super) struct TypeReference {
    pub(super) owner: String,
    pub(super) qualifier: Option<String>,
    pub(super) target: String,
    pub(super) alias: bool,
}

impl InterfaceSpellings {
    pub(super) fn for_overlay(source: &Source) -> Option<Self> {
        let root = source.tree.root_node();
        if root.has_error() {
            return None;
        }
        let mut spellings = interface_spellings(&source.text)?;
        if spellings.has_local_types {
            // Postings only need the presence of local types. Comparing edits
            // also needs their spelling because declaration summaries omit bodies.
            spellings.local_declarations = local_type_declarations(root, &source.text);
        }
        Some(spellings)
    }
}

fn local_type_declarations(root: Node<'_>, text: &str) -> Vec<String> {
    let mut declarations = Vec::new();
    let mut cursor = root.walk();
    loop {
        let node = cursor.node();
        if node.kind() == "type_declaration" && node.parent() != Some(root) {
            declarations.push(syntax_text(node, text));
        }
        if cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return declarations;
            }
        }
    }
}

pub(super) fn interface_spellings(text: &str) -> Option<InterfaceSpellings> {
    scan(text).ok()
}

fn scan(text: &str) -> Result<InterfaceSpellings, ()> {
    let mut spellings = InterfaceSpellings::default();
    let mut lexer = Lexer {
        rest: text.strip_prefix('\u{feff}').unwrap_or(text),
    };
    let mut groups = Vec::new();
    let mut previous = None;
    while let Some(token) = lexer.next()? {
        match token.kind {
            Kind::Word("type") if groups.is_empty() => record_types(lexer, &mut spellings)?,
            Kind::Word("type")
                if lexer
                    .peek()?
                    .is_some_and(|next| matches!(next.kind, Kind::Word(_) | Kind::Byte(b'('))) =>
            {
                spellings.has_local_types = true;
            }
            Kind::Word("import") if groups.is_empty() => record_imports(lexer, &mut spellings)?,
            Kind::Word("func") if groups.is_empty() => {
                let starts_statement = previous.is_none_or(|kind| {
                    kind == Kind::Byte(b';') || token.newline && ends_statement(kind)
                });
                record_receiver(lexer, &mut spellings, starts_statement)?;
            }
            Kind::Byte(end @ (b')' | b']' | b'}')) => close_group(&mut groups, end)?,
            kind => {
                if let Some(end) = group_end(kind) {
                    groups.push(end);
                }
            }
        }
        previous = Some(token.kind);
    }
    if !groups.is_empty() {
        return Err(());
    }
    Ok(spellings)
}

fn close_group(groups: &mut Vec<u8>, end: u8) -> Result<(), ()> {
    (groups.pop() == Some(end)).then_some(()).ok_or(())
}

fn record_imports(mut lexer: Lexer<'_>, spellings: &mut InterfaceSpellings) -> Result<(), ()> {
    let grouped = lexer.consume(b'(')?;
    loop {
        let token = lexer.next()?.ok_or(())?;
        let alias = match token.kind {
            Kind::Byte(b')') if grouped => return Ok(()),
            Kind::Byte(b';') if grouped => continue,
            Kind::Byte(b'.') => Some("."),
            Kind::Word(name) if identifier(token.kind).is_some() => Some(name),
            _ => None,
        };
        let token = if alias.is_some() {
            let token = lexer.next()?.ok_or(())?;
            if token.newline && alias != Some(".") {
                return Err(());
            }
            token
        } else {
            token
        };
        let Kind::Literal(literal) = token.kind else {
            return Err(());
        };
        let path = decoded_import_path(literal).ok_or(())?;
        match alias {
            Some(".") => {
                spellings.dot_imports.insert(path);
            }
            Some("_") => {}
            Some(alias) => {
                if spellings
                    .alias_imports
                    .insert(alias.to_owned(), path)
                    .is_some()
                {
                    return Err(());
                }
            }
            None => {
                spellings.default_imports.insert(path);
            }
        }
        if !grouped {
            return Ok(());
        }
    }
}

fn record_receiver(
    mut lexer: Lexer<'_>,
    spellings: &mut InterfaceSpellings,
    starts_statement: bool,
) -> Result<(), ()> {
    if !lexer.consume(b'(')? {
        return Ok(());
    }
    let mut tail = lexer;
    let receiver = receiver_name(&mut tail)?.or_else(|| {
        // A receiver can be unnamed or preceded by one parameter name.
        tail = lexer;
        identifier(tail.next().ok()??.kind)?;
        receiver_name(&mut tail).ok()?
    });
    let Some(receiver) = receiver else {
        // A file-level initializer can also contain a function literal.
        return if starts_statement { Err(()) } else { Ok(()) };
    };
    let Some(Token {
        kind: Kind::Word(name),
        newline,
    }) = tail.next()?
    else {
        return if starts_statement { Err(()) } else { Ok(()) };
    };
    let arity = match tail.next()? {
        Some(Token {
            kind: Kind::Byte(b'('),
            newline: false,
        }) if !newline => method_shape(tail, b'{'),
        _ if starts_statement => None,
        _ => return Ok(()),
    };
    spellings.receiver_methods.push(ReceiverMethod {
        receiver: receiver.to_owned(),
        name: name.to_owned(),
        arity,
    });
    Ok(())
}

fn receiver_name<'a>(lexer: &mut Lexer<'a>) -> Result<Option<&'a str>, ()> {
    let Some(named) = named_type(lexer)? else {
        return Ok(None);
    };
    lexer.consume(b',')?;
    Ok((named.qualifier.is_none() && lexer.consume(b')')?).then_some(named.target))
}

fn record_types(mut lexer: Lexer<'_>, spellings: &mut InterfaceSpellings) -> Result<(), ()> {
    let grouped = lexer.consume(b'(')?;
    let end = grouped.then_some(b')');
    loop {
        let token = lexer.next()?.ok_or(())?;
        match token.kind {
            Kind::Byte(b')') if grouped => return Ok(()),
            Kind::Byte(b';') if grouped => continue,
            Kind::Word(owner) if identifier(token.kind).is_some() => {
                spellings.type_names.insert(owner.to_owned());
                record_type(lexer, owner, end, spellings)?;
            }
            _ => return Err(()),
        }
        if !grouped {
            return Ok(());
        }
        skip_element(&mut lexer, end)?;
    }
}

fn record_type(
    mut lexer: Lexer<'_>,
    owner: &str,
    end: Option<u8>,
    spellings: &mut InterfaceSpellings,
) -> Result<(), ()> {
    let parameters = type_parameters(&mut lexer)?;
    let alias = lexer.consume(b'=')?;
    let mut tail = lexer;
    if let Some(named) = named_type(&mut tail)? {
        if element_end(tail.peek()?, end) {
            record_reference(spellings, owner, named, parameters, alias);
        } else {
            spellings.uncertain_types.insert(owner.to_owned());
        }
        return Ok(());
    }

    type_prefix(&mut lexer)?;
    match lexer.next()?.map(|token| token.kind) {
        Some(Kind::Word(kind @ ("struct" | "interface"))) if lexer.consume(b'{')? => {
            record_fields(lexer, owner, parameters, kind == "interface", spellings)?;
        }
        Some(Kind::Byte(b'[' | b'<') | Kind::Word("map" | "chan" | "func")) => {}
        _ => {
            spellings.uncertain_types.insert(owner.to_owned());
        }
    }
    Ok(())
}

fn type_parameters<'a>(lexer: &mut Lexer<'a>) -> Result<Option<&'a str>, ()> {
    let mut tail = *lexer;
    if !tail.consume(b'[')? {
        return Ok(None);
    }
    let parameters = tail.rest;
    let first = tail.next()?.ok_or(())?;
    let Some(second) = tail.peek()? else {
        return Err(());
    };
    if identifier(first.kind).is_none()
        || !matches!(
            second.kind,
            Kind::Word(_) | Kind::Byte(b',' | b'*' | b'[' | b'(' | b'~' | b'<')
        )
    {
        // An array length such as [n] or [n+1] is part of the underlying type.
        return Ok(None);
    }
    let mut tail = *lexer;
    tail.next()?;
    skip_group(&mut tail, b']')?;
    *lexer = tail;
    Ok(Some(&parameters[..parameters.len() - tail.rest.len() - 1]))
}

fn record_fields(
    mut lexer: Lexer<'_>,
    owner: &str,
    parameters: Option<&str>,
    interface: bool,
    spellings: &mut InterfaceSpellings,
) -> Result<(), ()> {
    while let Some(token) = lexer.peek()? {
        match token.kind {
            Kind::Byte(b'}') => return Ok(()),
            Kind::Byte(b';') => {
                lexer.next()?;
            }
            _ => {
                let mut tail = lexer;
                tail.next()?;
                if interface
                    && let Kind::Word(name) = token.kind
                    && identifier(token.kind).is_some()
                    && let Some(Token {
                        kind: Kind::Byte(b'('),
                        newline,
                    }) = tail.next()?
                {
                    spellings.interface_methods.push(ReceiverMethod {
                        receiver: owner.to_owned(),
                        name: name.to_owned(),
                        arity: if newline {
                            None
                        } else {
                            method_shape(tail, b'}')
                        },
                    });
                } else {
                    let mut tail = lexer;
                    if let Some(named) = named_type(&mut tail)? {
                        // A tagged field can also be an embedding.
                        if !interface
                            && tail.peek()?.is_some_and(|token| {
                                !token.newline && matches!(token.kind, Kind::Literal(_))
                            })
                        {
                            tail.next()?;
                        }
                        let next = tail.peek()?;
                        if element_end(next, Some(b'}')) {
                            record_reference(spellings, owner, named, parameters, false);
                        } else if next.is_some_and(|next| {
                            if interface {
                                next.kind != Kind::Byte(b'|')
                            } else {
                                !matches!(
                                    next.kind,
                                    Kind::Word(_) | Kind::Byte(b',' | b'*' | b'[' | b'(' | b'<')
                                )
                            }
                        }) {
                            spellings.uncertain_types.insert(owner.to_owned());
                        }
                    } else if interface {
                        type_prefix(&mut tail)?;
                        if tail
                            .peek()?
                            .is_some_and(|token| token.kind == Kind::Word("interface"))
                        {
                            // The inline embedding has no named target for the reverse index.
                            spellings.uncertain_types.insert(owner.to_owned());
                        }
                    }
                }
                skip_element(&mut lexer, Some(b'}'))?;
            }
        }
    }
    Err(())
}

fn record_reference(
    spellings: &mut InterfaceSpellings,
    owner: &str,
    named: NamedType<'_>,
    parameters: Option<&str>,
    alias: bool,
) {
    if named.qualifier.is_none()
        && parameters.is_some_and(|parameters| {
            parameters
                .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
                .any(|word| word == named.target)
        })
    {
        // An instantiation can substitute a type outside the declaring package.
        spellings.uncertain_types.insert(owner.to_owned());
    }
    spellings.type_references.push(TypeReference {
        owner: owner.to_owned(),
        qualifier: named.qualifier.map(str::to_owned),
        target: named.target.to_owned(),
        alias,
    });
}

struct NamedType<'a> {
    qualifier: Option<&'a str>,
    target: &'a str,
}

fn named_type<'a>(lexer: &mut Lexer<'a>) -> Result<Option<NamedType<'a>>, ()> {
    let mut tail = *lexer;
    let parentheses = type_prefix(&mut tail)?;
    let Some(Token {
        kind: Kind::Word(first),
        ..
    }) = tail.next()?
    else {
        return Ok(None);
    };
    if identifier(Kind::Word(first)).is_none() {
        return Ok(None);
    }
    let (qualifier, target) = if tail.consume(b'.')? {
        let Some(Token {
            kind: Kind::Word(target),
            ..
        }) = tail.next()?
        else {
            return Err(());
        };
        (Some(first), target)
    } else {
        (None, first)
    };
    if tail.consume(b'[')? {
        skip_group(&mut tail, b']')?;
    }
    for _ in 0..parentheses {
        if !tail.consume(b')')? {
            return Ok(None);
        }
    }
    *lexer = tail;
    Ok(Some(NamedType { qualifier, target }))
}

fn type_prefix(lexer: &mut Lexer<'_>) -> Result<usize, ()> {
    let mut parentheses = 0;
    loop {
        if lexer.consume(b'(')? {
            parentheses += 1;
        } else if !lexer.consume(b'*')? {
            return Ok(parentheses);
        }
    }
}

fn skip_element(lexer: &mut Lexer<'_>, end: Option<u8>) -> Result<(), ()> {
    let mut previous = None;
    let mut tail = *lexer;
    while let Some(token) = tail.next()? {
        if matches!(token.kind, Kind::Byte(byte) if Some(byte) == end || byte == b';')
            || token.newline && previous.is_some_and(ends_statement)
        {
            return Ok(());
        }
        previous = Some(if let Some(end) = group_end(token.kind) {
            skip_group(&mut tail, end)?;
            Kind::Byte(end)
        } else {
            token.kind
        });
        *lexer = tail;
    }
    Ok(())
}

fn skip_group(lexer: &mut Lexer<'_>, end: u8) -> Result<(), ()> {
    let mut groups = vec![end];
    while let Some(token) = lexer.next()? {
        if let Some(end) = group_end(token.kind) {
            groups.push(end);
        } else if let Kind::Byte(end @ (b')' | b']' | b'}')) = token.kind {
            close_group(&mut groups, end)?;
            if groups.is_empty() {
                return Ok(());
            }
        }
    }
    Err(())
}

fn group_end(kind: Kind<'_>) -> Option<u8> {
    match kind {
        Kind::Byte(b'(') => Some(b')'),
        Kind::Byte(b'[') => Some(b']'),
        Kind::Byte(b'{') => Some(b'}'),
        _ => None,
    }
}

fn element_end(token: Option<Token<'_>>, end: Option<u8>) -> bool {
    token.is_none_or(|token| {
        token.newline || matches!(token.kind, Kind::Byte(byte) if byte == b';' || Some(byte) == end)
    })
}

fn ends_statement(kind: Kind<'_>) -> bool {
    match kind {
        Kind::Word("break" | "continue" | "fallthrough" | "return") => true,
        Kind::Word(_) => identifier(kind).is_some(),
        Kind::Literal(_) | Kind::Byte(b')' | b']' | b'}' | b'0'..=b'9') => true,
        _ => false,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind<'a> {
    Word(&'a str),
    Byte(u8),
    Ellipsis,
    Literal(&'a str),
}

#[derive(Clone, Copy)]
struct Token<'a> {
    kind: Kind<'a>,
    newline: bool,
}

#[derive(Clone, Copy)]
struct Lexer<'a> {
    rest: &'a str,
}

impl<'a> Lexer<'a> {
    fn next(&mut self) -> Result<Option<Token<'a>>, ()> {
        let mut newline = false;
        loop {
            let rest = self
                .rest
                .trim_start_matches(|ch: char| ch.is_ascii_whitespace());
            newline |= self.rest[..self.rest.len() - rest.len()].contains('\n');
            self.rest = rest;
            if let Some(comment) = rest.strip_prefix("//") {
                self.rest = comment.find('\n').map_or("", |end| &comment[end..]);
            } else if let Some(comment) = rest.strip_prefix("/*") {
                let end = comment.find("*/").ok_or(())?;
                newline |= comment[..end].contains('\n');
                self.rest = &comment[end + 2..];
            } else {
                break;
            }
        }

        let Some(&first) = self.rest.as_bytes().first() else {
            return Ok(None);
        };
        let (kind, len) = match first {
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                let len = self
                    .rest
                    .bytes()
                    .take_while(|&byte| byte.is_ascii_alphanumeric() || byte == b'_')
                    .count();
                (Kind::Word(&self.rest[..len]), len)
            }
            b'`' | b'\'' | b'"' => {
                let len = if first == b'`' {
                    self.rest[1..].find('`').ok_or(())? + 2
                } else {
                    quoted_len(self.rest.as_bytes(), first).ok_or(())?
                };
                (Kind::Literal(&self.rest[..len]), len)
            }
            b'.' if self.rest.starts_with("...") => (Kind::Ellipsis, 3),
            b'@' | b'$' | b'#' | b'?' | b'\\' => return Err(()),
            b'!'..=b'~' => (Kind::Byte(first), 1),
            // Let the real parser handle Unicode identifiers and unusual bytes.
            _ => return Err(()),
        };
        self.rest = &self.rest[len..];
        Ok(Some(Token { kind, newline }))
    }

    fn peek(mut self) -> Result<Option<Token<'a>>, ()> {
        self.next()
    }

    fn consume(&mut self, byte: u8) -> Result<bool, ()> {
        let mut tail = *self;
        if tail
            .next()?
            .is_some_and(|token| token.kind == Kind::Byte(byte))
        {
            *self = tail;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

fn quoted_len(text: &[u8], quote: u8) -> Option<usize> {
    let mut escaped = false;
    for (index, &byte) in text.iter().enumerate().skip(1) {
        if byte == b'\n' {
            return None;
        }
        if byte == quote && !escaped {
            return Some(index + 1);
        }
        escaped = byte == b'\\' && !escaped;
    }
    None
}

fn method_shape(mut lexer: Lexer<'_>, terminator: u8) -> Option<Arity> {
    let (parameters, variadic) = parameter_list(&mut lexer, 0)?;
    let results = result_shape(&mut lexer, 0)?;
    element_end(lexer.peek().ok()?, Some(terminator)).then_some((parameters, results, variadic))
}

fn result_shape(lexer: &mut Lexer<'_>, depth: usize) -> Option<usize> {
    let Some(token) = lexer.peek().ok()? else {
        return Some(0);
    };
    match token.kind {
        Kind::Byte(b';' | b'{' | b'}' | b',' | b')') => Some(0),
        _ if token.newline => None,
        Kind::Byte(b'(') => {
            lexer.next().ok()?;
            let (results, variadic) = parameter_list(lexer, depth)?;
            (!variadic).then_some(results)
        }
        _ => (!simple_type(lexer, depth)?).then_some(1),
    }
}

// Every comma in these simple declarations adds either a parameter or a name
// sharing the following type. Complex types leave the shape unchecked.
fn parameter_list(lexer: &mut Lexer<'_>, depth: usize) -> Option<(usize, bool)> {
    let mut count = 0;
    let mut variadic = false;
    let mut pending_names = 0;
    let mut named = false;
    let mut unnamed = false;
    loop {
        if lexer.peek().ok()??.kind == Kind::Byte(b')') {
            lexer.next().ok()?;
            return (!named || pending_names == 0).then_some((count, variadic));
        }
        if variadic {
            return None;
        }
        let mut item = *lexer;
        let first = item.peek().ok()??.kind;
        let (is_variadic, end) = if let Some((is_variadic, end)) = parameter(&mut item, depth) {
            let mut after_first = *lexer;
            after_first.next().ok()?;
            if identifier(first).is_some() && after_first.peek().ok()??.kind == end {
                // A bare identifier can share the type of a later parameter.
                pending_names += 1;
            } else {
                if named {
                    return None;
                }
                unnamed = true;
                pending_names = 0;
            }
            (is_variadic, end)
        } else {
            item = *lexer;
            identifier(item.next().ok()??.kind)?;
            let (is_variadic, end) = parameter(&mut item, depth)?;
            if unnamed || is_variadic && pending_names != 0 {
                return None;
            }
            named = true;
            pending_names = 0;
            (is_variadic, end)
        };
        *lexer = item;
        count += 1;
        variadic = is_variadic;
        if end == Kind::Byte(b')') {
            return (!named || pending_names == 0).then_some((count, variadic));
        }
    }
}

fn parameter<'a>(lexer: &mut Lexer<'a>, depth: usize) -> Option<(bool, Kind<'a>)> {
    let variadic = simple_type(lexer, depth)?;
    let end = lexer.next().ok()??.kind;
    matches!(end, Kind::Byte(b',' | b')')).then_some((variadic, end))
}

fn simple_type(lexer: &mut Lexer<'_>, depth: usize) -> Option<bool> {
    let variadic = lexer.peek().ok()??.kind == Kind::Ellipsis;
    if variadic {
        lexer.next().ok()?;
    }
    loop {
        if lexer.consume(b'*').ok()? {
            continue;
        }
        if !lexer.consume(b'[').ok()? {
            break;
        }
        if !lexer.consume(b']').ok()? {
            return None;
        }
    }
    if lexer.peek().ok()??.kind == Kind::Word("func") {
        const MAX_FUNCTION_DEPTH: usize = 16;
        lexer.next().ok()?;
        if depth >= MAX_FUNCTION_DEPTH || !lexer.consume(b'(').ok()? {
            return None;
        }
        parameter_list(lexer, depth + 1)?;
        result_shape(lexer, depth + 1)?;
    } else {
        named_type(lexer).ok()??;
    }
    Some(variadic)
}

fn identifier(kind: Kind<'_>) -> Option<()> {
    let Kind::Word(name) = kind else { return None };
    (!matches!(
        name,
        "break"
            | "case"
            | "chan"
            | "const"
            | "continue"
            | "default"
            | "defer"
            | "else"
            | "fallthrough"
            | "for"
            | "func"
            | "go"
            | "goto"
            | "if"
            | "import"
            | "interface"
            | "map"
            | "package"
            | "range"
            | "return"
            | "select"
            | "struct"
            | "switch"
            | "type"
            | "var"
    ))
    .then_some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(spellings: &Option<InterfaceSpellings>, name: &str, shape: Arity) -> bool {
        spellings.as_ref().is_none_or(|spellings| {
            spellings.interface_methods.iter().any(|method| {
                name == method.name && method.arity.is_none_or(|candidate| shape == candidate)
            })
        })
    }

    fn finds(source: &str, name: &str, shape: Arity) -> bool {
        matches(&interface_spellings(source), name, shape)
    }

    #[test]
    fn probes_interface_syntax_without_treating_comments_and_literals_as_code() {
        let source = "package p\n \
            // type I interface { Match() }\n \
            var _ = `interface { Match() }`\n \
            type I[T any] interface /* { Match() } */ {\n \
                Outer(interface { Nested() })\n \
                Match /* comment */ (\n a, b *p.T, args ...[]T,\n) (x, y error)\n \
            }";
        let spellings = interface_spellings(source);
        assert!(matches(&spellings, "Match", (3, 2, true)));
        assert!(!matches(&spellings, "Match", (0, 0, false)));
        assert!(matches(&spellings, "Outer", (0, 0, false)));
        assert!(!matches(&spellings, "Nested", (0, 0, false)));
        assert!(!matches(&spellings, "Missing", (0, 0, false)));
    }

    #[test]
    fn checks_simple_result_arity_and_keeps_uncertain_syntax() {
        assert!(!finds(
            "type I interface { Close() error }",
            "Close",
            (0, 0, false)
        ));
        assert!(finds(
            "type I interface { Close() error }",
            "Close",
            (0, 1, false)
        ));
        assert!(finds(
            "type I interface { Close(); Other() }",
            "Close",
            (0, 0, false)
        ));
        let source = "type I interface { Use(func(int, string), func() func() (ok bool)) }";
        assert!(finds(source, "Use", (2, 0, false)));
        assert!(!finds(source, "Use", (0, 0, false)));
        assert!(finds(
            "type I interface { Use(map[int]string) }",
            "Use",
            (0, 0, false)
        ));
        assert!(finds("type I interface { Close( ", "Close", (0, 0, false)));
        assert!(finds("type I interface { 转换() }", "转换", (0, 0, false)));
    }

    #[test]
    fn distinguishes_receiver_and_interface_method_fingerprints() {
        let source = r#"package p
            type StringReader struct{}
            func (StringReader) Read() string { return "" }
            type BufferReader[T any] struct{}
            func (r *BufferReader[T],
            ) Read(input []byte) (n int, err error) { return }
            func (_ BufferReader[T]) Map(a, b T, rest ...[]Box[T]) (x, y *Box[T]) { return }
            type Reader interface { Read() string }
            type Mapper[T any] interface {
                Map(a, b T, rest ...[]Box[T]) (x, y *Box[T])
                Outer(interface { Nested() })
            }
            var f = func(input []byte) string { return "" }
            func Read() string { return "" }
        "#;
        let spellings = interface_spellings(source).expect("lexical summary");
        let methods = |methods: &[ReceiverMethod]| {
            methods
                .iter()
                .map(|method| (method.receiver.clone(), method.name.clone(), method.arity))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            methods(&spellings.receiver_methods),
            [
                ("StringReader".into(), "Read".into(), Some((0, 1, false))),
                ("BufferReader".into(), "Read".into(), Some((1, 2, false))),
                ("BufferReader".into(), "Map".into(), Some((3, 2, true))),
            ]
        );
        assert_eq!(
            methods(&spellings.interface_methods),
            [
                ("Reader".into(), "Read".into(), Some((0, 1, false))),
                ("Mapper".into(), "Map".into(), Some((3, 2, true))),
                ("Mapper".into(), "Outer".into(), None),
            ]
        );
    }

    #[test]
    fn records_grouped_definitions_aliases_and_direct_embeds() {
        let source = r#"package p
            import (
                "example.test/actual-dir/v2"
                alt "example.test/base"
                . `example.test/dot`
                _ "example.test/sideeffect"
            )
            type (
                Alias[T any] = alt.Generic[T]
                Underlying alt.Struct
                Holder[T any] struct {
                    *alt.Generic[T] `json:"generic"`
                    actual.Struct; Dot
                    Name alt.Struct
                    private[T] // an instantiated anonymous field
                }
                Interface[T any] interface {
                    alt.Interface[T]
                    error
                    ~int | ~int32
                }
                Array [4]alt.Struct
                Identity[T any] = T
                Projected[T any] struct { T }
                Narrow = (Holder[int])
            )
        "#;
        let spellings = interface_spellings(source).expect("lexical summary");
        let references: Vec<_> = spellings
            .type_references
            .iter()
            .map(|reference| {
                (
                    reference.owner.as_str(),
                    reference.qualifier.as_deref(),
                    reference.target.as_str(),
                    reference.alias,
                )
            })
            .collect();
        assert_eq!(
            references,
            [
                ("Alias", Some("alt"), "Generic", true),
                ("Underlying", Some("alt"), "Struct", false),
                ("Holder", Some("alt"), "Generic", false),
                ("Holder", Some("actual"), "Struct", false),
                ("Holder", None, "Dot", false),
                ("Holder", None, "private", false),
                ("Interface", Some("alt"), "Interface", false),
                ("Interface", None, "error", false),
                ("Identity", None, "T", true),
                ("Projected", None, "T", false),
                ("Narrow", None, "Holder", true),
            ]
        );
        assert_eq!(
            spellings.type_names,
            HashSet::from(
                [
                    "Alias",
                    "Underlying",
                    "Holder",
                    "Interface",
                    "Array",
                    "Identity",
                    "Projected",
                    "Narrow"
                ]
                .map(str::to_owned)
            )
        );
        assert_eq!(
            spellings.uncertain_types,
            HashSet::from(["Identity".into(), "Projected".into()])
        );
        assert_eq!(
            spellings.default_imports,
            HashSet::from(["example.test/actual-dir/v2".into()])
        );
        assert_eq!(
            spellings.alias_imports,
            HashMap::from([("alt".into(), "example.test/base".into())])
        );
        assert_eq!(
            spellings.dot_imports,
            HashSet::from(["example.test/dot".into()])
        );
        assert!(!spellings.has_local_types);
    }

    #[test]
    fn preserves_files_with_local_type_declarations_for_embedding_search() {
        let source = "package p\nfunc run() { type Local struct { T }; return }";
        let spellings = interface_spellings(source).expect("local types are scanned");
        assert!(spellings.has_local_types);
        assert!(!spellings.type_names.contains("Local"));
        let type_switch = "package p\nfunc run(a any) { switch a.(type) { default: return } }";
        assert!(!interface_spellings(type_switch).unwrap().has_local_types);
    }

    #[test]
    fn keeps_owners_of_anonymous_interface_embeddings() {
        let source = "package p\ntype Outer interface { (interface { Read() string }) }";
        let spellings = interface_spellings(source).expect("inline interface embedding");
        assert!(spellings.uncertain_types.contains("Outer"));
        assert!(spellings.interface_methods.is_empty());
    }

    #[test]
    fn preserves_fallbacks_for_partial_and_special_signatures() {
        let source = r#"package p
            type Reader struct{}
            func (r Reader) Complex(p func(map[int]int)) {}
            func (r Reader) Partial(a int, trailing) {}
            func (r Reader) Missing
        "#;
        let spellings = interface_spellings(source).expect("simple malformed shapes are by name");
        let methods: Vec<_> = spellings
            .receiver_methods
            .iter()
            .map(|method| (method.name.as_str(), method.arity))
            .collect();
        assert_eq!(
            methods,
            [("Complex", None), ("Partial", None), ("Missing", None)]
        );
        for malformed in [
            "package p\nfunc (Reader) Read(",
            "package p\ntype Reader struct { Base /*",
            "package p\nfunc (接收者) Read() string {}",
            "package p\ntype Broken interface { Read(] string }",
        ] {
            assert!(interface_spellings(malformed).is_none(), "{malformed}");
        }
    }
}
