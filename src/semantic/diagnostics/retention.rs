use std::collections::HashSet;
use std::ops::Range as Bytes;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use lsp_types::Range;
use tree_sitter::{Node, Tree};

use super::{DiagnosticCode, DiagnosticFixData, DiagnosticReport, DiagnosticSnapshotKey};
use crate::semantic::source::{LineIndex, Source, check_cancelled};

// Keep a cheap text/tree revision for an open document's last visible report.
// Larger documents can still deliver a complete report without keeping a second tree alive.
const MAX_RETAINED_SOURCE_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
pub(super) struct DiagnosticSource {
    text: Arc<String>,
    lines: LineIndex,
    tree: Tree,
}

impl DiagnosticSource {
    pub(super) fn capture(source: &Source) -> Option<Self> {
        (source.text.len() <= MAX_RETAINED_SOURCE_BYTES).then(|| Self {
            text: source.text.clone(),
            lines: source.lines.clone(),
            tree: source.tree.clone(),
        })
    }

    fn bytes(&self, range: Range) -> Option<Bytes<usize>> {
        let start = self.lines.offset(&self.text, range.start)?;
        let end = self.lines.offset(&self.text, range.end)?;
        (start <= end
            && self.lines.position(&self.text, start) == range.start
            && self.lines.position(&self.text, end) == range.end)
            .then_some(start..end)
    }
}

#[derive(Clone)]
pub(super) struct DeliveredReport {
    report: Arc<DiagnosticReport>,
    source: Option<DiagnosticSource>,
}

impl DeliveredReport {
    pub(super) fn new(report: Arc<DiagnosticReport>, source: Option<DiagnosticSource>) -> Self {
        Self { report, source }
    }

    pub(super) fn retention<'a>(
        &'a self,
        key: &DiagnosticSnapshotKey,
        current: &'a Source,
    ) -> Option<Retention<'a>> {
        let previous = self.source.as_ref()?;
        let changed = Difference::between(&previous.text, &current.text);
        Some(Retention {
            previous,
            visible: &self.report,
            current,
            changed,
            reusable_inputs: self.report.snapshot.open_generation() == key.open_generation()
                && self.report.snapshot.same_semantic_inputs(key),
        })
    }
}

pub(super) struct Retention<'a> {
    previous: &'a DiagnosticSource,
    visible: &'a DiagnosticReport,
    current: &'a Source,
    changed: Difference,
    reusable_inputs: bool,
}

impl Retention<'_> {
    pub(super) fn dirty_range(&self) -> Option<Bytes<usize>> {
        (self.reusable_inputs && self.changed.has_edit()).then(|| self.changed.current.clone())
    }

    pub(super) fn unresolved_to_verify(
        &self,
        cancelled: &AtomicBool,
    ) -> Result<HashSet<(usize, usize)>> {
        let mut verify = HashSet::new();
        for (index, item) in self.visible.items.iter().enumerate() {
            if index & 127 == 0 {
                check_cancelled(cancelled)?;
            }
            if item.code == DiagnosticCode::NameUnresolved
                && let Some(previous) = self.previous.bytes(item.range)
                && let Some(current) = self.changed.map(previous)
            {
                verify.insert((current.start, current.end));
            }
        }
        Ok(verify)
    }

    pub(super) fn merge(
        &self,
        mut report: DiagnosticReport,
        not_verified: HashSet<(usize, usize)>,
        cancelled: &AtomicBool,
    ) -> Result<Option<DiagnosticReport>> {
        let mut retained = Vec::new();
        let visible: HashSet<_> = report
            .items
            .iter()
            .map(|item| item.visible_identity(item.range))
            .collect();
        for (index, item) in self.visible.items.iter().enumerate() {
            if index & 127 == 0 {
                check_cancelled(cancelled)?;
            }
            if item.code == DiagnosticCode::SyntaxError {
                // The syntax pass checks the whole file on every analyzed version.
                continue;
            }
            if item.code == DiagnosticCode::PackageLoad {
                // A graph-backed fast report already reevaluated these errors.
                if report.snapshot.partial {
                    return Ok(None);
                }
                continue;
            }
            let Some(old_bytes) = self.previous.bytes(item.range) else {
                return Ok(None);
            };
            if let Some(new_bytes) = self.changed.map(old_bytes.clone()) {
                let range = self.current.byte_range(new_bytes.clone());
                if visible.contains(&item.visible_identity(range)) {
                    continue;
                }
                let span = (new_bytes.start, new_bytes.end);
                if item.code == DiagnosticCode::NameUnresolved && !not_verified.contains(&span) {
                    continue;
                }
                if self.reusable_inputs
                    && (!self.changed.has_edit()
                        || self.same_function(old_bytes.clone(), &new_bytes))
                {
                    let mut item = item.clone();
                    item.range = range;
                    item.fix_data = DiagnosticFixData::None;
                    retained.push(item);
                    continue;
                }
            }
            if item.code == DiagnosticCode::NameUnresolved
                && !old_bytes.is_empty()
                && !self.current.text.contains(&self.previous.text[old_bytes])
            {
                // A removed spelling cannot produce the earlier missing-name finding.
                continue;
            }
            return Ok(None);
        }
        if retained.is_empty() {
            return Ok(Some(report));
        }
        report.items.extend(retained);
        Ok(Some(DiagnosticReport::at_stage(
            report.snapshot,
            report.items,
            report.diagnostic_stage,
            report.context_completeness,
        )))
    }

    fn same_function(&self, old_item: Bytes<usize>, new_item: &Bytes<usize>) -> bool {
        let Some(old_scope) = top_level_function(&self.previous.tree, &old_item) else {
            return false;
        };
        let Some(new_scope) = top_level_function(&self.current.tree, new_item) else {
            return false;
        };
        old_scope.kind_id() == new_scope.kind_id()
            && !old_scope.has_error()
            && !new_scope.has_error()
            && self.changed.map(old_scope.byte_range()).as_ref() == Some(&new_scope.byte_range())
    }
}

fn top_level_function<'a>(tree: &'a Tree, bytes: &Bytes<usize>) -> Option<Node<'a>> {
    let root = tree.root_node();
    let mut node = root.named_descendant_for_byte_range(bytes.start, bytes.end)?;
    while let Some(parent) = node.parent() {
        if parent == root {
            return matches!(node.kind(), "function_declaration" | "method_declaration")
                .then_some(node);
        }
        node = parent;
    }
    None
}

struct Difference {
    previous: Bytes<usize>,
    current: Bytes<usize>,
}

impl Difference {
    fn between(previous: &str, current: &str) -> Self {
        let start = previous.floor_char_boundary(
            previous
                .bytes()
                .zip(current.bytes())
                .take_while(|(a, b)| a == b)
                .count(),
        );
        let shared_tail = previous[start..]
            .bytes()
            .rev()
            .zip(current[start..].bytes().rev())
            .take_while(|(a, b)| a == b)
            .count();
        let previous_end = previous.ceil_char_boundary(previous.len() - shared_tail);
        let current_end = current.len() - (previous.len() - previous_end);
        Self {
            previous: start..previous_end,
            current: start..current_end,
        }
    }

    fn has_edit(&self) -> bool {
        !self.previous.is_empty() || !self.current.is_empty()
    }

    fn map(&self, bytes: Bytes<usize>) -> Option<Bytes<usize>> {
        if bytes.end <= self.previous.start {
            Some(bytes)
        } else if bytes.start >= self.previous.end {
            Some(
                (bytes.start - self.previous.end + self.current.end)
                    ..(bytes.end - self.previous.end + self.current.end),
            )
        } else {
            None
        }
    }
}
