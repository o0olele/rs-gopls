use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::rc::Rc;

use lsp_types::{InlayHint, InlayHintKind, InlayHintLabel, Position, Range};
use tree_sitter::Node;

use super::super::constants::{ConstDefinition, const_definitions, integer_binary, parse_integer};
use super::{
    Editor, Parameter, SymbolId, descendants, named_children, terminal_type_name, unwrap_parens,
};

pub(super) fn hints(editor: &Editor<'_>, range: Range) -> Vec<InlayHint> {
    let mut hints = Hints {
        editor,
        range,
        items: Vec::new(),
    };
    let mut constants = Constants::new(editor);
    for node in descendants(editor.root()) {
        match node.kind() {
            "call_expression" => hints.call(node),
            "literal_value" => hints.fields(node),
            "return_statement" => hints.results(node),
            "const_declaration" => constants.collect(node),
            _ => {}
        }
    }
    let mut definitions: Vec<_> = constants.definitions.values().copied().collect();
    definitions.sort_unstable_by_key(|definition| definition.name.start_byte());
    for definition in definitions {
        let position = editor.position(definition.name.end_byte());
        if hints.contains(position)
            && let Some(value) = constants.evaluate(definition)
        {
            hints.push(position, format!("= {value}"), None);
        }
    }
    hints.items.sort_by(|left, right| {
        left.position
            .cmp(&right.position)
            .then_with(|| label(left).cmp(label(right)))
    });
    hints
        .items
        .dedup_by(|left, right| left.position == right.position && label(left) == label(right));
    hints.items
}

fn label(hint: &InlayHint) -> &str {
    match &hint.label {
        InlayHintLabel::String(label) => label,
        InlayHintLabel::LabelParts(_) => unreachable!("editor hints use plain labels"),
    }
}

struct Hints<'editor, 'tree> {
    editor: &'editor Editor<'tree>,
    range: Range,
    items: Vec<InlayHint>,
}

impl<'tree> Hints<'_, 'tree> {
    fn contains(&self, position: Position) -> bool {
        self.range.start <= position && position < self.range.end
    }

    fn push(&mut self, position: Position, label: String, kind: Option<InlayHintKind>) {
        self.items.push(InlayHint {
            position,
            label: InlayHintLabel::String(label),
            kind,
            text_edits: None,
            tooltip: None,
            padding_left: kind.is_none().then_some(true),
            padding_right: kind.is_some().then_some(true),
            data: None,
        });
    }

    fn call(&mut self, call: Node<'tree>) {
        let (Some(callee), Some(arguments)) = (
            call.child_by_field_name("function"),
            call.child_by_field_name("arguments"),
        ) else {
            return;
        };
        let parameters = match self
            .editor
            .symbol(unwrap_parens(callee))
            .map(|occurrence| occurrence.id)
        {
            Some(SymbolId::Builtin(name)) => builtin_parameters(name),
            _ => self.editor.call_parameters(call),
        };
        if let Some(parameters) = parameters {
            self.annotate(parameters, arguments);
        }
    }

    fn results(&mut self, statement: Node<'tree>) {
        if let Some(result) = self
            .editor
            .function_owner(statement)
            .and_then(|function| function.child_by_field_name("result"))
            .filter(|result| result.kind() == "parameter_list")
            && let Some(expressions) =
                named_children(statement).find(|node| node.kind() == "expression_list")
        {
            self.annotate(self.editor.parameters(result), expressions);
        }
    }

    fn annotate(&mut self, parameters: Vec<Parameter<'tree>>, arguments: Node<'tree>) {
        for (parameter, argument) in parameters.into_iter().zip(named_children(arguments)) {
            let name = parameter.name.unwrap_or_default();
            if name == "_" || (name.is_empty() && !parameter.variadic) {
                continue;
            }
            let position = self.editor.position(argument.start_byte());
            if self.contains(position) && show_inlay(self.editor, argument) {
                let suffix = if parameter.variadic { "...:" } else { ":" };
                self.push(
                    position,
                    format!("{name}{suffix}"),
                    Some(InlayHintKind::PARAMETER),
                );
            }
        }
    }

    fn fields(&mut self, literal: Node<'tree>) {
        let elements: Vec<_> = named_children(literal).collect();
        if elements.is_empty() || elements.iter().any(|node| node.kind() != "literal_element") {
            return;
        }
        let Some(fields) = self.editor.literal_fields(literal) else {
            return;
        };
        if elements.len() > fields.len() {
            return;
        }
        for (element, field) in elements.into_iter().zip(fields) {
            let position = self.editor.position(element.start_byte());
            if !self.contains(position) {
                continue;
            }
            let name = self.editor.text(field);
            let similar_name = named_children(element)
                .next()
                .filter(|expression| expression.kind() == "composite_literal")
                .and_then(|expression| expression.child_by_field_name("type"))
                .and_then(terminal_type_name)
                .map(|ty| self.editor.text(ty))
                .is_some_and(|ty| ty.to_lowercase() == name.to_lowercase());
            if !similar_name {
                self.push(position, format!("{name}:"), Some(InlayHintKind::PARAMETER));
            }
        }
    }
}

fn builtin_parameters(name: &str) -> Option<Vec<Parameter<'static>>> {
    let (names, variadic): (&[&str], bool) = match name {
        "len" | "cap" | "panic" => (&["v"], false),
        "append" => (&["slice", "elems"], true),
        "copy" => (&["dst", "src"], false),
        "delete" => (&["m", "key"], false),
        "close" | "real" | "imag" => (&["c"], false),
        "complex" => (&["r", "i"], false),
        "clear" => (&["t"], false),
        "min" | "max" => (&["x", "y"], true),
        "print" | "println" => (&["args"], true),
        _ => return None,
    };
    Some(
        names
            .iter()
            .enumerate()
            .map(|(index, &name)| Parameter {
                name: Some(name),
                variadic: variadic && index + 1 == names.len(),
            })
            .collect(),
    )
}

fn show_inlay<'tree>(editor: &Editor<'tree>, argument: Node<'tree>) -> bool {
    let expression = if argument.kind() == "variadic_argument" {
        let Some(expression) = named_children(argument).next() else {
            return false;
        };
        expression
    } else {
        argument
    };
    let simple = |node: Node<'tree>| match node.kind() {
        "nil" | "true" | "false" | "identifier" => matches!(
            editor.symbol(node).map(|occurrence| occurrence.id),
            Some(SymbolId::Builtin("nil" | "true" | "false"))
        ),
        "int_literal"
        | "float_literal"
        | "imaginary_literal"
        | "rune_literal"
        | "raw_string_literal"
        | "raw_string_literal_content"
        | "interpreted_string_literal"
        | "interpreted_string_literal_content"
        | "escape_sequence"
        | "binary_expression"
        | "unary_expression"
        | "parenthesized_expression" => true,
        _ => false,
    };
    !expression.has_error() && simple(expression) && named_children(expression).all(simple)
}

const MAX_EVALUATION_DEPTH: usize = 64;
const MAX_EVALUATION_NODES: usize = 4096;
const MAX_CONCATENATED_BYTES: usize = 1 << 20;

struct Constants<'editor, 'tree> {
    editor: &'editor Editor<'tree>,
    definitions: HashMap<usize, ConstDefinition<'tree>>,
    visiting: HashSet<usize>,
    values: HashMap<usize, Constant>,
    remaining: usize,
}

impl<'editor, 'tree> Constants<'editor, 'tree> {
    fn new(editor: &'editor Editor<'tree>) -> Self {
        Self {
            editor,
            definitions: HashMap::new(),
            visiting: HashSet::new(),
            values: HashMap::new(),
            remaining: 0,
        }
    }

    fn collect(&mut self, declaration: Node<'tree>) {
        for definition in const_definitions(declaration) {
            self.definitions
                .insert(definition.name.start_byte(), definition);
        }
    }

    fn evaluate(&mut self, definition: ConstDefinition<'tree>) -> Option<String> {
        self.remaining = MAX_EVALUATION_NODES;
        let value = self.constant(definition.name.start_byte(), 0)?.render()?;
        let expression = self.editor.text(definition.expression);
        if definition.explicit_value && unquote(&value) == unquote(expression) {
            return None;
        }
        Some(value)
    }

    fn constant(&mut self, offset: usize, depth: usize) -> Option<Constant> {
        if depth >= MAX_EVALUATION_DEPTH {
            return None;
        }
        if let Some(value) = self.values.get(&offset) {
            return Some(value.clone());
        }
        let definition = *self.definitions.get(&offset)?;
        if !self.visiting.insert(offset) {
            return None;
        }
        let value = self
            .expression(definition.expression, definition.iota, depth + 1)
            .and_then(|value| match definition.explicit_type {
                Some(ty) => value.with_type(self.constant_type(ty, 0)?),
                None => Some(value),
            });
        self.visiting.remove(&offset);
        if let Some(value) = &value {
            self.values.insert(offset, value.clone());
        }
        value
    }

    fn expression(&mut self, node: Node<'tree>, iota: usize, depth: usize) -> Option<Constant> {
        if depth >= MAX_EVALUATION_DEPTH || node.has_error() {
            return None;
        }
        self.remaining = self.remaining.checked_sub(1)?;
        let text = self.editor.text(node);
        let value = match node.kind() {
            "int_literal" => Value::Int(parse_integer(text)?),
            "float_literal" => Value::float(parse_float(text)?)?,
            "rune_literal" => Value::Int(i128::from(decode_rune(text)?)),
            "raw_string_literal" | "interpreted_string_literal" => {
                Value::String(decode_string(text)?.into())
            }
            "identifier" | "true" | "false" | "iota" => match self.editor.symbol(node)?.id {
                SymbolId::Builtin("iota") => Value::Int(iota as i128),
                SymbolId::Builtin("true") => Value::Bool(true),
                SymbolId::Builtin("false") => Value::Bool(false),
                SymbolId::Declaration(offset) => return self.constant(offset, depth + 1),
                _ => return None,
            },
            "parenthesized_expression" => {
                return self.expression(named_children(node).next()?, iota, depth + 1);
            }
            "unary_expression" => {
                let op = node.child_by_field_name("operator")?.kind();
                let operand =
                    self.expression(node.child_by_field_name("operand")?, iota, depth + 1)?;
                return operand.unary(op);
            }
            "binary_expression" => {
                let op = node.child_by_field_name("operator")?.kind();
                let left = self.expression(node.child_by_field_name("left")?, iota, depth + 1)?;
                let right = self.expression(node.child_by_field_name("right")?, iota, depth + 1)?;
                return left.binary(op, right);
            }
            "call_expression" => {
                let function = unwrap_parens(node.child_by_field_name("function")?);
                if self.editor.symbol(function)?.id != SymbolId::Builtin("len") {
                    return None;
                }
                let mut arguments = named_children(node.child_by_field_name("arguments")?);
                let argument = arguments.next()?;
                if arguments.next().is_some()
                    || node.child_by_field_name("type_arguments").is_some()
                {
                    return None;
                }
                let Value::String(string) = self.expression(argument, iota, depth + 1)?.value
                else {
                    return None;
                };
                return Constant::untyped(Value::Int(string.len() as i128))
                    .with_type(ConstantType::builtin("int")?);
            }
            _ => return None,
        };
        Some(Constant::untyped(value))
    }

    fn constant_type(&self, node: Node<'tree>, depth: usize) -> Option<ConstantType> {
        if depth >= MAX_EVALUATION_DEPTH {
            return None;
        }
        let node = unwrap_parens(node);
        match self.editor.symbol(node)?.id {
            SymbolId::Builtin(name) => ConstantType::builtin(name),
            SymbolId::Declaration(offset) => {
                let owner = self.editor.declaration(node)?.parent()?;
                let mut ty = self.constant_type(owner.child_by_field_name("type")?, depth + 1)?;
                match owner.kind() {
                    "type_spec" => ty.identity = TypeIdentity::Named(offset),
                    "type_alias" => {}
                    _ => return None,
                }
                Some(ty)
            }
        }
    }
}

#[derive(Clone)]
struct Constant {
    value: Value,
    ty: Option<ConstantType>,
}

#[derive(Clone)]
enum Value {
    Int(i128),
    Float(f64),
    Bool(bool),
    String(Rc<[u8]>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ConstantType {
    identity: TypeIdentity,
    class: TypeClass,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TypeIdentity {
    Builtin(&'static str),
    Named(usize),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TypeClass {
    Signed(u32),
    Unsigned(u32),
    Float32,
    Float64,
    Bool,
    String,
}

impl ConstantType {
    fn builtin(name: &str) -> Option<Self> {
        let (identity, class) = match name {
            "int" => ("int", TypeClass::Signed(usize::BITS)),
            "int8" => ("int8", TypeClass::Signed(8)),
            "int16" => ("int16", TypeClass::Signed(16)),
            "int32" | "rune" => ("int32", TypeClass::Signed(32)),
            "int64" => ("int64", TypeClass::Signed(64)),
            "uint" => ("uint", TypeClass::Unsigned(usize::BITS)),
            "uintptr" => ("uintptr", TypeClass::Unsigned(usize::BITS)),
            "uint8" | "byte" => ("uint8", TypeClass::Unsigned(8)),
            "uint16" => ("uint16", TypeClass::Unsigned(16)),
            "uint32" => ("uint32", TypeClass::Unsigned(32)),
            "uint64" => ("uint64", TypeClass::Unsigned(64)),
            "float32" => ("float32", TypeClass::Float32),
            "float64" => ("float64", TypeClass::Float64),
            "bool" => ("bool", TypeClass::Bool),
            "string" => ("string", TypeClass::String),
            _ => return None,
        };
        Some(Self {
            identity: TypeIdentity::Builtin(identity),
            class,
        })
    }
}

impl Constant {
    fn untyped(value: Value) -> Self {
        Self { value, ty: None }
    }

    fn with_type(self, ty: ConstantType) -> Option<Self> {
        if self.ty == Some(ty) {
            return Some(self);
        }
        if self.ty.is_some() {
            return None;
        }
        let value = match ty.class {
            TypeClass::Signed(bits) => {
                let integer = self.value.integer()?;
                let half = 1_i128 << (bits - 1);
                (-half..half)
                    .contains(&integer)
                    .then_some(Value::Int(integer))?
            }
            TypeClass::Unsigned(bits) => {
                let integer = self.value.integer()?;
                (0..(1_i128 << bits))
                    .contains(&integer)
                    .then_some(Value::Int(integer))?
            }
            TypeClass::Float32 => Value::float(f64::from(self.value.number()? as f32))?,
            TypeClass::Float64 => Value::float(self.value.number()?)?,
            TypeClass::Bool if matches!(self.value, Value::Bool(_)) => self.value,
            TypeClass::String if matches!(self.value, Value::String(_)) => self.value,
            _ => return None,
        };
        Some(Self {
            value,
            ty: Some(ty),
        })
    }

    fn result(value: Value, ty: Option<ConstantType>) -> Option<Self> {
        let value = Self::untyped(value);
        match ty {
            Some(ty) => value.with_type(ty),
            None => Some(value),
        }
    }

    fn unary(self, operator: &str) -> Option<Self> {
        let ty = self.ty;
        let value = match (operator, self.value) {
            ("+", value @ (Value::Int(_) | Value::Float(_))) => value,
            ("-", Value::Int(value)) => Value::Int(value.checked_neg()?),
            ("-", Value::Float(value)) => Value::Float(-value),
            ("!", Value::Bool(value)) => Value::Bool(!value),
            ("^", Value::Int(value)) => Value::Int(match ty.map(|ty| ty.class) {
                Some(TypeClass::Unsigned(bits)) => ((1_i128 << bits) - 1) ^ value,
                _ => !value,
            }),
            _ => return None,
        };
        Self::result(value, ty)
    }

    fn binary(self, operator: &str, other: Self) -> Option<Self> {
        if matches!(operator, "<<" | ">>") {
            return self.shift(operator, other);
        }
        let ty = self.ty.or(other.ty);
        let (left, right) = match ty {
            Some(ty) => (self.with_type(ty)?.value, other.with_type(ty)?.value),
            None => (self.value, other.value),
        };
        let value = match (left, right) {
            (Value::Int(left), Value::Int(right)) => integer_binary(operator, left, right)
                .map(Value::Int)
                .or_else(|| comparison(operator, &left, &right)),
            (Value::Bool(left), Value::Bool(right)) => Some(Value::Bool(match operator {
                "&&" => left && right,
                "||" => left || right,
                "==" => left == right,
                "!=" => left != right,
                _ => return None,
            })),
            (Value::String(left), Value::String(right)) if operator == "+" => {
                if left.len().checked_add(right.len())? > MAX_CONCATENATED_BYTES {
                    return None;
                }
                let mut joined = left.to_vec();
                joined.extend_from_slice(&right);
                Some(Value::String(joined.into()))
            }
            (Value::String(left), Value::String(right)) => comparison(operator, &left, &right),
            (left, right) => float_binary(operator, left.number()?, right.number()?),
        }?;
        let comparison = matches!(operator, "==" | "!=" | "<" | "<=" | ">" | ">=");
        Self::result(value, if comparison { None } else { ty })
    }

    fn shift(self, operator: &str, other: Self) -> Option<Self> {
        let shifted = integer_binary(operator, self.shift_integer()?, other.shift_integer()?)?;
        Self::result(Value::Int(shifted), self.ty)
    }

    fn shift_integer(&self) -> Option<i128> {
        match self.value {
            Value::Int(value) => Some(value),
            _ if self.ty.is_none() => self.value.integer(),
            _ => None,
        }
    }

    fn render(&self) -> Option<String> {
        Some(match &self.value {
            Value::Int(value) => value.to_string(),
            Value::Float(value) if *value == 0.0 => "0".into(),
            Value::Float(value) if self.ty.is_some_and(|ty| ty.class == TypeClass::Float32) => {
                (*value as f32).to_string()
            }
            Value::Float(value) => value.to_string(),
            Value::Bool(value) => value.to_string(),
            Value::String(value) => {
                if value.iter().any(|byte| matches!(byte, 7 | 8 | 10..=13)) {
                    return None;
                }
                // Go strings can contain arbitrary bytes. Preserve invalid UTF-8 in
                // the UTF-8 hint label using Go byte escapes.
                let mut rendered = String::from("\"");
                for chunk in value.utf8_chunks() {
                    rendered.push_str(chunk.valid());
                    for byte in chunk.invalid() {
                        write!(rendered, "\\x{byte:02x}").ok()?;
                    }
                }
                rendered.push('"');
                rendered
            }
        })
    }
}

impl Value {
    fn float(value: f64) -> Option<Self> {
        value.is_finite().then_some(Self::Float(value))
    }

    fn integer(&self) -> Option<i128> {
        match self {
            Self::Int(value) => Some(*value),
            Self::Float(value)
                if value.fract() == 0.0
                    && *value >= i128::MIN as f64
                    && *value < -(i128::MIN as f64) =>
            {
                Some(*value as i128)
            }
            _ => None,
        }
    }

    fn number(&self) -> Option<f64> {
        match self {
            Self::Int(value) => Some(*value as f64),
            Self::Float(value) => Some(*value),
            _ => None,
        }
    }
}

fn float_binary(operator: &str, left: f64, right: f64) -> Option<Value> {
    Value::float(match operator {
        "+" => left + right,
        "-" => left - right,
        "*" => left * right,
        "/" => left / right,
        _ => return comparison(operator, &left, &right),
    })
}

fn comparison<T: PartialOrd>(operator: &str, left: &T, right: &T) -> Option<Value> {
    Some(Value::Bool(match operator {
        "==" => left == right,
        "!=" => left != right,
        "<" => left < right,
        "<=" => left <= right,
        ">" => left > right,
        ">=" => left >= right,
        _ => return None,
    }))
}

fn parse_float(text: &str) -> Option<f64> {
    let text = text.replace('_', "");
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        let (mantissa, exponent) = hex.split_once(['p', 'P'])?;
        let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
        let whole = whole.chars().try_fold(0.0, |value, digit| {
            Some(value * 16.0 + f64::from(digit.to_digit(16)?))
        })?;
        let fraction = fraction.chars().rev().try_fold(0.0, |value, digit| {
            Some((value + f64::from(digit.to_digit(16)?)) / 16.0)
        })?;
        return Some((whole + fraction) * 2_f64.powi(exponent.parse().ok()?));
    }
    text.parse().ok()
}

fn unquote(text: &str) -> &str {
    for quote in ['"', '\'', '`'] {
        if let Some(inner) = text
            .strip_prefix(quote)
            .and_then(|text| text.strip_suffix(quote))
        {
            return inner;
        }
    }
    text
}

fn decode_string(text: &str) -> Option<Vec<u8>> {
    if let Some(raw) = text
        .strip_prefix('`')
        .and_then(|text| text.strip_suffix('`'))
    {
        return Some(raw.bytes().filter(|&byte| byte != b'\r').collect());
    }
    let mut contents = text.strip_prefix('"')?.strip_suffix('"')?;
    let mut decoded = Vec::with_capacity(contents.len());
    while let Some((prefix, escape)) = contents.split_once('\\') {
        decoded.extend_from_slice(prefix.as_bytes());
        let (character, rest) = decode_escape(escape, b'"')?;
        match character {
            Escape::Byte(byte) => decoded.push(byte),
            Escape::Scalar(scalar) => {
                decoded.extend_from_slice(scalar.encode_utf8(&mut [0; 4]).as_bytes());
            }
        }
        contents = rest;
    }
    decoded.extend_from_slice(contents.as_bytes());
    Some(decoded)
}

fn decode_rune(text: &str) -> Option<u32> {
    let contents = text.strip_prefix('\'')?.strip_suffix('\'')?;
    if let Some(escape) = contents.strip_prefix('\\') {
        let (escaped, rest) = decode_escape(escape, b'\'')?;
        return rest.is_empty().then_some(match escaped {
            Escape::Byte(byte) => u32::from(byte),
            Escape::Scalar(scalar) => u32::from(scalar),
        });
    }
    let mut characters = contents.chars();
    let rune = characters.next()?;
    (characters.next().is_none() && !matches!(rune, '\r' | '\n')).then_some(u32::from(rune))
}

enum Escape {
    Byte(u8),
    Scalar(char),
}

fn decode_escape(text: &str, quote: u8) -> Option<(Escape, &str)> {
    let (kind, rest) = text.split_at_checked(1)?;
    let (digits, remaining, radix, unicode) = match kind.as_bytes()[0] {
        b'x' | b'u' | b'U' => {
            let width = match kind {
                "x" => 2,
                "u" => 4,
                _ => 8,
            };
            let (digits, remaining) = rest.split_at_checked(width)?;
            (digits, remaining, 16, kind != "x")
        }
        b'0'..=b'7' => {
            let (digits, remaining) = text.split_at_checked(3)?;
            (digits, remaining, 8, false)
        }
        escape => {
            let byte = match escape {
                b'a' => 7,
                b'b' => 8,
                b'f' => 12,
                b'n' => b'\n',
                b'r' => b'\r',
                b't' => b'\t',
                b'v' => 11,
                b'\\' => b'\\',
                escape if escape == quote => quote,
                _ => return None,
            };
            return Some((Escape::Byte(byte), rest));
        }
    };
    if !digits.chars().all(|digit| digit.is_digit(radix)) {
        return None;
    }
    let value = u32::from_str_radix(digits, radix).ok()?;
    let escape = if unicode {
        Escape::Scalar(char::from_u32(value)?)
    } else {
        Escape::Byte(u8::try_from(value).ok()?)
    };
    Some((escape, remaining))
}
