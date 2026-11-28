use std::fs;
use std::path::Path;

use tree_sitter::{Node, Parser};

use crate::workspace::DiagnosticFileStamp;

use super::completion::sdk_names::{embed_directives, package_header_end};
use super::shared_disk::FileObservation;
use super::source::{Source, decoded_import_path, node_text};

#[derive(Clone)]
pub(super) struct DiskPackageSelection {
    pub(super) stamp: DiagnosticFileStamp,
    pub(super) selection: PackageSelection,
}

impl DiskPackageSelection {
    pub(super) fn read(
        parser: &mut Parser,
        path: &Path,
        expected: Option<DiagnosticFileStamp>,
    ) -> Option<Self> {
        let before = FileObservation::read(path).ok()?;
        if expected.is_some_and(|stamp| stamp != before.stamp) {
            return None;
        }
        Self::read_observed(parser, path, before)
    }

    pub(super) fn read_observed(
        parser: &mut Parser,
        path: &Path,
        before: FileObservation,
    ) -> Option<Self> {
        let text = fs::read_to_string(path).ok()?;
        let end = package_header_end(&text)?;
        let tree = parser.parse(&text[..end], None)?;
        let root = tree.root_node();
        if root.has_error() {
            return None;
        }
        let selection = PackageSelection::from_tree(root, &text, end)?;
        (FileObservation::read(path).ok() == Some(before)).then_some(Self {
            stamp: before.stamp,
            selection,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PackageSelection {
    package: Box<str>,
    imports: Vec<Box<str>>,
    build: BuildConstraints,
    cgo_preamble: Option<Box<str>>,
    embed_directives: Vec<Box<str>>,
}

impl PackageSelection {
    pub(super) fn heap_weight(&self) -> usize {
        fn directives_weight(directives: &Vec<Box<str>>) -> usize {
            directives.capacity() * size_of::<Box<str>>()
                + directives
                    .iter()
                    .map(|directive| directive.len())
                    .sum::<usize>()
        }

        self.package.len()
            + directives_weight(&self.imports)
            + directives_weight(&self.build.modern)
            + directives_weight(&self.build.legacy)
            + self
                .cgo_preamble
                .as_ref()
                .map_or(0, |preamble| preamble.len())
            + directives_weight(&self.embed_directives)
    }

    pub(super) fn from_source(source: &Source) -> Option<Self> {
        let end = package_header_end(&source.text)?;
        Self::from_tree(source.tree.root_node(), &source.text, end)
    }

    fn from_tree(root: Node<'_>, text: &str, end: usize) -> Option<Self> {
        let mut package = None;
        let mut imports = Vec::new();
        let mut package_end = 0;
        let mut import_end = 0;
        let mut has_cgo = false;
        let mut has_embed = false;
        let mut cursor = root.walk();
        for node in root
            .named_children(&mut cursor)
            .take_while(|node| node.start_byte() < end)
        {
            if node.has_error() || node.is_missing() || node.end_byte() > end {
                return None;
            }
            match node.kind() {
                "package_clause" if package.is_none() => {
                    package = Some(node_text(node, text).into());
                    package_end = node.end_byte();
                }
                "import_declaration" if package.is_some() => {
                    let mut cursor = node.walk();
                    let specs = node
                        .named_children(&mut cursor)
                        .find(|child| child.kind() == "import_spec_list")
                        .unwrap_or(node);
                    let mut cursor = specs.walk();
                    for spec in specs
                        .named_children(&mut cursor)
                        .filter(|child| child.kind() == "import_spec")
                    {
                        let literal = spec.child_by_field_name("path")?;
                        let path = decoded_import_path(node_text(literal, text))?;
                        has_cgo |= path == "C";
                        has_embed |= path == "embed";
                    }
                    imports.push(node_text(node, text).into());
                    import_end = node.end_byte();
                }
                "comment" => {}
                _ => return None,
            }
        }
        Some(Self {
            package: package?,
            imports,
            build: BuildConstraints::from_text(text),
            // The comment attached to import "C" depends on its placement
            // relative to the package, import declaration and import spec.
            cgo_preamble: has_cgo.then(|| text[package_end..import_end].into()),
            embed_directives: if has_embed {
                embed_directives(text, import_end)?
            } else {
                Vec::new()
            },
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct BuildConstraints {
    modern: Vec<Box<str>>,
    legacy: Vec<Box<str>>,
    binary_only: bool,
}

impl BuildConstraints {
    fn from_text(text: &str) -> Self {
        // Go accepts legacy constraints in the leading run of line comments
        // only through its last blank line. Modern directives take precedence.
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut legacy = Vec::new();
        let mut legacy_end = 0;
        let mut modern = Vec::new();
        let mut binary_only = false;
        let mut leading_line_comments = true;
        let mut block_comment = false;
        'lines: for line in text.lines() {
            let mut line = line.trim();
            if leading_line_comments {
                if line.is_empty() {
                    legacy_end = legacy.len();
                    continue;
                }
                if let Some(comment) = line.strip_prefix("//") {
                    if let Some(expression) = build_expression(comment.trim_start(), "+build") {
                        legacy.push(expression.into());
                    }
                } else {
                    leading_line_comments = false;
                }
            }
            if !block_comment {
                if let Some(expression) = build_expression(line, "//go:build") {
                    modern.push(expression.into());
                }
                binary_only |= line == "//go:binary-only-package";
            }
            while !line.is_empty() {
                if block_comment {
                    let Some((_, rest)) = line.split_once("*/") else {
                        continue 'lines;
                    };
                    line = rest.trim_start();
                    block_comment = false;
                } else if line.starts_with("//") {
                    continue 'lines;
                } else if let Some(rest) = line.strip_prefix("/*") {
                    line = rest.trim_start();
                    block_comment = true;
                } else {
                    break 'lines;
                }
            }
        }
        legacy.truncate(if modern.is_empty() { legacy_end } else { 0 });
        Self {
            modern,
            legacy,
            binary_only,
        }
    }
}

fn build_expression<'a>(line: &'a str, directive: &str) -> Option<&'a str> {
    let suffix = line.strip_prefix(directive)?;
    let expression = suffix.trim_start();
    (suffix.is_empty() || suffix.len() != expression.len()).then_some(expression)
}
