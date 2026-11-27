use super::{CallArguments, Node, Parser, Result, Source};

#[derive(Clone, Copy)]
pub(super) struct Call<'tree> {
    pub(super) callee: Node<'tree>,
    pub(super) type_arguments: Option<Node<'tree>>,
    pub(super) arguments: CallArguments<'tree>,
    pub(super) active_parameter: u32,
}

pub(super) enum CallSite<'tree> {
    Found(Call<'tree>),
    Blocked,
    Missing,
}

pub(super) fn call_at(source: &Source, offset: usize) -> CallSite<'_> {
    let end = source.text.len();
    if end == 0 {
        return CallSite::Missing;
    }
    let probe = offset.min(end - 1);
    let site = call_from_probe(source, offset, probe);
    if matches!(site, CallSite::Missing)
        && let Some(previous) = source.text[..offset].trim_end().len().checked_sub(1)
        && previous < probe
    {
        // An inserted missing ')' can end the argument list before subsequent
        // whitespace. Probe the preceding token to recover that list.
        return call_from_probe(source, offset, previous);
    }
    site
}

fn call_from_probe(source: &Source, offset: usize, probe: usize) -> CallSite<'_> {
    let mut node = source
        .tree
        .root_node()
        .descendant_for_byte_range(probe, probe + 1);
    while let Some(current) = node {
        match current.kind() {
            "argument_list" => {
                if let Some(call) = call_in_arguments(current, offset, &source.text) {
                    return CallSite::Found(call);
                }
                if delimiter(current, "(").is_some_and(|open| open.start_byte() == offset) {
                    return CallSite::Blocked;
                }
            }
            "type_conversion_expression" => {
                if let Some(call) = call_in_conversion(current, offset, &source.text) {
                    return CallSite::Found(call);
                }
                if delimiter(current, "(").is_some_and(|open| open.start_byte() == offset) {
                    return CallSite::Blocked;
                }
            }
            "func_literal" | "literal_value" if current.byte_range().contains(&offset) => {
                return CallSite::Blocked;
            }
            "index_expression" | "slice_expression" | "type_arguments"
                if within_delimiters(current, "[", "]", offset, &source.text) =>
            {
                return CallSite::Blocked;
            }
            "ERROR" => return CallSite::Missing,
            _ => {}
        }
        node = current.parent();
    }
    CallSite::Missing
}

fn delimiter<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    node.children(&mut node.walk())
        .find(|child| child.kind() == kind)
}

fn within_delimiters(node: Node<'_>, open: &str, close: &str, offset: usize, text: &str) -> bool {
    delimiter(node, open).is_some_and(|open| open.end_byte() <= offset)
        && delimiter(node, close).is_some_and(|close| {
            offset <= close.start_byte()
                || (close.is_missing() && text[close.start_byte()..offset].trim().is_empty())
        })
}

fn call_in_arguments<'tree>(list: Node<'tree>, offset: usize, text: &str) -> Option<Call<'tree>> {
    if !within_delimiters(list, "(", ")", offset, text) {
        return None;
    }
    let owner = list
        .parent()
        .filter(|parent| parent.kind() == "call_expression")?;
    let callee = owner.child_by_field_name("function")?;
    Some(Call {
        callee,
        type_arguments: owner.child_by_field_name("type_arguments"),
        arguments: CallArguments::List(Some(list)),
        active_parameter: commas_before(list, offset),
    })
}

fn call_in_conversion<'tree>(node: Node<'tree>, offset: usize, text: &str) -> Option<Call<'tree>> {
    // Tree-sitter also parses F[T](x) and callbacks[i](x) as conversions.
    // The callee binding determines whether it is actually callable.
    if !within_delimiters(node, "(", ")", offset, text) {
        return None;
    }
    let callee = node.child_by_field_name("type")?;
    let (callee, type_arguments) = if callee.kind() == "generic_type" {
        (
            callee.child_by_field_name("type")?,
            callee.child_by_field_name("type_arguments"),
        )
    } else {
        (callee, None)
    };
    Some(Call {
        callee,
        type_arguments,
        arguments: CallArguments::Single(node.child_by_field_name("operand")?),
        active_parameter: commas_before(node, offset),
    })
}

fn commas_before(list: Node<'_>, offset: usize) -> u32 {
    list.children(&mut list.walk())
        .take_while(|child| child.start_byte() < offset)
        .filter(|child| child.kind() == ",")
        .count() as u32
}

pub(super) fn repair_call(
    parser: &mut Parser,
    source: &Source,
    offset: usize,
) -> Result<Option<Source>> {
    // Go permits trailing whitespace after an opening paren or comma. The
    // recovery node typically ends at the last token before that whitespace.
    let end = source.text[..offset].trim_end().len();
    let Some(last) = end.checked_sub(1) else {
        return Ok(None);
    };
    let node = source.tree.root_node().descendant_for_byte_range(last, end);
    let error = std::iter::successors(node, |node| node.parent()).find(|node| node.is_error());
    let Some(error) = error else { return Ok(None) };
    let closes = parens_to_close(error, offset);
    if closes == 0 {
        return Ok(None);
    }

    let mut text = source.text.as_ref().clone();
    let suffix = ")".repeat(closes);
    text.insert_str(offset, &suffix);
    let point = source.lines.point(offset);
    let mut previous = source.tree.clone();
    previous.edit(&tree_sitter::InputEdit {
        start_byte: offset,
        old_end_byte: offset,
        new_end_byte: offset + closes,
        start_position: point,
        old_end_position: point,
        new_end_position: tree_sitter::Point::new(point.row, point.column + closes),
    });
    Source::parse(parser, source.path.clone(), text, Some(&previous)).map(Some)
}

fn parens_to_close(error: Node<'_>, offset: usize) -> usize {
    let mut stack = Vec::new();
    let mut cursor = error.walk();
    loop {
        let node = cursor.node();
        if node.start_byte() >= offset {
            break;
        }
        if !node.is_missing() {
            match node.kind() {
                "(" => stack.push(")"),
                "[" => stack.push("]"),
                "{" => stack.push("}"),
                ")" | "]" | "}" if stack.last() == Some(&node.kind()) => {
                    stack.pop();
                }
                // Tree-sitter has already lexed strings and comments.
                "comment"
                | "raw_string_literal"
                | "interpreted_string_literal"
                | "rune_literal" => {}
                _ if cursor.goto_first_child() => continue,
                _ => {}
            }
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return stack
                    .iter()
                    .rev()
                    .take_while(|&&close| close == ")")
                    .count();
            }
        }
    }
    stack
        .iter()
        .rev()
        .take_while(|&&close| close == ")")
        .count()
}
