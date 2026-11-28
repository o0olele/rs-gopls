use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::Result;
use lsp_types::Range;
use tree_sitter::Node;

use crate::semantic::{
    index::ContentIdentity,
    resolver::{
        GenericCall, ResolvedSymbol, Resolver, Substitutions, TypeIdentity, TypeKind, TypeOrigin,
        explicit_type_arguments, is_exported, is_predeclared_type, materialize_type_cancellable,
        untyped_default,
    },
    source::{Source, check_optional_cancelled, node_text},
};
use crate::symbols::{Container, FunctionSignature, InlineType, SymbolKind, TypeRef, TypeRefKind};
use crate::workspace::{Package, path_key};

use super::cursor::{ancestors, is_function};

pub(super) struct Expected {
    shape: Rc<ExpectedShape>,
    weights: RefCell<HashMap<MemoType, i32>>,
}

#[derive(Eq, Hash, PartialEq)]
struct ExpectedShape {
    exact: String,
    underlying: String,
    nil: bool,
    assertion: bool,
    methods: Vec<(String, String)>,
}

#[derive(Eq, Hash, PartialEq)]
pub(in crate::semantic) struct InferredMemo {
    expected: Rc<ExpectedShape>,
    package: String,
    file: PathBuf,
    range: Range,
    content: ContentIdentity,
}

impl InferredMemo {
    pub(in crate::semantic) fn package_id(&self) -> &str {
        &self.package
    }
}

#[derive(Eq, Hash, PartialEq)]
struct MemoType {
    package: String,
    pointers: u8,
    addressable: bool,
    expression: bool,
    kind: MemoKind,
}

#[derive(Eq, Hash, PartialEq)]
enum MemoKind {
    Named(String, Vec<Option<MemoType>>),
    Container(Box<Container<MemoType>>, Option<(PathBuf, Range)>),
    Function(
        PathBuf,
        SameArc<FunctionSignature>,
        MemoSubstitutions,
        Option<Box<MemoType>>,
    ),
    Inline(PathBuf, SameArc<InlineType>, MemoSubstitutions),
    Parameter(usize),
}

type MemoSubstitutions = Vec<(String, Option<MemoType>)>;

// Keep the signature/shape alive for the request, so separately synthesized
// types with the same source range cannot share a memo entry.
struct SameArc<T>(Arc<T>);

impl<T> PartialEq for SameArc<T> {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl<T> Eq for SameArc<T> {}

impl<T> Hash for SameArc<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.0).hash(state);
    }
}

impl MemoType {
    fn new(ty: &TypeIdentity<'_>) -> Self {
        let substitutions = |values: &Substitutions<'_>| {
            values
                .iter()
                .map(|(name, ty)| (name.clone(), ty.as_ref().map(Self::new)))
                .collect()
        };
        let kind = match &ty.kind {
            TypeKind::Named { name, arguments } => MemoKind::Named(
                name.clone(),
                arguments
                    .iter()
                    .map(|ty| ty.as_ref().map(Self::new))
                    .collect(),
            ),
            TypeKind::Container(shape, origin) => {
                let shape = match shape.as_ref() {
                    Container::Slice(ty) => Container::Slice(Self::new(ty)),
                    Container::Array { element, length } => Container::Array {
                        element: Self::new(element),
                        length: length.clone(),
                    },
                    Container::Map { key, value } => Container::Map {
                        key: Self::new(key),
                        value: Self::new(value),
                    },
                    Container::Channel {
                        element,
                        receive,
                        send,
                    } => Container::Channel {
                        element: Self::new(element),
                        receive: *receive,
                        send: *send,
                    },
                };
                MemoKind::Container(Box::new(shape), origin.clone())
            }
            TypeKind::Function {
                file,
                signature,
                substitutions: values,
                method_receiver,
            } => MemoKind::Function(
                path_key(file),
                SameArc(Arc::clone(signature)),
                substitutions(values),
                method_receiver.as_deref().map(Self::new).map(Box::new),
            ),
            TypeKind::Inline {
                file,
                shape,
                substitutions: values,
            } => MemoKind::Inline(
                path_key(file),
                SameArc(Arc::clone(shape)),
                substitutions(values),
            ),
            TypeKind::Parameter(index) => MemoKind::Parameter(*index),
        };
        Self {
            package: ty.package.id.clone(),
            pointers: ty.pointers,
            addressable: ty.addressable,
            expression: ty.type_expression,
            kind,
        }
    }
}

impl Expected {
    pub(super) fn accepts_nil(&self) -> bool {
        self.shape.nil && !self.shape.assertion
    }

    pub(super) fn from_cursor<'w>(
        resolver: &mut Resolver<'w, '_>,
        package: &'w Package,
        source: &Source,
        anchor: Node<'_>,
    ) -> Result<Option<Self>> {
        let syntax = syntax_type(resolver, package, source, anchor)?;
        resolver.check_cancelled()?;
        let Some((ty, assertion)) = syntax else {
            return Ok(None);
        };
        let Some(exact) = type_key(resolver, &ty, 0)? else {
            return Ok(None);
        };
        let Some(base) = resolver.underlying_type(ty)? else {
            return Ok(None);
        };
        let underlying = type_key(resolver, &base, 0)?.unwrap_or_else(|| exact.clone());
        let interface = is_interface(resolver, &base)?;
        let nil = interface
            || base.pointers > 0
            || matches!(
                base.kind,
                TypeKind::Function { .. } | TypeKind::Container(..)
            ) && !matches!(base.kind, TypeKind::Container(ref shape, _) if matches!(shape.as_ref(), Container::Array { .. }))
            || underlying == "unsafe:Pointer";
        let methods = if interface {
            methods(resolver, &base)?
        } else {
            Vec::new()
        };
        Ok(Some(Self {
            shape: Rc::new(ExpectedShape {
                exact,
                underlying,
                nil,
                assertion,
                methods,
            }),
            weights: RefCell::new(HashMap::new()),
        }))
    }

    pub(super) fn inferred_weight<'w>(
        &self,
        resolver: &mut Resolver<'w, '_>,
        source: &Source,
        resolved: &ResolvedSymbol<'w>,
        name: &str,
    ) -> Result<i32> {
        resolver.check_cancelled()?;
        let SymbolKind::InferredValue {
            initializer,
            result_index,
        } = &resolved.symbol.kind
        else {
            unreachable!("scoring an inferred initializer");
        };
        let file = path_key(&resolved.file);
        let key = resolver
            .indexes
            .content_identity(&file)
            .map(|content| InferredMemo {
                expected: Rc::clone(&self.shape),
                package: resolved.package.id.clone(),
                file: file.clone(),
                range: resolved.symbol.range,
                content,
            });
        if let Some(weight) = key
            .as_ref()
            .and_then(|key| resolver.indexes.inferred_weights.get(key))
        {
            if let Some(trace) = resolver.indexes.trace() {
                trace.expected_weight_hit();
            }
            return Ok(*weight);
        }
        let default = if *result_index != 0 {
            None
        } else if file == path_key(&source.path) {
            source
                .tree
                .root_node()
                .named_descendant_for_byte_range(initializer.start, initializer.end)
                .filter(|node| node.byte_range() == *initializer)
                .and_then(untyped_default)
        } else {
            resolver
                .indexes
                .declaration_source(&resolved.file)
                .ok()
                .flatten()
                .and_then(|decls| decls.initializer(initializer)?.untyped_default)
        };
        resolver.check_cancelled()?;
        // The resolver recognizes shadowing of true and false before the
        // default type applies. Other literal types need no syntax lookup.
        let inferred = if default.is_none_or(|ty| ty == "bool") {
            resolver.symbol_type(source, resolved, name, 0)?
        } else {
            None
        };
        let weight = if let Some(ty) = inferred {
            self.weight(resolver, &ty)?
        } else if let Some(value) = default {
            let key = named_key("builtin", value);
            self.keys_weight(&key, Some(&key))
        } else {
            0
        };
        if let Some(key) = key {
            let weights = &mut resolver.indexes.inferred_weights;
            if weights.len() >= 512 {
                weights.clear();
            }
            weights.insert(key, weight);
        }
        Ok(weight)
    }

    pub(super) fn weight<'w>(
        &self,
        resolver: &mut Resolver<'w, '_>,
        ty: &TypeIdentity<'w>,
    ) -> Result<i32> {
        resolver.check_cancelled()?;
        // A function's parameter types cannot affect a score against a
        // non-callable expected type with no required methods. Its single
        // result, when present, is scored separately by the caller.
        if matches!(ty.kind, TypeKind::Function { .. })
            && self.shape.methods.is_empty()
            && [&self.shape.exact, &self.shape.underlying]
                .into_iter()
                .all(|key| !key.trim_start_matches('*').starts_with("func("))
        {
            return Ok(0);
        }
        let key = MemoType::new(ty);
        if let Some(weight) = self.weights.borrow().get(&key) {
            if let Some(trace) = resolver.indexes.trace() {
                trace.expected_weight_hit();
            }
            return Ok(*weight);
        }
        let weight = self.compute_weight(resolver, ty)?;
        self.weights.borrow_mut().insert(key, weight);
        Ok(weight)
    }

    fn compute_weight<'w>(
        &self,
        resolver: &mut Resolver<'w, '_>,
        ty: &TypeIdentity<'w>,
    ) -> Result<i32> {
        let Some(exact) = type_key(resolver, ty, 0)? else {
            return Ok(0);
        };
        if self.shape.exact == exact {
            return Ok(5);
        }
        if !self.shape.methods.is_empty() {
            let mut receiver = ty.clone();
            receiver.addressable = false;
            receiver.type_expression = false;
            let available = methods(resolver, &receiver)?;
            if self
                .shape
                .methods
                .iter()
                .all(|method| available.contains(method))
            {
                return Ok(5);
            }
        }
        let Some(base) = resolver.underlying_type(ty.clone())? else {
            return Ok(0);
        };
        let underlying = type_key(resolver, &base, 0)?;
        Ok(self.keys_weight(&exact, underlying.as_deref()))
    }

    pub(super) fn keys_weight(&self, exact: &str, underlying: Option<&str>) -> i32 {
        if self.shape.exact == exact {
            5
        } else if !self.shape.assertion
            && underlying.is_some_and(|underlying| {
                self.shape.underlying == underlying
                    || numeric_family(&self.shape.underlying)
                        .is_some_and(|family| numeric_family(underlying) == Some(family))
            })
        {
            4
        } else {
            0
        }
    }
}

fn numeric_family(name: &str) -> Option<bool> {
    match name {
        "int" | "int8" | "int16" | "int32" | "int64" | "uint" | "uint8" | "uint16" | "uint32"
        | "uint64" | "uintptr" | "float32" | "float64" => Some(false),
        "complex64" | "complex128" => Some(true),
        _ => None,
    }
}

pub(super) fn is_interface<'w>(
    resolver: &mut Resolver<'w, '_>,
    ty: &TypeIdentity<'w>,
) -> Result<bool> {
    if ty.pointers != 0 {
        return Ok(false);
    }
    if let Some((_, details, _)) = resolver.type_context(ty)? {
        return Ok(details.interface);
    }
    Ok(matches!(&ty.kind, TypeKind::Named { name, .. }
        if matches!(name.as_str(), "any" | "error" | "comparable")))
}

fn methods<'w>(
    resolver: &mut Resolver<'w, '_>,
    ty: &TypeIdentity<'w>,
) -> Result<Vec<(String, String)>> {
    if let TypeKind::Named { name, .. } = &ty.kind
        && name == "error"
        && ty.pointers == 0
        && resolver.type_info(ty.package, name)?.is_none()
    {
        return Ok(vec![("Error".into(), "func():(string)".into())]);
    }
    let mut methods = Vec::new();
    for member in resolver.visible_members(ty.package, ty)? {
        resolver.check_cancelled()?;
        let crate::symbols::SymbolKind::Function(signature) = &member.resolved.symbol.kind else {
            continue;
        };
        let reference = TypeRef {
            pointers: 0,
            kind: TypeRefKind::Function(signature.clone()),
        };
        if let Some(ty) = resolver.type_from_symbol(&member.resolved, reference, false)?
            && let Some(signature) = type_key(resolver, &ty, 0)?
        {
            let name = if is_exported(&member.name) {
                member.name
            } else {
                format!("{}:{}", member.resolved.package.import_path(), member.name)
            };
            methods.push((name, signature));
        }
    }
    Ok(methods)
}

pub(super) fn named_key(package: &str, name: &str) -> String {
    match name {
        "byte" => "uint8".into(),
        "rune" => "int32".into(),
        _ if is_predeclared_type(name) => name.into(),
        _ => format!("{package}:{name}"),
    }
}

fn type_key<'w>(
    resolver: &mut Resolver<'w, '_>,
    ty: &TypeIdentity<'w>,
    depth: usize,
) -> Result<Option<String>> {
    resolver.check_cancelled()?;
    if depth >= 16 {
        return Ok(None);
    }
    let Some(ty) = resolver.resolve_alias(ty.clone())? else {
        return Ok(None);
    };
    let base = match ty.kind {
        TypeKind::Named { name, arguments } => {
            let package = ty.package.import_path();
            let mut key =
                if is_predeclared_type(&name) && resolver.type_info(ty.package, &name)?.is_some() {
                    format!("{package}:{name}")
                } else {
                    named_key(package, &name)
                };
            if !arguments.is_empty() {
                let mut keys = Vec::with_capacity(arguments.len());
                for argument in arguments {
                    let Some(key) = argument
                        .as_ref()
                        .map(|ty| type_key(resolver, ty, depth + 1))
                        .transpose()?
                        .flatten()
                    else {
                        return Ok(None);
                    };
                    keys.push(key);
                }
                key.push('[');
                key.push_str(&keys.join(","));
                key.push(']');
            }
            key
        }
        TypeKind::Container(container, _) => {
            let mut child = |ty| type_key(resolver, ty, depth + 1);
            match container.as_ref() {
                Container::Slice(element) => child(element)?.map(|key| format!("[]{key}")),
                Container::Array { element, length } => child(element)?
                    .map(|key| format!("[{}]{key}", length.as_deref().unwrap_or("..."))),
                Container::Map { key, value } => child(key)?
                    .zip(child(value)?)
                    .map(|(key, value)| format!("map[{key}]{value}")),
                Container::Channel {
                    element,
                    receive,
                    send,
                } => child(element)?.map(|key| format!("chan{receive}{send}:{key}")),
            }
            .unwrap_or_default()
        }
        TypeKind::Function {
            file,
            signature,
            substitutions,
            method_receiver,
        } => {
            let cancelled = resolver.indexes.cancelled;
            let imports = &resolver.indexes.file_index(&file)?.imports;
            let parse = |reference: &Option<TypeRef>| {
                materialize_type_cancellable(
                    resolver.workspace,
                    ty.package,
                    &file,
                    imports,
                    reference.clone()?,
                    &substitutions,
                    cancelled,
                )
            };
            let parameters: Vec<_> = method_receiver
                .into_iter()
                .map(|receiver| Some(*receiver))
                .chain(
                    signature
                        .parameters
                        .iter()
                        .take_while(|_| !is_cancelled(cancelled))
                        .map(parse),
                )
                .collect();
            let results: Vec<_> = signature
                .results
                .iter()
                .take_while(|_| !is_cancelled(cancelled))
                .map(parse)
                .collect();
            check_optional_cancelled(cancelled)?;
            let mut labels = |types: Vec<Option<TypeIdentity<'w>>>| -> Result<Option<String>> {
                let mut labels = Vec::new();
                for ty in types {
                    let Some(ty) = ty else { return Ok(None) };
                    let Some(label) = type_key(resolver, &ty, depth + 1)? else {
                        return Ok(None);
                    };
                    labels.push(label);
                }
                Ok(Some(labels.join(",")))
            };
            let Some((parameters, results)) = labels(parameters)?.zip(labels(results)?) else {
                return Ok(None);
            };
            let variadic = if signature.variadic { "..." } else { "" };
            format!("func({parameters}{variadic}):({results})")
        }
        TypeKind::Inline { file, shape, .. } => format!(
            "{}:{}:{}",
            file.display(),
            shape.origin.start.line,
            shape.origin.start.character
        ),
        TypeKind::Parameter(_) => return Ok(None),
    };
    Ok((!base.is_empty()).then(|| format!("{}{base}", "*".repeat(ty.pointers.into()))))
}

pub(super) fn argument_or_result<'w>(
    resolver: &mut Resolver<'w, '_>,
    package: &'w Package,
    file: &std::path::Path,
    substitutions: &Substitutions<'w>,
    reference: Option<&TypeRef>,
) -> Result<Option<TypeIdentity<'w>>> {
    let Some(reference) = reference else {
        return Ok(None);
    };
    let cancelled = resolver.indexes.cancelled;
    let imports = &resolver.indexes.file_index(file)?.imports;
    let ty = materialize_type_cancellable(
        resolver.workspace,
        package,
        file,
        imports,
        reference.clone(),
        substitutions,
        cancelled,
    );
    check_optional_cancelled(cancelled)?;
    Ok(ty)
}

fn syntax_type<'w>(
    resolver: &mut Resolver<'w, '_>,
    package: &'w Package,
    source: &Source,
    anchor: Node<'_>,
) -> Result<Option<(TypeIdentity<'w>, bool)>> {
    let cancelled = resolver.indexes.cancelled;
    for node in ancestors(anchor) {
        check_optional_cancelled(cancelled)?;
        let Some(parent) = node.parent() else { break };
        let ty = match parent.kind() {
            "var_spec" | "const_spec" if parent.child_by_field_name("value") == Some(node) => {
                parent
                    .child_by_field_name("type")
                    .and_then(|ty| resolver.source_type(package, source, ty))
            }
            "assignment_statement" | "short_var_declaration"
                if parent.child_by_field_name("right") == Some(node) =>
            {
                let index = index_of(node, anchor, cancelled);
                match parent
                    .child_by_field_name("left")
                    .and_then(|left| children(left, cancelled).nth(index))
                {
                    Some(lhs)
                        if parent.kind() == "assignment_statement"
                            || crate::scope::lexical_binding_cancellable(
                                lhs,
                                node_text(lhs, &source.text),
                                &source.text,
                                cancelled,
                            )
                            .is_some() =>
                    {
                        resolver.infer_type(package, source, lhs, 0, 0)?
                    }
                    _ => None,
                }
            }
            "return_statement" => ancestors(parent)
                .find(|node| is_function(*node))
                .and_then(|function| function.child_by_field_name("result"))
                .and_then(|result| {
                    return_type(result, index_of(node, anchor, cancelled), cancelled)
                })
                .and_then(|ty| resolver.source_type(package, source, ty)),
            "argument_list" => call_arg_type(resolver, package, source, parent, node)?,
            "literal_value" => element_type(resolver, package, source, parent, node, anchor)?,
            "type_assertion_expression" if parent.child_by_field_name("type") == Some(node) => {
                let assertion = parent.child_by_field_name("operand");
                return Ok(inferred(resolver, package, source, assertion)?.map(|ty| (ty, true)));
            }
            "type_case"
                if parent
                    .children_by_field_name("type", &mut parent.walk())
                    .any(|child| child == node) =>
            {
                let value = parent
                    .parent()
                    .and_then(|switch| switch.child_by_field_name("value"));
                return Ok(inferred(resolver, package, source, value)?.map(|ty| (ty, true)));
            }
            "expression_case" if parent.child_by_field_name("value") == Some(node) => {
                let value = parent
                    .parent()
                    .and_then(|switch| switch.child_by_field_name("value"));
                inferred(resolver, package, source, value)?.or_else(|| {
                    value
                        .is_none()
                        .then(|| TypeIdentity::named(package, "bool"))
                })
            }
            "block" | "statement_list" | "source_file" => break,
            _ => continue,
        };
        return Ok(ty.map(|ty| (ty, false)));
    }
    Ok(None)
}

fn inferred<'w>(
    resolver: &mut Resolver<'w, '_>,
    package: &'w Package,
    source: &Source,
    node: Option<Node<'_>>,
) -> Result<Option<TypeIdentity<'w>>> {
    let Some(node) = node else { return Ok(None) };
    Ok(resolver
        .infer_type(package, source, node, 0, 0)?
        .or_else(|| untyped_default(node).map(|name| TypeIdentity::named(package, name))))
}

fn is_cancelled(cancelled: Option<&AtomicBool>) -> bool {
    cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed))
}

fn children<'a, 'tree: 'a>(
    node: Node<'tree>,
    cancelled: Option<&'a AtomicBool>,
) -> impl Iterator<Item = Node<'tree>> + 'a {
    (0..node.named_child_count())
        .take_while(move |_| !is_cancelled(cancelled))
        .filter_map(move |index| node.named_child(index))
        .filter(|node| !node.is_extra())
}

fn index_of(list: Node<'_>, anchor: Node<'_>, cancelled: Option<&AtomicBool>) -> usize {
    children(list, cancelled)
        .take_while(|child| child.end_byte() <= anchor.start_byte())
        .count()
}

fn return_type<'tree>(
    result: Node<'tree>,
    mut index: usize,
    cancelled: Option<&AtomicBool>,
) -> Option<Node<'tree>> {
    if result.kind() != "parameter_list" {
        return (index == 0).then_some(result);
    }
    for parameter in children(result, cancelled) {
        let count = parameter
            .children_by_field_name("name", &mut parameter.walk())
            .take_while(|_| !is_cancelled(cancelled))
            .count()
            .max(1);
        if index < count {
            return parameter.child_by_field_name("type");
        }
        index -= count;
    }
    None
}

fn call_arg_type<'w>(
    resolver: &mut Resolver<'w, '_>,
    package: &'w Package,
    source: &Source,
    list: Node<'_>,
    active: Node<'_>,
) -> Result<Option<TypeIdentity<'w>>> {
    let Some(call) = list.parent() else {
        return Ok(None);
    };
    let Some(callee) = call.child_by_field_name("function") else {
        return Ok(None);
    };
    let Some(mut ty) = resolver.infer_type(package, source, callee, 0, 0)? else {
        return Ok(None);
    };
    if ty.type_expression {
        return Ok(Some(ty));
    }
    if let Some(arguments) = call.child_by_field_name("type_arguments") {
        let Some(specialized) = resolver.specialize_function(
            package,
            source,
            ty,
            explicit_type_arguments(arguments),
        )?
        else {
            return Ok(None);
        };
        ty = specialized;
    }
    let Some(ty) = resolver.underlying_type(ty)? else {
        return Ok(None);
    };
    let TypeKind::Function {
        file,
        signature,
        mut substitutions,
        method_receiver,
    } = ty.kind
    else {
        return Ok(None);
    };
    if ty.pointers != 0 {
        return Ok(None);
    }
    let cancelled = resolver.indexes.cancelled;
    let mut index = index_of(list, active, cancelled);
    check_optional_cancelled(cancelled)?;
    if let Some(receiver) = method_receiver {
        if index == 0 {
            return Ok(Some(*receiver));
        }
        index -= 1;
    } else if !signature.type_parameters.is_empty() {
        let arguments: Vec<_> = children(list, cancelled).take(index).collect();
        check_optional_cancelled(cancelled)?;
        let query = GenericCall {
            caller: package,
            source,
            arguments: &arguments,
            depth: 0,
            allow_partial: true,
        };
        if let Some(inferred) = resolver.infer_type_arguments(
            query,
            TypeOrigin {
                package: ty.package,
                file: &file,
            },
            &signature,
            &substitutions,
        )? {
            substitutions = inferred;
        }
    }
    let param = signature
        .parameters
        .get(index)
        .or_else(|| {
            signature
                .variadic
                .then(|| signature.parameters.last())
                .flatten()
        })
        .and_then(Option::as_ref);
    argument_or_result(resolver, ty.package, &file, &substitutions, param)
}

pub(super) fn literal_type<'w>(
    resolver: &mut Resolver<'w, '_>,
    package: &'w Package,
    source: &Source,
    body: Node<'_>,
) -> Result<Option<TypeIdentity<'w>>> {
    resolver.check_cancelled()?;
    let Some(parent) = body.parent() else {
        return Ok(None);
    };
    if parent.kind() == "composite_literal" {
        return Ok(parent
            .child_by_field_name("type")
            .and_then(|ty| resolver.source_type(package, source, ty)));
    }
    let Some(outer) = ancestors(parent).find(|node| node.kind() == "literal_value") else {
        return Ok(None);
    };
    let Some(element) = children(outer, resolver.indexes.cancelled)
        .find(|node| node.start_byte() <= body.start_byte() && body.end_byte() <= node.end_byte())
    else {
        return Ok(None);
    };
    element_type(resolver, package, source, outer, element, body)
}

fn element_type<'w>(
    resolver: &mut Resolver<'w, '_>,
    package: &'w Package,
    source: &Source,
    body: Node<'_>,
    element: Node<'_>,
    anchor: Node<'_>,
) -> Result<Option<TypeIdentity<'w>>> {
    let cancelled = resolver.indexes.cancelled;
    check_optional_cancelled(cancelled)?;
    let key = element.child_by_field_name("key");
    let in_key = key.is_some_and(|key| anchor.end_byte() <= key.end_byte());
    let Some(ty) = literal_type(resolver, package, source, body)? else {
        return Ok(None);
    };
    if let Some((container, _)) = resolver.container(ty.clone())? {
        return Ok(match container {
            Container::Map { key, value } => Some(if in_key { key } else { value }),
            Container::Slice(ty) | Container::Array { element: ty, .. } => Some(if in_key {
                TypeIdentity::named(package, "int")
            } else {
                ty
            }),
            Container::Channel { .. } => None,
        });
    }
    if in_key {
        return Ok(None);
    }
    let fields = resolver.struct_fields(package, &ty)?;
    let mut direct_fields = fields
        .into_iter()
        .filter(|field| field.member.embedded_path.is_empty());
    let field = if let Some(key) = key {
        let name = node_text(key, &source.text);
        direct_fields.find(|field| field.member.name == name)
    } else {
        // The index's field map is sorted by name. Unkeyed literals follow
        // declaration order, including fields that are invisible to the caller.
        let mut fields: Vec<_> = direct_fields.collect();
        fields.sort_by_key(|field| {
            let range = field.member.resolved.symbol.range;
            (range.start.line, range.start.character)
        });
        fields.into_iter().nth(index_of(body, element, cancelled))
    };
    check_optional_cancelled(cancelled)?;
    Ok(field.and_then(|field| field.ty))
}
