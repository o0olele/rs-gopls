use std::fmt::Write;

use crate::scope::{Binding, initializer_at};

use super::super::presentation::{function_signature, function_type, node_at_range, syntax_text};
use super::super::{
    Container, MAX_TYPE_DEPTH, Node, Package, ReferenceQuery, Resolver, Result, Source, TypeKind,
    is_predeclared, is_predeclared_type, node_text, untyped_default,
};
use super::{Declaration, declaration_owner};

pub(super) fn value_signature<'w>(
    source: &Source,
    owner: Node<'_>,
    binding: Binding<'_>,
    context: Option<(&mut Resolver<'w, '_>, &'w Package)>,
    allow_partial: bool,
) -> Result<Option<String>> {
    let (explicit, initializer, index) = value_parts(owner, binding);
    let ty = if let Some(ty) = explicit {
        let ty = syntax_text(ty, &source.text);
        Some(if owner.kind() == "variadic_parameter_declaration" {
            format!("[]{ty}")
        } else {
            ty
        })
    } else if let Some(initializer) = initializer {
        match context {
            Some((resolver, from)) => {
                resolver.value_type_text(from, source, initializer, index, 0)?
            }
            None => syntax_value_type(source, initializer, index),
        }
    } else {
        None
    };
    if ty.is_none() && !allow_partial {
        return Ok(None);
    }
    let kind = if owner.kind() == "const_spec" {
        "const"
    } else {
        "var"
    };
    let mut signature = format!("{kind} {}", node_text(binding.name, &source.text));
    if let Some(ty) = ty {
        let _ = write!(signature, " {ty}");
    }
    if !matches!(
        owner.kind(),
        "range_clause" | "type_switch_statement" | "receive_statement"
    ) && let Some(expression) =
        initializer.filter(|node| !node.has_error() && node.end_byte() - node.start_byte() <= 120)
    {
        let expression = syntax_text(expression, &source.text);
        if !expression.contains(['\r', '\n']) {
            let _ = write!(signature, " = {expression}");
        }
    }
    Ok(Some(signature))
}

pub(super) fn value_parts<'tree>(
    owner: Node<'tree>,
    binding: Binding<'tree>,
) -> (Option<Node<'tree>>, Option<Node<'tree>>, usize) {
    if owner.kind() == "const_spec"
        && binding.initializer.is_none()
        && let Some(inherited) =
            std::iter::successors(owner.prev_named_sibling(), |node| node.prev_named_sibling())
                .take_while(|node| matches!(node.kind(), "const_spec" | "comment"))
                .find(|node| {
                    node.kind() == "const_spec" && node.child_by_field_name("value").is_some()
                })
        && let Some(index) = owner
            .children_by_field_name("name", &mut owner.walk())
            .filter(|node| node.is_named() && !node.is_extra())
            .position(|name| name == binding.name)
    {
        let (initializer, index) = initializer_at(inherited.child_by_field_name("value"), index);
        return (inherited.child_by_field_name("type"), initializer, index);
    }
    (
        binding.explicit_type,
        binding.initializer,
        binding.result_index,
    )
}

pub(super) fn literal_value_type(expression: Node<'_>, index: usize) -> Option<&'static str> {
    (index == 0)
        .then(|| untyped_default(expression))
        .flatten()
        .filter(|ty| *ty != "bool")
}

fn syntax_value_type(source: &Source, mut expression: Node<'_>, index: usize) -> Option<String> {
    if index != 0 {
        return None;
    }
    while expression.kind() == "parenthesized_expression" {
        expression = expression
            .named_children(&mut expression.walk())
            .find(|node| !node.is_extra())?;
    }
    let render = |ty| syntax_text(ty, &source.text);
    match expression.kind() {
        "true" | "false" => None,
        "composite_literal" | "type_assertion_expression" => {
            expression.child_by_field_name("type").map(render)
        }
        "unary_expression"
            if expression
                .child_by_field_name("operator")
                .is_some_and(|op| op.kind() == "&") =>
        {
            let literal = expression.child_by_field_name("operand")?;
            if literal.kind() != "composite_literal" {
                return None;
            }
            Some(format!("*{}", render(literal.child_by_field_name("type")?)))
        }
        "func_literal" => Some(function_signature(expression, &source.text, None)),
        _ => untyped_default(expression).map(str::to_owned),
    }
}

impl<'w> Resolver<'w, '_> {
    fn value_type_text(
        &mut self,
        from: &'w Package,
        source: &Source,
        expression: Node<'_>,
        index: usize,
        depth: usize,
    ) -> Result<Option<String>> {
        if depth >= MAX_TYPE_DEPTH {
            return Ok(None);
        }
        if let Some(ty) = self.infer_type(from, source, expression, depth, index)?
            && !ty.type_expression
            && let Some(ty) = self.type_text(from, source, &ty)?
        {
            return Ok(Some(ty));
        }
        if let Some(ty) = syntax_value_type(source, expression, index) {
            return Ok(Some(ty));
        }
        if expression.kind() == "range_clause" {
            return self.range_type_text(from, source, expression, index, depth);
        }
        if index != 0 {
            return Ok(
                (index == 1 && self.has_ok_result(from, source, expression, depth)?)
                    .then(|| "bool".to_owned()),
            );
        }
        let infer = |resolver: &mut Self, operand| {
            resolver.value_type_text(from, source, operand, 0, depth + 1)
        };
        match expression.kind() {
            "parenthesized_expression" => {
                let inner = expression
                    .named_children(&mut expression.walk())
                    .find(|node| !node.is_extra());
                match inner {
                    Some(inner) => infer(self, inner),
                    None => Ok(None),
                }
            }
            "call_expression" => self.builtin_result_text(from, source, expression, depth),
            "identifier" | "true" | "false" | "iota" | "selector_expression" => {
                self.referenced_value_type(from, source, expression, depth)
            }
            "unary_expression" => {
                let (Some(operator), Some(operand)) = (
                    expression.child_by_field_name("operator"),
                    expression.child_by_field_name("operand"),
                ) else {
                    return Ok(None);
                };
                Ok(match operator.kind() {
                    "+" | "-" | "!" | "^" => infer(self, operand)?,
                    _ => None,
                })
            }
            "binary_expression" => {
                let (Some(left), Some(operator), Some(right)) = (
                    expression.child_by_field_name("left"),
                    expression.child_by_field_name("operator"),
                    expression.child_by_field_name("right"),
                ) else {
                    return Ok(None);
                };
                if matches!(operator.kind(), "==" | "!=" | "<" | "<=" | ">" | ">=") {
                    return Ok(Some("bool".into()));
                }
                let left_type = infer(self, left)?;
                if matches!(operator.kind(), "<<" | ">>") {
                    return Ok(left_type);
                }
                let right_type = infer(self, right)?;
                Ok(match (left_type, right_type) {
                    (Some(left_type), Some(right_type)) if left_type == right_type => {
                        Some(left_type)
                    }
                    (Some(left_type), Some(right_type)) => {
                        match (untyped_default(left), untyped_default(right)) {
                            (Some(a), Some(b)) => {
                                let types = ["int", "rune", "float64", "complex128"];
                                let rank = |ty| types.iter().position(|&name| name == ty);
                                match (rank(a), rank(b)) {
                                    (Some(a), Some(b)) => Some(types[a.max(b)].into()),
                                    _ => None,
                                }
                            }
                            (Some(_), None) => Some(right_type),
                            (None, Some(_)) => Some(left_type),
                            _ => None,
                        }
                    }
                    _ => None,
                })
            }
            _ => Ok(None),
        }
    }

    fn has_ok_result(
        &mut self,
        from: &'w Package,
        source: &Source,
        expression: Node<'_>,
        depth: usize,
    ) -> Result<bool> {
        match expression.kind() {
            "type_assertion_expression" => Ok(true),
            "unary_expression" => Ok(expression
                .child_by_field_name("operator")
                .is_some_and(|op| op.kind() == "<-")),
            "index_expression" => {
                if let Some(operand) = expression.child_by_field_name("operand")
                    && let Some(ty) = self.infer_type(from, source, operand, depth + 1, 0)?
                {
                    return Ok(matches!(
                        self.container(ty)?,
                        Some((Container::Map { .. }, 0))
                    ));
                }
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    fn referenced_value_type(
        &mut self,
        from: &'w Package,
        source: &Source,
        expression: Node<'_>,
        depth: usize,
    ) -> Result<Option<String>> {
        let resolve_type =
            |resolver: &mut Self, source: &Source, from, declaration: Declaration<'_>| {
                let owner = declaration_owner(declaration.name);
                if let Some(owner) = owner.filter(|owner| {
                    matches!(
                        owner.kind(),
                        "function_declaration" | "method_declaration" | "method_elem"
                    )
                }) {
                    return Ok(Some(function_type(owner, &source.text)));
                }
                let (Some(owner), Some(binding)) = (owner, declaration.binding) else {
                    return Ok(None);
                };
                let (explicit, initializer, index) = value_parts(owner, binding);
                if let Some(ty) = explicit {
                    return Ok(Some(syntax_text(ty, &source.text)));
                }
                match initializer {
                    Some(initializer) => {
                        resolver.value_type_text(from, source, initializer, index, depth + 1)
                    }
                    None => Ok(None),
                }
            };
        if let Some(declaration) =
            Declaration::local(source, ReferenceQuery::Name(expression), |name| {
                Ok(self
                    .indexes
                    .file_index(&source.path)?
                    .symbols
                    .declaration(name)
                    .map(|symbol| symbol.range))
            })?
        {
            return resolve_type(self, source, from, declaration);
        }
        let location = if expression.kind() == "selector_expression" {
            let (Some(receiver), Some(field)) = (
                expression.child_by_field_name("operand"),
                expression.child_by_field_name("field"),
            ) else {
                return Ok(None);
            };
            self.selector(
                from,
                source,
                receiver,
                node_text(field, &source.text),
                depth,
            )?
            .map(|symbol| (symbol.package, symbol.file, symbol.symbol.range))
        } else {
            self.source_name_location(from, source, expression)?
                .map(|(path, range)| (from, path, range))
        };
        if let Some((package, file, range)) = location {
            if let Some(definition) = self.indexes.reference_source(&file)?
                && let Some(name) = node_at_range(definition.as_ref(), range)
            {
                let definition = definition.as_ref();
                let package = self.declaring_package(definition, package);
                return resolve_type(
                    self,
                    definition,
                    package,
                    Declaration::new(definition, name),
                );
            }
            return Ok(None);
        }
        Ok(match expression.kind() {
            "iota" => Some("int".into()),
            "true" | "false" => Some("bool".into()),
            _ => None,
        })
    }

    fn range_type_text(
        &mut self,
        from: &'w Package,
        source: &Source,
        clause: Node<'_>,
        index: usize,
        depth: usize,
    ) -> Result<Option<String>> {
        let Some(iterable) = clause.child_by_field_name("right") else {
            return Ok(None);
        };
        if let Some(ty) = self.infer_type(from, source, iterable, depth + 1, 0)?
            && let Some(underlying) = self.underlying_type(ty.clone())?
        {
            match underlying.kind {
                TypeKind::Container(container, _) if index == 0 => {
                    if matches!(
                        (*container, underlying.pointers),
                        (Container::Slice(_), 0) | (Container::Array { .. }, 0 | 1)
                    ) {
                        return Ok(Some("int".into()));
                    }
                }
                TypeKind::Named { ref name, .. } if underlying.pointers == 0 => {
                    if name == "string" && self.type_info(underlying.package, name)?.is_none() {
                        return Ok(["int", "rune"].get(index).map(|name| (*name).into()));
                    }
                    if index == 0
                        && integer_type(name)
                        && self.type_info(underlying.package, name)?.is_none()
                    {
                        return self.type_text(from, source, &ty);
                    }
                }
                _ => {}
            }
        }
        let ty = self.value_type_text(from, source, iterable, 0, depth + 1)?;
        Ok(match ty.as_deref() {
            Some("string") => ["int", "rune"].get(index).map(|name| (*name).into()),
            Some(name) if index == 0 && integer_type(name) => ty,
            _ => None,
        })
    }

    fn builtin_result_text(
        &mut self,
        from: &'w Package,
        source: &Source,
        call: Node<'_>,
        depth: usize,
    ) -> Result<Option<String>> {
        let Some(function) = call
            .child_by_field_name("function")
            .filter(|node| node.kind() == "identifier")
        else {
            return Ok(None);
        };
        let name = node_text(function, &source.text);
        if !is_predeclared(name) || self.source_name_location(from, source, function)?.is_some() {
            return Ok(None);
        }
        let ty = match name {
            "len" | "cap" | "copy" => "int",
            "recover" => "any",
            ty if is_predeclared_type(ty) => ty,
            "append" | "min" | "max" => {
                if let Some(argument) = call.child_by_field_name("arguments").and_then(|args| {
                    args.named_children(&mut args.walk())
                        .find(|node| !node.is_extra())
                }) {
                    return self.value_type_text(from, source, argument, 0, depth + 1);
                }
                return Ok(None);
            }
            _ => return Ok(None),
        };
        Ok(Some(ty.to_owned()))
    }
}

fn integer_type(name: &str) -> bool {
    matches!(
        name,
        "int"
            | "int8"
            | "int16"
            | "int32"
            | "int64"
            | "uint"
            | "uint8"
            | "uint16"
            | "uint32"
            | "uint64"
            | "uintptr"
            | "byte"
            | "rune"
    )
}
