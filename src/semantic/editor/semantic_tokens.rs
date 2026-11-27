use lsp_types::{
    DocumentHighlightKind, SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokens,
    SemanticTokensLegend,
};

use super::{Editor, SymbolKind, descendants, document_highlight::access};

const TYPES: [(SymbolKind, SemanticTokenType); 11] = [
    (SymbolKind::Namespace, SemanticTokenType::NAMESPACE),
    (SymbolKind::Type, SemanticTokenType::TYPE),
    (SymbolKind::Struct, SemanticTokenType::STRUCT),
    (SymbolKind::Interface, SemanticTokenType::INTERFACE),
    (SymbolKind::TypeParameter, SemanticTokenType::TYPE_PARAMETER),
    (SymbolKind::Parameter, SemanticTokenType::PARAMETER),
    (SymbolKind::Variable, SemanticTokenType::VARIABLE),
    (SymbolKind::Property, SemanticTokenType::PROPERTY),
    (SymbolKind::Function, SemanticTokenType::FUNCTION),
    (SymbolKind::Method, SemanticTokenType::METHOD),
    (SymbolKind::Label, SemanticTokenType::new("label")),
];

pub(crate) fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: TYPES.iter().map(|(_, token)| token.clone()).collect(),
        token_modifiers: vec![
            SemanticTokenModifier::DECLARATION,
            SemanticTokenModifier::READONLY,
            SemanticTokenModifier::DEFAULT_LIBRARY,
            SemanticTokenModifier::MODIFICATION,
        ],
    }
}

const DECLARATION: u32 = 1 << 0;
const READONLY: u32 = 1 << 1;
const DEFAULT_LIBRARY: u32 = 1 << 2;
const MODIFICATION: u32 = 1 << 3;

pub(super) fn tokens(editor: &Editor<'_>) -> SemanticTokens {
    let mut absolute = Vec::new();
    for node in descendants(editor.root()) {
        let Some(symbol) = editor.symbol(node) else {
            continue;
        };
        let token_type = TYPES
            .iter()
            .position(|(kind, _)| *kind == symbol.kind)
            .unwrap() as u32;
        let mut modifiers = 0;
        if symbol.declaration {
            modifiers |= DECLARATION;
        } else if !symbol.default_library
            && !symbol.readonly
            && access(editor, node, &symbol) == DocumentHighlightKind::WRITE
        {
            modifiers |= MODIFICATION;
        }
        if symbol.readonly {
            modifiers |= READONLY;
        }
        if symbol.default_library {
            modifiers |= DEFAULT_LIBRARY;
        }

        let mut offset = node.start_byte();
        for line in editor.text(node).split_inclusive('\n') {
            let content = line
                .strip_suffix('\n')
                .map_or(line, |line| line.strip_suffix('\r').unwrap_or(line));
            if !content.is_empty() {
                let start = editor.position(offset);
                let length = content.encode_utf16().count() as u32;
                absolute.push((start, length, token_type, modifiers));
            }
            offset += line.len();
        }
    }
    absolute.sort_by_key(|&(position, _, _, _)| position);
    let mut data = Vec::with_capacity(absolute.len());
    let (mut previous_line, mut previous_start, mut previous_end) = (0, 0, 0);
    for (start, length, token_type, token_modifiers_bitset) in absolute {
        let delta_line = start.line - previous_line;
        if delta_line == 0 && start.character < previous_end {
            continue;
        }
        let delta_start = if delta_line == 0 {
            start.character - previous_start
        } else {
            start.character
        };
        data.push(SemanticToken {
            delta_line,
            delta_start,
            length,
            token_type,
            token_modifiers_bitset,
        });
        (previous_line, previous_start, previous_end) =
            (start.line, start.character, start.character + length);
    }
    SemanticTokens {
        result_id: None,
        data,
    }
}
