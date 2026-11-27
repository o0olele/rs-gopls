mod code_action;
mod formatting;
mod imports;

pub(crate) use imports::{ImportEditError, ImportOperation, import_edits};
pub(crate) use {
    code_action::{code_actions, needs_diagnostics},
    formatting::{GofmtCache, format_document},
};

#[cfg(test)]
mod tests;
