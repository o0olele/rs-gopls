use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
    index::{CompletionRead, text_fingerprint},
    presentation::{documentation, function_signature, syntax_text},
    snapshot::{DocumentSnapshot, EngineSnapshot},
    source::{Source, node_at_range, node_text},
};
use crate::workspace::path_key;

use super::CompletionClientCapabilities;
use super::cursor::{Edit, ancestors};
use super::ranking::{Candidate, Insert, rank};

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Target {
    pub(super) file: PathBuf,
    pub(super) range: Range,
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
    pub(super) graph_generation: Option<u64>,
    pub(super) target: Option<Target>,
    pub(super) detail: Option<String>,
    pub(super) pending_import: Option<PendingImport>,
}

#[derive(Clone)]
pub(in crate::semantic) struct Description {
    pub(super) kind: CompletionItemKind,
    pub(super) detail: Option<String>,
    pub(super) label_suffix: Option<String>,
    pub(super) deprecated: bool,
    pub(super) has_parameters: bool,
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
    describe_token(
        node_at_range(source, range),
        &source.text,
        name,
        default_kind,
        detect_deprecation,
    )
}

fn describe_token(
    token: Option<Node<'_>>,
    text: &str,
    name: &str,
    default_kind: CompletionItemKind,
    detect_deprecation: bool,
) -> Description {
    let mut description = Description {
        kind: default_kind,
        detail: None,
        label_suffix: None,
        deprecated: false,
        has_parameters: false,
    };
    let Some(owner) = token.and_then(owner) else {
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
    if detect_deprecation {
        description.deprecated = documentation(owner, text).contains("Deprecated:");
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
    let _phase = request.trace.as_ref().map(|trace| trace.phase("render"));
    let mut items = Vec::new();
    let mut import_edits = HashMap::new();
    let mut docs = HashMap::new();
    let mut resolver = snapshot
        .workspace()
        .map(|workspace| snapshot.resolver_with_completion(workspace, Some(request)));
    let uri = path_uri(document.path())?;
    let version = document.version();
    let fingerprint = format!("{:016x}", text_fingerprint(document.text()));
    let defer_detail = caps.resolve_detail && version.is_some();
    let defer_docs = caps.resolve_documentation && version.is_some();
    let defer_import = caps.resolve_additional_text_edits && version.is_some();
    for (sort_text, candidate) in rank(candidates) {
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
            docs.entry((path_key(&target.file), target.range))
                .or_insert_with(|| {
                    if path_key(&target.file) == path_key(document.path()) {
                        return markup(document.indexed(), target.range, caps);
                    }
                    target_markup(&mut resolver.as_mut()?.indexes, target, caps)
                })
                .clone()
        } else {
            None
        };
        let detail = candidate.detail.or_else(|| candidate.import_path.clone());
        let data = ItemData {
            uri: uri.clone(),
            document_version: version,
            document_fingerprint: fingerprint.clone(),
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

pub(super) fn target_markup(
    indexes: &mut crate::semantic::index::IndexReader<'_>,
    target: &Target,
    caps: &CompletionClientCapabilities,
) -> Option<Documentation> {
    let source = indexes.declaration_source(&target.file).ok()??;
    markup(source.as_ref(), target.range, caps)
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
