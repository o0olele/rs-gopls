use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionItemLabelDetails, CompletionItemTag,
    CompletionResponse, CompletionTextEdit, Documentation, InsertReplaceEdit, InsertTextFormat,
    MarkupContent, MarkupKind, Position, Range, TextEdit, Uri,
};
use serde::{Deserialize, Serialize};
use tree_sitter::Node;

use crate::protocol::path_uri;
use crate::semantic::{
    actions::{ImportOperation, import_edits},
    index::CompletionRead,
    presentation::{documentation, function_signature, syntax_text},
    shared_disk::SharedDeclarations,
    snapshot::{DocumentSnapshot, EngineSnapshot},
    source::{Source, check_optional_cancelled, node_at_range, node_text},
};
use crate::workspace::path_key;

use super::CompletionClientCapabilities;
use super::cursor::{Edit, ancestors};
use super::ranking::{Candidate, Insert, rank};

pub(in crate::semantic) use crate::semantic::index::SourceRev;

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Target {
    pub(super) file: PathBuf,
    pub(super) range: Range,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) revision: Option<SourceRev>,
}

impl SourceRev {
    pub(super) fn document(document: &DocumentSnapshot<'_>) -> Option<Self> {
        Self::overlay(document.indexed()).or_else(|| {
            document
                .read_ticket()
                .map(|ticket| Self::Disk(ticket.source_revision()))
        })
    }
}

impl Target {
    pub(super) fn key(&self) -> (&Path, u32, u32) {
        (
            &self.file,
            self.range.start.line,
            self.range.start.character,
        )
    }
}

#[derive(Clone, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub(super) struct PendingImport {
    pub(super) path: String,
    pub(super) alias: Option<String>,
}

#[derive(Deserialize, Serialize)]
pub(super) struct ItemData {
    pub(super) uri: Uri,
    pub(super) document_version: Option<i32>,
    pub(super) document_fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) document_revision: Option<SourceRev>,
    pub(super) graph_generation: Option<u64>,
    pub(super) target: Option<Target>,
    pub(super) detail: Option<String>,
    pub(super) pending_import: Option<PendingImport>,
}

#[derive(Clone)]
pub(in crate::semantic) struct Description {
    pub(in crate::semantic) kind: CompletionItemKind,
    pub(in crate::semantic) detail: Option<String>,
    pub(in crate::semantic) label_suffix: Option<String>,
    pub(in crate::semantic) deprecated: bool,
    pub(in crate::semantic) has_parameters: bool,
}

pub(in crate::semantic) fn describe_with_docs(
    source: &Source,
    range: Range,
    name: &str,
    default_kind: CompletionItemKind,
) -> (Description, String) {
    let owner = node_at_range(source, range).and_then(owner);
    let mut description = describe_owner(owner, &source.text, name, default_kind);
    let docs = owner.map_or_else(String::new, |node| documentation(node, &source.text));
    description.deprecated = docs.contains("Deprecated:");
    (description, docs)
}

fn owner(token: Node<'_>) -> Option<Node<'_>> {
    ancestors(token).find(|node| {
        matches!(
            node.kind(),
            "function_declaration"
                | "method_declaration"
                | "method_elem"
                | "type_spec"
                | "type_alias"
                | "field_declaration"
                | "var_spec"
                | "const_spec"
                | "parameter_declaration"
                | "variadic_parameter_declaration"
                | "type_parameter_declaration"
                | "short_var_declaration"
                | "range_clause"
                | "receive_statement"
                | "labeled_statement"
                | "package_clause"
                | "import_spec"
        )
    })
}

pub(in crate::semantic) fn describe(
    source: &Source,
    range: Range,
    name: &str,
    default_kind: CompletionItemKind,
    detect_deprecation: bool,
) -> Description {
    let owner = node_at_range(source, range).and_then(owner);
    let mut description = describe_owner(owner, &source.text, name, default_kind);
    if detect_deprecation && let Some(owner) = owner {
        description.deprecated = documentation(owner, &source.text).contains("Deprecated:");
    }
    description
}

fn describe_owner(
    owner: Option<Node<'_>>,
    text: &str,
    name: &str,
    default_kind: CompletionItemKind,
) -> Description {
    let mut description = Description {
        kind: default_kind,
        detail: None,
        label_suffix: None,
        deprecated: false,
        has_parameters: false,
    };
    let Some(owner) = owner else {
        return description;
    };
    let ty = owner.child_by_field_name("type");
    let prefix = match owner.kind() {
        "function_declaration" | "method_declaration" | "method_elem" => {
            description.detail = Some(function_signature(owner, text, None));
            if let Some(parameters) = owner.child_by_field_name("parameters") {
                description.label_suffix = Some(syntax_text(parameters, text));
                description.has_parameters = parameters
                    .named_children(&mut parameters.walk())
                    .any(|parameter| !parameter.is_extra());
            }
            None
        }
        "type_spec" | "type_alias" => {
            if let Some(ty) = ty {
                let parameters = owner
                    .child_by_field_name("type_parameters")
                    .map_or_else(String::new, |list| syntax_text(list, text));
                let equals = if owner.kind() == "type_alias" {
                    " ="
                } else {
                    ""
                };
                description.detail = Some(format!(
                    "type {name}{parameters}{equals} {}",
                    syntax_text(ty, text)
                ));
            }
            None
        }
        "const_spec" => {
            description.kind = CompletionItemKind::CONSTANT;
            Some("const")
        }
        "var_spec" | "short_var_declaration" => Some("var"),
        "type_parameter_declaration" => Some("type"),
        "field_declaration" => Some("field"),
        _ => None,
    };
    if description.detail.is_none()
        && let Some(prefix) = prefix
    {
        description.detail = Some(ty.map_or_else(
            || format!("{prefix} {name}"),
            |ty| format!("{prefix} {name} {}", syntax_text(ty, text)),
        ));
    }
    description
}

pub(in crate::semantic) fn package_name(source: &Source) -> Option<&str> {
    let root = source.tree.root_node();
    let clause = root
        .named_children(&mut root.walk())
        .find(|node| node.kind() == "package_clause")?;
    Some(node_text(clause.named_child(0)?, &source.text))
}

pub(super) fn response(
    snapshot: &mut EngineSnapshot<'_>,
    document: &DocumentSnapshot<'_>,
    edit: &Edit,
    candidates: Vec<Candidate>,
    caps: &CompletionClientCapabilities,
    graph_generation: Option<u64>,
    request: &mut CompletionRead,
) -> Result<CompletionResponse> {
    let cancelled = snapshot.completion_cancelled();
    let candidates = {
        let _phase = request.trace.as_ref().map(|trace| trace.phase("rank"));
        rank(candidates, cancelled)?
    };
    let _phase = request.trace.as_ref().map(|trace| trace.phase("render"));
    #[cfg(test)]
    if let Some(trace) = &request.trace {
        trace.checkpoint("render", cancelled);
    }
    check_optional_cancelled(cancelled)?;
    let mut items = Vec::new();
    let mut import_edits = HashMap::new();
    let mut docs = HashMap::new();
    let uri = path_uri(document.path())?;
    let version = document.version();
    let fingerprint = format!(
        "{:016x}",
        document.indexed().fingerprint_cancellable(cancelled)?
    );
    let document_revision = SourceRev::document(document);
    let defer_detail = caps.resolve_detail && version.is_some();
    let defer_docs = caps.resolve_documentation && version.is_some();
    let defer_import = caps.resolve_additional_text_edits && version.is_some();
    for candidate in candidates {
        let (sort_text, candidate) = candidate?;
        if candidate
            .replace_start
            .is_some_and(|start| document.text()[start..edit.replace.start].contains('\n'))
        {
            continue;
        }
        let (primary, format) = primary_edit(document, edit, &candidate, caps);
        let mut additional_text_edits = None;
        if let Some(import) = &candidate.pending_import {
            let edits = import_edits
                .entry(import.clone())
                .or_insert_with(|| build_import_edits(document, import));
            let Some(edits) = edits
                .as_ref()
                .filter(|edits| nonoverlapping(&primary, edits))
            else {
                continue;
            };
            if !defer_import && !edits.is_empty() {
                additional_text_edits = Some(edits.clone());
            }
        }
        let documentation = if defer_docs {
            None
        } else if let Some(target) = &candidate.target {
            docs.entry((
                path_key(&target.file),
                target.range,
                target.revision.clone(),
            ))
            .or_insert_with(|| eager_target_markup(snapshot, document, target, caps, request))
            .clone()
        } else {
            None
        };
        check_optional_cancelled(cancelled)?;
        let detail = candidate.detail.or_else(|| candidate.import_path.clone());
        let data = ItemData {
            uri: uri.clone(),
            document_version: version,
            document_fingerprint: fingerprint.clone(),
            document_revision: document_revision.clone(),
            graph_generation,
            target: candidate.target,
            detail: defer_detail.then(|| detail.clone()).flatten(),
            pending_import: if defer_import {
                candidate.pending_import
            } else {
                None
            },
        };
        let label_details = if caps.label_details_support
            && (candidate.label_suffix.is_some() || candidate.import_path.is_some())
        {
            Some(CompletionItemLabelDetails {
                detail: candidate.label_suffix,
                description: candidate.import_path,
            })
        } else {
            None
        };
        let tags = (caps.deprecated_support && candidate.deprecated)
            .then(|| vec![CompletionItemTag::DEPRECATED]);
        items.push(CompletionItem {
            kind: Some(candidate.kind),
            label_details,
            detail: if defer_detail { None } else { detail },
            documentation,
            deprecated: (caps.deprecated_support && candidate.deprecated).then_some(true),
            tags,
            sort_text: Some(sort_text),
            filter_text: Some(candidate.filter.unwrap_or_else(|| candidate.label.clone())),
            label: candidate.label,
            text_edit: Some(primary),
            insert_text_format: Some(format),
            additional_text_edits,
            data: Some(serde_json::to_value(data)?),
            ..CompletionItem::default()
        });
    }
    check_optional_cancelled(cancelled)?;
    Ok(CompletionResponse::Array(items))
}

fn primary_edit(
    document: &DocumentSnapshot<'_>,
    edit: &Edit,
    candidate: &Candidate,
    caps: &CompletionClientCapabilities,
) -> (CompletionTextEdit, InsertTextFormat) {
    let start = candidate.replace_start.unwrap_or(edit.replace.start);
    let replace = document.byte_range(start..edit.replace.end);
    let right = document.text()[edit.replace.end..].trim_start_matches([' ', '\t']);
    let mut new_text = match &candidate.qualifier {
        Some(qualifier) => format!("{qualifier}.{}", candidate.label),
        None => candidate.label.clone(),
    };
    let mut snippet = false;
    match &candidate.insert {
        Insert::Name => {}
        Insert::Call { has_parameters } if !right.starts_with(['(', '[', '.']) => {
            new_text.push_str(match (caps.snippet_support, has_parameters) {
                (true, true) => "($1)$0",
                (true, false) => "()$0",
                _ => "()",
            });
            snippet = caps.snippet_support;
        }
        Insert::Braces if !right.starts_with(['{', '(', '[', '.']) => {
            new_text.push_str(if caps.snippet_support { "{$0}" } else { "{}" });
            snippet = caps.snippet_support;
        }
        Insert::Package if !right.starts_with('.') => new_text.push('.'),
        Insert::Field { nesting } if !right.starts_with(':') => {
            let value = if caps.snippet_support { "$0" } else { "" };
            new_text.push_str(": ");
            new_text.push_str(value);
            for (field, ty) in nesting.iter().rev() {
                new_text = format!("{field}: {ty}{{{new_text}}}");
            }
            snippet = caps.snippet_support;
        }
        Insert::ImportPath { close } => {
            new_text.extend(*close);
            if caps.insert_replace_support {
                return (
                    InsertReplaceEdit {
                        new_text,
                        insert: document.byte_range(start..edit.offset),
                        replace,
                    }
                    .into(),
                    InsertTextFormat::PLAIN_TEXT,
                );
            }
        }
        _ => {}
    }
    let format = if snippet {
        InsertTextFormat::SNIPPET
    } else {
        InsertTextFormat::PLAIN_TEXT
    };
    (TextEdit::new(replace, new_text).into(), format)
}

pub(super) fn build_import_edits(
    document: &DocumentSnapshot<'_>,
    import: &PendingImport,
) -> Option<Vec<TextEdit>> {
    import_edits(
        document,
        ImportOperation::CompleteImport {
            path: &import.path,
            alias: import.alias.as_deref(),
        },
    )
    .ok()
}

pub(super) fn nonoverlapping(primary: &CompletionTextEdit, edits: &[TextEdit]) -> bool {
    let main = match primary {
        CompletionTextEdit::Edit(edit) => edit.range,
        CompletionTextEdit::InsertAndReplace(edit) => edit.replace,
    };
    let position = |pos: Position| (pos.line, pos.character);
    edits.iter().all(|edit| {
        let other = edit.range;
        main.start != other.start
            && (position(main.end) <= position(other.start)
                || position(other.end) <= position(main.start))
    })
}

pub(super) fn markup(
    source: &Source,
    range: Range,
    caps: &CompletionClientCapabilities,
) -> Option<Documentation> {
    let value = documentation(owner(node_at_range(source, range)?)?, &source.text);
    (!value.is_empty()).then(|| documentation_markup(value, caps))
}

fn eager_target_markup(
    snapshot: &mut EngineSnapshot<'_>,
    document: &DocumentSnapshot<'_>,
    target: &Target,
    caps: &CompletionClientCapabilities,
    request: &mut CompletionRead,
) -> Option<Documentation> {
    let key = path_key(&target.file);
    if key == path_key(document.path()) && target.revision == SourceRev::document(document) {
        return markup(document.indexed(), target.range, caps);
    }
    if let Some(source) = snapshot.docs().get(&key) {
        return (target.revision == SourceRev::overlay(source))
            .then(|| markup(source, target.range, caps))
            .flatten();
    }
    let Some(SourceRev::Disk(expected)) = &target.revision else {
        return None;
    };
    let disk = Arc::clone(snapshot.shared_disk());
    if let Some(source) = request.declarations(&key)
        && source
            .text
            .read_ticket
            .as_ref()
            .is_some_and(|ticket| ticket.source_revision() == *expected)
        && disk.validates_read(&target.file, &source.text)
    {
        return declaration_markup(&source.declarations, target.range, caps);
    }
    let cancelled = snapshot.completion_cancelled();
    let source = disk
        .load_documentation(snapshot.parser(), &target.file, None, cancelled, None)
        .ok()?;
    let actual = source.text.read_ticket.as_ref()?.source_revision();
    if actual != *expected {
        return None;
    }
    declaration_markup(&source.declarations, target.range, caps)
}

pub(super) fn declaration_markup(
    source: &SharedDeclarations,
    range: Range,
    caps: &CompletionClientCapabilities,
) -> Option<Documentation> {
    let value = source.presentation.get(&range)?.documentation.as_deref()?;
    Some(documentation_markup(value.to_owned(), caps))
}

fn documentation_markup(value: String, caps: &CompletionClientCapabilities) -> Documentation {
    Documentation::MarkupContent(MarkupContent {
        kind: if caps.markdown_docs {
            MarkupKind::Markdown
        } else {
            MarkupKind::PlainText
        },
        value,
    })
}
