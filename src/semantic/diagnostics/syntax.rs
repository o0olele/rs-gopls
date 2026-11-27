use std::ops::Range as Bytes;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use lsp_types::Range;

use super::{DiagnosticCode, DiagnosticItem, DiagnosticSeverity, DocumentSnapshot, checkpoint};

pub(super) fn has_reliable_declarations(root: tree_sitter::Node<'_>) -> bool {
    !root.has_error()
        || (!root.is_error()
            && root.children(&mut root.walk()).all(|decl| {
                !decl.has_error()
                    || (matches!(decl.kind(), "function_declaration" | "method_declaration")
                        && decl.child_by_field_name("body").is_some_and(|body| {
                            body.has_error()
                                && decl
                                    .children(&mut decl.walk())
                                    .all(|part| part == body || !part.has_error())
                        }))
            }))
}

pub(super) fn collect_syntax_diagnostics(
    document: &DocumentSnapshot<'_>,
    cancelled: Option<&AtomicBool>,
) -> Result<Vec<DiagnosticItem>> {
    let root = document.root_node();
    if !root.has_error() {
        return Ok(Vec::new());
    }
    let mut errors: Vec<(Bytes<usize>, String)> = Vec::new();
    let mut pending = vec![(root, "")];
    while let Some((node, parent_kind)) = pending.pop() {
        checkpoint(cancelled)?;
        let before = pending.len();
        for (i, child) in node.children(&mut node.walk()).enumerate() {
            if i & 1023 == 0 {
                checkpoint(cancelled)?;
            }
            if child.has_error() {
                pending.push((child, node.kind()));
            }
        }
        pending[before..].reverse();
        if before != pending.len() || !(node.is_error() || node.is_missing()) {
            continue;
        }
        let (bytes, message) = if node.is_missing() {
            (
                node.byte_range(),
                match node.kind() {
                    "identifier" if parent_kind == "package_clause" => {
                        "Missing package name".into()
                    }
                    "identifier" => "Expected an identifier or expression".into(),
                    token => format!("Missing {token:?}"),
                },
            )
        } else {
            unexpected(node, document.text())
        };
        if let Some((last, _)) = errors.last_mut()
            && bytes.start >= last.start
            && (bytes.start <= last.end
                || document.text()[last.end..bytes.start]
                    .bytes()
                    .all(|b| matches!(b, b' ' | b'\t')))
        {
            last.end = last.end.max(bytes.end);
        } else {
            errors.push((bytes, message));
        }
    }
    Ok(errors
        .into_iter()
        .map(|(bytes, message)| {
            DiagnosticItem::new(
                DiagnosticCode::SyntaxError,
                DiagnosticSeverity::Error,
                visible_range(document, bytes),
                message,
            )
        })
        .collect())
}

fn unexpected(node: tree_sitter::Node<'_>, text: &str) -> (Bytes<usize>, String) {
    let mut closers = Vec::new();
    for child in node.children(&mut node.walk()) {
        match child.kind() {
            "(" => closers.push(")"),
            "[" => closers.push("]"),
            "{" => closers.push("}"),
            token @ (")" | "]" | "}") if closers.pop() != Some(token) => {
                return (
                    child.byte_range(),
                    format!("Unexpected {token:?}, expected an expression or declaration"),
                );
            }
            _ => {}
        }
    }
    if let Some(token) = closers.last() {
        let end = node.start_byte() + text[node.byte_range()].trim_end().len();
        return (end..end, format!("Missing {token:?}"));
    }
    let fragment = text[node.byte_range()]
        .trim()
        .chars()
        .take(36)
        .collect::<String>();
    (
        node.byte_range(),
        format!("Unexpected Go syntax near {fragment:?}"),
    )
}

pub(super) fn visible_range(document: &DocumentSnapshot<'_>, bytes: Bytes<usize>) -> Range {
    let text = document.text();
    let start = text.floor_char_boundary(bytes.start.min(text.len()));
    let end = text.ceil_char_boundary(bytes.end.min(text.len()));
    if end > start {
        let trimmed = text[start..end].trim_end_matches(['\r', '\n']);
        if !trimmed.is_empty() {
            return document.byte_range(start..start + trimmed.len());
        }
    }
    if let Some(ch) = text[start..]
        .chars()
        .next()
        .filter(|ch| !matches!(ch, '\r' | '\n'))
    {
        return document.byte_range(start..start + ch.len_utf8());
    }
    let prefix = text[..start].trim_end_matches(['\r', '\n']);
    if let Some((offset, _)) = prefix.char_indices().next_back() {
        return document.byte_range(offset..prefix.len());
    }
    let newline = text[start..].starts_with("\r\n");
    document.byte_range(start..(start + if newline { 2 } else { 1 }).min(text.len()))
}
