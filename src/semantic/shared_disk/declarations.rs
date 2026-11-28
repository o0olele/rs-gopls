use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::ops::Range as Bytes;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use lsp_types::{CompletionItemKind, Range};
use tree_sitter::Node;

use crate::semantic::completion::render::{Description, describe_with_docs};
use crate::semantic::diagnostics::{has_reliable_declarations, has_reliable_package_scope};
use crate::semantic::hover::{projected_package_documentation, projected_signature};
use crate::semantic::index::{DeclarationSummary, FileIndex, IndexedSource};
use crate::semantic::resolver::untyped_default;
use crate::semantic::source::{DiagnosticImport, EditorNames, Import, check_cancelled, node_text};
use crate::symbols::{
    DiagnosticOutline, FunctionSignature, SymbolKind, collect_diagnostic_outline, named_type_name,
};

/// File declarations derived from the exact bytes in the content key. All
/// offsets refer to those bytes, including when function interiors were elided.
pub(in crate::semantic) struct SharedDeclarations {
    pub(in crate::semantic) index: Arc<FileIndex>,
    pub(in crate::semantic) summary: Option<Arc<DeclarationSummary>>,
    pub(in crate::semantic) reliable_declarations: bool,
    pub(in crate::semantic) reliable_package_scope: bool,
    pub(in crate::semantic) editor_names: Arc<EditorNames>,
    pub(in crate::semantic) diagnostic_imports: Arc<[DiagnosticImport]>,
    pub(in crate::semantic) outline: DiagnosticOutline,
    pub(in crate::semantic) presentation: HashMap<Range, DeclPresentation>,
    pub(in crate::semantic) package_doc: Option<(Arc<str>, Arc<str>)>,
    initializers: Vec<InitStub>,
    cache_weight: usize,
}

pub(in crate::semantic) struct DeclPresentation {
    pub(in crate::semantic) documentation: Option<Arc<str>>,
    hover: Option<HoverSignature>,
    // The indexed callable retains the receiver and parameter types.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(in crate::semantic) callable: Option<Arc<FunctionSignature>>,
    kind_override: Option<CompletionItemKind>,
    detail: Option<Box<str>>,
    label_suffix: Option<Box<str>>,
    deprecated: bool,
    has_parameters: bool,
}

enum HoverSignature {
    CompletionDetail,
    Separate(Box<str>),
}

impl DeclPresentation {
    pub(in crate::semantic) fn hover(&self) -> Option<(&str, &str)> {
        let signature = match self.hover.as_ref()? {
            HoverSignature::CompletionDetail => self.detail.as_deref()?,
            HoverSignature::Separate(signature) => signature,
        };
        Some((signature, self.documentation.as_deref().unwrap_or_default()))
    }

    pub(in crate::semantic) fn description(
        &self,
        default_kind: CompletionItemKind,
        detect_deprecation: bool,
    ) -> Description {
        Description {
            kind: self.kind_override.unwrap_or(default_kind),
            detail: self.detail.as_deref().map(str::to_owned),
            label_suffix: self.label_suffix.as_deref().map(str::to_owned),
            deprecated: detect_deprecation && self.deprecated,
            has_parameters: self.has_parameters,
        }
    }
}

pub(in crate::semantic) struct InitStub {
    pub(in crate::semantic) expression: Bytes<usize>,
    pub(in crate::semantic) untyped_default: Option<&'static str>,
}

const ARC_HEADER: usize = 2 * size_of::<usize>();

impl SharedDeclarations {
    pub(in crate::semantic) fn collect(
        source: &IndexedSource,
        cancelled: &AtomicBool,
    ) -> Result<Self> {
        let index = source.declaration_index(cancelled)?;
        let root = source.tree.root_node();
        let reliable_declarations = has_reliable_declarations(root, Some(cancelled))?;
        let reliable_package_scope =
            reliable_declarations && has_reliable_package_scope(root, Some(cancelled))?;
        let outline = collect_diagnostic_outline(&source.tree, &source.text, Some(cancelled))?;
        let mut presentation = HashMap::new();
        let mut docs = HashSet::<Arc<str>>::new();
        let mut intern_doc = |documentation: String| {
            if documentation.is_empty() {
                None
            } else if let Some(existing) = docs.get(documentation.as_str()) {
                Some(existing.clone())
            } else {
                let documentation: Arc<str> = documentation.into();
                docs.insert(documentation.clone());
                Some(documentation)
            }
        };
        let package_doc = projected_package_documentation(source)
            .and_then(|(name, doc)| Some((Arc::from(name), intern_doc(doc)?)));
        let mut project = |range, name: &str, callable| {
            let std::collections::hash_map::Entry::Vacant(entry) = presentation.entry(range) else {
                return;
            };
            let default_kind = CompletionItemKind::TEXT;
            let (description, documentation) =
                describe_with_docs(source, range, name, default_kind);
            let documentation = intern_doc(documentation);
            let hover = projected_signature(source, range, description.detail.as_deref()).map(
                |signature| match signature {
                    Cow::Borrowed(_) => HoverSignature::CompletionDetail,
                    Cow::Owned(signature) if description.detail.as_deref() == Some(&signature) => {
                        HoverSignature::CompletionDetail
                    }
                    Cow::Owned(signature) => HoverSignature::Separate(signature.into_boxed_str()),
                },
            );
            entry.insert(DeclPresentation {
                documentation,
                hover,
                callable,
                kind_override: (description.kind != default_kind).then_some(description.kind),
                detail: description.detail.map(String::into_boxed_str),
                label_suffix: description.label_suffix.map(String::into_boxed_str),
                deprecated: description.deprecated,
                has_parameters: description.has_parameters,
            });
        };
        for (name, symbol) in index.symbols.declarations().chain(index.symbols.members()) {
            check_cancelled(cancelled)?;
            let callable = match &symbol.kind {
                SymbolKind::Function(signature) => Some(signature.clone()),
                _ => None,
            };
            project(symbol.range, name, callable);
        }
        collect_member_names(root, cancelled, |name| {
            project(source.range(name), node_text(name, &source.text), None);
        })?;

        let mut initializers = Vec::new();
        collect_initializers(root, cancelled, &mut initializers)?;
        let summary = source.declaration_summary().cloned();
        check_cancelled(cancelled)?;
        let mut declarations = Self {
            index,
            summary,
            reliable_declarations,
            reliable_package_scope,
            editor_names: source.editor_names.clone(),
            diagnostic_imports: source.diagnostic_imports.clone(),
            outline,
            presentation,
            package_doc,
            initializers,
            cache_weight: 0,
        };
        declarations.cache_weight =
            declarations.measure(docs.iter().map(|doc| doc.len() + ARC_HEADER).sum());
        check_cancelled(cancelled)?;
        Ok(declarations)
    }

    pub(in crate::semantic) fn initializer(&self, expression: &Bytes<usize>) -> Option<&InitStub> {
        let index = self
            .initializers
            .binary_search_by_key(&expression.start, |stub| stub.expression.start)
            .ok()?;
        self.initializers
            .get(index)
            .filter(|stub| stub.expression == *expression)
    }

    pub(in crate::semantic) fn cache_weight(&self) -> usize {
        self.cache_weight
    }

    fn measure(&self, documentation_weight: usize) -> usize {
        size_of::<Self>()
            + ARC_HEADER
            + size_of::<FileIndex>()
            + ARC_HEADER
            + self.index.symbols.heap_weight()
            + self.index.package_name.as_ref().map_or(0, String::capacity)
            + self
                .index
                .package_selection
                .as_ref()
                .map_or(0, |selection| selection.heap_weight())
            + import_weight(&self.index.imports)
            + self.summary.as_ref().map_or(0, |summary| {
                ARC_HEADER + size_of::<DeclarationSummary>() + summary.heap_weight()
            })
            + self.editor_names.cache_weight()
            + ARC_HEADER
            + diagnostic_import_weight(&self.diagnostic_imports)
            + self.outline.heap_weight()
            + (self.presentation.capacity() * 8).div_ceil(7)
                * (size_of::<(Range, DeclPresentation)>() + 1)
            + self
                .presentation
                .values()
                .map(|presentation| {
                    presentation
                        .detail
                        .as_ref()
                        .map_or(0, |detail| detail.len())
                        + presentation
                            .label_suffix
                            .as_ref()
                            .map_or(0, |suffix| suffix.len())
                        + presentation.hover.as_ref().map_or(0, |hover| match hover {
                            HoverSignature::CompletionDetail => 0,
                            HoverSignature::Separate(signature) => signature.len(),
                        })
                })
                .sum::<usize>()
            + documentation_weight
            + self
                .package_doc
                .as_ref()
                .map_or(0, |(name, _)| name.len() + ARC_HEADER)
            + self.initializers.capacity() * size_of::<InitStub>()
    }
}

// Fields and methods inside anonymous types are referenced by their selection
// ranges, but do not have an entry in the file's named receiver index.
fn collect_member_names<'tree>(
    root: Node<'tree>,
    cancelled: &AtomicBool,
    mut member: impl FnMut(Node<'tree>),
) -> Result<()> {
    let mut cursor = root.walk();
    loop {
        check_cancelled(cancelled)?;
        let node = cursor.node();
        if matches!(node.kind(), "field_declaration" | "method_elem") {
            let mut names_cursor = node.walk();
            let mut names = node
                .children_by_field_name("name", &mut names_cursor)
                .peekable();
            if names.peek().is_some() {
                for name in names {
                    member(name);
                }
            } else if node.kind() == "field_declaration"
                && let Some(name) = node.child_by_field_name("type").and_then(named_type_name)
            {
                member(name);
            }
        }
        if node.kind() != "block" && cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return Ok(());
            }
        }
    }
}

fn collect_initializers(
    declaration: Node<'_>,
    cancelled: &AtomicBool,
    initializers: &mut Vec<InitStub>,
) -> Result<()> {
    check_cancelled(cancelled)?;
    match declaration.kind() {
        "source_file" | "var_declaration" | "var_spec_list" | "const_declaration" => {
            for child in declaration.named_children(&mut declaration.walk()) {
                collect_initializers(child, cancelled, initializers)?;
            }
        }
        "var_spec" | "const_spec" => {
            if let Some(list) = declaration.child_by_field_name("value") {
                for expression in list
                    .named_children(&mut list.walk())
                    .filter(|node| !node.is_extra() && !node.is_missing())
                {
                    check_cancelled(cancelled)?;
                    initializers.push(InitStub {
                        expression: expression.byte_range(),
                        untyped_default: untyped_default(expression),
                    });
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn import_weight(imports: &[Import]) -> usize {
    ARC_HEADER
        + size_of_val(imports)
        + imports
            .iter()
            .map(|import| {
                import.path.capacity() + import.alias.as_ref().map_or(0, String::capacity)
            })
            .sum::<usize>()
}

fn diagnostic_import_weight(imports: &[DiagnosticImport]) -> usize {
    ARC_HEADER
        + size_of_val(imports)
        + imports
            .iter()
            .map(|import| {
                [&import.alias, &import.path, &import.decoded_path]
                    .into_iter()
                    .flatten()
                    .map(String::capacity)
                    .sum::<usize>()
            })
            .sum::<usize>()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use lsp_types::Position;
    use tree_sitter::Parser;

    use super::*;
    use crate::semantic::completion::sdk_names::scan_function_bodies;
    use crate::semantic::source::Source;

    #[test]
    fn elided_projection_preserves_docs_signature_and_initializer_ranges() {
        let text = concat!(
            "package sample\n",
            "// Transform returns its first argument.\n",
            "// Deprecated: use Convert.\n",
            "func Transform[A any](first A, rest ...A) (A, error) {\n",
            "    ignored := 42\n",
            "    _ = ignored\n",
            "    return first, nil\n",
            "}\n",
            "const (\n",
            "    // Count is a documented constant.\n",
            "    Count = 5 // inline documentation\n",
            ")\n",
            "var First, Second = (\"🙂\"), -2; var Tail int\n",
        );
        let path = Path::new("sample.go");
        let cancelled = AtomicBool::new(false);
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
            .unwrap();
        let full = Source::parse(&mut parser, PathBuf::from(path), text.to_owned(), None).unwrap();
        let full =
            SharedDeclarations::collect(&IndexedSource::new(full, None), &cancelled).unwrap();
        let bodies = scan_function_bodies(text).unwrap();
        assert_eq!(bodies.len(), 1);
        let elided = Source::try_parse_dependency_declarations(
            &mut parser,
            path,
            Arc::new(text.to_owned()),
            &bodies,
            &cancelled,
        )
        .unwrap()
        .unwrap();
        let elided =
            SharedDeclarations::collect(&IndexedSource::new(elided, None), &cancelled).unwrap();
        assert!(elided.reliable_declarations && elided.reliable_package_scope);
        assert_eq!(elided.summary, full.summary);

        let projected = |name, default_kind| {
            let (elided_symbol, full_symbol) = (
                elided.index.symbols.declaration(name).unwrap(),
                full.index.symbols.declaration(name).unwrap(),
            );
            assert_eq!(elided_symbol.range, full_symbol.range);
            let stored = &elided.presentation[&elided_symbol.range];
            let full = &full.presentation[&full_symbol.range];
            let actual = stored.description(default_kind, true);
            let expected = full.description(default_kind, true);
            assert_eq!(actual.kind, expected.kind);
            assert_eq!(actual.detail, expected.detail);
            assert_eq!(actual.label_suffix, expected.label_suffix);
            assert_eq!(actual.deprecated, expected.deprecated);
            assert_eq!(actual.has_parameters, expected.has_parameters);
            assert_eq!(stored.documentation, full.documentation);
            (stored, actual)
        };
        let (function, description) = projected("Transform", CompletionItemKind::FUNCTION);
        assert!(function.callable.as_ref().unwrap().variadic);
        assert_eq!(
            description.detail.as_deref(),
            Some("func Transform[A any](first A, rest ...A) (A, error)")
        );
        assert_eq!(
            description.label_suffix.as_deref(),
            Some("(first A, rest ...A)")
        );
        assert!(description.deprecated && description.has_parameters);
        let (constant, description) = projected("Count", CompletionItemKind::VARIABLE);
        assert_eq!(description.kind, CompletionItemKind::CONSTANT);
        assert_eq!(
            constant.documentation.as_deref(),
            Some("Count is a documented constant.\n\ninline documentation")
        );

        for (expression, default_type) in [("(\"🙂\")", "string"), ("-2", "int"), ("5", "int")] {
            let start = text.find(expression).unwrap();
            let bytes = start..start + expression.len();
            let stub = elided.initializer(&bytes).unwrap();
            assert_eq!(stub.expression, bytes);
            assert_eq!(stub.untyped_default, Some(default_type));
        }
        let body_local = text.find("42").unwrap();
        assert!(elided.initializer(&(body_local..body_local + 2)).is_none());
        let tail = elided.index.symbols.declaration("Tail").unwrap();
        let last_line = text.lines().last().unwrap();
        let tail_column = last_line[..last_line.find("Tail").unwrap()]
            .encode_utf16()
            .count();
        let tail_line = text.lines().count() - 1;
        assert_eq!(
            tail.range.start,
            Position::new(tail_line as u32, tail_column as u32)
        );
    }
}
