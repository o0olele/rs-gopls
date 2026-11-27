use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyItem, CallHierarchyOutgoingCall, Position, Range,
    SymbolKind, Uri,
};
use tree_sitter::Node;

use super::{BATCH_TIME, Hierarchy, SearchResults, declaration_at, document, in_package, walk};
use crate::protocol::path_uri;
use crate::semantic::references::ReferenceSearch;
use crate::semantic::resolver::Resolver;
use crate::semantic::source::{
    ReferenceQuery, Source, is_declaration_name, is_test_source, is_type_declaration,
    node_at_range, node_text,
};
use crate::semantic::{Engine, QueryProgress, SemanticTask};
use crate::workspace::Package;

fn callable_name(source: &Source, declaration: Node<'_>) -> Option<String> {
    let name = node_text(declaration.child_by_field_name("name")?, &source.text);
    if name.is_empty() || name == "_" {
        return None;
    }
    match declaration.kind() {
        "function_declaration" => Some(name.to_owned()),
        "method_declaration" => {
            let receiver = declaration.child_by_field_name("receiver")?;
            let parameter = receiver
                .named_children(&mut receiver.walk())
                .find(|node| node.kind() == "parameter_declaration")?;
            let ty = parameter.child_by_field_name("type")?;
            Some(format!("({}).{name}", node_text(ty, &source.text)))
        }
        "method_elem" => {
            let owner = declaration
                .parent()?
                .parent()
                .filter(|node| is_type_declaration(*node))
                .and_then(|node| node.child_by_field_name("name"));
            Some(match owner {
                Some(owner) => format!("{}.{name}", node_text(owner, &source.text)),
                None => name.to_owned(),
            })
        }
        _ => None,
    }
}

fn call_item(source: &Source, declaration: Node<'_>) -> Result<Option<CallHierarchyItem>> {
    let Some(name) = callable_name(source, declaration) else {
        return Ok(None);
    };
    let selection_range = source.range(
        declaration
            .child_by_field_name("name")
            .expect("named callable"),
    );
    let uri = path_uri(&source.path)?;
    Ok(Some(CallHierarchyItem {
        name,
        kind: if declaration.kind() == "function_declaration" {
            SymbolKind::FUNCTION
        } else {
            SymbolKind::METHOD
        },
        tags: None,
        detail: source.outline().package.clone(),
        data: Some(Hierarchy::Call.data(&uri, selection_range)?),
        uri,
        range: source.range(declaration),
        selection_range,
    }))
}

fn resolve_call<'w>(
    resolver: &mut Resolver<'w, '_>,
    package: &'w Package,
    source: &Source,
    reference: ReferenceQuery<'_>,
) -> Result<Option<CallHierarchyItem>> {
    let location = match reference {
        ReferenceQuery::Name(name) => resolver.source_name_location(package, source, name)?,
        ReferenceQuery::Member { receiver, name } => resolver
            .selector(package, source, receiver, node_text(name, &source.text), 0)?
            .map(|symbol| symbol.location()),
        ReferenceQuery::LiteralKey { .. } => None,
    };
    let Some((file, range)) = location else {
        return Ok(None);
    };
    let Some(origin) = resolver.indexes.reference_source(&file)? else {
        return Ok(None);
    };
    let origin = origin.as_ref();
    let Some(declaration) = declaration_at(origin, range.start) else {
        return Ok(None);
    };
    call_item(origin, declaration)
}

pub(crate) fn prepare_call_hierarchy(
    engine: &mut Engine,
    path: &Path,
    position: Position,
) -> Result<QueryProgress<Option<Vec<CallHierarchyItem>>>> {
    engine.include_tests |= is_test_source(path);
    let mut snapshot = engine.snapshot();
    let Some(document) = document(&mut snapshot, path)? else {
        return Ok(QueryProgress::Ready(None));
    };
    let source = document.indexed();
    let Some(reference) = source.reference_at(position) else {
        return Ok(QueryProgress::Ready(None));
    };
    if let Some(declaration) = declaration_at(source, position) {
        return Ok(QueryProgress::Ready(
            call_item(source, declaration)?.map(|item| vec![item]),
        ));
    }
    in_package(&mut snapshot, &document, |resolver, package| {
        Ok(resolve_call(resolver, package, source, reference)?.map(|item| vec![item]))
    })
}

fn item_declaration<'a>(
    source: &'a Source,
    start: Position,
    item: &CallHierarchyItem,
) -> Option<Node<'a>> {
    let declaration = declaration_at(source, start)?;
    let name = declaration.child_by_field_name("name")?;
    (source.range(name).start == start
        && callable_name(source, declaration).as_ref() == Some(&item.name))
    .then_some(declaration)
}

#[derive(Default)]
struct CallSites(BTreeMap<(Uri, Position), (CallHierarchyItem, Vec<Range>)>);

impl CallSites {
    fn insert(&mut self, item: CallHierarchyItem, range: Range) {
        self.0
            .entry((item.uri.clone(), item.selection_range.start))
            .or_insert_with(|| (item, Vec::new()))
            .1
            .push(range);
    }

    fn groups(self) -> impl Iterator<Item = (CallHierarchyItem, Vec<Range>)> {
        self.0.into_values().map(|(item, mut ranges)| {
            ranges.sort_unstable_by_key(|range| (range.start, range.end));
            ranges.dedup();
            (item, ranges)
        })
    }
}

pub(crate) struct IncomingCallsTask {
    generation: u64,
    references: SearchResults<ReferenceSearch>,
    callers: CallSites,
}

pub(crate) fn incoming_calls(
    engine: &mut Engine,
    item: &CallHierarchyItem,
) -> Result<QueryProgress<Option<Vec<CallHierarchyIncomingCall>>, IncomingCallsTask>> {
    let Some((path, start)) =
        Hierarchy::Call.anchor(&item.uri, item.selection_range, item.data.as_ref())?
    else {
        return Ok(QueryProgress::Ready(None));
    };
    let mut snapshot = engine.snapshot();
    let Some(document) = document(&mut snapshot, &path)? else {
        return Ok(QueryProgress::Ready(None));
    };
    if item_declaration(document.indexed(), start, item).is_none() {
        return Ok(QueryProgress::Ready(None));
    }
    let references = match SearchResults::new(engine.references(&path, start, false)?) {
        Ok(references) => references,
        Err(load) => return Ok(QueryProgress::Pending(load)),
    };
    Ok(if references.is_empty() {
        QueryProgress::Ready(Some(Vec::new()))
    } else {
        QueryProgress::Searching(IncomingCallsTask {
            generation: engine.workspace_load().generation,
            references,
            callers: CallSites::default(),
        })
    })
}

impl SemanticTask<Option<Vec<CallHierarchyIncomingCall>>> for IncomingCallsTask {
    fn ready(&self, engine: &Engine) -> bool {
        self.references
            .search
            .as_ref()
            .is_none_or(|search| search.ready(engine))
    }

    fn advance(&mut self, engine: &mut Engine) -> Result<bool> {
        ensure!(
            engine.workspace_load().generation == self.generation,
            "workspace changed during incoming calls"
        );
        if self.references.search.is_some() {
            self.references.advance(engine)?;
            return Ok(self.references.is_empty());
        }
        let mut snapshot = engine.snapshot();
        let workspace = snapshot
            .workspace()
            .context("incoming calls need a workspace")?;
        let mut resolver = snapshot.resolver(workspace);
        let started = Instant::now();
        for _ in 0..64 {
            let Some((file, range)) = self.references.locations.pop() else {
                break;
            };
            if let Some(source) = resolver.indexes.reference_source(&file)? {
                let source = source.as_ref();
                if let Some(name) = node_at_range(source, range)
                    && !is_declaration_name(name)
                    && let Some(caller) =
                        std::iter::successors(name.parent(), Node::parent).find(|node| {
                            matches!(node.kind(), "function_declaration" | "method_declaration")
                        })
                    && caller
                        .child_by_field_name("body")
                        .is_some_and(|body| body.byte_range().contains(&name.start_byte()))
                    && let Some(item) = call_item(source, caller)?
                {
                    self.callers.insert(item, range);
                }
            }
            if started.elapsed() >= BATCH_TIME {
                break;
            }
        }
        Ok(self.references.is_empty())
    }

    fn finish(self) -> Option<Vec<CallHierarchyIncomingCall>> {
        Some(
            self.callers
                .groups()
                .map(|(from, from_ranges)| CallHierarchyIncomingCall { from, from_ranges })
                .collect(),
        )
    }
}

fn callee_name(call: Node<'_>) -> Option<Node<'_>> {
    // A single argument can make a generic call parse as a type conversion.
    let mut node = match call.kind() {
        "call_expression" => call.child_by_field_name("function")?,
        "type_conversion_expression" => call.child_by_field_name("type")?,
        _ => return None,
    };
    loop {
        node = match node.kind() {
            "identifier" | "type_identifier" => return Some(node),
            "selector_expression" => return node.child_by_field_name("field"),
            "qualified_type" => return node.child_by_field_name("name"),
            "generic_type" | "type_instantiation_expression" => node.child_by_field_name("type")?,
            "index_expression" => node.child_by_field_name("operand")?,
            "parenthesized_expression" | "parenthesized_type" => node
                .named_children(&mut node.walk())
                .find(|child| !child.is_extra())?,
            _ => return None,
        };
    }
}

pub(crate) fn outgoing_calls(
    engine: &mut Engine,
    item: &CallHierarchyItem,
) -> Result<QueryProgress<Option<Vec<CallHierarchyOutgoingCall>>>> {
    let Some((path, start)) =
        Hierarchy::Call.anchor(&item.uri, item.selection_range, item.data.as_ref())?
    else {
        return Ok(QueryProgress::Ready(None));
    };
    engine.include_tests |= is_test_source(&path);
    let mut snapshot = engine.snapshot();
    let Some(document) = document(&mut snapshot, &path)? else {
        return Ok(QueryProgress::Ready(None));
    };
    let source = document.indexed();
    let Some(declaration) = item_declaration(source, start, item) else {
        return Ok(QueryProgress::Ready(None));
    };
    let Some(body) = declaration.child_by_field_name("body") else {
        return Ok(QueryProgress::Ready(Some(Vec::new())));
    };
    in_package(&mut snapshot, &document, |resolver, package| {
        let mut callees = CallSites::default();
        walk(body, |node| {
            if let Some(name) = callee_name(node)
                && let Some(reference) = source.reference_from_leaf(name, name.start_byte())
                && let Some(item) = resolve_call(resolver, package, source, reference)?
            {
                callees.insert(item, source.range(name));
            }
            Ok(())
        })?;
        Ok(Some(
            callees
                .groups()
                .map(|(to, from_ranges)| CallHierarchyOutgoingCall { to, from_ranges })
                .collect(),
        ))
    })
}
