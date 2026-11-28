use std::collections::{HashMap, HashSet};
use std::fmt;
use std::ops::Range;

use std::sync::LazyLock;

use lsp_types::TextEdit;
use regex::Regex;
use tree_sitter::Node;

use crate::semantic::snapshot::DocumentSnapshot;
use crate::semantic::source::{decoded_import_path, node_text};

#[derive(Clone, Copy, Debug)]
pub(crate) enum ImportOperation<'a> {
    AddImport {
        path: &'a str,
        alias: Option<&'a str>,
    },
    CompleteImport {
        path: &'a str,
        alias: Option<&'a str>,
    },
    OrganizeImports {
        unused_specs: &'a [usize],
        local_prefixes: &'a [&'a str],
        package_names: &'a HashMap<&'a str, &'a str>,
    },
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ImportEditError {
    MissingPackageClause,
    InvalidImportPath,
    InvalidImportAlias,
    UnsafeSyntaxTree,
    ConflictingBinding {
        path: String,
        alias: Option<String>,
        existing_path: String,
        existing_alias: Option<String>,
    },
}

impl fmt::Display for ImportEditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingPackageClause => f.write_str("missing Go package clause"),
            Self::InvalidImportPath => f.write_str("invalid Go import path"),
            Self::InvalidImportAlias => f.write_str("invalid Go import alias"),
            Self::ConflictingBinding {
                path,
                alias,
                existing_path,
                existing_alias,
            } => {
                write!(
                    f,
                    "import {path:?} with alias {alias:?} conflicts with {existing_path:?} with alias {existing_alias:?}"
                )
            }
            Self::UnsafeSyntaxTree => {
                f.write_str("cannot safely edit the Go package or import declarations")
            }
        }
    }
}

impl std::error::Error for ImportEditError {}

pub(crate) fn import_edits(
    document: &DocumentSnapshot<'_>,
    operation: ImportOperation<'_>,
) -> Result<Vec<TextEdit>, ImportEditError> {
    let text = document.text();
    let eol = match text.split_once('\n') {
        Some((first, _)) if first.ends_with('\r') => "\r\n",
        _ => "\n",
    };
    let header = Header::parse(document)?;
    match operation {
        ImportOperation::AddImport { path, alias }
        | ImportOperation::CompleteImport { path, alias } => {
            let completion = matches!(operation, ImportOperation::CompleteImport { .. });
            header.add(document, path, alias, eol, completion)
        }
        ImportOperation::OrganizeImports {
            unused_specs,
            local_prefixes,
            package_names,
        } => {
            let unused: HashSet<_> = unused_specs.iter().copied().collect();
            let redundant = redundant_aliases(&header, &unused, package_names);
            let mut merged = ImportBlock::default();
            let mut sections: Vec<Range<usize>> = Vec::new();
            for decl in header.declarations {
                if decl.cgo() {
                    continue;
                }
                if let Some(previous) = sections.last_mut()
                    && text[previous.end..decl.bytes.start].trim().is_empty()
                {
                    previous.end = decl.bytes.end;
                } else {
                    sections.push(decl.bytes);
                }
                merged.leading.extend(decl.block.leading);
                merged.footer.extend(decl.block.footer);
                merged.trailing.extend(decl.block.trailing);
                for mut spec in decl.block.specs {
                    if spec.is_unused(&unused) {
                        continue;
                    }
                    if redundant.contains(&spec.start) {
                        spec.alias = None;
                    }
                    spec.group = classify(&spec.path, local_prefixes);
                    merged.specs.push(spec);
                }
            }
            merged.sort();
            merged.specs.dedup_by(|later, first| {
                if first.path == later.path && first.alias == later.alias {
                    first.leading.append(&mut later.leading);
                    first.trailing.append(&mut later.trailing);
                    true
                } else {
                    false
                }
            });
            let mut replacement = merged.render(eol, false);
            Ok(sections
                .into_iter()
                .filter_map(|bytes| replace(document, bytes, std::mem::take(&mut replacement), eol))
                .collect())
        }
    }
}

struct Header<'a> {
    package: Node<'a>,
    declarations: Vec<Declaration<'a>>,
}

impl<'a> Header<'a> {
    fn parse(document: &'a DocumentSnapshot<'_>) -> Result<Self, ImportEditError> {
        let text = document.text();
        let root = document.root_node();
        if root.is_error() {
            return Err(ImportEditError::UnsafeSyntaxTree);
        }
        let mut package = None;
        let mut declarations = Vec::new();
        let mut previous_end = 0;
        let mut in_body = false;
        for node in root.children(&mut root.walk()) {
            if !in_body && (node.is_missing() || node.is_error()) {
                return Err(ImportEditError::UnsafeSyntaxTree);
            }
            if !node.is_named() {
                continue;
            }
            match node.kind() {
                "comment" => continue,
                "package_clause" => {
                    if in_body || package.is_some() || node.has_error() {
                        return Err(ImportEditError::UnsafeSyntaxTree);
                    }
                    if !node.named_children(&mut node.walk()).any(|child| {
                        child.kind() == "package_identifier"
                            && node_text(child, text) != "_"
                            && valid_identifier(node_text(child, text))
                    }) {
                        return Err(ImportEditError::MissingPackageClause);
                    }
                    (previous_end, _) = declaration_tail(node, text);
                    package = Some(node);
                }
                "import_declaration" => {
                    if in_body || node.has_error() {
                        return Err(ImportEditError::UnsafeSyntaxTree);
                    }
                    package.ok_or(ImportEditError::MissingPackageClause)?;
                    let decl = Declaration::parse(node, text, previous_end)?;
                    previous_end = decl.bytes.end;
                    declarations.push(decl);
                }
                "ERROR" if error_contains_import(node) => {
                    return Err(ImportEditError::UnsafeSyntaxTree);
                }
                _ if package.is_none() => return Err(ImportEditError::MissingPackageClause),
                _ => in_body = true,
            }
        }
        Ok(Self {
            package: package.ok_or(ImportEditError::MissingPackageClause)?,
            declarations,
        })
    }

    fn add(
        &self,
        document: &DocumentSnapshot<'_>,
        path: &str,
        alias: Option<&str>,
        eol: &str,
        completion: bool,
    ) -> Result<Vec<TextEdit>, ImportEditError> {
        if !valid_path(path) {
            return Err(ImportEditError::InvalidImportPath);
        }
        if alias.is_some_and(|name| {
            path == "C" || name == "init" || (name != "." && !valid_identifier(name))
        }) {
            return Err(ImportEditError::InvalidImportAlias);
        }
        let specs = || self.declarations.iter().flat_map(|decl| &decl.block.specs);
        if specs().any(|s| {
            s.path == path && (s.alias == alias || (!completion && s.equivalent(path, alias)))
        }) {
            return Ok(Vec::new());
        }
        if let Some(existing) = specs().find(|s| binding_conflicts(s, path, alias)) {
            return Err(ImportEditError::ConflictingBinding {
                path: path.to_owned(),
                alias: alias.map(str::to_owned),
                existing_path: existing.path.clone(),
                existing_alias: existing.alias.map(str::to_owned),
            });
        }
        let text = document.text();
        let spec = match alias {
            Some(alias) => format!("{alias} \"{path}\""),
            None => format!("\"{path}\""),
        };
        let target = self
            .declarations
            .iter()
            .filter(|decl| path != "C" && !decl.cgo())
            .min_by_key(|decl| {
                let group = insertion_group(path, &decl.block.specs);
                (
                    decl.close.is_none(),
                    !decl.block.specs.iter().any(|spec| spec.group == group),
                )
            });
        let (bytes, new_text) = if let Some(decl) = target {
            let key = (insertion_group(path, &decl.block.specs), path, alias);
            if let Some(close) = decl.close {
                insert_in_block(&decl.block.specs, close, text, eol, &spec, key)
            } else {
                group_single(decl, text, eol, &spec, key, completion)
            }
        } else {
            insert_declaration(
                self.declarations.last().map_or(self.package, |d| d.node),
                text,
                eol,
                &spec,
            )
        };
        Ok(vec![TextEdit::new(document.byte_range(bytes), new_text)])
    }
}

struct Declaration<'a> {
    bytes: Range<usize>,
    node: Node<'a>,
    close: Option<Node<'a>>,
    block: ImportBlock<'a>,
}

impl<'a> Declaration<'a> {
    fn cgo(&self) -> bool {
        self.block.specs.iter().any(|s| s.path == "C")
    }

    fn parse(node: Node<'a>, text: &'a str, previous_end: usize) -> Result<Self, ImportEditError> {
        let mut start = node;
        let mut block = ImportBlock::default();
        while let Some(comment) = start.prev_named_sibling() {
            if comment.kind() != "comment"
                || comment.start_byte() < previous_end
                || comment.end_position().row + 1 < start.start_position().row
            {
                break;
            }
            block.leading.push(comment_text(comment, text));
            start = comment;
        }
        block.leading.reverse();
        let mut close = None;
        for child in node.named_children(&mut node.walk()) {
            match child.kind() {
                "comment" => block.leading.push(comment_text(child, text)),
                "import_spec" => {
                    let mut spec = Spec::parse(child, text)?;
                    block.leading.append(&mut spec.leading);
                    spec.leading = std::mem::take(&mut block.leading);
                    block.specs.push(spec);
                }
                "import_spec_list" => {
                    close = Some(
                        child
                            .children(&mut child.walk())
                            .find(|n| n.kind() == ")" && !n.is_missing())
                            .ok_or(ImportEditError::UnsafeSyntaxTree)?,
                    );
                    block.read_list(child, text)?;
                }
                _ => return Err(ImportEditError::UnsafeSyntaxTree),
            }
        }
        let (end, trailing) = declaration_tail(node, text);
        if let [spec] = &mut block.specs[..]
            && close.is_none()
        {
            spec.trailing = trailing;
        } else {
            block.trailing = trailing;
        }
        Ok(Self {
            bytes: start.start_byte()..end,
            node,
            close,
            block,
        })
    }
}

fn comment_text<'a>(node: Node<'_>, text: &'a str) -> &'a str {
    node_text(node, text).trim_end_matches('\r')
}

fn declaration_tail<'a>(mut node: Node<'_>, text: &'a str) -> (usize, Vec<&'a str>) {
    let mut comments = Vec::new();
    while let Some(next) = node.next_sibling() {
        if next.start_position().row != node.end_position().row {
            break;
        }
        match next.kind() {
            "comment" => comments.push(comment_text(next, text)),
            ";" => {}
            _ => break,
        }
        node = next;
    }
    let end = node.start_byte() + node_text(node, text).trim_end_matches('\r').len();
    (end, comments)
}

#[derive(Default)]
struct ImportBlock<'a> {
    leading: Vec<&'a str>,
    specs: Vec<Spec<'a>>,
    footer: Vec<&'a str>,
    trailing: Vec<&'a str>,
}

impl<'a> ImportBlock<'a> {
    fn read_list(&mut self, list: Node<'_>, text: &'a str) -> Result<(), ImportEditError> {
        let mut previous_end = list
            .child(0)
            .map_or(list.start_byte(), |open| trailing_end(open, text));
        let mut pending = Vec::new();
        let mut last_line = list.start_position().row;
        let mut gap = false;
        let mut seen_external = false;
        let mut in_local = false;
        for child in list.named_children(&mut list.walk()) {
            let line = child.start_position().row;
            gap |= line > last_line + 1;
            match child.kind() {
                "comment" => {
                    let comment = comment_text(child, text);
                    if line == last_line
                        && pending.is_empty()
                        && let Some(spec) = self.specs.last_mut()
                    {
                        spec.trailing.push(comment);
                    } else {
                        pending.push(comment);
                    }
                }
                "import_spec" => {
                    let mut spec = Spec::parse(child, text)?;
                    spec.bytes.start = leading_start(child, previous_end, text);
                    spec.indent = spec.indent.or_else(|| line_indent(text, spec.bytes.start));
                    previous_end = spec.bytes.end;
                    pending.append(&mut spec.leading);
                    spec.leading = std::mem::take(&mut pending);
                    in_local |= gap
                        && !self.specs.is_empty()
                        && (seen_external || spec.group == Group::Standard);
                    seen_external |= spec.group == Group::ThirdParty;
                    if in_local {
                        spec.group = Group::Local;
                    }
                    self.specs.push(spec);
                    gap = false;
                }
                _ => return Err(ImportEditError::UnsafeSyntaxTree),
            }
            last_line = child.end_position().row;
        }
        self.footer = pending;
        Ok(())
    }

    fn sort(&mut self) {
        self.specs
            .sort_by(|a, b| (a.group, &a.path, a.alias).cmp(&(b.group, &b.path, b.alias)));
    }

    fn render(&self, eol: &str, keep_group: bool) -> String {
        let mut lines: Vec<String> = self.leading.iter().map(|s| s.to_string()).collect();
        match self.specs.as_slice() {
            [] => {
                lines.extend(
                    self.footer
                        .iter()
                        .chain(&self.trailing)
                        .map(|s| s.to_string()),
                );
                return lines.join(eol);
            }
            [spec]
                if !keep_group
                    && self.leading.is_empty()
                    && spec.leading.is_empty()
                    && self.footer.is_empty() =>
            {
                lines.push(format!("import {}", spec.line(&self.trailing)));
                return lines.join(eol);
            }
            specs => {
                lines.push("import (".into());
                let mut previous = None;
                for spec in specs {
                    if previous.is_some_and(|group| group != spec.group) {
                        lines.push(String::new());
                    }
                    lines.extend(spec.leading.iter().map(|comment| format!("\t{comment}")));
                    lines.push(format!("\t{}", spec.line(&[])));
                    previous = Some(spec.group);
                }
                lines.extend(self.footer.iter().map(|comment| format!("\t{comment}")));
                lines.push(")".into());
            }
        }
        with_trailing_comments(lines.join(eol), self.trailing.iter().copied())
    }
}

struct Spec<'a> {
    path: String,
    alias: Option<&'a str>,
    literal: &'a str,
    start: usize,
    bytes: Range<usize>,
    indent: Option<&'a str>,
    leading: Vec<&'a str>,
    trailing: Vec<&'a str>,
    group: Group,
}

impl<'a> Spec<'a> {
    fn parse(node: Node<'_>, text: &'a str) -> Result<Self, ImportEditError> {
        let literal = node
            .child_by_field_name("path")
            .ok_or(ImportEditError::UnsafeSyntaxTree)?;
        let literal = node_text(literal, text);
        let path = decoded_import_path(literal).ok_or(ImportEditError::UnsafeSyntaxTree)?;
        if !valid_path(&path) {
            return Err(ImportEditError::InvalidImportPath);
        }
        let mut spec = Self {
            group: classify(&path, &[]),
            path,
            alias: node.child_by_field_name("name").map(|n| node_text(n, text)),
            literal,
            start: node.start_byte(),
            bytes: node.start_byte()..trailing_end(node, text),
            indent: line_indent(text, node.start_byte()),
            leading: Vec::new(),
            trailing: Vec::new(),
        };
        spec.leading.extend(
            node.named_children(&mut node.walk())
                .filter(|n| n.kind() == "comment")
                .map(|n| comment_text(n, text)),
        );
        Ok(spec)
    }

    fn binding(&self) -> &str {
        self.alias.unwrap_or_else(|| assumed_name(&self.path))
    }

    fn equivalent(&self, path: &str, alias: Option<&str>) -> bool {
        self.path == path
            && match (self.alias, alias) {
                (Some(name), None) | (None, Some(name)) => {
                    !matches!(name, "_" | ".") && name == assumed_name(path)
                }
                (a, b) => a == b,
            }
    }

    fn is_unused(&self, unused: &HashSet<usize>) -> bool {
        self.path != "C" && !matches!(self.alias, Some("_" | ".")) && unused.contains(&self.start)
    }

    fn line(&self, extra: &[&str]) -> String {
        let line = match self.alias {
            Some(name) => format!("{name} {}", self.literal),
            None => self.literal.to_string(),
        };
        with_trailing_comments(line, self.trailing.iter().chain(extra).copied())
    }
}

fn with_trailing_comments<'a>(
    mut line: String,
    comments: impl Iterator<Item = &'a str> + Clone,
) -> String {
    // A line comment would swallow the opening delimiter of a later block comment.
    for comment in comments
        .clone()
        .filter(|s| !s.starts_with("//"))
        .chain(comments.filter(|s| s.starts_with("//")))
    {
        line.push(' ');
        line.push_str(comment);
    }
    line
}

fn valid_path(path: &str) -> bool {
    const FORBIDDEN: &str = concat!(r##"!"#$%&'()*,:;<=>?[\]^{|}"##, "\x60\u{fffd}");
    static GRAPHIC: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\A[\p{L}\p{M}\p{N}\p{P}\p{S}]+\z").unwrap());
    !path.split('/').any(str::is_empty)
        && GRAPHIC.is_match(path)
        && !path.contains(|ch| FORBIDDEN.contains(ch))
}

fn valid_identifier(name: &str) -> bool {
    static IDENTIFIER: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\A[_\p{L}][_\p{L}\p{Nd}]*\z").unwrap());
    IDENTIFIER.is_match(name)
        && !matches!(
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
        )
}

fn assumed_name(path: &str) -> &str {
    let mut parts = path.rsplit('/');
    let mut name = parts.next().unwrap_or(path);
    if name.strip_prefix('v').is_some_and(|n| {
        !n.is_empty() && n != "1" && !n.starts_with('0') && n.bytes().all(|c| c.is_ascii_digit())
    }) && let Some(parent) = parts.next()
    {
        name = parent;
    }
    name.strip_prefix("go-").unwrap_or(name)
}

fn redundant_aliases(
    header: &Header<'_>,
    unused: &HashSet<usize>,
    package_names: &HashMap<&str, &str>,
) -> HashSet<usize> {
    let mut bindings = HashMap::new();
    let mut conflicts = HashSet::new();
    let mut renamed = HashSet::new();
    let mut redundant = Vec::new();
    for spec in header.declarations.iter().flat_map(|decl| {
        let cgo = decl.cgo();
        decl.block
            .specs
            .iter()
            .filter(move |spec| cgo || !spec.is_unused(unused))
    }) {
        if matches!(spec.alias, Some("_" | ".")) {
            continue;
        }
        let actual_name = package_names.get(spec.path.as_str()).copied();
        let name = spec.alias.or(actual_name).unwrap_or_else(|| spec.binding());
        if bindings
            .insert(name, &spec.path)
            .is_some_and(|p| p != &spec.path)
        {
            conflicts.insert(name);
        }
        if spec.alias.is_some() {
            if actual_name == Some(name) {
                redundant.push((spec.start, &spec.path, name));
            } else {
                renamed.insert(&spec.path);
            }
        }
    }
    redundant
        .into_iter()
        .filter(|(_, path, name)| !renamed.contains(path) && !conflicts.contains(name))
        .map(|(start, ..)| start)
        .collect()
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
enum Group {
    Standard,
    ThirdParty,
    Local,
}

fn classify(path: &str, local_prefixes: &[&str]) -> Group {
    if local_prefixes.iter().any(|prefix| {
        let prefix = prefix.trim_end_matches('/');
        !prefix.is_empty()
            && (path == prefix
                || path
                    .strip_prefix(prefix)
                    .is_some_and(|s| s.starts_with('/')))
    }) {
        Group::Local
    } else if path.split('/').next().is_some_and(|s| s.contains('.')) {
        Group::ThirdParty
    } else {
        Group::Standard
    }
}

fn insertion_group(path: &str, existing: &[Spec<'_>]) -> Group {
    let default = classify(path, &[]);
    // AddImport has no module prefixes. An existing paragraph with a shared path
    // prefix gives a new sibling the same placement in this document snapshot.
    existing
        .iter()
        .filter(|s| s.group == default || s.group == Group::Local)
        .map(|s| {
            let shared = path
                .split('/')
                .zip(s.path.split('/'))
                .take_while(|(a, b)| a == b)
                .count();
            (shared, std::cmp::Reverse(s.group))
        })
        .filter(|(shared, _)| *shared > 0)
        .max()
        .map_or(default, |(_, group)| group.0)
}

fn binding_conflicts(existing: &Spec<'_>, path: &str, alias: Option<&str>) -> bool {
    if existing.alias == Some("_") || alias == Some("_") {
        return false;
    }
    if existing.path == path {
        return existing.alias != alias;
    }
    // Default package names require package resolution, which the AST edit
    // contract leaves to the caller. Explicit named aliases are known here.
    alias.is_some_and(|alias| alias != "." && existing.alias == Some(alias))
}

fn error_contains_import(node: Node<'_>) -> bool {
    node.children(&mut node.walk())
        .any(|child| match child.kind() {
            "import" | "import_declaration" => true,
            "ERROR" => error_contains_import(child),
            _ => false,
        })
}

fn leading_start(node: Node<'_>, previous_end: usize, text: &str) -> usize {
    let mut start = node.start_byte();
    let mut sibling = node.prev_named_sibling();
    while let Some(comment) = sibling.filter(|node| node.kind() == "comment") {
        if comment.start_byte() < previous_end || has_blank_line(&text[comment.end_byte()..start]) {
            break;
        }
        start = comment.start_byte();
        sibling = comment.prev_named_sibling();
    }
    start
}

fn trailing_end(node: Node<'_>, text: &str) -> usize {
    let mut end = node.end_byte();
    let mut sibling = node.next_named_sibling();
    while let Some(comment) = sibling.filter(|node| node.kind() == "comment") {
        let gap = text[end..comment.start_byte()].trim_matches([' ', '\t']);
        if !(gap.is_empty() || (gap == ";" && node_text(comment, text).starts_with("//"))) {
            break;
        }
        end = comment.end_byte();
        if text[..end].ends_with('\r') && text.as_bytes().get(end) == Some(&b'\n') {
            end -= 1;
        }
        sibling = comment.next_named_sibling();
    }
    end
}

fn has_blank_line(gap: &str) -> bool {
    gap.split_inclusive('\n')
        .skip(1)
        .any(|line| line.ends_with('\n') && line.trim().is_empty())
}

type ImportKey<'a> = (Group, &'a str, Option<&'a str>);

impl Spec<'_> {
    fn key(&self) -> ImportKey<'_> {
        (self.group, &self.path, self.alias)
    }
}

fn insert_declaration(node: Node<'_>, text: &str, eol: &str, spec: &str) -> (Range<usize>, String) {
    let mut at = trailing_end(node, text);
    let rest = text[at..].trim_start_matches([' ', '\t']);
    if let Some(rest) = rest.strip_prefix(';') {
        at = text.len() - rest.len();
    }
    let rest = &text[at..];
    let next = rest.trim_start_matches([' ', '\t', '\r', '\n']);
    let gap = &rest[..rest.len() - next.len()];
    let newlines = gap.bytes().filter(|byte| *byte == b'\n').take(2).count();
    let (end, needed) = if next.is_empty() {
        (text.len(), 1)
    } else if newlines == 0 {
        (text.len() - next.len(), 2)
    } else {
        // Keep the original line's trailing spaces on that line.
        at += rest.len() - rest.trim_start_matches([' ', '\t']).len();
        (at, 2 - newlines)
    };
    (
        at..end,
        format!("{eol}{eol}import {spec}{}", eol.repeat(needed)),
    )
}

fn group_single(
    decl: &Declaration<'_>,
    text: &str,
    eol: &str,
    spec: &str,
    key: ImportKey<'_>,
    completion: bool,
) -> (Range<usize>, String) {
    let original = &decl.block.specs[0];
    let end = trailing_end(decl.node, text);
    let first_child = decl.node.named_child(0).expect("single import spec");
    let mut existing = String::new();
    let mut start = decl.node.start_byte();
    // Completion edits leave the declaration's documentation in place. A quick fix
    // moves that documentation with the original spec when constructing the group.
    if !completion && decl.bytes.start < start {
        let docs = text[decl.bytes.start..start].trim_end();
        existing.push_str(&docs.replace(eol, &format!("{eol}\t")));
        existing.push_str(eol);
        existing.push('\t');
        start = decl.bytes.start;
    }
    existing.push_str(&text[first_child.start_byte()..end]);
    let (first, second) = if key < original.key() {
        (spec, existing.as_str())
    } else {
        (existing.as_str(), spec)
    };
    let gap = if key.0 != original.group { eol } else { "" };
    (
        start..end,
        format!("import ({eol}\t{first}{eol}{gap}\t{second}{eol})"),
    )
}

fn insert_in_block(
    specs: &[Spec<'_>],
    close: Node<'_>,
    text: &str,
    eol: &str,
    spec: &str,
    key: ImportKey<'_>,
) -> (Range<usize>, String) {
    let indent = specs
        .iter()
        .filter_map(|entry| entry.indent)
        .find(|indent| !indent.is_empty())
        .unwrap_or("\t");
    let group = specs
        .chunk_by(|left, right| !has_blank_line(&text[left.bytes.end..right.bytes.start]))
        .find(|group| group.iter().any(|entry| entry.key().0 == key.0));
    if let Some(group) = group {
        if let Some(after) = group.iter().find(|entry| entry.key() > key) {
            return insert_before(after.bytes.start, text, eol, indent, spec, false);
        }
        return insert_after(
            group.last().unwrap().bytes.end,
            text,
            eol,
            indent,
            spec,
            false,
        );
    }
    if let Some(next) = specs.iter().find(|s| s.group > key.0) {
        return insert_before(next.bytes.start, text, eol, indent, spec, true);
    }
    match specs.last() {
        Some(last)
            if let Some(close_indent) = line_indent(text, close.start_byte())
                && text[last.bytes.end..close.start_byte()].trim().is_empty()
                && has_blank_line(&text[last.bytes.end..close.start_byte()]) =>
        {
            let at = close.start_byte() - close_indent.len();
            (at..at, format!("{indent}{spec}{eol}"))
        }
        Some(last) => insert_after(last.bytes.end, text, eol, indent, spec, true),
        _ => {
            let (at, prefix) = line_indent(text, close.start_byte())
                .map_or((close.start_byte(), eol), |indent| {
                    (close.start_byte() - indent.len(), "")
                });
            (at..at, format!("{prefix}{indent}{spec}{eol}"))
        }
    }
}

fn line_indent(text: &str, at: usize) -> Option<&str> {
    let line_start = text[..at].rfind('\n').map_or(0, |index| index + 1);
    let indent = &text[line_start..at];
    indent
        .bytes()
        .all(|byte| matches!(byte, b' ' | b'\t'))
        .then_some(indent)
}

fn insert_before(
    at: usize,
    text: &str,
    eol: &str,
    indent: &str,
    spec: &str,
    separate: bool,
) -> (Range<usize>, String) {
    let gap = if separate { eol } else { "" };
    if let Some(previous_indent) = line_indent(text, at) {
        let start = at - previous_indent.len();
        (start..start, format!("{indent}{spec}{eol}{gap}"))
    } else {
        (at..at, format!("{eol}{indent}{spec}{eol}{gap}{indent}"))
    }
}

fn insert_after(
    mut at: usize,
    text: &str,
    eol: &str,
    indent: &str,
    spec: &str,
    separate: bool,
) -> (Range<usize>, String) {
    let rest = text[at..].trim_start_matches([' ', '\t']);
    if rest.starts_with(['\r', '\n']) {
        at = text.len() - rest.len();
    }
    let gap = if separate { eol } else { "" };
    let suffix = if rest.starts_with(')') { eol } else { "" };
    (at..at, format!("{eol}{gap}{indent}{spec}{suffix}"))
}

fn whitespace_after(text: &str, end: usize) -> Range<usize> {
    let rest = &text[end..];
    let gap = rest.len() - rest.trim_start_matches([' ', '\t', '\r', '\n']).len();
    let len = if gap == rest.len() {
        gap
    } else {
        rest[..gap].rfind('\n').map_or(gap, |i| i + 1)
    };
    end..end + len
}

fn replace(
    document: &DocumentSnapshot<'_>,
    mut bytes: Range<usize>,
    mut new: String,
    eol: &str,
) -> Option<TextEdit> {
    let text = document.text();
    if new.is_empty() {
        let line_start = text[..bytes.start].rfind('\n').map_or(0, |i| i + 1);
        if text[line_start..bytes.start].trim().is_empty() {
            bytes.start = line_start;
        }
        bytes.end = whitespace_after(text, bytes.end).end;
        if bytes.end == text.len() {
            let prefix = &text[..bytes.start];
            let space = prefix.trim_end_matches([' ', '\t', '\r', '\n']).len();
            if let Some(first_break) = prefix[space..].find('\n') {
                bytes.start = space + first_break + 1;
            }
        }
    } else if let Some(declaration) = text[bytes.clone()].strip_suffix(';') {
        if declaration == new {
            return None;
        }
        let rest = &text[bytes.end..];
        let next = rest.trim_start_matches([' ', '\t']);
        if !next.is_empty() && !next.starts_with(['\r', '\n']) {
            new.push_str(eol);
            bytes.end += rest.len() - next.len();
        }
    }
    (text[bytes.clone()] != new).then(|| TextEdit::new(document.byte_range(bytes), new))
}

#[cfg(test)]
mod tests {
    use lsp_types::{Position, Range, TextEdit};

    use crate::semantic::Engine;
    use crate::semantic::actions::{ImportEditError, ImportOperation, import_edits};
    use crate::test_support::TestProject;

    fn apply(
        text: &str,
        operation: ImportOperation<'_>,
    ) -> Result<(Vec<TextEdit>, String), ImportEditError> {
        apply_with(text, |document| import_edits(document, operation))
    }

    fn apply_with(
        text: &str,
        edit: impl FnOnce(&super::DocumentSnapshot<'_>) -> Result<Vec<TextEdit>, ImportEditError>,
    ) -> Result<(Vec<TextEdit>, String), ImportEditError> {
        let project = TestProject::new();
        let path = project.root.join("p.go");
        let mut engine = Engine::new(project.root.clone());
        engine.open(path.clone(), text.into(), 1).unwrap();
        let document = engine.snapshot().document(&path).unwrap().unwrap();
        let edits = edit(&document)?;
        for pair in edits.windows(2) {
            assert!(
                pair[0].range.start < pair[1].range.start
                    && pair[0].range.end <= pair[1].range.start
            );
        }
        let mut result = text.to_owned();
        for edit in edits.iter().rev() {
            let start = document.offset(edit.range.start).unwrap();
            let end = document.offset(edit.range.end).unwrap();
            assert_eq!(document.byte_range(start..end), edit.range);
            result.replace_range(start..end, &edit.new_text);
        }
        Ok((edits, result))
    }

    fn add<'a>(path: &'a str, alias: Option<&'a str>) -> ImportOperation<'a> {
        ImportOperation::AddImport { path, alias }
    }

    fn check_add(before: &str, path: &str, alias: Option<&str>, after: &str) -> Vec<TextEdit> {
        let (edits, result) = apply(before, add(path, alias)).unwrap();
        assert_eq!(result, after);
        assert!(apply(after, add(path, alias)).unwrap().0.is_empty());
        edits
    }

    fn check_organize(
        before: &str,
        unused_paths: &[&str],
        local_prefixes: &[&str],
        after: &str,
    ) -> Vec<TextEdit> {
        let organize = |document: &super::DocumentSnapshot<'_>| {
            let header = super::Header::parse(document)?;
            let specs = header
                .declarations
                .iter()
                .flat_map(|decl| &decl.block.specs);
            let package_names = specs
                .clone()
                .map(|spec| (spec.path.as_str(), super::assumed_name(&spec.path)))
                .collect();
            let unused_specs: Vec<_> = specs
                .filter(|spec| unused_paths.contains(&spec.path.as_str()))
                .map(|spec| spec.start)
                .collect();
            import_edits(
                document,
                ImportOperation::OrganizeImports {
                    unused_specs: &unused_specs,
                    local_prefixes,
                    package_names: &package_names,
                },
            )
        };
        let (edits, result) = apply_with(before, organize).unwrap();
        assert_eq!(result, after);
        assert!(apply_with(after, organize).unwrap().0.is_empty());
        edits
    }

    #[test]
    fn adds_after_package_and_before_declaration_docs() {
        check_add("package p", "fmt", None, "package p\n\nimport \"fmt\"\n");
        check_add(
            "// License\npackage p // keep\n// F runs.\nfunc F() {}\n",
            "fmt",
            None,
            "// License\npackage p // keep\n\nimport \"fmt\"\n\n// F runs.\nfunc F() {}\n",
        );
        check_add(
            "package p /* one\n two */\n",
            "io",
            None,
            "package p /* one\n two */\n\nimport \"io\"\n",
        );
    }

    #[test]
    fn upgrades_single_import_and_moves_its_comments_with_it() {
        check_add(
            "package p\n\n// HTTP client.\nimport \"net/http\" // transport\n",
            "fmt",
            None,
            "package p\n\nimport (\n\t\"fmt\"\n\t// HTTP client.\n\t\"net/http\" // transport\n)\n",
        );
        check_add(
            "package p\nimport \"fmt\"\n",
            "example.org/log",
            Some("logger"),
            "package p\nimport (\n\t\"fmt\"\n\n\tlogger \"example.org/log\"\n)\n",
        );
    }

    #[test]
    fn inserts_into_all_three_existing_groups_by_path_segments() {
        let before = concat!(
            "package p\n\nimport (\n\t\"fmt\"\n\t\"os\"\n\n",
            "\t\"host.test/deps/x\"\n\n\t// Project package.\n\t\"host.test/project/z\" // Z\n)\n",
        );
        for (path, anchor, addition) in [
            ("net/http", "\t\"os\"", "\t\"net/http\"\n"),
            (
                "host.test/deps/a",
                "\t\"host.test/deps/x\"",
                "\t\"host.test/deps/a\"\n",
            ),
            (
                "another.test/x",
                "\t\"host.test/deps/x\"",
                "\t\"another.test/x\"\n",
            ),
            (
                "host.test/project/a",
                "\t// Project",
                "\t\"host.test/project/a\"\n",
            ),
        ] {
            check_add(
                before,
                path,
                None,
                &before.replace(anchor, &format!("{addition}{anchor}")),
            );
        }
    }

    #[test]
    fn adds_to_existing_parenthesized_declaration_without_organizing_other_imports() {
        check_add(
            "package p\nimport \"os\"\nimport ()\n",
            "io",
            None,
            "package p\nimport \"os\"\nimport (\n\t\"io\"\n)\n",
        );
    }

    #[test]
    fn equivalent_bindings_return_no_edits_and_explicit_aliases_stay_distinct() {
        for (import, path, alias) in [
            ("\"fmt\"", "fmt", None),
            ("fmt \"fmt\"", "fmt", None),
            (
                "\"host.test/go-client/v12\"",
                "host.test/go-client/v12",
                Some("client"),
            ),
            (
                "renamed \"host.test/client\"",
                "host.test/client",
                Some("renamed"),
            ),
            ("_ \"host.test/hooks\"", "host.test/hooks", Some("_")),
            (". \"host.test/dot\"", "host.test/dot", Some(".")),
        ] {
            let source = format!("package p\nimport {import}\n");
            assert!(check_add(&source, path, alias, &source).is_empty());
        }
        check_add(
            "package p\nimport _ \"fmt\"\n",
            "fmt",
            Some("f"),
            "package p\nimport (\n\t_ \"fmt\"\n\tf \"fmt\"\n)\n",
        );
    }

    #[test]
    fn organizes_multiple_declarations_and_preserves_comment_ownership() {
        check_organize(
            concat!(
                "package p\n\nimport (\n\t// Wire codec.\n\twire \"host.test/go-wire/v2\" // codec\n",
                "\t\"host.test/project/sub\"\n\t\"os\"\n\t\"fmt\" // formatter\n)\n",
                "import \"fmt\"\nimport _ \"host.test/hooks\"\nimport . \"host.test/dot\"\n",
                "import \"host.test/project2\"\nimport \"host.test/project\"\n\nvar _ = 1\n",
            ),
            &["os", "host.test/hooks", "host.test/dot"],
            &["host.test/project"],
            concat!(
                "package p\n\nimport (\n\t\"fmt\" // formatter\n\n",
                "\t. \"host.test/dot\"\n\t// Wire codec.\n\t\"host.test/go-wire/v2\" // codec\n",
                "\t_ \"host.test/hooks\"\n\t\"host.test/project2\"\n\n\t\"host.test/project\"\n",
                "\t\"host.test/project/sub\"\n)\n\nvar _ = 1\n",
            ),
        );
    }

    #[test]
    fn keeps_different_aliases_and_skips_conflicting_assumed_names() {
        let before = concat!(
            "package p\nimport (\n\tthing \"host.test/thing\"\n\ta \"host.test/thing\"\n",
            "\tsame \"one.test/same\"\n\t\"two.test/same\"\n\tlib \"host.test/lib/v3\"\n",
            "\t\"host.test/lib/v3\"\n\tv1 \"host.test/v1\"\n)\n",
        );
        check_organize(
            before,
            &[],
            &[],
            concat!(
                "package p\nimport (\n\t\"host.test/lib/v3\"\n\ta \"host.test/thing\"\n",
                "\tthing \"host.test/thing\"\n\t\"host.test/v1\"\n",
                "\tsame \"one.test/same\"\n\t\"two.test/same\"\n)\n",
            ),
        );
    }

    #[test]
    fn retains_all_comments_on_deduplicated_specs() {
        check_organize(
            "package p\nimport (\n\t// First\n\tf \"fmt\" /* one */\n\t// Second\n\tf \"fmt\" // two\n)\n",
            &[],
            &[],
            "package p\nimport (\n\t// First\n\t// Second\n\tf \"fmt\" /* one */ // two\n)\n",
        );
        check_organize(
            "package p\nimport (\n\t\"fmt\" // one\n\t\"fmt\" /* two\n\tmore */\n) // three\n",
            &[],
            &[],
            "package p\nimport \"fmt\" /* two\n\tmore */ // one // three\n",
        );
    }

    #[test]
    fn groups_exact_and_child_paths_for_multiple_local_prefixes() {
        check_organize(
            "package p\nimport (\n\t\"local/util\"\n\t\"app.test/mod2\"\n\t\"fmt\"\n\t\"app.test/mod\"\n)\n",
            &[],
            &["app.test/mod", "local/"],
            "package p\nimport (\n\t\"fmt\"\n\n\t\"app.test/mod2\"\n\n\t\"app.test/mod\"\n\t\"local/util\"\n)\n",
        );
        let grouped =
            "package p\nimport (\n\t\"fmt\"\n\n\t\"app.test/mod2\"\n\n\t\"local/util\"\n)\n";
        check_add(
            grouped,
            "local/app",
            None,
            &grouped.replace("\t\"local/util\"", "\t\"local/app\"\n\t\"local/util\""),
        );
    }

    #[test]
    fn cgo_preamble_and_declaration_are_outside_every_edit() {
        let cgo = "// #cgo CFLAGS: -std=c11\n/*\n#include <stdio.h>\n*/\nimport  \"C\" /* keep */";
        let before =
            format!("package p\n\nimport \"os\"\n\n{cgo}\n\nimport \"fmt\"\n\nvar _ C.int\n");
        let after =
            format!("package p\n\nimport (\n\t\"fmt\"\n\t\"os\"\n)\n\n{cgo}\n\nvar _ C.int\n");
        let edits = check_organize(&before, &["C"], &[], &after);
        assert_eq!(edits.len(), 2);
        assert!(edits[0].range.end < Position::new(4, 0));
        assert!(edits[1].range.start > Position::new(8, 0));
        let only_cgo = format!("package p\n\n{cgo}\n");
        check_add(
            &only_cgo,
            "fmt",
            None,
            &format!("{only_cgo}\nimport \"fmt\"\n"),
        );
        assert!(check_organize(&only_cgo, &["C"], &[], &only_cgo).is_empty());
    }

    #[test]
    fn adds_cgo_separately_and_leaves_parenthesized_cgo_in_place() {
        check_add(
            "package p\nimport \"fmt\"\n",
            "C",
            None,
            "package p\nimport \"fmt\"\n\nimport \"C\"\n",
        );
        let before = "package p\n\nimport (\n\t// #include <stdlib.h>\n\t\"C\"\n\t\"os\"\n)\n";
        assert!(check_organize(before, &["C"], &[], before).is_empty());
        check_add(before, "fmt", None, &format!("{before}\nimport \"fmt\"\n"));
    }

    #[test]
    fn cleans_up_empty_declarations_and_collapses_one_surviving_spec() {
        check_organize(
            "package p\n\nimport ()\n\nfunc f() {}\n",
            &[],
            &[],
            "package p\n\nfunc f() {}\n",
        );
        check_organize(
            "package p\n\nimport \"os\"\n\nimport ()\n",
            &["os"],
            &[],
            "package p\n",
        );
        check_organize(
            "package p\nimport (\n\t\"fmt\" // output\n\t\"os\"\n)\n",
            &["os"],
            &[],
            "package p\nimport \"fmt\" // output\n",
        );
        let canonical =
            "package p\n\nimport (\n\t\"fmt\"\n\n\t\"external.test/x\"\n\n\t\"here.test/app\"\n)\n";
        assert!(check_organize(canonical, &[], &["here.test/app"], canonical).is_empty());
    }

    #[test]
    fn collapsing_before_an_inline_declaration_keeps_code_after_a_line_comment() {
        check_organize(
            "package p; import (\n\t\"fmt\" // output\n); var x = fmt.Sprint\n",
            &[],
            &[],
            "package p; import \"fmt\" // output\nvar x = fmt.Sprint\n",
        );
        check_organize(
            "package p; import \"os\"; var x int\n",
            &["os"],
            &[],
            "package p; var x int\n",
        );
    }

    #[test]
    fn preserves_standalone_comments_at_declaration_boundaries() {
        check_organize(
            "package p\n\nimport \"os\"\n\n// File note.\n\nimport \"fmt\"\n\nfunc f() {}\n",
            &[],
            &[],
            "package p\n\nimport (\n\t\"fmt\"\n\t\"os\"\n)\n\n// File note.\n\nfunc f() {}\n",
        );
        check_organize(
            "package p\n\nimport (\n\t// File note.\n)\n",
            &[],
            &[],
            "package p\n\n// File note.\n",
        );
        let documented = "package p\n// IO dependencies.\nimport (\n\t\"fmt\"\n)\n";
        assert!(check_organize(documented, &[], &[], documented).is_empty());
    }

    #[test]
    fn preserves_crlf_and_measures_both_edit_endpoints_in_utf16() {
        let edits = check_add(
            "package p // 😀\r\n\r\n// F.\r\nfunc F() {}\r\n",
            "fmt",
            None,
            "package p // 😀\r\n\r\nimport \"fmt\"\r\n\r\n// F.\r\nfunc F() {}\r\n",
        );
        assert_eq!(edits[0].range.start, Position::new(0, 15));
        let before = "package 𐐀; import \"os\" // 🧪";
        let after = "package 𐐀; import (\r\n\t\"fmt\"\r\n\t\"os\" // 🧪\r\n)";
        let edits = check_add(
            &format!("{before}\r\n"),
            "fmt",
            None,
            &format!("{after}\r\n"),
        );
        assert_eq!(
            edits[0].range,
            Range::new(
                Position::new(0, 12),
                Position::new(0, before.encode_utf16().count() as u32)
            )
        );
    }

    #[test]
    fn sorts_by_decoded_paths_and_preserves_raw_and_escaped_spellings() {
        let before = "package p\nimport (\n\t\x60os\x60\n\t\"\\x66\\155\\u0074\"\n)\n";
        assert!(check_add(before, "fmt", None, before).is_empty());
        check_organize(
            before,
            &[],
            &[],
            "package p\nimport (\n\t\"\\x66\\155\\u0074\"\n\t\x60os\x60\n)\n",
        );
        let cgo = "package p\n// #include <stdio.h>\nimport \"\\x43\"\n";
        assert!(check_organize(cgo, &["C"], &[], cgo).is_empty());
        let escaped = "package p\nimport \"host.test/\\x63afe\\xc3\\xa9\"\n";
        assert!(check_add(escaped, "host.test/cafeé", None, escaped).is_empty());
    }

    #[test]
    fn rejects_missing_or_unsafe_package_and_import_regions() {
        for source in ["", "// No package\n", "import \"fmt\"\n"] {
            assert_eq!(
                apply(source, add("fmt", None)),
                Err(ImportEditError::MissingPackageClause),
                "{source:?}"
            );
        }
        for source in [
            "package\n",
            "package @\n",
            "package p import \"fmt\"\n",
            "package\nimport \"fmt\"\n",
            "package p\nimport (\n\t\"fmt\"\n",
            "package p\nimport @\n",
            "package p\nimport \"fmt\"\nimport (\n\tbad\n)\n",
            "package p\nimport \"unterminated\n",
            "package p\nvar x int\nimport \"fmt\"\n",
        ] {
            for operation in [
                add("fmt", None),
                ImportOperation::OrganizeImports {
                    unused_specs: &[],
                    local_prefixes: &[],
                    package_names: &Default::default(),
                },
            ] {
                assert_eq!(
                    apply(source, operation),
                    Err(ImportEditError::UnsafeSyntaxTree),
                    "{source:?}"
                );
            }
        }
    }

    #[test]
    fn incomplete_function_bodies_allow_import_completion() {
        check_add(
            "package p\n\nfunc f() { value. }\n",
            "fmt",
            None,
            "package p\n\nimport \"fmt\"\n\nfunc f() { value. }\n",
        );
    }

    #[test]
    fn rejects_invalid_paths_and_aliases_before_emitting_edits() {
        for path in ["", "x y", "x\\y", "x\nvar", "a\"b", "x//y", "/absolute"] {
            assert_eq!(
                apply("package p", add(path, None)),
                Err(ImportEditError::InvalidImportPath)
            );
        }
        for alias in ["", "7f", "import", "log/path", "x\ny"] {
            assert_eq!(
                apply("package p", add("fmt", Some(alias))),
                Err(ImportEditError::InvalidImportAlias)
            );
        }
        assert_eq!(
            apply("package p\nimport \"bad path\"\n", add("fmt", None)),
            Err(ImportEditError::InvalidImportPath)
        );
    }
}

#[cfg(test)]
mod completion_tests {
    use lsp_types::Position;

    use super::*;
    use crate::semantic::Engine;
    use crate::test_support::TestProject;

    fn with_document<T>(text: &str, f: impl FnOnce(&DocumentSnapshot<'_>) -> T) -> T {
        let project = TestProject::new();
        let path = project.root.join("sample.go");
        let mut engine = Engine::new(project.root.clone());
        engine.open(path.clone(), text.into(), 1).unwrap();
        let mut snapshot = engine.snapshot();
        let document = snapshot.document(&path).unwrap().unwrap();
        f(&document)
    }

    fn add(text: &str, path: &str, alias: Option<&str>) -> Result<Vec<TextEdit>, ImportEditError> {
        with_document(text, |doc| {
            import_edits(doc, ImportOperation::CompleteImport { path, alias })
        })
    }

    fn byte_offset(text: &str, position: Position) -> usize {
        let start = std::iter::once(0)
            .chain(text.match_indices('\n').map(|(byte, _)| byte + 1))
            .nth(position.line as usize)
            .expect("valid line");
        let line = text[start..]
            .split('\n')
            .next()
            .unwrap()
            .trim_end_matches('\r');
        let mut column = 0;
        for (byte, ch) in line.char_indices() {
            if column == position.character {
                return start + byte;
            }
            column += ch.len_utf16() as u32;
        }
        assert_eq!(column, position.character, "valid UTF-16 boundary");
        start + line.len()
    }

    fn apply(text: &str, edits: &[TextEdit]) -> String {
        let mut output = String::new();
        let mut from = 0;
        for edit in edits {
            let start = byte_offset(text, edit.range.start);
            let end = byte_offset(text, edit.range.end);
            assert!(from <= start && start <= end, "ordered, disjoint edits");
            output.push_str(&text[from..start]);
            output.push_str(&edit.new_text);
            from = end;
        }
        output.push_str(&text[from..]);
        output
    }

    fn assert_added(text: &str, path: &str, alias: Option<&str>, expected: &str) {
        let edits = add(text, path, alias).unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(apply(text, &edits), expected);
        assert_eq!(add(expected, path, alias).unwrap(), []);
    }

    #[test]
    fn needs_a_valid_package_before_any_declarations() {
        for text in [
            "",
            "// no package\n",
            "var x int\npackage p\n",
            "package _\n",
        ] {
            assert_eq!(
                add(text, "fmt", None),
                Err(ImportEditError::MissingPackageClause),
                "{text:?}"
            );
        }
        assert_eq!(
            add("package\n", "fmt", None),
            Err(ImportEditError::UnsafeSyntaxTree)
        );
    }

    #[test]
    fn inserts_first_import_with_a_single_trailing_newline_at_eof() {
        assert_added(
            "package p",
            "io",
            Some("io"),
            "package p\n\nimport io \"io\"\n",
        );
        assert_added(
            "package p\n",
            "io",
            Some("名٣"),
            "package p\n\nimport 名٣ \"io\"\n",
        );
        for eol in ["\n", "\r\n"] {
            for suffix in [eol.to_owned(), eol.repeat(2)] {
                let text = format!("package p{suffix}");
                let expected = format!("package p{eol}{eol}import io \"io\"{eol}");
                assert_added(&text, "io", Some("io"), &expected);
            }
        }
    }

    #[test]
    fn separates_package_and_declaration_comments_in_lf_and_crlf() {
        let text =
            "package /* 🧠猫 */ p // trailing 🦀\n// Doc belongs to the function.\nfunc F() {}\n";
        let expected = "package /* 🧠猫 */ p // trailing 🦀\n\nimport \"fmt\"\n\n// Doc belongs to the function.\nfunc F() {}\n";
        assert_added(text, "fmt", None, expected);
        assert_added(
            &text.replace('\n', "\r\n"),
            "fmt",
            None,
            &expected.replace('\n', "\r\n"),
        );
        assert_added(
            "package p; /* decl docs */ var a int\n",
            "fmt",
            None,
            "package p;\n\nimport \"fmt\"\n\n/* decl docs */ var a int\n",
        );
    }

    #[test]
    fn groups_single_import_with_its_exact_alias_literal_and_comments() {
        let text = concat!(
            "package p\n\n// import declaration docs\n",
            "import /* 🐱 lead */ alias /* keep */ \x60example.test/a\x60 // 尾 🦀\n",
            "\nfunc f() {}\n",
        );
        let expected = concat!(
            "package p\n\n// import declaration docs\n",
            "import (\n\t\"fmt\"\n\n\t/* 🐱 lead */ alias /* keep */ \x60example.test/a\x60 // 尾 🦀\n)\n",
            "\nfunc f() {}\n",
        );
        assert_added(text, "fmt", None, expected);
        assert_added(
            &text.replace('\n', "\r\n"),
            "fmt",
            None,
            &expected.replace('\n', "\r\n"),
        );
        assert_added(
            "package p\nimport /* immediately after import */\n\n\"io\"\n",
            "fmt",
            None,
            "package p\nimport (\n\t\"fmt\"\n\t/* immediately after import */\n\n\"io\"\n)\n",
        );
    }

    #[test]
    fn preserves_the_blank_import_when_adding_a_named_binding_for_its_path() {
        assert_added(
            "package p\nimport _ \"fmt\" // side effects\n",
            "fmt",
            Some("printer"),
            "package p\nimport (\n\t_ \"fmt\" // side effects\n\tprinter \"fmt\"\n)\n",
        );
        assert_added(
            "package p\nimport _ \"fmt\"\n",
            "fmt",
            None,
            "package p\nimport (\n\t\"fmt\"\n\t_ \"fmt\"\n)\n",
        );
    }

    #[test]
    fn preserves_import_groups_and_inserts_before_the_comments_for_the_next_spec() {
        let text = concat!(
            "package p\nimport (\n",
            "    \"bytes\" // existing stdlib import\n    // Relative docs\n    _ \"os\"\n",
            "\n    \"example.test/b\"\n    // belongs to the dot import 😀\n    . \"example.test/z\"\n",
            "\n    \"local/module\"\n)\n",
        );
        let expected = text.replace("    // Relative", "    \"fmt\"\n    // Relative");
        assert_added(text, "fmt", None, &expected);
        let expected = text.replace(
            "    // belongs",
            "    client \"example.test/c\"\n    // belongs",
        );
        assert_added(text, "example.test/c", Some("client"), &expected);
        assert_added(
            "package p\nimport ( // group header\n\t// belongs to os\n\t\"os\"\n)\n",
            "fmt",
            None,
            "package p\nimport ( // group header\n\t\"fmt\"\n\t// belongs to os\n\t\"os\"\n)\n",
        );
    }

    #[test]
    fn creates_a_missing_group_without_moving_existing_specs() {
        assert_added(
            "package p\nimport (\n\t// belongs to module\n\t\"example.test/module\"\n)\n",
            "fmt",
            None,
            "package p\nimport (\n\t\"fmt\"\n\n\t// belongs to module\n\t\"example.test/module\"\n)\n",
        );
        assert_added(
            "package p\nimport (\n  \"fmt\" // keep here\n)\n",
            "example.test/module",
            None,
            "package p\nimport (\n  \"fmt\" // keep here\n\n  \"example.test/module\"\n)\n",
        );
        assert_added(
            "package p\r\nimport (\r\n\t\"fmt\"\r\n\r\n)\r\n",
            "example.test/module",
            None,
            "package p\r\nimport (\r\n\t\"fmt\"\r\n\r\n\t\"example.test/module\"\r\n)\r\n",
        );
    }

    #[test]
    fn chooses_the_first_matching_block_and_preserves_cgo_preamble() {
        let text = concat!(
            "package p\n\n/*\n#include <stdio.h>\n*/\nimport \"C\" // cgo\n",
            "import ()\n",
            "import (\n\t\"example.test/a\"\n)\n",
            "import (\n  \"io\"\n)\n",
            "import (\n    \"os\"\n)\n",
        );
        let expected = text.replace("  \"io\"", "  \"fmt\"\n  \"io\"");
        assert_added(text, "fmt", None, &expected);

        let cgo_only = "package p\n\n// preamble\nimport \"C\" // cgo\n// F doc\nfunc F() {}\n";
        let expected = "package p\n\n// preamble\nimport \"C\" // cgo\n\nimport \"fmt\"\n\n// F doc\nfunc F() {}\n";
        assert_added(cgo_only, "fmt", None, expected);
    }

    #[test]
    fn handles_compact_and_empty_import_blocks() {
        assert_added(
            "package p\nimport ()\n",
            "fmt",
            None,
            "package p\nimport (\n\t\"fmt\"\n)\n",
        );
        assert_added(
            "package p\nimport (\"bytes\"; /* keep with os */ \"os\")\n",
            "fmt",
            None,
            "package p\nimport (\"bytes\"; \n\t\"fmt\"\n\t/* keep with os */ \"os\")\n",
        );
        assert_added(
            "package p; import \"os\"; var x int\n",
            "fmt",
            None,
            "package p; import (\n\t\"fmt\"\n\t\"os\"\n); var x int\n",
        );
    }

    #[test]
    fn detects_duplicates_by_go_decoded_path_and_explicit_alias() {
        let text = concat!(
            "package p\n",
            "import (\n\t_ \"\\x66mt\"\n\t. \"\\143ontext\"\n\talias \x60io\x60\n\t\"net/\\u0068ttp\"\n)\n",
        );
        for (path, alias) in [
            ("fmt", Some("_")),
            ("context", Some(".")),
            ("io", Some("alias")),
            ("net/http", None),
        ] {
            assert_eq!(add(text, path, alias).unwrap(), []);
        }
        assert!(matches!(
            add(text, "context", None),
            Err(ImportEditError::ConflictingBinding { .. })
        ));
        assert!(matches!(
            add(text, "io", None),
            Err(ImportEditError::ConflictingBinding { .. })
        ));
        assert!(matches!(
            add(text, "os", Some("alias")),
            Err(ImportEditError::ConflictingBinding { .. })
        ));
    }

    #[test]
    fn rejects_malformed_import_declarations_and_invalid_add_requests() {
        for text in [
            "package p\nimport (\n\"fmt\"\n",
            "package p\nimport (\n\"fmt\"\nfunc f() {}\n",
            "package p\nimport (\n\"io\"\n???\n)\n",
            "package p\nimport \"\\invalid\"\n",
            "package p\nimport ",
            "package p\nimport \"",
            "package p\nfunc f() {}\nimport \"io\"\n",
        ] {
            assert_eq!(
                add(text, "fmt", None),
                Err(ImportEditError::UnsafeSyntaxTree),
                "{text:?}"
            );
        }
        for path in [
            "",
            "two words",
            "bad\"path",
            "bad\npath",
            "bad\\path",
            "bad\x60path",
            "bad\u{200d}path",
        ] {
            assert_eq!(
                add("package p\n", path, None),
                Err(ImportEditError::InvalidImportPath)
            );
        }
        for alias in [
            "",
            "2x",
            "two names",
            "var",
            "init",
            "bad-name",
            "x½",
            "x\u{0345}",
        ] {
            assert_eq!(
                add("package p\n", "fmt", Some(alias)),
                Err(ImportEditError::InvalidImportAlias)
            );
        }
        let edits = with_document("package p\n", |doc| {
            import_edits(
                doc,
                ImportOperation::OrganizeImports {
                    unused_specs: &[],
                    local_prefixes: &[],
                    package_names: &Default::default(),
                },
            )
        })
        .unwrap();
        assert!(edits.is_empty());
    }

    #[test]
    fn utf16_import_edit_and_main_completion_edit_apply_to_the_same_snapshot() {
        let text = "package p; import /* 🦀猫 */ \"io\"; var x = \"😀\" + fmt.Pr";
        with_document(text, |document| {
            let mut edits = import_edits(
                document,
                ImportOperation::CompleteImport {
                    path: "fmt",
                    alias: None,
                },
            )
            .unwrap();
            let start = text.find("Pr").unwrap();
            let main = TextEdit::new(document.byte_range(start..text.len()), "Print".into());
            assert!(edits[0].range.end.character < main.range.start.character);
            edits.push(main);
            assert_eq!(
                apply(text, &edits),
                "package p; import (\n\t\"fmt\"\n\t/* 🦀猫 */ \"io\"\n); var x = \"😀\" + fmt.Print",
            );
        });
    }
}
