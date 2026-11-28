use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use regex::Regex;

use super::super::source::{check_cancelled, decoded_import_path, import_path};

const CHECK_BYTES: usize = 8 * 1024;
const MAX_POSTING_BYTES: usize = 32 * 1024 * 1024;

// The Go grammar uses Unicode letters and decimal digits. Tree-sitter
// recognizes XID instead. Keep the original AST search where they disagree.
static GO_START: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\A\pL\z").unwrap());
static GO_CONTINUE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\A[\pL\p{Nd}]\z").unwrap());
static PARSER_START: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\A\p{XID_Start}\z").unwrap());
static PARSER_CONTINUE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\A\p{XID_Continue}\z").unwrap());

#[derive(Default)]
pub(in crate::semantic) struct ReferenceIdentifiers {
    complete: bool,
    offsets: HashMap<Box<str>, Vec<usize>>,
    imports: Option<Vec<String>>,
    weight: usize,
}

impl ReferenceIdentifiers {
    pub(in crate::semantic) fn scan(text: &str, cancelled: &AtomicBool) -> Result<Self> {
        check_cancelled(cancelled)?;
        let mut lexer = Lexer {
            text,
            at: usize::from(text.starts_with('\u{feff}')) * '\u{feff}'.len_utf8(),
            next_check: 0,
            cancelled,
        };
        let mut offsets = HashMap::<Box<str>, Vec<usize>>::new();
        let mut imports = Vec::new();
        let mut import = ImportState::Idle;
        let mut delimiters = Vec::new();
        let mut weight = 0;
        loop {
            let token = lexer.next()?;
            match token {
                Token::Identifier(at, name) => {
                    if let Some(postings) = offsets.get_mut(name) {
                        postings.push(at);
                    } else {
                        // Include the hash table entry, its owned name, and
                        // the Vec allocation's first capacity increment.
                        weight += name.len() + 64;
                        offsets.insert(name.into(), vec![at]);
                    }
                    weight += 2 * size_of::<usize>();
                }
                Token::Punctuation(open @ (b'(' | b'[' | b'{')) => delimiters.push(open),
                Token::Punctuation(close @ (b')' | b']' | b'}')) => {
                    let expected = match close {
                        b')' => b'(',
                        b']' => b'[',
                        _ => b'{',
                    };
                    if delimiters.pop() != Some(expected) {
                        return Ok(Self::default());
                    }
                }
                Token::Unknown => return Ok(Self::default()),
                Token::End => break,
                _ => {}
            }
            import = import.advance(token, &mut imports);
            if weight > MAX_POSTING_BYTES || delimiters.len() > CHECK_BYTES {
                return Ok(Self::default());
            }
        }
        check_cancelled(cancelled)?;
        if !delimiters.is_empty() {
            return Ok(Self::default());
        }
        let imports = matches!(import, ImportState::Idle).then_some(imports);
        weight += imports.as_ref().map_or(0, |paths| {
            paths.capacity() * size_of::<String>()
                + paths.iter().map(String::capacity).sum::<usize>()
        });
        Ok(Self {
            complete: true,
            offsets,
            imports,
            weight,
        })
    }

    pub(in crate::semantic) fn offsets(&self, spelling: &str) -> Option<&[usize]> {
        self.complete
            .then(|| self.offsets.get(spelling).map_or(&[][..], Vec::as_slice))
    }

    pub(in crate::semantic) fn imports(&self) -> Option<&[String]> {
        self.imports.as_deref()
    }

    pub(in crate::semantic) fn weight(&self) -> usize {
        self.weight
    }
}

#[derive(Clone, Copy)]
enum Token<'a> {
    Identifier(usize, &'a str),
    String(&'a str),
    Punctuation(u8),
    Other,
    End,
    Unknown,
}

enum ImportState {
    Idle,
    Single,
    SinglePath,
    Block,
    BlockPath,
    Unknown,
}

impl ImportState {
    fn advance(self, token: Token<'_>, imports: &mut Vec<String>) -> Self {
        use ImportState::*;

        match (self, token) {
            (Idle, Token::Identifier(_, "import")) => Single,
            (Idle, _) => Idle,
            (Single, Token::Punctuation(b'(')) => Block,
            (state @ (Single | Block), Token::Identifier(..) | Token::Punctuation(b'.')) => {
                if matches!(state, Single) {
                    SinglePath
                } else {
                    BlockPath
                }
            }
            (Block, Token::Punctuation(b';')) => Block,
            (Block, Token::Punctuation(b')')) => Idle,
            (state, Token::String(literal)) if !matches!(state, Unknown) => {
                if let Some(path) = import_path(literal).or_else(|| decoded_import_path(literal)) {
                    imports.push(path);
                    if matches!(state, Single | SinglePath) {
                        Idle
                    } else {
                        Block
                    }
                } else {
                    Unknown
                }
            }
            _ => Unknown,
        }
    }
}

struct Lexer<'a> {
    text: &'a str,
    at: usize,
    next_check: usize,
    cancelled: &'a AtomicBool,
}

impl<'a> Lexer<'a> {
    fn byte(&self) -> Option<u8> {
        self.text.as_bytes().get(self.at).copied()
    }

    fn advance(&mut self, bytes: usize) -> Result<()> {
        self.at += bytes;
        if self.at >= self.next_check {
            check_cancelled(self.cancelled)?;
            self.next_check = self.at + CHECK_BYTES;
        }
        Ok(())
    }

    fn next(&mut self) -> Result<Token<'a>> {
        loop {
            let Some(byte) = self.byte() else {
                return Ok(Token::End);
            };
            let start = self.at;
            let next = self.text.as_bytes().get(start + 1).copied();
            match (byte, next) {
                (b' ' | b'\t' | b'\r' | b'\n', _) => self.advance(1)?,
                (b'/', Some(b'/')) => {
                    self.advance(2)?;
                    if !self.line_comment()? {
                        return Ok(Token::Unknown);
                    }
                }
                (b'/', Some(b'*')) => {
                    self.advance(2)?;
                    if !self.block_comment()? {
                        return Ok(Token::Unknown);
                    }
                }
                (quote @ (b'"' | b'`' | b'\''), _) => {
                    self.advance(1)?;
                    if !self.quoted(quote)? {
                        return Ok(Token::Unknown);
                    }
                    return Ok(if quote == b'\'' {
                        Token::Other
                    } else {
                        Token::String(&self.text[start..self.at])
                    });
                }
                (b'0'..=b'9', _) | (b'.', Some(b'0'..=b'9')) => {
                    return Ok(if self.number()? {
                        Token::Other
                    } else {
                        Token::Unknown
                    });
                }
                (b'a'..=b'z' | b'A'..=b'Z' | b'_' | 0x80..=0xff, _) => {
                    if !self.identifier()? {
                        return Ok(Token::Unknown);
                    }
                    return Ok(Token::Identifier(start, &self.text[start..self.at]));
                }
                (
                    b'+' | b'-' | b'*' | b'/' | b'%' | b'&' | b'|' | b'^' | b'<' | b'>' | b'='
                    | b'!' | b'~' | b':' | b'.' | b',' | b';' | b'(' | b')' | b'[' | b']' | b'{'
                    | b'}',
                    _,
                ) => {
                    self.advance(1)?;
                    return Ok(Token::Punctuation(byte));
                }
                _ => return Ok(Token::Unknown),
            }
        }
    }

    fn identifier(&mut self) -> Result<bool> {
        if self.byte().is_some_and(|byte| byte.is_ascii()) {
            self.advance(1)?;
        } else {
            let ch = self.text[self.at..].chars().next().unwrap();
            let mut encoded = [0; 4];
            let spelling = ch.encode_utf8(&mut encoded);
            if !GO_START.is_match(spelling) || !PARSER_START.is_match(spelling) {
                return Ok(false);
            }
            self.advance(ch.len_utf8())?;
        }
        while let Some(byte) = self.byte() {
            if byte.is_ascii_alphanumeric() || byte == b'_' {
                self.advance(1)?;
            } else if !byte.is_ascii() {
                let ch = self.text[self.at..].chars().next().unwrap();
                let mut encoded = [0; 4];
                let spelling = ch.encode_utf8(&mut encoded);
                if !GO_CONTINUE.is_match(spelling) || !PARSER_CONTINUE.is_match(spelling) {
                    return Ok(false);
                }
                self.advance(ch.len_utf8())?;
            } else {
                break;
            }
        }
        Ok(true)
    }

    fn line_comment(&mut self) -> Result<bool> {
        while let Some(byte) = self.byte() {
            match byte {
                b'\n' => break,
                b'\r' if !matches!(self.text.as_bytes().get(self.at + 1), None | Some(b'\n')) => {
                    return Ok(false);
                }
                0 => return Ok(false),
                0xe2 if self.text.as_bytes()[self.at..].starts_with(&[0xe2, 0x80, 0xa8])
                    || self.text.as_bytes()[self.at..].starts_with(&[0xe2, 0x80, 0xa9]) =>
                {
                    return Ok(false);
                }
                _ => {}
            }
            self.advance(1)?;
        }
        Ok(true)
    }

    fn block_comment(&mut self) -> Result<bool> {
        while let Some(byte) = self.byte() {
            if byte == b'*' && self.text.as_bytes().get(self.at + 1) == Some(&b'/') {
                self.advance(2)?;
                return Ok(true);
            }
            if byte == 0 {
                return Ok(false);
            }
            self.advance(1)?;
        }
        Ok(false)
    }

    fn quoted(&mut self, quote: u8) -> Result<bool> {
        let mut units = 0;
        while let Some(byte) = self.byte() {
            if byte == quote {
                self.advance(1)?;
                return Ok(quote != b'\'' || units == 1);
            }
            if byte == 0 || (byte == b'\n' && quote != b'`') || (quote == b'\'' && units == 1) {
                return Ok(false);
            }
            if byte == b'\\' && quote != b'`' {
                self.advance(1)?;
                if !self.escape(quote)? {
                    return Ok(false);
                }
            } else if quote == b'\'' && !byte.is_ascii() {
                self.advance(self.text[self.at..].chars().next().unwrap().len_utf8())?;
            } else {
                self.advance(1)?;
            }
            units += 1;
        }
        Ok(false)
    }

    fn escape(&mut self, quote: u8) -> Result<bool> {
        let Some(byte) = self.byte() else {
            return Ok(false);
        };
        let (digits, is_digit): (usize, fn(u8) -> bool) = match byte {
            b'x' => (2, |byte| byte.is_ascii_hexdigit()),
            b'u' => (4, |byte| byte.is_ascii_hexdigit()),
            b'U' => (8, |byte| byte.is_ascii_hexdigit()),
            b'0'..=b'7' => (3, |byte| (b'0'..=b'7').contains(&byte)),
            b'a' | b'b' | b'f' | b'n' | b'r' | b't' | b'v' | b'\\' => {
                self.advance(1)?;
                return Ok(true);
            }
            byte if byte == quote => {
                self.advance(1)?;
                return Ok(true);
            }
            _ => return Ok(false),
        };
        if byte.is_ascii_alphabetic() {
            self.advance(1)?;
        }
        if !self
            .text
            .as_bytes()
            .get(self.at..self.at + digits)
            .is_some_and(|bytes| bytes.iter().copied().all(is_digit))
        {
            return Ok(false);
        }
        self.advance(digits)?;
        Ok(true)
    }

    fn number(&mut self) -> Result<bool> {
        let prefix = self.text.as_bytes().get(self.at..self.at + 2);
        let (radix, exponent) = match prefix {
            Some(b"0x" | b"0X") => (16, b'p'),
            Some(b"0o" | b"0O") => (8, 0),
            Some(b"0b" | b"0B") => (2, 0),
            _ => (10, b'e'),
        };
        if radix != 10 {
            self.advance(2)?;
            if self.byte() == Some(b'_') {
                self.advance(1)?;
            }
        }
        let mut has_digits = self.digits(radix)?;
        if exponent != 0 && self.byte() == Some(b'.') {
            self.advance(1)?;
            has_digits |= self.digits(radix)?;
        }
        if exponent != 0
            && self
                .byte()
                .is_some_and(|byte| byte.to_ascii_lowercase() == exponent)
        {
            self.advance(1)?;
            if matches!(self.byte(), Some(b'+' | b'-')) {
                self.advance(1)?;
            }
            has_digits &= self.digits(10)?;
        }
        if self.byte() == Some(b'i') {
            self.advance(1)?;
        }
        // An identifier-like suffix means the grammar may see another token
        // at this boundary, as in a partially typed number or selector.
        Ok(has_digits
            && !self.byte().is_some_and(|byte| {
                byte.is_ascii_alphanumeric() || byte == b'_' || !byte.is_ascii()
            }))
    }

    fn digits(&mut self, radix: u32) -> Result<bool> {
        let mut found = false;
        while self
            .byte()
            .is_some_and(|byte| char::from(byte).is_digit(radix))
        {
            found = true;
            self.advance(1)?;
            if self.byte() == Some(b'_') {
                if !self
                    .text
                    .as_bytes()
                    .get(self.at + 1)
                    .is_some_and(|&byte| char::from(byte).is_digit(radix))
                {
                    break;
                }
                self.advance(1)?;
            }
        }
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postings_keep_go_identifier_offsets_and_exclude_literal_text() {
        let text = r#"package p
// Get 𐐀Get١ OnlyInComments
/* Get OtherCommentWord */
var _ = `Get OnlyInRawString`
var _ = "Get OnlyInString\"still in the string"
var _ = '\''
var _ = 0x_f + 0b11 + 0o11 + 3e-2i + .3 + 0x.1p0
var _ = GetUser + Get𐐀 + Get١ + 𐐀Get١ + Get
"#;
        let identifiers = ReferenceIdentifiers::scan(text, &AtomicBool::new(false)).unwrap();
        assert_eq!(
            identifiers.offsets("Get"),
            Some(&[text.rfind("Get").unwrap()][..])
        );
        assert_eq!(
            identifiers.offsets("𐐀Get١"),
            Some(&[text.rfind("𐐀Get١").unwrap()][..]),
        );
        assert_eq!(identifiers.offsets("GetUser").unwrap().len(), 1);
        assert_eq!(identifiers.offsets("Get𐐀").unwrap().len(), 1);
        assert_eq!(identifiers.offsets("Get١").unwrap().len(), 1);
        for name in [
            "OnlyInComments",
            "OtherCommentWord",
            "OnlyInRawString",
            "OnlyInString",
            "i",
        ] {
            assert_eq!(identifiers.offsets(name), Some(&[][..]), "{name}");
        }
    }

    #[test]
    fn import_postings_include_aliases_and_escaped_paths() {
        let text = r#"package p
import p "example.test/p"
import (
    . `../m`
    alias "example.test/\u006e"
    _ "example.test/side"
)
var _ = "example.test/other"
"#;
        let identifiers = ReferenceIdentifiers::scan(text, &AtomicBool::new(false)).unwrap();
        assert_eq!(
            identifiers.imports().unwrap(),
            [
                "example.test/p",
                "../m",
                "example.test/n",
                "example.test/side"
            ],
        );
    }

    #[test]
    fn lexical_ambiguities_keep_candidates_for_recovery() {
        for text in [
            "package p\nfunc f() { Get()",
            "package p\nvar _ = \"Get\nGet()",
            "package p\nvar _ = \"\\x Get\"",
            "package p\nvar _ = 0xGet",
            "package p\n/* Get",
            "package p\nvar _ = Get\u{301}",
        ] {
            let identifiers = ReferenceIdentifiers::scan(text, &AtomicBool::new(false)).unwrap();
            assert!(identifiers.offsets("Get").is_none(), "{text}");
            assert!(identifiers.imports().is_none(), "{text}");
        }
    }
}
