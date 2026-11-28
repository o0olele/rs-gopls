use std::ops::Range as Bytes;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use lsp_types::Range;

use super::{DiagnosticCode, DiagnosticItem, DiagnosticSeverity, DocumentSnapshot, checkpoint};

pub(super) fn has_reliable_declarations(
    root: tree_sitter::Node<'_>,
    cancelled: Option<&AtomicBool>,
) -> Result<bool> {
    if !root.has_error() {
        return Ok(true);
    }
    if root.is_error() {
        return Ok(false);
    }
    for decl in root.children(&mut root.walk()) {
        checkpoint(cancelled)?;
        if !decl.has_error() {
            continue;
        }
        let Some(body) = decl
            .child_by_field_name("body")
            .filter(|_| matches!(decl.kind(), "function_declaration" | "method_declaration"))
        else {
            return Ok(false);
        };
        let brace = |node: Option<tree_sitter::Node<'_>>, kind| {
            node.is_some_and(|node| !node.is_missing() && node.kind() == kind)
        };
        if !brace(body.child(0), "{")
            || !brace(body.child(body.child_count().saturating_sub(1)), "}")
            || decl
                .children(&mut decl.walk())
                .any(|part| part != body && part.has_error())
        {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(in crate::semantic) fn has_reliable_package_scope(
    root: tree_sitter::Node<'_>,
    cancelled: Option<&AtomicBool>,
) -> Result<bool> {
    if !has_reliable_declarations(root, cancelled)? {
        return Ok(false);
    }
    let mut seen_package = false;
    let mut seen_declaration = false;
    for node in root
        .named_children(&mut root.walk())
        .filter(|node| !node.is_extra())
    {
        checkpoint(cancelled)?;
        match node.kind() {
            "package_clause" if !seen_package => seen_package = true,
            "import_declaration" if seen_package && !seen_declaration => {}
            "function_declaration"
            | "method_declaration"
            | "type_declaration"
            | "var_declaration"
            | "const_declaration"
                if seen_package =>
            {
                seen_declaration = true
            }
            // The grammar also accepts snippets with statements at file scope.
            _ => return Ok(false),
        }
    }
    Ok(seen_package)
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
            if i & 255 == 0 {
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
            unexpected(node, document.text(), cancelled)?
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

fn unexpected(
    node: tree_sitter::Node<'_>,
    text: &str,
    cancelled: Option<&AtomicBool>,
) -> Result<(Bytes<usize>, String)> {
    let mut closers = Vec::new();
    for (i, child) in node.children(&mut node.walk()).enumerate() {
        if i & 255 == 0 {
            checkpoint(cancelled)?;
        }
        match child.kind() {
            "(" => closers.push(")"),
            "[" => closers.push("]"),
            "{" => closers.push("}"),
            token @ (")" | "]" | "}") if closers.pop() != Some(token) => {
                return Ok((
                    child.byte_range(),
                    format!("Unexpected {token:?}, expected an expression or declaration"),
                ));
            }
            _ => {}
        }
    }
    if let Some(token) = closers.last() {
        let end = node.start_byte() + text[node.byte_range()].trim_end().len();
        return Ok((end..end, format!("Missing {token:?}")));
    }
    let fragment = text[node.byte_range()]
        .trim()
        .chars()
        .take(36)
        .collect::<String>();
    Ok((
        node.byte_range(),
        format!("Unexpected Go syntax near {fragment:?}"),
    ))
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
