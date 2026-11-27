use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use lsp_types::{Position, SymbolKind, TypeHierarchyItem, Uri};
use tree_sitter::Node;

use super::{
    BATCH_TIME, Hierarchy, Location, SearchResults, declaration_at, document, in_package, walk,
};
use crate::protocol::path_uri;
use crate::semantic::implementation::{
    ImplementationSearch, InterfaceSources, TypeHierarchyDirection, stats,
};
use crate::semantic::index::active_package;
use crate::semantic::resolver::{MAX_TYPE_DEPTH, Resolver, TypeKind};
use crate::semantic::snapshot::EngineSnapshot;
use crate::semantic::source::{Source, is_test_source, is_type_declaration, node_text};
use crate::semantic::{Engine, QueryProgress, SemanticTask};
use crate::workspace::{Workspace, path_key};

fn type_syntax(spec: Node<'_>) -> Option<Node<'_>> {
    let mut ty = spec.child_by_field_name("type")?;
    while ty.kind() == "parenthesized_type" {
        ty = ty
            .named_children(&mut ty.walk())
            .find(|child| !child.is_extra())?;
    }
    Some(ty)
}

fn type_kind(ty: Node<'_>) -> SymbolKind {
    match ty.kind() {
        "interface_type" => SymbolKind::INTERFACE,
        "struct_type" => SymbolKind::STRUCT,
        _ => SymbolKind::CLASS,
    }
}

fn type_item(source: &Source, spec: Node<'_>) -> Result<Option<TypeHierarchyItem>> {
    if !is_type_declaration(spec) {
        return Ok(None);
    }
    let (Some(name), Some(ty)) = (spec.child_by_field_name("name"), type_syntax(spec)) else {
        return Ok(None);
    };
    let spelling = node_text(name, &source.text);
    if spelling.is_empty() || spelling == "_" {
        return Ok(None);
    }
    let declaration = spec
        .parent()
        .filter(|parent| {
            parent.kind() == "type_declaration"
                && !parent
                    .children(&mut parent.walk())
                    .any(|child| child.kind() == "(")
        })
        .unwrap_or(spec);
    let selection_range = source.range(name);
    let uri = path_uri(&source.path)?;
    Ok(Some(TypeHierarchyItem {
        name: spelling.to_owned(),
        kind: type_kind(ty),
        tags: None,
        detail: source.outline().package.clone(),
        data: Some(Hierarchy::Type.data(&uri, selection_range)?),
        uri,
        range: source.range(declaration),
        selection_range,
    }))
}

fn item_at(
    resolver: &mut Resolver<'_, '_>,
    location: &Location,
) -> Result<Option<TypeHierarchyItem>> {
    let Some(origin) = resolver.indexes.reference_source(&location.0)? else {
        return Ok(None);
    };
    let source = origin.as_ref();
    let Some(spec) = declaration_at(source, location.1.start) else {
        return Ok(None);
    };
    type_item(source, spec)
}

fn expression_at(source: &Source, position: Position) -> Option<Node<'_>> {
    let offset = source.lines.offset(&source.text, position)?;
    if offset >= source.text.len() {
        return None;
    }
    let leaf = source
        .tree
        .root_node()
        .descendant_for_byte_range(offset, offset + 1)?;
    for node in std::iter::successors(Some(leaf), Node::parent) {
        match node.kind() {
            "call_expression"
            | "type_conversion_expression"
            | "composite_literal"
            | "index_expression"
            | "selector_expression"
            | "type_assertion_expression"
            | "parenthesized_expression"
            | "unary_expression" => return Some(node),
            "argument_list" | "literal_value" if !leaf.is_named() || node == leaf => {}
            _ if node.is_named() && node != leaf => return None,
            "comment" => return None,
            _ => {}
        }
    }
    None
}

pub(crate) fn prepare_type_hierarchy(
    engine: &mut Engine,
    path: &Path,
    position: Position,
) -> Result<QueryProgress<Option<Vec<TypeHierarchyItem>>>> {
    engine.include_tests |= is_test_source(path);
    let mut snapshot = engine.snapshot();
    let Some(document) = document(&mut snapshot, path)? else {
        return Ok(QueryProgress::Ready(None));
    };
    let source = document.indexed();
    if let Some(spec) = declaration_at(source, position)
        && is_type_declaration(spec)
    {
        return Ok(QueryProgress::Ready(
            type_item(source, spec)?.map(|item| vec![item]),
        ));
    }
    let reference = source.reference_at(position);
    let expression = expression_at(source, position);
    if reference.is_none() && expression.is_none() {
        return Ok(QueryProgress::Ready(None));
    }
    let root = snapshot.root();
    in_package(&mut snapshot, &document, |resolver, package| {
        if let Some(reference) = reference {
            if reference.name().kind() == "type_identifier"
                && let Some(location) =
                    resolver.declaration_location(package, source, reference, root)?
                && let Some(item) = item_at(resolver, &location)?
            {
                return Ok(Some(vec![item]));
            }
            if let Some(location) = resolver.type_definition(package, source, reference, root)?
                && let Some(item) = item_at(resolver, &location)?
            {
                return Ok(Some(vec![item]));
            }
        }
        if let Some(expression) = expression
            && let Some(ty) = resolver.infer_type(package, source, expression, 0, 0)?
            && let TypeKind::Named { name, .. } = ty.kind
            && let Some(symbol) = resolver.find_symbol(ty.package, &name, None)?
        {
            return Ok(item_at(resolver, &symbol.location())?.map(|item| vec![item]));
        }
        Ok(None)
    })
}

fn type_name(mut ty: Node<'_>) -> Option<Node<'_>> {
    loop {
        ty = match ty.kind() {
            "type_identifier" | "identifier" => return Some(ty),
            "qualified_type" => return ty.child_by_field_name("name"),
            "generic_type" => ty.child_by_field_name("type")?,
            "parenthesized_type" => ty
                .named_children(&mut ty.walk())
                .find(|child| !child.is_extra())?,
            _ => return None,
        };
    }
}

fn embedded_names(ty: Node<'_>) -> Vec<Node<'_>> {
    if ty.kind() == "interface_type" {
        return ty
            .named_children(&mut ty.walk())
            .filter(|element| element.kind() == "type_elem")
            .filter_map(|element| {
                let mut cursor = element.walk();
                let mut terms = element
                    .named_children(&mut cursor)
                    .filter(|term| !term.is_extra());
                let term = terms.next()?;
                if terms.next().is_some() {
                    return None;
                }
                type_name(term)
            })
            .collect();
    }
    let Some(fields) = ty
        .named_children(&mut ty.walk())
        .find(|node| node.kind() == "field_declaration_list")
    else {
        return Vec::new();
    };
    fields
        .named_children(&mut fields.walk())
        .filter(|field| {
            field.kind() == "field_declaration" && field.child_by_field_name("name").is_none()
        })
        .filter_map(|field| type_name(field.child_by_field_name("type")?))
        .collect()
}

fn is_local_type(spec: Node<'_>) -> bool {
    std::iter::successors(spec.parent(), Node::parent)
        .any(|ancestor| ancestor.kind() == "statement_list")
}

struct TypeShape {
    // Aliases share identity. A defined type only shares its underlying body.
    identity: Location,
    body: Location,
    kind: SymbolKind,
    empty_interface_body: bool,
}

struct TypeRelations<'w, 'idx> {
    resolver: Resolver<'w, 'idx>,
    new_documents: &'w HashSet<PathBuf>,
    root: &'w Path,
}

impl<'w, 'idx> TypeRelations<'w, 'idx> {
    fn new(snapshot: &'idx mut EngineSnapshot<'w>, workspace: &'w Workspace) -> Self {
        Self {
            new_documents: snapshot.new_documents(),
            root: snapshot.root(),
            resolver: snapshot.resolver(workspace),
        }
    }

    fn resolve(&mut self, source: &Source, name: Node<'_>) -> Result<Option<Location>> {
        let Some(package) = active_package(self.resolver.workspace, source, self.new_documents)
        else {
            return Ok(None);
        };
        let Some(reference) = source.reference_from_leaf(name, name.start_byte()) else {
            return Ok(None);
        };
        self.resolver
            .declaration_location(package, source, reference, self.root)
    }

    fn shape(&mut self, location: &Location) -> Result<Option<TypeShape>> {
        let mut body = location.clone();
        let mut identity = None;
        for _ in 0..MAX_TYPE_DEPTH {
            let Some(origin) = self.resolver.indexes.reference_source(&body.0)? else {
                return Ok(None);
            };
            let source = origin.as_ref();
            let Some(spec) =
                declaration_at(source, body.1.start).filter(|node| is_type_declaration(*node))
            else {
                return Ok(None);
            };
            let Some(ty) = type_syntax(spec) else {
                return Ok(None);
            };
            if spec.kind() != "type_alias" && identity.is_none() {
                identity = Some(body.clone());
            }
            if let Some(name) = type_name(ty)
                && let Some(next) = self.resolve(source, name)?
                && !same_type(&next, &body)
            {
                body = next;
                continue;
            }
            return Ok(Some(TypeShape {
                identity: identity.unwrap_or_else(|| body.clone()),
                body,
                kind: type_kind(ty),
                empty_interface_body: ty.kind() == "interface_type"
                    && ty
                        .named_children(&mut ty.walk())
                        .all(|node| node.is_extra()),
            }));
        }
        Ok(None)
    }

    fn parents(&mut self, body: &Location) -> Result<Vec<Location>> {
        let Some(origin) = self.resolver.indexes.reference_source(&body.0)? else {
            return Ok(Vec::new());
        };
        let source = origin.as_ref();
        let Some(ty) = declaration_at(source, body.1.start).and_then(type_syntax) else {
            return Ok(Vec::new());
        };
        let mut parents = Vec::new();
        for name in embedded_names(ty) {
            if let Some(parent) = self.resolve(source, name)? {
                parents.push(parent);
            }
        }
        Ok(parents)
    }
}

fn same_type(left: &Location, right: &Location) -> bool {
    left.1 == right.1 && path_key(&left.0) == path_key(&right.0)
}

type Items = BTreeMap<(Uri, Position), TypeHierarchyItem>;

fn insert_item(items: &mut Items, item: TypeHierarchyItem) {
    items.insert((item.uri.clone(), item.selection_range.start), item);
}

struct EmbeddingSearch {
    target: TypeShape,
    files: Option<Vec<PathBuf>>,
    candidates: Vec<Location>,
    elapsed: Option<Duration>,
}

impl EmbeddingSearch {
    fn new(target: TypeShape) -> Self {
        Self {
            target,
            files: None,
            candidates: Vec::new(),
            elapsed: stats::clock().map(|_| Duration::ZERO),
        }
    }

    fn files(
        &self,
        relations: &mut TypeRelations<'_, '_>,
        sources: &InterfaceSources,
    ) -> Result<Vec<PathBuf>> {
        let workspace = relations.resolver.workspace;
        let mut files = workspace.project_symbol_files();
        for ((directory, _), paths) in &relations.resolver.indexes.drafts {
            if workspace.contains_project_path(directory) {
                for path in paths {
                    files.insert(path_key(path), (*path).clone());
                }
            }
        }
        let (path, range) = &self.target.identity;
        if let Some(origin) = relations.resolver.indexes.reference_source(path)?
            && let Some(spec) = declaration_at(origin.as_ref(), range.start)
            && let Some(name) = spec.child_by_field_name("name")
            && let Some(indexed) = sources.embedding_files(
                path,
                node_text(name, &origin.as_ref().text),
                self.target.kind == SymbolKind::INTERFACE,
            )
        {
            files.retain(|key, _| indexed.contains(key));
        }
        Ok(files.into_values().collect())
    }

    fn advance(
        &mut self,
        relations: &mut TypeRelations<'_, '_>,
        sources: &InterfaceSources,
        items: &mut Items,
    ) -> Result<bool> {
        let started = self.elapsed.map(|_| Instant::now());
        if self.files.is_none() {
            self.files = Some(self.files(relations, sources)?);
        }
        let files = self.files.as_mut().unwrap();
        if let Some(candidate) = self.candidates.pop() {
            for parent in relations.parents(&candidate)? {
                if same_type(&parent, &self.target.identity)
                    || relations
                        .shape(&parent)?
                        .is_some_and(|shape| same_type(&shape.identity, &self.target.identity))
                {
                    if !same_type(&candidate, &self.target.identity)
                        && let Some(item) = item_at(&mut relations.resolver, &candidate)?
                    {
                        insert_item(items, item);
                    }
                    break;
                }
            }
        } else if let Some(file) = files.pop()
            && let Some(origin) = relations.resolver.indexes.reference_source(&file)?
        {
            let source = origin.as_ref();
            let interface = self.target.kind == SymbolKind::INTERFACE;
            let syntax = if interface {
                "interface_type"
            } else {
                "struct_type"
            };
            walk(source.tree.root_node(), |node| {
                if is_type_declaration(node)
                    && (!interface || is_local_type(node))
                    && type_syntax(node)
                        .is_some_and(|ty| ty.kind() == syntax && !embedded_names(ty).is_empty())
                    && let Some(name) = node.child_by_field_name("name")
                {
                    self.candidates.push((file.clone(), source.range(name)));
                }
                Ok(())
            })?;
        }
        let done = self.candidates.is_empty() && files.is_empty();
        if let (Some(elapsed), Some(started)) = (&mut self.elapsed, started) {
            *elapsed += started.elapsed();
            if done {
                eprintln!(
                    "rs-gopls-wp2 {{\"kind\":\"embeddings\",\"scan_us\":{}}}",
                    elapsed.as_micros()
                );
            }
        }
        Ok(done)
    }
}

pub(crate) struct TypeHierarchyTask {
    generation: u64,
    implementations: SearchResults<ImplementationSearch>,
    embeddings: Option<EmbeddingSearch>,
    items: Items,
}

pub(crate) fn supertypes(
    engine: &mut Engine,
    item: &TypeHierarchyItem,
) -> Result<QueryProgress<Option<Vec<TypeHierarchyItem>>, TypeHierarchyTask>> {
    related_types(engine, item, TypeHierarchyDirection::Super)
}

pub(crate) fn subtypes(
    engine: &mut Engine,
    item: &TypeHierarchyItem,
) -> Result<QueryProgress<Option<Vec<TypeHierarchyItem>>, TypeHierarchyTask>> {
    related_types(engine, item, TypeHierarchyDirection::Sub)
}

fn related_types(
    engine: &mut Engine,
    item: &TypeHierarchyItem,
    direction: TypeHierarchyDirection,
) -> Result<QueryProgress<Option<Vec<TypeHierarchyItem>>, TypeHierarchyTask>> {
    let Some((path, start)) =
        Hierarchy::Type.anchor(&item.uri, item.selection_range, item.data.as_ref())?
    else {
        return Ok(QueryProgress::Ready(None));
    };
    let require_tests = direction == TypeHierarchyDirection::Sub || is_test_source(&path);
    engine.include_tests |= require_tests;
    let mut snapshot = engine.snapshot();
    let workspace = match snapshot.active_graph(require_tests) {
        QueryProgress::Ready(workspace) => workspace,
        QueryProgress::Pending(load) => return Ok(QueryProgress::Pending(load)),
        QueryProgress::Searching(task) => match task {},
    };
    let mut relations = TypeRelations::new(&mut snapshot, workspace);
    let Some(origin) = relations.resolver.indexes.reference_source(&path)? else {
        return Ok(QueryProgress::Ready(None));
    };
    let source = origin.as_ref();
    let Some(spec) = declaration_at(source, start).filter(|node| is_type_declaration(*node)) else {
        return Ok(QueryProgress::Ready(None));
    };
    let Some(name) = spec.child_by_field_name("name") else {
        return Ok(QueryProgress::Ready(None));
    };
    let selection = source.range(name);
    if selection.start != start || node_text(name, &source.text) != item.name {
        return Ok(QueryProgress::Ready(None));
    }
    let Some(shape) = relations.shape(&(path.clone(), selection))? else {
        return Ok(QueryProgress::Ready(None));
    };
    let interface = shape.kind == SymbolKind::INTERFACE;
    let local_interface = interface && is_local_type(spec);
    let mut items = Items::new();
    let (embeddings, progress) = match direction {
        _ if shape.empty_interface_body => return Ok(QueryProgress::Ready(Some(Vec::new()))),
        _ if interface && !local_interface => {
            let Some(progress) = engine.interface_hierarchy(&path, start, direction)? else {
                return Ok(QueryProgress::Ready(Some(Vec::new())));
            };
            // The package index covers structural candidates. Function-local
            // declarations retain their direct embedding edges.
            let embeddings =
                (direction == TypeHierarchyDirection::Sub).then(|| EmbeddingSearch::new(shape));
            (embeddings, progress)
        }
        TypeHierarchyDirection::Super => {
            for parent in relations.parents(&shape.body)? {
                if (!local_interface
                    || relations
                        .shape(&parent)?
                        .is_some_and(|parent| parent.kind == SymbolKind::INTERFACE))
                    && !same_type(&parent, &shape.identity)
                    && let Some(item) = item_at(&mut relations.resolver, &parent)?
                {
                    insert_item(&mut items, item);
                }
            }
            let progress = if local_interface {
                QueryProgress::Ready(Vec::new())
            } else {
                engine.implementations(&path, start)?
            };
            (None, progress)
        }
        TypeHierarchyDirection::Sub => (
            Some(EmbeddingSearch::new(shape)),
            QueryProgress::Ready(Vec::new()),
        ),
    };
    let implementations = match SearchResults::new(progress) {
        Ok(results) => results,
        Err(load) => return Ok(QueryProgress::Pending(load)),
    };
    Ok(if implementations.is_empty() && embeddings.is_none() {
        QueryProgress::Ready(Some(items.into_values().collect()))
    } else {
        QueryProgress::Searching(TypeHierarchyTask {
            generation: engine.workspace_load().generation,
            implementations,
            embeddings,
            items,
        })
    })
}

impl SemanticTask<Option<Vec<TypeHierarchyItem>>> for TypeHierarchyTask {
    fn ready(&self, engine: &Engine) -> bool {
        self.implementations
            .search
            .as_ref()
            .is_none_or(|search| search.ready(engine))
    }

    fn advance(&mut self, engine: &mut Engine) -> Result<bool> {
        ensure!(
            engine.workspace_load().generation == self.generation,
            "workspace changed during type hierarchy search"
        );
        if self.implementations.search.is_some() {
            self.implementations.advance(engine)?;
            return Ok(self.implementations.is_empty() && self.embeddings.is_none());
        }
        let mut snapshot = engine.state.snapshot(engine.include_tests);
        let workspace = snapshot
            .workspace()
            .context("type hierarchy needs a workspace")?;
        let mut relations = TypeRelations::new(&mut snapshot, workspace);
        let started = Instant::now();
        for _ in 0..32 {
            if let Some(location) = self.implementations.locations.pop() {
                if let Some(item) = item_at(&mut relations.resolver, &location)? {
                    insert_item(&mut self.items, item);
                }
            } else if let Some(search) = &mut self.embeddings {
                if search.advance(
                    &mut relations,
                    &engine.implementation_sources,
                    &mut self.items,
                )? {
                    self.embeddings = None;
                }
            } else {
                break;
            }
            if started.elapsed() >= BATCH_TIME {
                break;
            }
        }
        Ok(self.implementations.is_empty() && self.embeddings.is_none())
    }

    fn finish(self) -> Option<Vec<TypeHierarchyItem>> {
        Some(self.items.into_values().collect())
    }
}
