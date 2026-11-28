use std::iter::Peekable;
use std::ops::Range;

use crate::semantic::resolver::is_exported;

use super::cursor::is_keyword;

#[derive(Debug)]
pub(in crate::semantic) struct Names {
    pub(super) exported: Vec<String>,
    pub(super) private: Vec<String>,
    pub(in crate::semantic) types: Vec<String>,
    pub(in crate::semantic) bodies: Vec<Range<usize>>,
}

/// A negative result is safe only if the lexer could follow the declarations.
/// The recovering parser handles damaged headers, groups, and literals.
pub(in crate::semantic) fn scan_names(text: &str) -> Option<Names> {
    scan_declarations(text, true)
}

pub(in crate::semantic) fn scan_function_bodies(text: &str) -> Option<Vec<Range<usize>>> {
    Some(scan_declarations(text, false)?.bodies)
}

fn scan_declarations(text: &str, elide_initializers: bool) -> Option<Names> {
    let mut tokens = Tokens::new(text).peekable();
    let mut exported = Vec::new();
    let mut private = Vec::new();
    let mut types = Vec::new();
    let mut delimiters = Vec::new();
    let mut group = None;
    let mut spec_start = false;
    let mut bodies = Vec::new();
    let mut body_start = None;
    let mut declaration_start = true;
    let mut function = false;
    let mut variable = false;
    let mut initializer = None;
    let mut may_end_declaration = false;
    let mut anonymous_type = false;
    while let Some(token) = tokens.next() {
        let mut opens_body = false;
        if delimiters.is_empty() {
            match token {
                Token::Newline if !may_end_declaration => {}
                Token::Newline | Token::Punct(b';', _) => {
                    declaration_start = true;
                    function = false;
                    variable = false;
                    initializer = None;
                    may_end_declaration = false;
                    anonymous_type = false;
                }
                _ => {
                    if declaration_start {
                        if !matches!(
                            token,
                            Token::Word("package" | "import" | "func" | "type" | "var" | "const")
                        ) {
                            return None;
                        }
                        function = matches!(token, Token::Word("func"));
                        variable = matches!(token, Token::Word("var"));
                        declaration_start = false;
                    }
                    opens_body =
                        function && !anonymous_type && matches!(token, Token::Punct(b'{', _));
                    anonymous_type = matches!(token, Token::Word("struct" | "interface"));
                    may_end_declaration = matches!(token, Token::Word(word) if !is_keyword(word))
                        || matches!(token, Token::Literal(_));
                }
            }
        }
        match token {
            Token::Word(kind @ ("func" | "type" | "var" | "const")) if delimiters.is_empty() => {
                match next_nonnewline(&mut tokens)? {
                    Token::Punct(b'(', _) => {
                        delimiters.push(b')');
                        // For func, this parenthesis starts a method receiver.
                        group = (kind != "func").then_some(kind);
                        spec_start = true;
                    }
                    Token::Word(name) => {
                        record(
                            &mut tokens,
                            &mut exported,
                            &mut private,
                            &mut types,
                            name,
                            kind,
                        )?;
                        may_end_declaration = true;
                    }
                    _ => return None,
                }
            }
            Token::Word(name) if delimiters.len() == 1 && spec_start && group.is_some() => {
                record(
                    &mut tokens,
                    &mut exported,
                    &mut private,
                    &mut types,
                    name,
                    group?,
                )?;
                spec_start = false;
            }
            Token::Word("func") if matches!(tokens.peek(), Some(Token::Word(_))) => {
                // A named function in nested syntax can indicate that the
                // recovering parser sees a different declaration boundary.
                return None;
            }
            Token::Punct(b'(', _) => delimiters.push(b')'),
            Token::Punct(b'[', _) => delimiters.push(b']'),
            Token::Punct(b'=', offset)
                if variable && delimiters.is_empty()
                    || group == Some("var") && delimiters.len() == 1 =>
            {
                initializer.get_or_insert((delimiters.len(), offset + 1));
            }
            Token::Punct(b'{', offset) => {
                if opens_body {
                    body_start = Some((delimiters.len(), offset + 1, 1));
                } else if let Some((depth, start)) = initializer
                    && depth == delimiters.len()
                {
                    initializer = None;
                    if elide_initializers && composite_literal_type(&text[start..offset]) {
                        body_start = Some((depth, offset + 1, 4096));
                    }
                }
                delimiters.push(b'}');
            }
            Token::Punct(p @ (b')' | b']' | b'}'), offset) => {
                if delimiters.pop() != Some(p) {
                    return None;
                }
                if delimiters.is_empty() {
                    group = None;
                    may_end_declaration = true;
                }
                if let Some((depth, start, minimum)) = body_start
                    && delimiters.len() == depth
                {
                    body_start = None;
                    if offset - start >= minimum {
                        bodies.push(start..offset);
                    }
                    function = false;
                }
            }
            Token::Newline | Token::Punct(b';', _) if delimiters.len() == 1 => {
                spec_start = true;
                if group == Some("var") {
                    initializer = None;
                }
            }
            Token::Invalid => return None,
            _ => {}
        }
    }
    if !delimiters.is_empty() {
        return None;
    }
    exported.sort_unstable();
    exported.dedup();
    private.sort_unstable();
    private.dedup();
    types.sort_unstable();
    types.dedup();
    Some(Names {
        exported,
        private,
        types,
        bodies,
    })
}

/// Table initializers can be much larger than their declarations. Only
/// elide literals with an explicit, unambiguous type before the opening brace.
fn composite_literal_type(text: &str) -> bool {
    if text.len() > 128
        || !text.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'_' | b' ' | b'\t' | b'.' | b'[' | b']' | b'&' | b'*')
        })
    {
        return false;
    }
    let text = text.trim().trim_start_matches('&').trim_start();
    let tail = if text.starts_with('[') || text.starts_with("map[") {
        let Some((_, tail)) = text.rsplit_once(']') else {
            return false;
        };
        tail.trim_start().trim_start_matches('*').trim_start()
    } else {
        text
    };
    !tail.is_empty()
        && tail.split('.').all(|name| {
            !is_keyword(name)
                && name
                    .as_bytes()
                    .first()
                    .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        })
}

fn record<'a>(
    tokens: &mut Peekable<Tokens<'a>>,
    exported: &mut Vec<String>,
    private: &mut Vec<String>,
    types: &mut Vec<String>,
    mut name: &'a str,
    kind: &str,
) -> Option<()> {
    loop {
        if !name
            .chars()
            .next()
            .is_some_and(|ch| ch == '_' || ch.is_alphabetic())
            || !name.is_ascii() && !name.chars().all(|ch| ch == '_' || ch.is_alphanumeric())
            || is_keyword(name)
        {
            return None;
        }
        if is_exported(name) {
            exported.push(name.to_owned());
        } else if name != "_" {
            private.push(name.to_owned());
        }
        if kind == "type" {
            types.push(name.to_owned());
        }
        if !matches!(kind, "var" | "const") || !matches!(tokens.peek(), Some(Token::Punct(b',', _)))
        {
            break;
        }
        tokens.next();
        let Token::Word(next) = next_nonnewline(tokens)? else {
            return None;
        };
        name = next;
    }
    Some(())
}

pub(super) fn next_nonnewline<'a>(
    tokens: &mut impl Iterator<Item = Token<'a>>,
) -> Option<Token<'a>> {
    tokens.find(|token| !matches!(token, Token::Newline))
}

pub(super) enum Token<'a> {
    Word(&'a str),
    Punct(u8, usize),
    Newline,
    Literal(&'a str),
    Invalid,
}

pub(super) struct Tokens<'a> {
    text: &'a str,
    offset: usize,
}

impl<'a> Tokens<'a> {
    pub(super) fn new(text: &'a str) -> Self {
        Self {
            text,
            offset: usize::from(text.starts_with('\u{feff}')) * 3,
        }
    }
}

impl<'a> Iterator for Tokens<'a> {
    type Item = Token<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let rest = self.text.get(self.offset..)?;
            let byte = *rest.as_bytes().first()?;
            let token = match byte {
                b' ' | b'\r' | b'\t' => {
                    self.offset += rest
                        .bytes()
                        .take_while(|byte| matches!(byte, b' ' | b'\r' | b'\t'))
                        .count();
                    continue;
                }
                b'\n' => {
                    self.offset += 1;
                    Token::Newline
                }
                b'/' if rest.starts_with("//") => {
                    self.offset += rest.find('\n').unwrap_or(rest.len());
                    continue;
                }
                b'/' if rest.starts_with("/*") => {
                    let Some(end) = rest[2..].find("*/").map(|end| end + 4) else {
                        self.offset += rest.len();
                        return Some(Token::Invalid);
                    };
                    self.offset += end;
                    if !rest[..end].contains('\n') {
                        continue;
                    }
                    Token::Newline
                }
                b'`' => {
                    let Some(end) = rest[1..].find('`').map(|end| end + 2) else {
                        self.offset += rest.len();
                        return Some(Token::Invalid);
                    };
                    self.offset += end;
                    Token::Literal(&rest[..end])
                }
                quote @ (b'"' | b'\'') => {
                    let bytes = rest.as_bytes();
                    let mut end = 1;
                    let mut closed = false;
                    while let Some(byte) = bytes.get(end).copied() {
                        end += 1;
                        if byte == quote {
                            closed = true;
                            break;
                        }
                        if byte == b'\n' {
                            break;
                        }
                        if byte == b'\\' && end < bytes.len() {
                            end += 1;
                        }
                    }
                    self.offset += end;
                    if closed && !rest[..end].contains('\n') {
                        Token::Literal(&rest[..end])
                    } else {
                        Token::Invalid
                    }
                }
                byte if word(byte) => {
                    let end = rest.bytes().take_while(|byte| word(*byte)).count();
                    self.offset += end;
                    Token::Word(&rest[..end])
                }
                _ => {
                    self.offset += 1;
                    if matches!(byte, 0..=31 | b'@' | b'$' | b'#') {
                        Token::Invalid
                    } else {
                        Token::Punct(byte, self.offset - 1)
                    }
                }
            };
            return Some(token);
        }
    }
}

fn word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || !byte.is_ascii()
}
