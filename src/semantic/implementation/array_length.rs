use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use lsp_types::Range;
use tree_sitter::Node;

use crate::semantic::constants::{const_definitions, integer_binary, parse_integer};
use crate::semantic::source::{node_at_range, node_text};
use crate::semantic::{Package, Resolver, Source, path_key};

impl<'w> Resolver<'w, '_> {
    pub(super) fn array_length(
        &mut self,
        package: &'w Package,
        origin: Option<(PathBuf, Range)>,
        length: Option<&str>,
    ) -> Result<Option<u64>> {
        let Some(length) = length else {
            return Ok(None);
        };
        if length.as_bytes().first().is_some_and(u8::is_ascii_digit)
            && let Some(value) = parse_integer(length)
        {
            return Ok(as_length(value));
        }
        let Some((file, bracket)) = origin else {
            return Ok(None);
        };
        let Some(source) = self.indexes.reference_source(&file)? else {
            return Ok(None);
        };
        let source = source.as_ref();
        let expression = node_at_range(source, bracket)
            .and_then(|node| node.parent())
            .filter(|node| node.kind() == "array_type")
            .and_then(|node| node.child_by_field_name("length"));
        let Some(expression) = expression.filter(|node| node_text(*node, &source.text) == length)
        else {
            return Ok(None);
        };
        let mut evaluation = Evaluation {
            remaining: 256,
            values: HashMap::new(),
        };
        let value = evaluation.expression(self, package, source, expression, None, 0)?;
        Ok(value.and_then(|integer| as_length(integer.value)))
    }
}

fn as_length(value: i128) -> Option<u64> {
    let value = i64::try_from(value).ok()?;
    u64::try_from(value).ok()
}

struct Evaluation<'w> {
    remaining: usize,
    // A pending entry breaks cycles. Both results and pending entries belong to this snapshot.
    values: HashMap<(&'w str, PathBuf, Range), Option<Integer>>,
}

impl<'w> Evaluation<'w> {
    fn expression(
        &mut self,
        resolver: &mut Resolver<'w, '_>,
        package: &'w Package,
        source: &Source,
        node: Node<'_>,
        iota: Option<usize>,
        depth: usize,
    ) -> Result<Option<Integer>> {
        if depth >= 48 || self.remaining == 0 || node.is_missing() || node.has_error() {
            return Ok(None);
        }
        self.remaining -= 1;
        let text = node_text(node, &source.text);
        let mut evaluate =
            |child| self.expression(resolver, package, source, child, iota, depth + 1);
        let result = match node.kind() {
            "int_literal" => parse_integer(text).map(Integer::untyped),
            "parenthesized_expression" => {
                let mut cursor = node.walk();
                let Some(inner) = node
                    .named_children(&mut cursor)
                    .find(|child| !child.is_extra())
                else {
                    return Ok(None);
                };
                return evaluate(inner);
            }
            "unary_expression" => {
                let (Some(op), Some(operand)) = (
                    node.child_by_field_name("operator"),
                    node.child_by_field_name("operand"),
                ) else {
                    return Ok(None);
                };
                evaluate(operand)?.and_then(|operand| operand.unary(op.kind()))
            }
            "binary_expression" => {
                let (Some(op), Some(left), Some(right)) = (
                    node.child_by_field_name("operator"),
                    node.child_by_field_name("left"),
                    node.child_by_field_name("right"),
                ) else {
                    return Ok(None);
                };
                let Some(left) = evaluate(left)? else {
                    return Ok(None);
                };
                evaluate(right)?.and_then(|right| left.binary(op.kind(), right))
            }
            "call_expression" | "type_conversion_expression" => {
                let Some((ty, operand)) = conversion(node) else {
                    return Ok(None);
                };
                let Some(ty) = IntegerType::resolve(resolver, package, source, ty)? else {
                    return Ok(None);
                };
                let operand =
                    self.expression(resolver, package, source, operand, iota, depth + 1)?;
                operand.and_then(|value| Integer::untyped(value.value).with_type(Some(ty)))
            }
            "identifier" | "iota" => {
                let Some((file, range)) = resolver.source_name_location(package, source, node)?
                else {
                    return Ok(iota
                        .filter(|_| text == "iota")
                        .map(|i| Integer::untyped(i as i128)));
                };
                // An unqualified name can denote a declaration in a dot-imported package.
                let owner = source
                    .imports
                    .iter()
                    .filter(|import| import.alias.as_deref() == Some("."))
                    .filter_map(|import| resolver.workspace.resolve_import(package, &import.path))
                    .find(|owner| resolver.indexes.files(owner).contains(&file))
                    .unwrap_or(package);
                return self.declaration(resolver, owner, file, range, depth + 1);
            }
            "selector_expression" => {
                let (Some(receiver), Some(field)) = (
                    node.child_by_field_name("operand"),
                    node.child_by_field_name("field"),
                ) else {
                    return Ok(None);
                };
                let Some(found) = resolver.selector(
                    package,
                    source,
                    receiver,
                    node_text(field, &source.text),
                    0,
                )?
                else {
                    return Ok(None);
                };
                return self.declaration(
                    resolver,
                    found.package,
                    found.file,
                    found.symbol.range,
                    depth + 1,
                );
            }
            _ => None,
        };
        Ok(result)
    }

    fn declaration(
        &mut self,
        resolver: &mut Resolver<'w, '_>,
        package: &'w Package,
        file: PathBuf,
        range: Range,
        depth: usize,
    ) -> Result<Option<Integer>> {
        let key = (package.id.as_str(), path_key(&file), range);
        if let Some(result) = self.values.get(&key) {
            return Ok(*result);
        }
        self.values.insert(key.clone(), None);
        let Some(source) = resolver.indexes.reference_source(&file)? else {
            return Ok(None);
        };
        let source = source.as_ref();
        let Some(name) = node_at_range(source, range) else {
            return Ok(None);
        };
        let Some(group) = name
            .parent()
            .filter(|spec| spec.kind() == "const_spec")
            .and_then(|spec| spec.parent())
            .filter(|group| group.kind() == "const_declaration" && !group.has_error())
        else {
            return Ok(None);
        };
        let Some(def) = const_definitions(group).find(|def| def.name == name) else {
            return Ok(None);
        };
        // In a local group an omitted expression can see newly shadowing local declarations.
        if !def.explicit_value
            && group
                .parent()
                .is_none_or(|parent| parent.kind() != "source_file")
        {
            return Ok(None);
        }
        let Some(mut result) = self.expression(
            resolver,
            package,
            source,
            def.expression,
            Some(def.iota),
            depth,
        )?
        else {
            return Ok(None);
        };
        if let Some(ty) = def.explicit_type {
            let Some(kind) = IntegerType::resolve(resolver, package, source, ty)? else {
                return Ok(None);
            };
            let Some(typed) = result.with_type(Some(kind)) else {
                return Ok(None);
            };
            result = typed;
        }
        self.values.insert(key, Some(result));
        Ok(Some(result))
    }
}

fn conversion(node: Node<'_>) -> Option<(Node<'_>, Node<'_>)> {
    if node.kind() == "type_conversion_expression" {
        return Some((
            node.child_by_field_name("type")?,
            node.child_by_field_name("operand")?,
        ));
    }
    if node.child_by_field_name("type_arguments").is_some() {
        return None;
    }
    let arguments = node.child_by_field_name("arguments")?;
    let mut cursor = arguments.walk();
    let mut values = arguments
        .named_children(&mut cursor)
        .filter(|node| !node.is_extra());
    let operand = values.next()?;
    values
        .next()
        .is_none()
        .then_some((node.child_by_field_name("function")?, operand))
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum IntegerType {
    Signed(u32),
    Unsigned(u32),
    Int,
    Uint,
    Uintptr,
}

impl IntegerType {
    fn resolve<'w>(
        resolver: &mut Resolver<'w, '_>,
        package: &'w Package,
        source: &Source,
        ty: Node<'_>,
    ) -> Result<Option<Self>> {
        let kind = match node_text(ty, &source.text) {
            "int8" => Self::Signed(8),
            "int16" => Self::Signed(16),
            "int32" | "rune" => Self::Signed(32),
            "int64" => Self::Signed(64),
            "uint8" | "byte" => Self::Unsigned(8),
            "uint16" => Self::Unsigned(16),
            "uint32" => Self::Unsigned(32),
            "uint64" => Self::Unsigned(64),
            "int" => Self::Int,
            "uint" => Self::Uint,
            "uintptr" => Self::Uintptr,
            _ => return Ok(None),
        };
        Ok(resolver
            .source_name_location(package, source, ty)?
            .is_none()
            .then_some(kind))
    }

    fn represents(self, value: i128) -> bool {
        match self {
            Self::Signed(bits) => {
                let bound = 1_i128 << (bits - 1);
                (-bound..bound).contains(&value)
            }
            Self::Unsigned(bits) => (0..1_i128 << bits).contains(&value),
            // The loaded Go build can target a different word size from the server.
            Self::Int => i32::try_from(value).is_ok(),
            Self::Uint | Self::Uintptr => u32::try_from(value).is_ok(),
        }
    }
}

#[derive(Clone, Copy)]
struct Integer {
    value: i128,
    ty: Option<IntegerType>,
}

impl Integer {
    fn untyped(value: i128) -> Self {
        Self { value, ty: None }
    }

    fn with_type(self, ty: Option<IntegerType>) -> Option<Self> {
        let ty = ty.or(self.ty);
        if self.ty.is_some() && self.ty != ty || ty.is_some_and(|ty| !ty.represents(self.value)) {
            return None;
        }
        Some(Self {
            value: self.value,
            ty,
        })
    }

    fn unary(self, op: &str) -> Option<Self> {
        let value = match (op, self.ty) {
            ("+", _) => self.value,
            ("-", _) => self.value.checked_neg()?,
            ("^", Some(IntegerType::Unsigned(bits))) => ((1_i128 << bits) - 1) ^ self.value,
            ("^", Some(IntegerType::Uint | IntegerType::Uintptr)) => return None,
            ("^", _) => !self.value,
            _ => return None,
        };
        Self::untyped(value).with_type(self.ty)
    }

    fn binary(self, op: &str, right: Self) -> Option<Self> {
        let ty = if matches!(op, "<<" | ">>") {
            self.ty
        } else {
            let ty = self.ty.or(right.ty);
            self.with_type(ty)?;
            right.with_type(ty)?;
            ty
        };
        Self::untyped(integer_binary(op, self.value, right.value)?).with_type(ty)
    }
}
