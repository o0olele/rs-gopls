use std::borrow::Cow;
use std::fmt::Write;
use std::path::Path;

use anyhow::Result;

use lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position, Range};
use tree_sitter::Node;

use crate::rpc_telemetry;
use crate::scope::{Binding, declaration_binding, lexical_binding, reference_binding};
use crate::symbols::{FileSymbols, Symbol};

use super::index::{IndexedSource, draft_package};
use super::presentation::{builtin_function, documentation, function_signature, syntax_text};
use super::query::QueryProgress;
use super::resolver::{
    Resolver, TypeKind, is_predeclared, is_predeclared_type, receiver_parameter_origin,
};
use super::shared_disk::QueryReadLimit;
use super::snapshot::DocumentSnapshot;
use super::source::{
    ReferenceQuery, Source, import_path, is_declaration_name, is_test_source, is_type_declaration,
    node_at_range, node_text,
};
use super::{Engine, QueryReadMemo, SharedLoadPending};
use crate::workspace::{Package, path_key};

mod package_docs;
mod values;

pub(super) use package_docs::HoverDocCache;
use package_docs::PackageDocs;
use values::{value_parts, value_signature};

#[derive(Clone, Copy)]
enum Target<'tree> {
    Reference(ReferenceQuery<'tree>),
    Import {
        token: Node<'tree>,
        spec: Node<'tree>,
    },
    Package(Node<'tree>),
}

impl<'tree> Target<'tree> {
    fn at(document: &'tree DocumentSnapshot<'_>, position: Position) -> Option<Self> {
        let offset = document.offset(position)?;
        if offset == document.text().len() {
            return None;
        }
        let leaf = document
            .root_node()
            .descendant_for_byte_range(offset, offset + 1)?;
        let reference = document.indexed().reference_from_leaf(leaf, offset);
        let token = reference.map_or(leaf, ReferenceQuery::name);
        if !token.byte_range().contains(&offset) || node_text(token, document.text()) == "_" {
            return None;
        }
        match token.parent()?.kind() {
            "package_clause" if token.kind() == "package_identifier" => Some(Self::Package(token)),
            "import_spec" => {
                let spec = token.parent()?;
                (spec.child_by_field_name("path") == Some(token)
                    || spec.child_by_field_name("name") == Some(token))
                .then_some(Self::Import { token, spec })
            }
            _ => reference.map(Self::Reference),
        }
    }

    fn token(self) -> Node<'tree> {
        match self {
            Self::Reference(reference) => reference.name(),
            Self::Import { token, .. } | Self::Package(token) => token,
        }
    }
}

struct Presentation {
    signature: String,
    documentation: String,
}

impl Presentation {
    fn at(self, range: Range) -> Hover {
        // Struct tags and raw string expressions can contain backticks.
        let mut fence = String::from("```");
        while self.signature.contains(&fence) {
            fence.push('`');
        }
        let mut value = format!("{fence}go\n{}\n{fence}", self.signature);
        if !self.documentation.is_empty() {
            let _ = write!(value, "\n\n{}", self.documentation);
        }
        Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }),
            range: Some(range),
        }
    }
}

impl Engine {
    #[cfg(test)]
    pub fn hover(
        &mut self,
        path: &Path,
        position: Position,
    ) -> Result<QueryProgress<Option<Hover>>> {
        self.hover_query(path, position, true, None)
    }

    #[cfg(test)]
    pub fn hover_after_load_failure(
        &mut self,
        path: &Path,
        position: Position,
    ) -> Result<Option<Hover>> {
        match self.hover_query(path, position, false, None)? {
            QueryProgress::Ready(hover) => Ok(hover),
            _ => unreachable!("hover fallback uses only the query source"),
        }
    }

    pub(crate) fn hover_foreground(
        &mut self,
        path: &Path,
        position: Position,
        load_workspace: bool,
        memo: &QueryReadMemo,
    ) -> Result<QueryProgress<Option<Hover>>> {
        self.hover_query(path, position, load_workspace, Some(memo))
    }

    fn hover_query(
        &mut self,
        path: &Path,
        position: Position,
        load_workspace: bool,
        memo: Option<&QueryReadMemo>,
    ) -> Result<QueryProgress<Option<Hover>>> {
        let anchor = rpc_telemetry::phase("anchor");
        let mut snapshot = self.state.snapshot(self.include_tests);
        if let Some(memo) = memo {
            snapshot = snapshot.with_foreground(memo);
        }
        let document = match snapshot.document(path) {
            Ok(Some(document)) => document,
            Ok(None) => return Ok(QueryProgress::Ready(None)),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(QueryProgress::Ready(None));
            }
            Err(error) => return Err(error),
        };
        let source = document.indexed();
        let Some(target) = Target::at(&document, position) else {
            return Ok(QueryProgress::Ready(None));
        };
        drop(anchor);
        let ready = |presentation: Option<Presentation>| {
            QueryProgress::Ready(presentation.map(|info| {
                let _markup = rpc_telemetry::phase("markup");
                info.at(source.range(target.token()))
            }))
        };
        let symbol = rpc_telemetry::phase("symbol");
        let local = match target {
            Target::Reference(reference) => Declaration::local(source, reference, |name| {
                Ok(snapshot
                    .navigation_symbols(source)?
                    .symbols
                    .declaration(name)
                    .map(|symbol| symbol.range))
            })?,
            _ => None,
        };
        let local_index =
            if local.is_some_and(|local| receiver_parameter_origin(local.name).is_some()) {
                Some(snapshot.navigation_symbols(source)?)
            } else {
                None
            };
        let local_symbols = local_index.as_ref().map(|index| &index.symbols);
        if let Some(local) = local
            && let Some(info) = local.present(source, None, !load_workspace, local_symbols)?
        {
            drop(symbol);
            return Ok(ready(Some(info)));
        }
        if !load_workspace {
            let info = match target {
                Target::Package(name) => {
                    let _package_doc = rpc_telemetry::phase("package_doc");
                    Some(package_in_file(source, name))
                }
                Target::Reference(ReferenceQuery::Name(name)) if local.is_none() => {
                    builtin_presentation(node_text(name, &source.text))
                }
                _ => None,
            };
            drop(symbol);
            return Ok(ready(info));
        }

        let test_source = is_test_source(path);
        self.include_tests |= test_source;
        let from = match snapshot.package_for(&document, test_source) {
            QueryProgress::Ready(from) => from,
            QueryProgress::Pending(load) => return Ok(QueryProgress::Pending(load)),
            QueryProgress::Searching(task) => match task {},
        };
        let workspace = snapshot
            .workspace()
            .expect("package selection needs a workspace");
        let root = snapshot.root();
        let package_docs = PackageDocs {
            cache: snapshot.hover_doc_cache(),
            graph_generation: snapshot.installed_graph_generation(),
        };
        let mut resolver = snapshot.resolver(workspace);
        let info = if let Some(local) = local {
            local.present(
                source,
                from.map(|from| (&mut resolver, from)),
                true,
                local_symbols,
            )?
        } else {
            resolver.hover_target(from, source, target, root, package_docs)?
        };
        drop(symbol);
        Ok(ready(info))
    }
}

impl<'w> Resolver<'w, '_> {
    fn hover_target(
        &mut self,
        from: Option<&'w Package>,
        source: &IndexedSource,
        target: Target<'_>,
        root: &Path,
        package_docs: PackageDocs<'_>,
    ) -> Result<Option<Presentation>> {
        let reference = match target {
            Target::Package(name) => {
                let mut info = package_in_file(source, name);
                if let Some(from) = from {
                    info.documentation = package_docs.documentation(self, from)?;
                }
                return Ok(Some(info));
            }
            Target::Import { token, spec } => {
                let Some(path) = spec
                    .child_by_field_name("path")
                    .and_then(|literal| import_path(node_text(literal, &source.text)))
                else {
                    return Ok(None);
                };
                let package = match from {
                    Some(from) => self.workspace.resolve_import(from, &path),
                    None => self.workspace.package(&path),
                };
                let Some(package) = package else {
                    return Ok(None);
                };
                let signature = if spec.child_by_field_name("path") == Some(token) {
                    format!("import {path:?}")
                } else {
                    format!("package {}", package.name)
                };
                return Ok(Some(Presentation {
                    signature,
                    documentation: package_docs.documentation(self, package)?,
                }));
            }
            Target::Reference(reference) => reference,
        };

        if let Some(from) = from {
            let location = match reference {
                ReferenceQuery::Name(name) => {
                    let spelling = node_text(name, &source.text);
                    if let Some((_, package)) =
                        source.imported_package(self.workspace, from, spelling)
                    {
                        return Ok(Some(Presentation {
                            signature: format!("package {}", package.name),
                            documentation: package_docs.documentation(self, package)?,
                        }));
                    }
                    self.source_name_location(from, source, name)?
                }
                _ => self.declaration_location(from, source, reference, root)?,
            };
            if let Some((path, range)) = location {
                if path_key(&path) == path_key(&source.path) {
                    return self.present_at(source, range, from);
                }
                if let Some(declarations) = self.indexes.declaration_source(&path)?
                    && let Some(declaration) = declarations.presentation.get(&range)
                    && let Some((signature, documentation)) = declaration.hover()
                {
                    return Ok(Some(Presentation {
                        signature: signature.to_string(),
                        documentation: documentation.to_string(),
                    }));
                }
                if let Some(declaring_source) = self.indexes.reference_source(&path)? {
                    return self.present_at(declaring_source.as_ref(), range, from);
                }
                return Ok(None);
            }
            if let ReferenceQuery::Member { receiver, name } = reference
                && node_text(name, &source.text) == "Error"
                && self.is_builtin_error(from, source, receiver)?
            {
                let info = self
                    .sdk_builtin(root, |symbols| symbols.member("error", "Error"))?
                    .unwrap_or_else(|| Presentation {
                        signature: "func (error) Error() string".into(),
                        documentation: String::new(),
                    });
                return Ok(Some(info));
            }
        }
        if let ReferenceQuery::Name(name) = reference
            && name.kind() != "package_identifier"
        {
            return self.builtin_hover(root, node_text(name, &source.text));
        }
        Ok(None)
    }

    pub(super) fn is_builtin_error(
        &mut self,
        from: &'w Package,
        source: &Source,
        receiver: Node<'_>,
    ) -> Result<bool> {
        if let Some(ty) = self.infer_type(from, source, receiver, 0, 0)?
            && let Some(ty) = self.underlying_type(ty)?
            && ty.pointers == 0
            && matches!(&ty.kind, TypeKind::Named { name, .. } if name == "error")
        {
            return Ok(self.type_info(ty.package, "error")?.is_none());
        }
        Ok(receiver.kind() == "identifier"
            && node_text(receiver, &source.text) == "error"
            && self.source_name_location(from, source, receiver)?.is_none())
    }

    fn present_at(
        &mut self,
        source: &IndexedSource,
        range: Range,
        from: &'w Package,
    ) -> Result<Option<Presentation>> {
        let Some(name) = node_at_range(source, range) else {
            return Ok(None);
        };
        let package = self.declaring_package(source, from);
        Declaration::new(source, name).present(source, Some((self, package)), true, None)
    }

    pub(super) fn declaring_package(&self, source: &Source, from: &'w Package) -> &'w Package {
        self.workspace
            .package_of_file(&source.path)
            .or_else(|| draft_package(self.workspace, source))
            .unwrap_or(from)
    }

    fn builtin_hover(&mut self, root: &Path, name: &str) -> Result<Option<Presentation>> {
        if !is_predeclared(name) {
            return Ok(None);
        }
        Ok(self
            .sdk_builtin(root, |symbols| symbols.declaration(name))?
            .or_else(|| builtin_presentation(name)))
    }

    fn sdk_builtin(
        &mut self,
        root: &Path,
        lookup: impl FnOnce(&FileSymbols) -> Option<&Symbol>,
    ) -> Result<Option<Presentation>> {
        let Some(path) = optional_sdk_read(self.indexes.builtin_path(root))? else {
            return Ok(None);
        };
        let Some(declarations) =
            optional_sdk_read(self.indexes.declaration_source(&path))?.flatten()
        else {
            return Ok(None);
        };
        let Some(symbol) = lookup(&declarations.index.symbols) else {
            return Ok(None);
        };
        if let Some(declaration) = declarations.presentation.get(&symbol.range)
            && let Some((signature, documentation)) = declaration.hover()
        {
            return Ok(Some(Presentation {
                signature: signature.to_string(),
                documentation: documentation.to_string(),
            }));
        }
        let Some(source) = optional_sdk_read(self.indexes.reference_source(&path))?.flatten()
        else {
            return Ok(None);
        };
        let source = source.as_ref();
        let Some(name) = node_at_range(source, symbol.range) else {
            return Ok(None);
        };
        optional_sdk_read(Declaration::new(source, name).present(source, None, true, None))
            .map(Option::flatten)
    }
}

fn optional_sdk_read<T>(result: Result<T>) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error)
            if error.is::<SharedLoadPending>()
                || error.is::<QueryReadLimit>()
                || error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::Interrupted) =>
        {
            Err(error)
        }
        Err(_) => Ok(None),
    }
}

pub(super) fn projected_signature<'a>(
    source: &IndexedSource,
    range: Range,
    completion_detail: Option<&'a str>,
) -> Option<Cow<'a, str>> {
    let name = node_at_range(source, range)?;
    let owner = field_declaration(name).or_else(|| declaration_owner(name))?;
    if matches!(
        owner.kind(),
        "function_declaration" | "method_declaration" | "type_spec" | "type_alias"
    ) {
        return completion_detail.map(Cow::Borrowed);
    }
    let declaration = Declaration::new(source, name);
    // Inferred declarations can depend on imports and other package files.
    let syntax_only = matches!(
        owner.kind(),
        "method_elem" | "field_declaration" | "type_parameter_declaration"
    ) || declaration.binding.is_some_and(|binding| {
        let (explicit, initializer, index) = value_parts(owner, binding);
        explicit.is_some()
            || initializer.is_some_and(|value| values::literal_value_type(value, index).is_some())
    });
    if !syntax_only {
        return None;
    }
    Some(Cow::Owned(
        declaration.signature(source, owner, None, false).ok()??,
    ))
}

pub(super) fn projected_package_documentation(source: &Source) -> Option<(&str, String)> {
    let root = source.tree.root_node();
    let clause = root
        .named_children(&mut root.walk())
        .find(|node| node.kind() == "package_clause")?;
    let name = clause
        .named_children(&mut clause.walk())
        .find(|node| node.kind() == "package_identifier")?;
    Some((
        node_text(name, &source.text),
        documentation(clause, &source.text),
    ))
}

#[derive(Clone, Copy)]
struct Declaration<'tree> {
    name: Node<'tree>,
    binding: Option<Binding<'tree>>,
}

impl<'tree> Declaration<'tree> {
    fn new(source: &Source, name: Node<'tree>) -> Self {
        Self {
            name,
            binding: declaration_binding(name, &source.text),
        }
    }

    fn local(
        source: &'tree IndexedSource,
        reference: ReferenceQuery<'tree>,
        file_declaration: impl FnOnce(&str) -> Result<Option<Range>>,
    ) -> Result<Option<Self>> {
        let name = reference.name();
        if field_declaration(name).is_some_and(|field| field.child_by_field_name("name").is_none())
        {
            return Ok(Some(Self::new(source, name)));
        }
        let ReferenceQuery::Name(name) = reference else {
            return Ok(None);
        };
        if let Some(target) = reference_binding(name, &source.text) {
            let mut declaration = Self::new(source, target);
            if target != name {
                declaration.binding =
                    lexical_binding(name, node_text(name, &source.text), &source.text)
                        .filter(|binding| binding.name == target)
                        .or(declaration.binding);
            }
            return Ok(Some(declaration));
        }
        if is_declaration_name(name) {
            return Ok(Some(Self::new(source, name)));
        }
        if name.kind() == "package_identifier" {
            return Ok(None);
        }
        Ok(file_declaration(node_text(name, &source.text))?
            .and_then(|range| node_at_range(source, range))
            .map(|target| Self::new(source, target)))
    }

    fn present<'w>(
        self,
        source: &IndexedSource,
        context: Option<(&mut Resolver<'w, '_>, &'w Package)>,
        allow_partial: bool,
        local_symbols: Option<&FileSymbols>,
    ) -> Result<Option<Presentation>> {
        let Some(owner) = field_declaration(self.name).or_else(|| declaration_owner(self.name))
        else {
            return Ok(None);
        };
        if owner.kind() == "type_elem" {
            let name = node_text(self.name, &source.text);
            let constraint =
                receiver_parameter_constraint(source, self.name, context, local_symbols)?;
            return Ok(match constraint {
                Some(constraint) => Some(Presentation {
                    signature: format!("type param {name} {constraint}"),
                    documentation: documentation(owner, &source.text),
                }),
                None if allow_partial => Some(Presentation {
                    signature: format!("type param {name}"),
                    documentation: String::new(),
                }),
                None => None,
            });
        }
        Ok(self
            .signature(source, owner, context, allow_partial)?
            .map(|signature| Presentation {
                signature,
                documentation: documentation(owner, &source.text),
            }))
    }

    fn signature<'w>(
        self,
        source: &IndexedSource,
        owner: Node<'_>,
        context: Option<(&mut Resolver<'w, '_>, &'w Package)>,
        allow_partial: bool,
    ) -> Result<Option<String>> {
        let name = node_text(self.name, &source.text);
        let signature = match owner.kind() {
            "function_declaration" | "method_declaration" | "method_elem" => {
                let interface = (owner.kind() == "method_elem")
                    .then(|| enclosing_type_name(owner.parent()?, &source.text))
                    .flatten();
                function_signature(owner, &source.text, interface)
            }
            "type_spec" | "type_alias" => {
                let parameters = owner
                    .child_by_field_name("type_parameters")
                    .map_or_else(String::new, |node| syntax_text(node, &source.text));
                let Some(ty) = owner.child_by_field_name("type") else {
                    return Ok(None);
                };
                let equals = if owner.kind() == "type_alias" {
                    " ="
                } else {
                    ""
                };
                format!(
                    "type {name}{parameters}{equals} {}",
                    syntax_text(ty, &source.text)
                )
            }
            "field_declaration" => {
                let Some(ty) = owner.child_by_field_name("type") else {
                    return Ok(None);
                };
                let pointer = if owner
                    .children(&mut owner.walk())
                    .any(|node| node.kind() == "*")
                {
                    "*"
                } else {
                    ""
                };
                let mut field = format!("field {name} {pointer}{}", syntax_text(ty, &source.text));
                if let Some(tag) = owner.child_by_field_name("tag") {
                    let _ = write!(field, " {}", node_text(tag, &source.text));
                }
                if let Some(structure) = owner.parent().and_then(|list| list.parent())
                    && let Some(owner) = enclosing_type_name(structure, &source.text)
                {
                    let _ = write!(field, " of {owner} type");
                }
                field
            }
            "type_parameter_declaration" => {
                let Some(constraint) = owner.child_by_field_name("type") else {
                    return Ok(None);
                };
                format!(
                    "type param {name} {}",
                    syntax_text(constraint, &source.text)
                )
            }
            _ => {
                let Some(binding) = self.binding else {
                    return Ok(None);
                };
                let Some(value) = value_signature(source, owner, binding, context, allow_partial)?
                else {
                    return Ok(None);
                };
                value
            }
        };
        Ok(Some(signature))
    }
}

fn declaration_owner(name: Node<'_>) -> Option<Node<'_>> {
    let parent = name.parent()?;
    if parent.kind() == "expression_list" {
        parent.parent()
    } else {
        Some(parent)
    }
}

fn field_declaration(name: Node<'_>) -> Option<Node<'_>> {
    let mut node = name;
    while let Some(parent) = node.parent() {
        match parent.kind() {
            "qualified_type" if parent.child_by_field_name("name") == Some(node) => node = parent,
            "generic_type" if parent.child_by_field_name("type") == Some(node) => node = parent,
            "field_declaration" if parent.child_by_field_name("name").is_none() => {
                return (parent.child_by_field_name("type") == Some(node)).then_some(parent);
            }
            "field_declaration" if is_declaration_name(name) => return Some(parent),
            _ => return None,
        }
    }
    None
}

fn enclosing_type_name<'a>(ty: Node<'_>, text: &'a str) -> Option<&'a str> {
    let declaration = ty.parent().filter(|node| is_type_declaration(*node))?;
    let name = declaration.child_by_field_name("name")?;
    (declaration.child_by_field_name("type") == Some(ty)).then(|| node_text(name, text))
}

fn package_in_file(source: &Source, name: Node<'_>) -> Presentation {
    Presentation {
        signature: format!("package {}", node_text(name, &source.text)),
        documentation: name
            .parent()
            .map_or_else(String::new, |clause| documentation(clause, &source.text)),
    }
}

fn receiver_parameter_constraint<'w>(
    source: &IndexedSource,
    parameter: Node<'_>,
    context: Option<(&mut Resolver<'w, '_>, &'w Package)>,
    local_symbols: Option<&FileSymbols>,
) -> Result<Option<String>> {
    let Some((base, index)) = receiver_parameter_origin(parameter) else {
        return Ok(None);
    };
    let base = node_text(base, &source.text);
    let constraint = |source: &Source, range| {
        let declaration = node_at_range(source, range)?.parent()?;
        let parameters = declaration.child_by_field_name("type_parameters")?;
        let mut cursor = parameters.walk();
        let mut remaining = index;
        for parameter in parameters.named_children(&mut cursor) {
            let count = parameter
                .children_by_field_name("name", &mut parameter.walk())
                .filter(|name| name.is_named())
                .count();
            if remaining < count {
                let ty = parameter.child_by_field_name("type")?;
                return Some(syntax_text(ty, &source.text));
            }
            remaining -= count;
        }
        None
    };
    if let Some(symbol) = local_symbols.and_then(|symbols| symbols.declaration(base)) {
        return Ok(constraint(source, symbol.range));
    }
    if let Some((resolver, from)) = context {
        if local_symbols.is_none()
            && let Some(symbol) = resolver
                .indexes
                .file_index(&source.path)?
                .symbols
                .declaration(base)
        {
            return Ok(constraint(source, symbol.range));
        }
        if let Some(symbol) = resolver.find_symbol(from, base, None)?
            && let Some(definition) = resolver.indexes.reference_source(&symbol.file)?
        {
            return Ok(constraint(definition.as_ref(), symbol.symbol.range));
        }
    }
    Ok(None)
}

fn builtin_presentation(name: &str) -> Option<Presentation> {
    let signature = match name {
        "error" => "type error interface { Error() string }",
        "any" => "type any = interface{}",
        "comparable" => "type comparable interface{ comparable }",
        "byte" => "type byte = uint8",
        "rune" => "type rune = int32",
        "true" => "const true bool",
        "false" => "const false bool",
        "iota" => "const iota int",
        "nil" => "var nil Type",
        _ if is_predeclared_type(name) => {
            return Some(Presentation {
                signature: format!("type {name} {name}"),
                documentation: String::new(),
            });
        }
        _ => builtin_function(name)?,
    };
    Some(Presentation {
        signature: signature.into(),
        documentation: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use std::{fs, path::PathBuf};

    use lsp_types::TextDocumentContentChangeEvent;

    use super::*;
    use crate::semantic::CacheEvent;
    use crate::semantic::source::LineIndex;
    use crate::test_support::TestProject;
    use crate::workspace::Workspace;

    fn fixture_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tools/fixtures/hover-signature")
    }

    fn open(text: &str) -> (Engine, PathBuf) {
        let root = fixture_root();
        let path = root.join("main.go");
        let mut engine = Engine::new(root);
        engine.open(path.clone(), text.into(), 1).unwrap();
        (engine, path)
    }

    fn token_range(text: &str, anchor: &str, token: &str) -> Range {
        let start = text.find(anchor).expect("anchor in source")
            + anchor.find(token).expect("token in anchor");
        let lines = LineIndex::new(text);
        Range::new(
            lines.position(text, start),
            lines.position(text, start + token.len()),
        )
    }

    fn lookup(engine: &mut Engine, path: &Path, position: Position) -> Option<Hover> {
        let query = engine.hover(path, position).unwrap();
        if let QueryProgress::Ready(hover) = query {
            return hover;
        }
        let QueryProgress::Pending(load) = query else {
            panic!("hover unexpectedly started a search");
        };
        let workspace = Workspace::load(engine.root(), load.include_tests).unwrap();
        assert!(engine.install_workspace(load, workspace));
        match engine.hover(path, position).unwrap() {
            QueryProgress::Ready(hover) => hover,
            _ => panic!("hover did not resolve after loading the workspace"),
        }
    }

    fn lookup_foreground(engine: &mut Engine, path: &Path, position: Position) -> Hover {
        let memo = QueryReadMemo::default();
        for _ in 0..64 {
            match engine.hover_foreground(path, position, true, &memo) {
                Ok(QueryProgress::Ready(Some(hover))) => return hover,
                Err(error) if error.is::<SharedLoadPending>() => {
                    let pending = error.downcast::<SharedLoadPending>().unwrap();
                    while !pending.ready() {
                        engine
                            .shared_disk_wake()
                            .recv_timeout(Duration::from_secs(10))
                            .unwrap();
                    }
                }
                Ok(QueryProgress::Ready(None)) => panic!("foreground hover did not find a target"),
                Ok(QueryProgress::Pending(load)) => panic!("foreground hover requested {load:?}"),
                Ok(QueryProgress::Searching(task)) => match task {},
                Err(error) => panic!("foreground hover failed: {error:#}"),
            }
        }
        panic!("foreground hover kept replaying");
    }

    fn markdown(hover: Hover) -> String {
        let HoverContents::Markup(contents) = hover.contents else {
            panic!("expected Markdown");
        };
        assert_eq!(contents.kind, MarkupKind::Markdown);
        contents.value
    }

    fn at(engine: &mut Engine, path: &Path, text: &str, anchor: &str, token: &str) -> String {
        let range = token_range(text, anchor, token);
        let hover = lookup(engine, path, range.start)
            .unwrap_or_else(|| panic!("missing hover for {token} in {anchor}"));
        assert_eq!(hover.range, Some(range));
        markdown(hover)
    }

    #[test]
    fn keeps_doc_paragraphs_and_parameter_names_and_filters_directives() {
        let text = concat!(
            "package main\r\n\r\n",
            "// Detached.\r\n\r\n",
            "/**\r\n * Sum adds the values.\r\n *\r\n * It reports the total.\r\n */\r\n",
            "// Each\r\n// line can describe a result.\r\n",
            "//go:noinline\r\n// +build ignore\r\n//line main.go:5\r\n",
            "//extern sum\r\n//export Sum\r\n//noinspection GoUnusedFunction\r\n",
            "func Sum(a, b int) (total, count int, err error) { return a + b, 2, nil } // public\r\n",
            "func Next() {}\r\n",
        );
        let (mut engine, path) = open(text);
        let sum = at(&mut engine, &path, text, "func Sum", "Sum");
        assert_eq!(
            sum,
            "```go\nfunc Sum(a int, b int) (total int, count int, err error)\n```\n\nSum adds the values.\n\nIt reports the total.\nEach\nline can describe a result.\n\npublic"
        );
        assert_eq!(
            at(&mut engine, &path, text, "func Next", "Next"),
            "```go\nfunc Next()\n```"
        );
        assert!(engine.state.workspace.is_none());
    }

    #[test]
    fn group_doc_fallback_keeps_item_inline_comments_and_respects_own_doc() {
        let text = r#"package main

// Constants are shared.
const (
    First = 1 // first inline
    // Second has its own doc.
    Second = 2 // second inline
    //go:nosplit
    Third = 3
)

// Settings are configurable.
var (
    State = "ok" // current state
)

// Shared types.
type (
    Counter int // exact count
    // Label describes text.
    Label = string
)

// Limits the count.
const Limit = 5 // at most five
"#;
        let (mut engine, path) = open(text);
        let cases = [
            (
                "First =",
                "First",
                "const First int = 1",
                "Constants are shared.\n\nfirst inline",
            ),
            (
                "Second =",
                "Second",
                "const Second int = 2",
                "Second has its own doc.\n\nsecond inline",
            ),
            (
                "Third =",
                "Third",
                "const Third int = 3",
                "Constants are shared.",
            ),
            (
                "State =",
                "State",
                "var State string = \"ok\"",
                "Settings are configurable.\n\ncurrent state",
            ),
            (
                "Counter int",
                "Counter",
                "type Counter int",
                "Shared types.\n\nexact count",
            ),
            (
                "Label =",
                "Label",
                "type Label = string",
                "Label describes text.",
            ),
            (
                "const Limit",
                "Limit",
                "const Limit int = 5",
                "Limits the count.\n\nat most five",
            ),
        ];
        for (anchor, name, signature, docs) in cases {
            assert_eq!(
                at(&mut engine, &path, text, anchor, name),
                format!("```go\n{signature}\n```\n\n{docs}"),
                "{name}",
            );
        }
        assert!(engine.state.workspace.is_none());
    }

    #[test]
    fn renders_receiver_params_struct_tags_and_named_results() {
        let text = r#"package main

type Box[T ~int] struct {
    // Stored count.
    Count T `json:"count" example:"http://example.com"` // updated by Set
    *Metadata `json:"metadata"`
}

type Metadata struct{ Name string }
type IntBox = Box[int]

func (b *Box[Item]) Get() (value Item) { return b.Count }
func Wrap[T any](in T) (out T, err error) { return in, nil }
"#;
        let (mut engine, path) = open(text);
        let field = at(&mut engine, &path, text, "Count T", "Count");
        assert_eq!(
            field,
            "```go\nfield Count T `json:\"count\" example:\"http://example.com\"` of Box type\n```\n\nStored count.\n\nupdated by Set"
        );
        assert!(
            at(&mut engine, &path, text, "*Metadata", "Metadata")
                .contains("field Metadata *Metadata `json:\"metadata\"` of Box type")
        );
        let structure = at(&mut engine, &path, text, "type Box", "Box");
        assert!(structure.contains("type Box[T ~int] struct"));
        assert!(structure.contains("http://example.com"));
        assert!(!structure.contains("updated by Set"));
        for (anchor, token, expected) in [
            ("type IntBox", "IntBox", "type IntBox = Box[int]"),
            ("Get()", "Get", "func (b *Box[Item]) Get() (value Item)"),
            ("return b.Count", "b", "var b *Box[Item]"),
            ("Box[Item]", "Item", "type param Item ~int"),
            ("value Item", "Item", "type param Item ~int"),
            (
                "Wrap[T",
                "Wrap",
                "func Wrap[T any](in T) (out T, err error)",
            ),
            ("Wrap[T", "T", "type param T any"),
            ("in T)", "T", "type param T any"),
        ] {
            assert_eq!(
                at(&mut engine, &path, text, anchor, token),
                format!("```go\n{expected}\n```")
            );
        }
        assert!(engine.state.workspace.is_none());
    }

    #[test]
    fn resolves_fields_of_local_structs_and_methods_of_local_interfaces() {
        let text = r#"package main

func run() {
    type Local struct {
        // Count is the total.
        Count int
    }
    var item Local
    _ = item.Count
    type Reader interface {
        // Next returns the next item.
        Next() int
    }
    var reader Reader
    _ = reader.Next()
}
"#;
        let (mut engine, path) = open(text);
        assert_eq!(
            at(&mut engine, &path, text, "item.Count", "Count"),
            "```go\nfield Count int of Local type\n```\n\nCount is the total.",
        );
        assert_eq!(
            at(&mut engine, &path, text, "reader.Next", "Next"),
            "```go\nfunc (Reader) Next() int\n```\n\nNext returns the next item.",
        );
    }

    #[test]
    fn infers_values_and_switch_and_range_bindings_in_the_current_scope() {
        let text = r#"package main

const (
    first = iota
    next
)

func get() (count int64, err error) { return 3, nil }

func run(payload any) {
    // Word in the next line.
    word := "hello"
    number, fault := get() // multiple results
    _ = number
    _ = fault
    for index, letter := range word {
        _, _ = index, letter
    }
    for k, v := range []int64{1, 2} {
        _, _ = k, v
    }
    for ordinal := range 6 {
        _ = ordinal
    }
    switch typed := payload.(type) {
    case string:
        _ = typed
    case int, bool:
        _ = typed // multiple case types
    default:
        _ = typed // default case
    }
    pair := get
    _ = pair
    values := map[string]int{"one": 1}
    _, found := values["one"]
    _ = found
    last := 42 // trailing local
}
"#;
        let (mut engine, path) = open(text);
        let word = at(&mut engine, &path, text, "word :=", "word");
        assert!(word.contains("var word string = \"hello\"\n```\n\nWord in the next line."));
        assert!(engine.state.workspace.is_none());
        for (anchor, token, expected) in [
            ("\n    next\n", "next", "const next int = iota"),
            ("_ = number", "number", "var number int64 = get()"),
            ("_ = fault", "fault", "var fault error = get()"),
            ("index, letter :=", "index", "var index int"),
            ("_, _ = index, letter", "letter", "var letter rune"),
            ("k, v :=", "k", "var k int"),
            ("_, _ = k, v", "v", "var v int64"),
            ("ordinal :=", "ordinal", "var ordinal int"),
            ("switch typed :=", "typed", "var typed any"),
            (
                "case string:\n        _ = typed",
                "typed",
                "var typed string",
            ),
            ("_ = typed // multiple case", "typed", "var typed any"),
            ("_ = typed // default", "typed", "var typed any"),
            (
                "_ = pair",
                "pair",
                "var pair func() (count int64, err error) = get",
            ),
            ("_ = found", "found", "var found bool = values[\"one\"]"),
        ] {
            let value = at(&mut engine, &path, text, anchor, token);
            assert!(
                value.starts_with(&format!("```go\n{expected}\n```")),
                "{token}: {value}"
            );
        }
        assert!(at(&mut engine, &path, text, "last :=", "last").ends_with("\n\ntrailing local"));
    }

    #[test]
    fn imports_and_qualifiers_use_package_docs_and_exact_utf16_ranges() {
        let text = concat!(
            "package main\n",
            "import /* 😀 */ util `example.com/hoversig/helper`\n",
            "func run() { _ = util.DefaultName }\n",
            "func shadow(util int) { _ = util }\n",
        );
        let (mut engine, path) = open(text);
        let range = token_range(text, "util `example", "util");
        assert_eq!(range.start.character, 16);
        assert!(matches!(
            engine.hover(&path, range.start).unwrap(),
            QueryProgress::Pending(_)
        ));
        for (anchor, token, spelling) in [
            ("util `example", "util", "package helper"),
            ("util.DefaultName", "util", "package helper"),
            (
                "`example.com/hoversig/helper`",
                "`example.com/hoversig/helper`",
                "import \"example.com/hoversig/helper\"",
            ),
        ] {
            assert_eq!(
                at(&mut engine, &path, text, anchor, token),
                format!(
                    "```go\n{spelling}\n```\n\nPackage helper provides utilities for hover and signature help baselines."
                ),
            );
        }
        assert_eq!(
            at(&mut engine, &path, text, "_ = util }", "util"),
            "```go\nvar util int\n```"
        );
    }

    #[test]
    fn follows_unsaved_cross_package_signature_and_doc_edits_and_close() {
        let root = fixture_root();
        let caller = root.join("main.go");
        let helper = root.join("helper/helper.go");
        let docs = root.join("helper/doc.go");
        let text = fs::read_to_string(&caller).unwrap();
        let disk = fs::read_to_string(&helper).unwrap();
        let mut engine = Engine::new(root);
        engine.open(caller.clone(), text.clone(), 1).unwrap();
        engine.open(helper.clone(), disk.clone(), 1).unwrap();
        let get_map = |engine: &mut Engine| at(engine, &caller, &text, "helper.Map([]int", "Map");
        let initial = get_map(&mut engine);
        assert!(initial.contains("items []T, fn func(T) U"));
        assert!(initial.contains("Map transforms each element using fn."));
        for (version, old, new) in [
            (2, "items []T", "values []T"),
            (
                3,
                "Map transforms each element using fn.",
                "Map documents the edited buffer.",
            ),
        ] {
            engine
                .edit(
                    &helper,
                    &[TextDocumentContentChangeEvent {
                        range: Some(token_range(&disk, old, old)),
                        range_length: None,
                        text: new.into(),
                    }],
                    version,
                )
                .unwrap();
        }
        let edited = get_map(&mut engine);
        assert!(edited.contains("values []T, fn func(T) U"));
        assert!(edited.contains("Map documents the edited buffer."));
        engine.close(&helper);
        assert_eq!(get_map(&mut engine), initial);

        let import = "\"example.com/hoversig/helper\"";
        let get_import = |engine: &mut Engine| at(engine, &caller, &text, import, import);
        let initial_doc = get_import(&mut engine);
        engine
            .open(
                docs.clone(),
                "// Package helper has unsaved docs.\npackage helper\n".into(),
                1,
            )
            .unwrap();
        assert!(get_import(&mut engine).contains("Package helper has unsaved docs."));
        engine
            .open(docs.clone(), "package helper\n".into(), 2)
            .unwrap();
        engine
            .open(
                helper.clone(),
                format!("// Package helper has docs in helper.go.\n{disk}"),
                4,
            )
            .unwrap();
        assert!(get_import(&mut engine).contains("Package helper has docs in helper.go."));
        engine.close(&helper);
        engine.close(&docs);
        assert_eq!(get_import(&mut engine), initial_doc);
    }

    #[test]
    fn foreground_package_docs_revalidate_the_ordered_prefix() {
        let project = TestProject::new();
        let text = "package sample\nfunc run() {}\n";
        let origin = project.write("main.go", text);
        let extra = project.write(
            "package.go",
            "// Fallback package documentation.\npackage sample\n",
        );
        let preferred = project.write("doc.go", "package sample\n");
        let workspace = project.workspace(&["package.go", "main.go", "doc.go"], false);
        fs::remove_file(&preferred).unwrap();
        let mut engine = Engine::new(project.root.clone());
        assert!(engine.install_workspace(engine.workspace_load(), workspace));
        engine.open(origin.clone(), text.into(), 1).unwrap();
        let position = Position::new(0, 9);
        let docs = |engine: &mut Engine| markdown(lookup_foreground(engine, &origin, position));
        let hits = |engine: &Engine| engine.state.hover_docs.borrow().hits;

        let fallback = docs(&mut engine);
        assert!(fallback.ends_with("Fallback package documentation."));
        assert_eq!(docs(&mut engine), fallback);
        assert_eq!(hits(&engine), 1);

        engine
            .open(preferred.clone(), "package sample\n".into(), 1)
            .unwrap();
        assert_eq!(docs(&mut engine), fallback);
        assert_eq!(hits(&engine), 1);

        engine
            .edit(
                &preferred,
                &[TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: "// Preferred package documentation.\npackage sample\n".into(),
                }],
                2,
            )
            .unwrap();
        let preferred_doc = docs(&mut engine);
        assert!(preferred_doc.ends_with("Preferred package documentation."));
        assert_eq!(hits(&engine), 1);

        engine
            .open(
                extra,
                "// Edited fallback documentation.\npackage sample\n".into(),
                1,
            )
            .unwrap();
        assert_eq!(docs(&mut engine), preferred_doc);
        assert_eq!(hits(&engine), 2);

        engine.close(&preferred);
        assert!(docs(&mut engine).ends_with("Edited fallback documentation."));
        assert_eq!(hits(&engine), 2);
    }

    #[test]
    fn foreground_hover_uses_lightweight_member_presentation() {
        let project = TestProject::new();
        let text = "package sample\nfunc (box Box[Item]) run(value Item) { /* 😀 */ _ = Shape{}.Field; value.Run() }\n";
        let origin = project.write("main.go", text);
        let target = project.write("types.go", concat!(
            "package sample\n",
            "type Box[Value T] struct{}\n",
            "type T interface {\n// Run describes the next step.\nRun()\n}\n",
            "type Shape struct {\n// Field holds a label.\nField string `json:\"field\"` // used by clients\n}\n",
                "func body() { skipped := 123; _ = skipped }\n",
        ));
        let mut engine = Engine::new(project.root.clone());
        assert!(engine.install_workspace(
            engine.workspace_load(),
            project.workspace(&["main.go", "types.go"], false)
        ));
        engine.open(origin.clone(), text.into(), 1).unwrap();
        let full_parses = Arc::new(AtomicUsize::new(0));
        let count = full_parses.clone();
        engine.set_shared_disk_hook(Some(Arc::new(move |path, event| {
            if path == path_key(&target) && event == CacheEvent::Parse {
                count.fetch_add(1, Ordering::Relaxed);
            }
        })));

        for (anchor, token, expected) in [
            (
                "value.Run()",
                "Run",
                "```go\nfunc (T) Run()\n```\n\nRun describes the next step.",
            ),
            (
                "Shape{}.Field",
                "Field",
                "```go\nfield Field string `json:\"field\"` of Shape type\n```\n\nField holds a label.\n\nused by clients",
            ),
        ] {
            let range = token_range(text, anchor, token);
            let hover = lookup_foreground(&mut engine, &origin, range.start);
            assert_eq!(hover.range, Some(range));
            assert_eq!(markdown(hover), expected);
        }
        assert_eq!(full_parses.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn uses_sdk_builtin_docs_and_has_signatures_without_a_workspace() {
        let text = r#"package main
func run(values []int) {
    _ = append(values, 1)
    _ = len(values)
    var err error
    clear(values)
    _ = err.Error()
}
func shadow(len int) { _ = len }
"#;
        let (mut engine, path) = open(text);
        for (anchor, name, signature) in [
            (
                "append(",
                "append",
                "func append(slice []Type, elems ...Type) []Type",
            ),
            ("len(values)", "len", "func len(v Type) int"),
            ("err error", "error", "type error interface"),
            ("clear(values)", "clear", "func clear"),
        ] {
            let span = token_range(text, anchor, name);
            let fallback = engine
                .hover_after_load_failure(&path, span.start)
                .unwrap()
                .unwrap();
            assert_eq!(fallback.range, Some(span));
            assert!(markdown(fallback).starts_with(&format!("```go\n{signature}")));
        }
        let sdk = at(&mut engine, &path, text, "append(", "append");
        assert!(
            sdk.contains("The append built-in function appends elements to the end of a slice.")
        );
        assert_eq!(
            at(&mut engine, &path, text, "err.Error", "Error"),
            "```go\nfunc (error) Error() string\n```",
        );
        assert_eq!(
            at(&mut engine, &path, text, "_ = len }", "len"),
            "```go\nvar len int\n```"
        );
    }

    #[test]
    fn returns_null_at_separators_comments_literals_and_unresolved_names() {
        let text =
            "package main\n\nfunc run() { /* 😀 */ v := 123; _ = \"value\"; _ = Unknown; _ = v }\n";
        let (mut engine, path) = open(text);
        for (anchor, token) in [
            ("func run", " "),
            ("/* 😀 */", "😀"),
            ("123", "123"),
            ("\"value\"", "value"),
            ("Unknown", "Unknown"),
            ("_ = Unknown", "_"),
            ("v := ", " := "),
        ] {
            let position = token_range(text, anchor, token).start;
            assert!(lookup(&mut engine, &path, position).is_none(), "{anchor}");
        }
        let value = at(&mut engine, &path, text, "_ = v }", "v");
        assert_eq!(value, "```go\nvar v int = 123\n```");
        let emoji = token_range(text, "/* 😀 */", "😀").start;
        assert!(
            lookup(
                &mut engine,
                &path,
                Position::new(emoji.line, emoji.character + 1)
            )
            .is_none()
        );
    }
}
