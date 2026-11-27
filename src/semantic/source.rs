use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result, bail};
use lsp_types::{Position, Range, TextDocumentContentChangeEvent};
use tree_sitter::{InputEdit, Node, Parser, Point, Tree};

use crate::symbols::{self, FileOutline};

use super::editor::HighlightCache;

#[derive(Clone, Copy)]
pub(super) enum ReferenceQuery<'tree> {
    Name(Node<'tree>),
    Member {
        receiver: Node<'tree>,
        name: Node<'tree>,
    },
    LiteralKey {
        value: Node<'tree>,
        name: Node<'tree>,
    },
}

impl<'tree> ReferenceQuery<'tree> {
    pub(super) fn name(self) -> Node<'tree> {
        match self {
            Self::Name(name) | Self::Member { name, .. } | Self::LiteralKey { name, .. } => name,
        }
    }
}

pub(super) fn is_test_source(path: &Path) -> bool {
    path.file_name()
        .and_then(|filename| filename.to_str())
        .is_some_and(|filename| filename.ends_with("_test.go"))
}

pub(super) fn read_source(parser: &mut Parser, path: &Path) -> Result<Source> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Source::parse(parser, path.to_owned(), text, None)
}

pub(super) fn read_navigation_source(
    parser: &mut Parser,
    path: &Path,
    probe: Option<&super::navigation_trace::NavigationProbe>,
) -> Result<Source> {
    use super::navigation_trace::{record, sample};

    let Some(probe) = probe else {
        return read_source(parser, path);
    };
    let text = {
        let _read = sample(Some(probe), |counts| &mut counts.read);
        record(Some(probe), |counts| {
            counts.text_misses += 1;
            counts.disk_reads += 1;
        });
        fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?
    };
    parse_navigation_source(parser, path, text, Some(probe))
}

pub(super) fn parse_navigation_source(
    parser: &mut Parser,
    path: &Path,
    text: impl Into<Arc<String>>,
    probe: Option<&super::navigation_trace::NavigationProbe>,
) -> Result<Source> {
    use super::navigation_trace::{record, sample};

    record(probe, |counts| counts.ast_parses += 1);
    let _parse = sample(probe, |counts| &mut counts.parse);
    let text = text.into();
    let lines = LineIndex::new(&text);
    Source::parse_with_lines(parser, path.to_owned(), text, lines, None)
}

#[derive(Clone)]
pub(super) struct Import {
    pub(super) alias: Option<String>,
    pub(super) path: String,
    pub(super) binding: Range,
}

#[derive(Default)]
pub(super) struct EditorNames {
    pub(super) raw_imports: Vec<Import>,
    pub(super) imports: HashMap<String, Option<std::ops::Range<usize>>>,
    files: HashMap<String, FileName>,
    pub(super) methods: HashSet<String>,
}

enum FileName {
    Declaration(std::ops::Range<usize>),
    Incomplete,
    Ambiguous,
}

impl EditorNames {
    pub(super) fn ambiguous(&self, name: &str) -> bool {
        matches!(self.files.get(name), Some(FileName::Ambiguous))
    }

    pub(super) fn declaration(&self, name: &str) -> Option<std::ops::Range<usize>> {
        match self.files.get(name)? {
            FileName::Declaration(bytes) => Some(bytes.clone()),
            FileName::Incomplete | FileName::Ambiguous => None,
        }
    }

    fn collect_declaration(&mut self, declaration: Node<'_>, text: &str) {
        if declaration.is_extra() || declaration.is_missing() {
            return;
        }
        let mut cursor = declaration.walk();
        match declaration.kind() {
            "method_declaration" => {
                if let Some(name) = declaration.child_by_field_name("name") {
                    self.methods.insert(node_text(name, text).to_owned());
                }
            }
            "var_declaration" | "var_spec_list" | "const_declaration" | "type_declaration" => {
                for child in declaration.named_children(&mut cursor) {
                    self.collect_declaration(child, text);
                }
            }
            "function_declaration" | "type_spec" | "type_alias" | "var_spec" | "const_spec" => {
                let complete = !is_type_declaration(declaration)
                    || declaration.child_by_field_name("type").is_some();
                for name in declaration
                    .children_by_field_name("name", &mut cursor)
                    .filter(Node::is_named)
                {
                    let spelling = node_text(name, text);
                    let is_value = matches!(declaration.kind(), "var_spec" | "const_spec");
                    let file_name = if complete
                        && !spelling.is_empty()
                        && spelling != "_"
                        && (if is_value {
                            name.kind() == "identifier"
                        } else {
                            declaration.child_by_field_name("name") == Some(name)
                        }) {
                        FileName::Declaration(name.byte_range())
                    } else {
                        FileName::Incomplete
                    };
                    self.files
                        .entry(spelling.to_owned())
                        .and_modify(|entry| *entry = FileName::Ambiguous)
                        .or_insert(file_name);
                }
            }
            _ => {}
        }
    }
}

pub(super) struct DiagnosticImport {
    pub(super) alias: Option<String>,
    pub(super) path: Option<String>,
    pub(super) decoded_path: Option<String>,
    pub(super) spec: std::ops::Range<usize>,
    pub(super) literal: std::ops::Range<usize>,
    pub(super) binding: std::ops::Range<usize>,
}

pub(super) struct TextEditDelta<'a> {
    pub(super) range: Option<Range>,
    pub(super) bytes: std::ops::Range<usize>,
    pub(super) inserted: &'a str,
    pub(super) new_end_line: usize,
    pub(super) text: &'a str,
    pub(super) tree: &'a Tree,
}

#[derive(Default)]
struct HighlightNames(Mutex<HashMap<String, Arc<[usize]>>>);

pub(super) struct Source {
    pub(super) path: PathBuf,
    pub(super) text: Arc<String>,
    pub(super) lines: LineIndex,
    pub(super) tree: Tree,
    pub(super) imports: std::sync::Arc<[Import]>,
    pub(super) diagnostic_imports: std::sync::Arc<[DiagnosticImport]>,
    pub(super) editor_names: Arc<EditorNames>,
    pub(super) highlights: Arc<HighlightCache>,
    pub(super) outline: OnceCell<FileOutline>,
    reference_names: Arc<OnceLock<HashMap<String, Vec<usize>>>>,
    highlight_names: Arc<HighlightNames>,
    fingerprint: OnceCell<u64>,
}

pub(super) struct SourceRevision {
    path: PathBuf,
    text: Arc<String>,
    lines: LineIndex,
    tree: Tree,
    imports: Arc<[Import]>,
    diagnostic_imports: Arc<[DiagnosticImport]>,
    editor_names: Arc<EditorNames>,
    highlights: Arc<HighlightCache>,
    reference_names: Arc<OnceLock<HashMap<String, Vec<usize>>>>,
    highlight_names: Arc<HighlightNames>,
    fingerprint: Option<u64>,
}

impl SourceRevision {
    pub(super) fn into_source(self) -> Source {
        Source {
            path: self.path,
            text: self.text,
            lines: self.lines,
            tree: self.tree,
            imports: self.imports,
            diagnostic_imports: self.diagnostic_imports,
            editor_names: self.editor_names,
            highlights: self.highlights,
            outline: OnceCell::new(),
            reference_names: self.reference_names,
            highlight_names: self.highlight_names,
            fingerprint: self.fingerprint.map_or_else(OnceCell::new, OnceCell::from),
        }
    }
}

impl Source {
    pub(super) fn parse(
        parser: &mut Parser,
        path: PathBuf,
        text: String,
        previous: Option<&Tree>,
    ) -> Result<Self> {
        let lines = LineIndex::new(&text);
        Self::parse_with_lines(parser, path, Arc::new(text), lines, previous)
    }

    fn parse_with_lines(
        parser: &mut Parser,
        path: PathBuf,
        text: Arc<String>,
        lines: LineIndex,
        previous: Option<&Tree>,
    ) -> Result<Self> {
        let tree = parser
            .parse(text.as_bytes(), previous)
            .context("parsing Go source")?;
        Ok(Self::from_tree(path, text, lines, tree))
    }

    pub(super) fn parse_declarations(
        parser: &mut Parser,
        path: PathBuf,
        text: String,
        bodies: &[std::ops::Range<usize>],
    ) -> Result<(Self, bool)> {
        let lines = LineIndex::new(&text);
        if !bodies.is_empty() {
            // Keep both braces and all original byte/UTF-16 offsets. The
            // declaration grammar and source text are otherwise unchanged.
            let mut start = 0;
            let mut included = Vec::with_capacity(bodies.len() + 1);
            for body in bodies {
                included.push(tree_sitter::Range {
                    start_byte: start,
                    end_byte: body.start,
                    start_point: lines.point(start),
                    end_point: lines.point(body.start),
                });
                start = body.end;
            }
            included.push(tree_sitter::Range {
                start_byte: start,
                end_byte: text.len(),
                start_point: lines.point(start),
                end_point: lines.point(text.len()),
            });
            if parser.set_included_ranges(&included).is_ok() {
                let tree = parser.parse(&text, None);
                parser
                    .set_included_ranges(&[])
                    .expect("resetting Tree-sitter input ranges");
                if let Some(tree) = tree.filter(|tree| !tree.root_node().has_error()) {
                    return Ok((Self::from_tree(path, Arc::new(text), lines, tree), true));
                }
            }
        }
        Self::parse_with_lines(parser, path, Arc::new(text), lines, None)
            .map(|source| (source, false))
    }

    fn from_tree(path: PathBuf, text: Arc<String>, lines: LineIndex, tree: Tree) -> Self {
        let (imports, diagnostic_imports, editor_names) =
            source_names(tree.root_node(), &text, &lines);
        Self {
            path,
            text,
            lines,
            tree,
            imports: imports.into(),
            diagnostic_imports: diagnostic_imports.into(),
            editor_names: Arc::new(editor_names),
            highlights: Arc::default(),
            outline: OnceCell::new(),
            reference_names: Arc::default(),
            highlight_names: Arc::default(),
            fingerprint: OnceCell::new(),
        }
    }

    pub(super) fn fingerprint(&self) -> u64 {
        *self
            .fingerprint
            .get_or_init(|| super::index::text_fingerprint(&self.text))
    }

    pub(super) fn revision(&self) -> SourceRevision {
        SourceRevision {
            path: self.path.clone(),
            text: Arc::clone(&self.text),
            lines: self.lines.clone(),
            tree: self.tree.clone(),
            imports: self.imports.clone(),
            diagnostic_imports: self.diagnostic_imports.clone(),
            editor_names: self.editor_names.clone(),
            highlights: self.highlights.clone(),
            reference_names: self.reference_names.clone(),
            highlight_names: self.highlight_names.clone(),
            fingerprint: self.fingerprint.get().copied(),
        }
    }

    pub(super) fn read_cancellable(
        parser: &mut Parser,
        path: &Path,
        cancelled: &AtomicBool,
    ) -> Result<Self> {
        check_cancelled(cancelled)?;
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse_cancellable(parser, path, Arc::new(text), cancelled)
    }

    pub(super) fn parse_cancellable(
        parser: &mut Parser,
        path: &Path,
        text: Arc<String>,
        cancelled: &AtomicBool,
    ) -> Result<Self> {
        check_cancelled(cancelled)?;
        let mut progress = |_: &tree_sitter::ParseState| cancelled.load(Ordering::Relaxed);
        let options = tree_sitter::ParseOptions::new().progress_callback(&mut progress);
        let tree = parser.parse_with_options(
            &mut |offset, _| &text.as_bytes()[offset.min(text.len())..],
            None,
            Some(options),
        );
        if tree.is_none() {
            parser.reset();
        }
        let tree = tree.context("parsing Go source")?;
        check_cancelled(cancelled)?;
        let lines = LineIndex::new(&text);
        let source = Self::from_tree(path.to_owned(), text, lines, tree);
        check_cancelled(cancelled)?;
        Ok(source)
    }

    // The observer sees each validated change against the current working copy.
    // Its decisions should be applied only after the entire batch succeeds.
    pub(super) fn edit(
        &mut self,
        parser: &mut Parser,
        changes: &[TextDocumentContentChangeEvent],
        mut observe: Option<&mut dyn FnMut(TextEditDelta<'_>) -> bool>,
    ) -> Result<()> {
        let mut text = self.text.as_ref().clone();
        let mut tree = self.tree.clone();
        let mut lines = self.lines.clone();
        for change in changes {
            let (start, end) = if let Some(range) = change.range {
                (
                    lines
                        .offset(&text, range.start)
                        .context("invalid edit start")?,
                    lines.offset(&text, range.end).context("invalid edit end")?,
                )
            } else {
                (0, text.len())
            };
            if start > end {
                bail!("edit ends before it begins");
            }

            let start_position = lines.point(start);
            let new_end_position = match change.text.rfind('\n') {
                Some(last_break) => Point::new(
                    start_position.row + change.text.bytes().filter(|byte| *byte == b'\n').count(),
                    change.text.len() - last_break - 1,
                ),
                None => Point::new(
                    start_position.row,
                    start_position.column + change.text.len(),
                ),
            };
            if let Some(observer) = &mut observe
                && !observer(TextEditDelta {
                    range: change.range,
                    bytes: start..end,
                    inserted: &change.text,
                    new_end_line: new_end_position.row,
                    text: &text,
                    tree: &tree,
                })
            {
                observe = None;
            }
            tree.edit(&InputEdit {
                start_byte: start,
                old_end_byte: end,
                new_end_byte: start + change.text.len(),
                start_position,
                old_end_position: lines.point(end),
                new_end_position,
            });
            lines.edit(start, end, &change.text);
            text.replace_range(start..end, &change.text);
        }

        *self = Self::parse_with_lines(
            parser,
            self.path.clone(),
            Arc::new(text),
            lines,
            Some(&tree),
        )?;
        Ok(())
    }

    pub(super) fn reference_at(&self, position: Position) -> Option<ReferenceQuery<'_>> {
        let offset = self.lines.offset(&self.text, position)?;
        self.reference_at_offset(offset)
    }

    pub(super) fn reference_at_offset(&self, offset: usize) -> Option<ReferenceQuery<'_>> {
        let root = self.tree.root_node();
        let leaf = root.named_descendant_for_byte_range(offset, offset)?;
        self.reference_from_leaf(leaf, offset)
    }

    pub(super) fn reference_at_spelling(
        &self,
        offset: usize,
        spelling: &str,
    ) -> Option<ReferenceQuery<'_>> {
        let end = offset.checked_add(spelling.len())?;
        let root = self.tree.root_node();
        let ascii_letters = |byte: u8| byte.is_ascii_alphabetic() || byte == b'_';
        if spelling.bytes().all(ascii_letters)
            && !root.has_error()
            && let Some(leaf) = root.named_descendant_for_byte_range(offset, offset)
            && is_offset_identifier(leaf)
            && leaf.child_count() == 0
            && leaf.start_byte() <= offset
            && end <= leaf.end_byte()
            && node_text(leaf, &self.text).bytes().all(ascii_letters)
        {
            // A parser-confirmed identifier token has no nested identifiers.
            // Reject its ASCII substrings without making assumptions about
            // token boundaries in a recovery node or around a number.
            return (leaf.start_byte() == offset
                && leaf.end_byte() == end
                && node_text(leaf, &self.text) == spelling)
                .then(|| self.reference_from_leaf(leaf, offset))
                .flatten();
        }
        let node = root.named_descendant_for_byte_range(offset, end)?;
        // Match the same node kinds and spelling as the full offsets table.
        // Looking up the full range rejects substrings and matches in strings
        // before entering the existing point-based binding lookup.
        if node.start_byte() == offset
            && node.end_byte() == end
            && is_offset_identifier(node)
            && node_text(node, &self.text) == spelling
        {
            return self.reference_at_offset(offset);
        }
        None
    }

    pub(super) fn for_each_name<'tree>(
        &self,
        root: Node<'tree>,
        spelling: &str,
        visit: impl FnMut(Node<'tree>),
    ) {
        self.for_each_name_cancellable(root, spelling, None, visit);
    }

    pub(super) fn for_each_name_cancellable<'tree>(
        &self,
        root: Node<'tree>,
        spelling: &str,
        cancelled: Option<&AtomicBool>,
        mut visit: impl FnMut(Node<'tree>),
    ) {
        Self::for_each_identifier(root, cancelled, |node| {
            if node_text(node, &self.text) == spelling {
                visit(node);
            }
        });
    }

    pub(super) fn name_offsets(&self, spelling: &str) -> &[usize] {
        self.navigation_name_offsets(spelling, None)
    }

    pub(super) fn navigation_name_offsets(
        &self,
        spelling: &str,
        probe: Option<&super::navigation_trace::NavigationProbe>,
    ) -> &[usize] {
        self.reference_names
            .get_or_init(|| {
                super::navigation_trace::record(probe, |counts| counts.offset_tables += 1);
                let mut names = HashMap::<String, Vec<usize>>::new();
                Self::for_each_identifier(self.tree.root_node(), None, |node| {
                    let name = node_text(node, &self.text);
                    if let Some(offsets) = names.get_mut(name) {
                        offsets.push(node.start_byte());
                    } else {
                        names.insert(name.to_owned(), vec![node.start_byte()]);
                    }
                });
                names
            })
            .get(spelling)
            .map_or(&[], Vec::as_slice)
    }

    pub(super) fn ready_name_offsets(&self, spelling: &str) -> Option<&[usize]> {
        let names = self.reference_names.get()?;
        Some(names.get(spelling).map_or(&[], Vec::as_slice))
    }

    pub(super) fn highlight_name_offsets(
        &self,
        spelling: &str,
        cancelled: Option<&AtomicBool>,
    ) -> Option<Arc<[usize]>> {
        if let Some(offsets) = self
            .highlight_names
            .0
            .lock()
            .unwrap()
            .get(spelling)
            .cloned()
        {
            return Some(offsets);
        }
        // Seed just the requested spelling on a cold revision. The caller
        // verifies each slice against the syntax tree and canonical binding.
        const CHUNK_BYTES: usize = 64 * 1024;
        let text = self.text.as_str();
        let Some(first) = spelling.chars().next() else {
            return Some(Arc::default());
        };
        let next_boundary = |mut byte| {
            while !text.is_char_boundary(byte) {
                byte += 1;
            }
            byte
        };
        let mut offsets = Vec::new();
        let check_cancelled = || !cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed));
        for start in (0..text.len()).step_by(CHUNK_BYTES) {
            if !check_cancelled() {
                return None;
            }
            let end = (start + CHUNK_BYTES).min(text.len());
            // Include matches that straddle a cancellation checkpoint once.
            let overlap = next_boundary(
                end.saturating_add(spelling.len().saturating_sub(1))
                    .min(text.len()),
            );
            let mut next = next_boundary(start);
            while let Some(found) = text[next..overlap].find(spelling) {
                let offset = next + found;
                if offset >= end {
                    break;
                }
                if !check_cancelled() {
                    return None;
                }
                offsets.push(offset);
                next = offset + first.len_utf8();
            }
        }
        let offsets: Arc<[usize]> = offsets.into();
        const MAX_OFFSETS: usize = 131_072;
        if check_cancelled() && offsets.len() <= MAX_OFFSETS {
            let mut cache = self.highlight_names.0.lock().unwrap();
            let cached: usize = cache.values().map(|offsets| offsets.len()).sum();
            if cache.len() >= 32 || cached + offsets.len() > MAX_OFFSETS {
                cache.clear();
            }
            cache.insert(spelling.to_owned(), offsets.clone());
        }
        Some(offsets)
    }

    fn for_each_identifier<'tree>(
        root: Node<'tree>,
        cancelled: Option<&AtomicBool>,
        mut visit: impl FnMut(Node<'tree>),
    ) {
        let mut cursor = root.walk();
        loop {
            if cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
                return;
            }
            let node = cursor.node();
            if is_offset_identifier(node) {
                visit(node);
            }
            if cursor.goto_first_child() {
                continue;
            }
            while !cursor.goto_next_sibling() {
                if !cursor.goto_parent() {
                    return;
                }
            }
        }
    }

    pub(super) fn reference_from_leaf<'tree>(
        &self,
        leaf: Node<'tree>,
        offset: usize,
    ) -> Option<ReferenceQuery<'tree>> {
        let mut current = Some(leaf);
        while let Some(node) = current {
            let fields = match node.kind() {
                "selector_expression" => Some(("operand", "field")),
                "qualified_type" => Some(("package", "name")),
                _ => None,
            };
            if let Some((receiver_field, name_field)) = fields
                && let Some(name) = node.child_by_field_name(name_field)
                && name.start_byte() <= offset
                && offset <= name.end_byte()
            {
                let receiver = node.child_by_field_name(receiver_field)?;
                return Some(ReferenceQuery::Member { receiver, name });
            }
            if node.kind() == "import_spec" {
                if let Some(path) = node.child_by_field_name("path")
                    && path.start_byte() <= offset
                    && offset < path.end_byte()
                {
                    return Some(ReferenceQuery::Name(path));
                }
                break;
            }
            if node.kind() == "comment" {
                return None;
            }
            current = node.parent();
        }
        let parent = leaf.parent()?;
        let named = matches!(
            leaf.kind(),
            "identifier" | "type_identifier" | "package_identifier"
        );
        let field_key = parent.kind() == "literal_element"
            && parent.parent().is_some_and(|node| {
                node.kind() == "keyed_element" && node.child_by_field_name("key") == Some(parent)
            });
        if named && field_key {
            let value = parent.parent()?.parent()?;
            if value.kind() == "literal_value" {
                return Some(ReferenceQuery::LiteralKey { value, name: leaf });
            }
        }
        let predeclared_literal = matches!(leaf.kind(), "nil" | "true" | "false" | "iota");
        (named || predeclared_literal || is_declaration_name(leaf))
            .then_some(ReferenceQuery::Name(leaf))
    }
    pub(super) fn range(&self, node: Node<'_>) -> Range {
        Range::new(
            position_on_line(&self.text, node.start_position(), node.start_byte()),
            position_on_line(&self.text, node.end_position(), node.end_byte()),
        )
    }

    pub(super) fn byte_range(&self, range: std::ops::Range<usize>) -> Range {
        Range::new(
            self.lines.position(&self.text, range.start),
            self.lines.position(&self.text, range.end),
        )
    }

    pub(super) fn outline(&self) -> &FileOutline {
        self.outline
            .get_or_init(|| symbols::collect_outline(&self.tree, &self.text))
    }
}

fn is_offset_identifier(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "identifier" | "type_identifier" | "field_identifier" | "package_identifier"
    )
}

pub(super) fn import_path(literal: &str) -> Option<String> {
    if let Some(raw) = literal.strip_prefix('`').and_then(|s| s.strip_suffix('`')) {
        Some(raw.replace('\r', ""))
    } else {
        serde_json::from_str(literal).ok()
    }
}

// String escapes in Go include byte escapes, which can encode one piece of a
// UTF-8 character at a time. Decode before comparing import paths.
pub(super) fn decoded_import_path(literal: &str) -> Option<String> {
    if let Some(raw) = literal.strip_prefix('`').and_then(|s| s.strip_suffix('`')) {
        return Some(raw.replace('\r', ""));
    }
    let mut contents = literal.strip_prefix('"')?.strip_suffix('"')?;
    let mut decoded = Vec::with_capacity(contents.len());
    while let Some((prefix, escape)) = contents.split_once('\\') {
        decoded.extend_from_slice(prefix.as_bytes());
        let (kind, rest) = escape.split_at_checked(1)?;
        let (digits, unicode) = match kind.as_bytes()[0] {
            b'x' => (2, false),
            b'u' => (4, true),
            b'U' => (8, true),
            b'0'..=b'7' => {
                let (digits, rest) = escape.split_at_checked(3)?;
                decoded.push(u8::from_str_radix(digits, 8).ok()?);
                contents = rest;
                continue;
            }
            _ => {
                decoded.push(match kind {
                    "a" => 7,
                    "b" => 8,
                    "f" => 12,
                    "n" => b'\n',
                    "r" => b'\r',
                    "t" => b'\t',
                    "v" => 11,
                    "\\" => b'\\',
                    "\"" => b'"',
                    _ => return None,
                });
                contents = rest;
                continue;
            }
        };
        let (digits, rest) = rest.split_at_checked(digits)?;
        if !digits.bytes().all(|digit| digit.is_ascii_hexdigit()) {
            return None;
        }
        let value = u32::from_str_radix(digits, 16).ok()?;
        if unicode {
            decoded.extend_from_slice(char::from_u32(value)?.encode_utf8(&mut [0; 4]).as_bytes());
        } else {
            decoded.push(u8::try_from(value).ok()?);
        }
        contents = rest;
    }
    decoded.extend_from_slice(contents.as_bytes());
    String::from_utf8(decoded).ok()
}

fn source_names(
    root: Node<'_>,
    text: &str,
    lines: &LineIndex,
) -> (Vec<Import>, Vec<DiagnosticImport>, EditorNames) {
    let mut navigation = Vec::new();
    let mut diagnostics = Vec::new();
    let mut editor = EditorNames::default();
    let mut cursor = root.walk();
    for declaration in root.named_children(&mut cursor) {
        if declaration.kind() != "import_declaration" {
            editor.collect_declaration(declaration, text);
            continue;
        }
        let mut cursor = declaration.walk();
        let container = declaration
            .named_children(&mut cursor)
            .find(|node| node.kind() == "import_spec_list")
            .unwrap_or(declaration);
        let mut cursor = container.walk();
        for spec in container
            .named_children(&mut cursor)
            .filter(|node| node.kind() == "import_spec")
        {
            let Some(literal) = spec.child_by_field_name("path") else {
                continue;
            };
            let alias = spec.child_by_field_name("name");
            let binding = alias.unwrap_or(literal);
            let alias = alias.map(|node| node_text(node, text).to_owned());
            let spelling = node_text(literal, text);
            let path = import_path(spelling);
            let range = Range::new(
                lines.position(text, binding.start_byte()),
                lines.position(text, binding.end_byte()),
            );
            // Raw imports recognize Go's byte and octal escapes, including for
            // blank and dot imports. Navigation uses its existing path rules.
            let decoded_path = decoded_import_path(spelling);
            if let Some(decoded) = decoded_path.as_ref().filter(|_| !spec.has_error()) {
                let name = match alias.as_deref() {
                    Some("." | "_") => None,
                    Some(name) => Some(name),
                    None => implicit_import_name(decoded),
                };
                if let Some(name) = name {
                    editor
                        .imports
                        .entry(name.to_owned())
                        .and_modify(|entry| *entry = None)
                        .or_insert_with(|| Some(binding.byte_range()));
                }
                editor.raw_imports.push(Import {
                    alias: alias.clone(),
                    path: decoded.clone(),
                    binding: range,
                });
            }
            if let Some(path) = path.as_ref().filter(|_| alias.as_deref() != Some("_")) {
                navigation.push(Import {
                    alias: alias.clone(),
                    path: path.clone(),
                    binding: range,
                });
            }
            diagnostics.push(DiagnosticImport {
                alias,
                decoded_path: decoded_path.filter(|_| path.is_none()),
                path,
                spec: spec.byte_range(),
                literal: literal.byte_range(),
                binding: binding.byte_range(),
            });
        }
    }
    (navigation, diagnostics, editor)
}

fn implicit_import_name(path: &str) -> Option<&str> {
    let mut parts = path.rsplit('/');
    let mut name = parts.next()?;
    if name.strip_prefix('v').is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|digit| digit.is_ascii_digit())
    }) {
        name = parts.next()?;
    }
    let mut chars = name.chars();
    if !chars
        .next()
        .is_some_and(|ch| ch == '_' || ch.is_alphabetic())
        || !chars.all(|ch| ch == '_' || ch.is_alphanumeric())
        || name == "_"
    {
        return None;
    }
    Some(name)
}

pub(super) fn is_type_declaration(node: Node<'_>) -> bool {
    matches!(node.kind(), "type_spec" | "type_alias")
}

pub(super) fn is_declaration_name(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    is_declaration_name_in(node, parent)
}

pub(super) fn is_declaration_name_in(node: Node<'_>, parent: Node<'_>) -> bool {
    if parent.kind() == "import_spec" {
        return parent.child_by_field_name("path") == Some(node);
    }
    if !matches!(
        parent.kind(),
        "field_declaration"
            | "function_declaration"
            | "method_declaration"
            | "method_elem"
            | "parameter_declaration"
            | "variadic_parameter_declaration"
            | "type_parameter_declaration"
            | "type_alias"
            | "type_spec"
            | "const_spec"
            | "var_spec"
    ) {
        return false;
    }
    let mut cursor = parent.walk();
    parent
        .children_by_field_name("name", &mut cursor)
        .any(|name| name == node)
}

pub(super) fn node_text<'text>(node: Node<'_>, text: &'text str) -> &'text str {
    &text[node.byte_range()]
}

pub(super) fn check_cancelled(cancelled: &AtomicBool) -> Result<()> {
    anyhow::ensure!(!cancelled.load(Ordering::Relaxed), "diagnostics cancelled");
    Ok(())
}

#[derive(Clone)]
pub(super) struct LineIndex {
    starts: Arc<Vec<usize>>,
}

impl LineIndex {
    pub(super) fn new(text: &str) -> Self {
        let starts = std::iter::once(0)
            .chain(text.match_indices('\n').map(|(index, _)| index + 1))
            .collect();
        Self {
            starts: Arc::new(starts),
        }
    }

    pub(super) fn offset(&self, text: &str, position: Position) -> Option<usize> {
        let line = position.line as usize;
        let start = *self.starts.get(line)?;
        let end = self
            .starts
            .get(line + 1)
            .map_or(text.len(), |next| next - 1);
        let content = &text[start..end];
        let content = content.strip_suffix('\r').unwrap_or(content);
        let character = position.character as usize;
        if content.is_ascii() {
            return Some(start + character.min(content.len()));
        }
        let mut units = 0;
        for (index, ch) in content.char_indices() {
            if units == character {
                return Some(start + index);
            }
            units += ch.len_utf16();
            if units > character {
                return None;
            }
        }
        Some(start + content.len())
    }

    pub(super) fn point(&self, offset: usize) -> Point {
        let line = self.starts.partition_point(|start| *start <= offset) - 1;
        Point::new(line, offset - self.starts[line])
    }

    pub(super) fn position(&self, text: &str, offset: usize) -> Position {
        position_on_line(text, self.point(offset), offset)
    }

    pub(super) fn edit(&mut self, start: usize, end: usize, inserted: &str) {
        let starts = Arc::make_mut(&mut self.starts);
        let first = starts.partition_point(|offset| *offset <= start);
        let last = starts.partition_point(|offset| *offset <= end);
        for offset in &mut starts[last..] {
            *offset = *offset - (end - start) + inserted.len();
        }
        starts.splice(
            first..last,
            inserted
                .match_indices('\n')
                .map(|(index, _)| start + index + 1),
        );
    }
}

fn position_on_line(text: &str, point: Point, offset: usize) -> Position {
    let prefix = &text[offset - point.column..offset];
    let prefix = if text.as_bytes().get(offset) == Some(&b'\n') {
        prefix.strip_suffix('\r').unwrap_or(prefix)
    } else {
        prefix
    };
    let column = if prefix.is_ascii() {
        prefix.len()
    } else {
        prefix.encode_utf16().count()
    };
    Position::new(point.row as u32, column as u32)
}

pub(super) fn function_at(source: &Source, origin: Range) -> Option<Node<'_>> {
    let node = node_at_range(source, origin)?.parent()?;
    matches!(
        node.kind(),
        "function_declaration"
            | "method_declaration"
            | "method_elem"
            | "function_type"
            | "func_literal"
    )
    .then_some(node)
}

pub(super) fn node_at_range(source: &Source, range: Range) -> Option<Node<'_>> {
    let start = source.lines.offset(&source.text, range.start)?;
    let end = source.lines.offset(&source.text, range.end)?;
    let node = source
        .tree
        .root_node()
        .descendant_for_byte_range(start, end)?;
    (node.byte_range() == (start..end)).then_some(node)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::lexical_binding;

    fn parser() -> Parser {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
            .unwrap();
        parser
    }

    fn change(range: Option<Range>, text: &str) -> TextDocumentContentChangeEvent {
        TextDocumentContentChangeEvent {
            range,
            range_length: None,
            text: text.into(),
        }
    }

    #[test]
    fn import_qualifier_respects_shadowing_and_utf16() {
        let text = concat!(
            "package test\n",
            "import alias \"example.test/lib\"\n",
            "func a() { _ = \"😀\"; alias.Foo() }\n",
            "func b(alias T) { alias.Foo() }\n",
            "func c() { alias := T{}; alias.Foo() }\n",
            "func d() { if alias := T{}; true { alias.Foo() }; alias.Foo() }\n",
        );
        let source = Source::parse(&mut parser(), PathBuf::new(), text.into(), None).unwrap();
        let queries: Vec<_> = text
            .match_indices("Foo")
            .map(|(offset, _)| {
                let query = source.reference_at(source.lines.position(text, offset))?;
                let ReferenceQuery::Member { receiver, name } = query else {
                    return None;
                };
                let qualifier = node_text(receiver, text);
                lexical_binding(receiver, qualifier, text)
                    .is_none()
                    .then_some((qualifier, node_text(name, text)))
            })
            .collect();
        assert_eq!(
            queries,
            [
                Some(("alias", "Foo")),
                None,
                None,
                None,
                Some(("alias", "Foo"))
            ],
        );
        assert!(source.lines.offset(text, Position::new(2, 17)).is_none());
    }

    #[test]
    fn line_index_tracks_multiline_insertions_and_deletions() {
        for (text, start, end, insert) in [
            ("abc\ndef\n", 2, 6, "\n\n"),
            ("abc\ndef\n", 4, 4, "new\n"),
            ("abc\ndef\n", 0, 4, ""),
            ("abc\ndef\n", 0, 8, "single"),
            ("", 0, 0, "\r\n"),
        ] {
            let mut lines = LineIndex::new(text);
            lines.edit(start, end, insert);
            let mut edited = text.to_owned();
            edited.replace_range(start..end, insert);
            assert_eq!(lines.starts, LineIndex::new(&edited).starts);
        }
    }

    #[test]
    fn observes_batch_coordinates_in_utf16_and_preserves_crlf() {
        let mut parser = parser();
        let original = "package p\r\nvar S = \"😀\"\r\nvar Tail int\r\n";
        let mut source = Source::parse(&mut parser, PathBuf::new(), original.into(), None).unwrap();
        let after_insertion = "package p\r\n// top 😀\r\nvar S = \"😀\"\r\nvar Tail int\r\n";
        let after_emoji = "package p\r\n// top 😀\r\nvar S = \"x\"\r\nvar Tail int\r\n";
        let range = |line, from, to| {
            Some(Range::new(
                Position::new(line, from),
                Position::new(line, to),
            ))
        };
        let changes = [
            change(range(1, 0, 0), "// top 😀\r\n"),
            change(range(2, 9, 11), "x"),
            change(range(2, 99, 99), " // tail"),
        ];
        let expected = [
            (original, original.find("var S").unwrap(), "", 2),
            (
                after_insertion,
                after_insertion.find("\"😀").unwrap() + 1,
                "😀",
                2,
            ),
            (after_emoji, after_emoji.find("\"\r\n").unwrap() + 1, "", 2),
        ];
        let mut observed = 0;
        source
            .edit(
                &mut parser,
                &changes,
                Some(&mut |delta| {
                    let (before, start, replaced, end_line) = expected[observed];
                    assert_eq!(delta.range, changes[observed].range);
                    assert_eq!(delta.inserted, changes[observed].text);
                    assert_eq!(delta.text, before);
                    assert_eq!(delta.bytes, start..start + replaced.len());
                    assert_eq!(&delta.text[delta.bytes], replaced);
                    assert_eq!(delta.new_end_line, end_line);
                    assert_eq!(delta.tree.root_node().end_byte(), before.len());
                    observed += 1;
                    true
                }),
            )
            .unwrap();
        assert_eq!(observed, changes.len());
        assert_eq!(
            source.text.as_str(),
            "package p\r\n// top 😀\r\nvar S = \"x\" // tail\r\nvar Tail int\r\n"
        );
        assert!(!source.tree.root_node().has_error());
        assert_eq!(source.lines.starts, LineIndex::new(&source.text).starts);
    }

    #[test]
    fn rejects_a_surrogate_boundary_without_committing_earlier_edits() {
        let mut parser = parser();
        let original = "package p\nvar S = \"😀\"\n";
        let mut source = Source::parse(&mut parser, PathBuf::new(), original.into(), None).unwrap();
        let original_root = source.tree.root_node().id();
        let original_outline = std::ptr::from_ref(source.outline());
        let range = |line, character| {
            let position = Position::new(line, character);
            Some(Range::new(position, position))
        };
        let mut observed = 0;
        let error = source
            .edit(
                &mut parser,
                &[change(range(1, 0), "\n"), change(range(2, 10), "invalid")],
                Some(&mut |_| {
                    observed += 1;
                    true
                }),
            )
            .unwrap_err();
        assert_eq!(error.to_string(), "invalid edit start");
        assert_eq!(observed, 1);
        assert_eq!(source.text.as_str(), original);
        assert_eq!(source.tree.root_node().id(), original_root);
        assert_eq!(std::ptr::from_ref(source.outline()), original_outline);
        assert_eq!(source.lines.starts, LineIndex::new(original).starts);
    }

    #[test]
    fn reports_full_replacement_and_honors_the_observers_stop() {
        let mut parser = parser();
        let original = "package p\nvar Original int\n";
        let mut source = Source::parse(&mut parser, PathBuf::new(), original.into(), None).unwrap();
        let full_text = "package p\nvar Updated int\n";
        let changes = [
            change(None, full_text),
            change(
                Some(Range::new(Position::new(1, 4), Position::new(1, 11))),
                "Final",
            ),
        ];
        let mut observed = 0;
        source
            .edit(
                &mut parser,
                &changes,
                Some(&mut |delta| {
                    assert_eq!(delta.range, None);
                    assert_eq!(delta.bytes, 0..original.len());
                    assert_eq!(delta.inserted, full_text);
                    assert_eq!(delta.new_end_line, 2);
                    observed += 1;
                    false
                }),
            )
            .unwrap();
        assert_eq!(observed, 1);
        assert_eq!(source.text.as_str(), "package p\nvar Final int\n");
        assert!(!source.tree.root_node().has_error());
    }
}
