use tree_sitter::Node;

#[derive(Clone, Copy)]
pub(super) struct ConstDefinition<'tree> {
    pub(super) name: Node<'tree>,
    pub(super) expression: Node<'tree>,
    pub(super) explicit_type: Option<Node<'tree>>,
    pub(super) explicit_value: bool,
    pub(super) iota: usize,
}

/// Pairs names with their effective initializer, including omitted Go const specifications.
pub(super) fn const_definitions(
    declaration: Node<'_>,
) -> impl Iterator<Item = ConstDefinition<'_>> {
    let mut definitions = Vec::new();
    let mut previous = None;
    for (iota, spec) in declaration
        .named_children(&mut declaration.walk())
        .filter(|node| node.kind() == "const_spec" && !node.is_missing() && !node.is_extra())
        .enumerate()
    {
        let explicit = spec.child_by_field_name("value");
        if let Some(list) = explicit {
            let expressions = list
                .named_children(&mut list.walk())
                .filter(|node| !node.is_extra() && !node.is_missing())
                .collect::<Vec<_>>();
            previous = Some((expressions, spec.child_by_field_name("type")));
        } else if spec.has_error() {
            previous = None;
        }
        let Some((expressions, explicit_type)) = &previous else {
            continue;
        };
        let names: Vec<_> = spec
            .children_by_field_name("name", &mut spec.walk())
            .filter(|node| node.kind() == "identifier" && !node.is_missing())
            .collect();
        if names.len() == expressions.len() {
            definitions.extend(
                names
                    .into_iter()
                    .zip(expressions)
                    .map(|(name, &expression)| ConstDefinition {
                        name,
                        expression,
                        explicit_type: *explicit_type,
                        explicit_value: explicit.is_some(),
                        iota,
                    }),
            );
        }
    }
    definitions.into_iter()
}

pub(super) fn integer_binary(operator: &str, left: i128, right: i128) -> Option<i128> {
    match operator {
        "+" => left.checked_add(right),
        "-" => left.checked_sub(right),
        "*" => left.checked_mul(right),
        "/" => left.checked_div(right),
        "%" => left.checked_rem(right),
        "&" => Some(left & right),
        "|" => Some(left | right),
        "^" => Some(left ^ right),
        "&^" => Some(left & !right),
        "<<" | ">>" if right < 0 => None,
        ">>" => Some(left >> right.min(i128::from(i128::BITS - 1)) as u32),
        "<<" if left == 0 => Some(0),
        "<<" => {
            let amount = u32::try_from(right).ok()?;
            let shifted = left.checked_shl(amount)?;
            // Rust checks the shift count. Go constant evaluation also needs the lost value bits.
            (shifted >> amount == left).then_some(shifted)
        }
        _ => None,
    }
}

pub(super) fn parse_integer(text: &str) -> Option<i128> {
    let text = text.replace('_', "");
    let (digits, radix) = match text.as_bytes() {
        [b'0', b'x' | b'X', ..] => (&text[2..], 16),
        [b'0', b'b' | b'B', ..] => (&text[2..], 2),
        [b'0', b'o' | b'O', ..] => (&text[2..], 8),
        [b'0', _, ..] => (&text[1..], 8),
        _ => (text.as_str(), 10),
    };
    // from_str_radix accepts a sign, but a Go radix prefix must be followed by digits.
    // In particular, the '+' in "0+10" belongs to an expression.
    if !digits.as_bytes().first()?.is_ascii_hexdigit() {
        return None;
    }
    i128::from_str_radix(digits, radix).ok()
}
