use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;

use lsp_types::{Diagnostic, NumberOrString, Position, Range};
use serde::{Serialize, Serializer};

use crate::workspace::DiagnosticFileStamp;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum DiagnosticDocumentRevision {
    Open {
        version: i32,
        open_generation: u64,
        fingerprint: u64,
        content_revision: u64,
    },
    Disk {
        stamp: DiagnosticFileStamp,
        epoch: u64,
    },
}

impl DiagnosticDocumentRevision {
    fn version(self) -> Option<i32> {
        match self {
            Self::Open { version, .. } => Some(version),
            Self::Disk { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct DiagnosticSnapshotKey {
    pub(crate) path_key: PathBuf,
    pub(crate) version: Option<i32>,
    pub(super) revision: DiagnosticDocumentRevision,
    pub(crate) partial: bool,
    pub(super) semantic_inputs: u64,
}

impl DiagnosticSnapshotKey {
    pub(super) fn new(
        path_key: PathBuf,
        revision: DiagnosticDocumentRevision,
        semantic_inputs: Option<u64>,
    ) -> Self {
        Self {
            path_key,
            version: revision.version(),
            revision,
            partial: semantic_inputs.is_none(),
            semantic_inputs: semantic_inputs.unwrap_or_default(),
        }
    }

    pub(crate) fn result_id(&self, content_hash: u64) -> String {
        format!("{:016x}", hash((self, content_hash)))
    }

    pub(crate) fn same_document(&self, other: &Self) -> bool {
        self.path_key == other.path_key && self.revision == other.revision
    }

    pub(crate) fn open_generation(&self) -> Option<u64> {
        match self.revision {
            DiagnosticDocumentRevision::Open {
                open_generation, ..
            } => Some(open_generation),
            DiagnosticDocumentRevision::Disk { .. } => None,
        }
    }

    pub(super) fn same_semantic_inputs(&self, other: &Self) -> bool {
        !self.partial && !other.partial && self.semantic_inputs == other.semantic_inputs
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum DiagnosticCode {
    SyntaxError,
    PackageLoad,
    ImportUnused,
    ImportInvalid,
    ImportSelf,
    ImportCycle,
    ImportDuplicateAlias,
    NameUnresolved,
    NameUnexported,
    TypeUnresolved,
    FieldOrMethodUnresolved,
    NameRedeclared,
    VarUnused,
    ShortVarNoNewVars,
    ConstReassign,
    CallNonFunction,
    CallArity,
    ReturnArity,
    TypeMismatch,
    InterfaceNotImplemented,
    ConditionNotBool,
    NilWithoutType,
    CompositeLiteralUnknownField,
    DuplicateSwitchCase,
    BlankAsValue,
}

impl DiagnosticCode {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::SyntaxError => "syntax.error",
            Self::PackageLoad => "package.load",
            Self::ImportUnused => "import.unused",
            Self::ImportInvalid => "import.invalid",
            Self::ImportSelf => "import.self",
            Self::ImportCycle => "import.cycle",
            Self::ImportDuplicateAlias => "import.duplicate_alias",
            Self::NameUnresolved => "name.unresolved",
            Self::NameUnexported => "name.unexported",
            Self::TypeUnresolved => "type.unresolved",
            Self::FieldOrMethodUnresolved => "member.unresolved",
            Self::NameRedeclared => "name.redeclared",
            Self::VarUnused => "var.unused",
            Self::ShortVarNoNewVars => "shortvar.no_new_vars",
            Self::ConstReassign => "const.reassign",
            Self::CallNonFunction => "call.non_function",
            Self::CallArity => "call.arity",
            Self::ReturnArity => "return.arity",
            Self::TypeMismatch => "type.mismatch",
            Self::InterfaceNotImplemented => "type.interface_not_implemented",
            Self::ConditionNotBool => "type.condition_not_bool",
            Self::NilWithoutType => "type.nil_without_type",
            Self::CompositeLiteralUnknownField => "literal.unknown_field",
            Self::DuplicateSwitchCase => "switch.duplicate_case",
            Self::BlankAsValue => "expr.blank_as_value",
        }
    }
}

impl Serialize for DiagnosticCode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[expect(
    dead_code,
    reason = "the diagnostic model represents all LSP severities"
)]
pub(crate) enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum DiagnosticTag {
    Unnecessary,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the diagnostic model supports the LSP deprecated tag"
        )
    )]
    Deprecated,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum DiagnosticFixData {
    None,
    RemoveUnusedImport {
        import_path: String,
        spec_range: Range,
    },
    ImportMissingPackage {
        qualifier: String,
        ident_range: Range,
    },
    ReplaceUnusedVarWithBlank {
        name: String,
        ident_range: Range,
    },
    ChangeShortDeclToAssign {
        operator_range: Range,
    },
}

impl DiagnosticFixData {
    fn sort_key(&self) -> (u8, &str, Position, Position) {
        let (kind, name, range) = match self {
            Self::RemoveUnusedImport {
                import_path,
                spec_range,
            } => (0, import_path.as_str(), *spec_range),
            Self::ImportMissingPackage {
                qualifier,
                ident_range,
            } => (1, qualifier.as_str(), *ident_range),
            Self::ReplaceUnusedVarWithBlank { name, ident_range } => {
                (2, name.as_str(), *ident_range)
            }
            Self::ChangeShortDeclToAssign { operator_range } => (3, "", *operator_range),
            Self::None => (4, "", Range::default()),
        };
        (kind, name, range.start, range.end)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct DiagnosticItem {
    pub(crate) code: DiagnosticCode,
    pub(crate) severity: DiagnosticSeverity,
    pub(crate) range: Range,
    pub(crate) message: String,
    pub(crate) tags: Vec<DiagnosticTag>,
    pub(crate) fix_data: DiagnosticFixData,
    pub(crate) source: &'static str,
}

impl DiagnosticItem {
    pub(crate) fn new(
        code: DiagnosticCode,
        severity: DiagnosticSeverity,
        range: Range,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            severity,
            range,
            message: message.into(),
            tags: Vec::new(),
            fix_data: DiagnosticFixData::None,
            source: "rs-gopls",
        }
    }

    pub(crate) fn to_lsp(&self) -> Diagnostic {
        let severity = match self.severity {
            DiagnosticSeverity::Error => lsp_types::DiagnosticSeverity::ERROR,
            DiagnosticSeverity::Warning => lsp_types::DiagnosticSeverity::WARNING,
            DiagnosticSeverity::Information => lsp_types::DiagnosticSeverity::INFORMATION,
            DiagnosticSeverity::Hint => lsp_types::DiagnosticSeverity::HINT,
        };
        let tags = (!self.tags.is_empty()).then(|| {
            self.tags
                .iter()
                .map(|tag| match tag {
                    DiagnosticTag::Unnecessary => lsp_types::DiagnosticTag::UNNECESSARY,
                    DiagnosticTag::Deprecated => lsp_types::DiagnosticTag::DEPRECATED,
                })
                .collect()
        });
        Diagnostic {
            range: self.range,
            severity: Some(severity),
            code: Some(NumberOrString::String(self.code.as_str().into())),
            source: Some(self.source.into()),
            message: self.message.clone(),
            tags,
            ..Diagnostic::default()
        }
    }

    fn identity(&self) -> (Position, Position, &'static str, &str) {
        (
            self.range.start,
            self.range.end,
            self.code.as_str(),
            &self.message,
        )
    }

    pub(super) fn visible_identity(
        &self,
        range: Range,
    ) -> (
        Range,
        DiagnosticCode,
        DiagnosticSeverity,
        &str,
        &[DiagnosticTag],
        &str,
    ) {
        (
            range,
            self.code,
            self.severity,
            &self.message,
            &self.tags,
            self.source,
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum DiagnosticStage {
    Stage1,
    Stage2,
}

impl DiagnosticStage {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Stage1 => "stage1",
            Self::Stage2 => "stage2",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ContextCompleteness {
    Partial,
    Complete,
}

impl ContextCompleteness {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Partial => "partial",
            Self::Complete => "full",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DiagnosticProvenance {
    pub(crate) job: u64,
    pub(crate) run: u64,
    pub(crate) lane: &'static str,
    pub(crate) stage: DiagnosticStage,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DiagnosticReport {
    pub(crate) snapshot: DiagnosticSnapshotKey,
    pub(crate) result_id: String,
    pub(crate) items: Vec<DiagnosticItem>,
    pub(crate) diagnostic_stage: DiagnosticStage,
    pub(crate) context_completeness: ContextCompleteness,
    pub(crate) provenance: Option<DiagnosticProvenance>,
}

impl DiagnosticReport {
    pub(crate) fn new(
        snapshot: DiagnosticSnapshotKey,
        items: Vec<DiagnosticItem>,
        is_partial: bool,
    ) -> Self {
        let (stage, context) = if is_partial {
            (DiagnosticStage::Stage1, ContextCompleteness::Partial)
        } else {
            (DiagnosticStage::Stage2, ContextCompleteness::Complete)
        };
        Self::at_stage(snapshot, items, stage, context)
    }

    pub(crate) fn at_stage(
        snapshot: DiagnosticSnapshotKey,
        mut items: Vec<DiagnosticItem>,
        diagnostic_stage: DiagnosticStage,
        context_completeness: ContextCompleteness,
    ) -> Self {
        items.sort_unstable_by(|left, right| {
            left.identity()
                .cmp(&right.identity())
                .then_with(|| left.severity.cmp(&right.severity))
                .then_with(|| left.fix_data.sort_key().cmp(&right.fix_data.sort_key()))
                .then_with(|| right.tags.cmp(&left.tags))
                .then_with(|| left.source.cmp(right.source))
        });
        items.dedup_by(|left, right| left.identity() == right.identity());
        let mut visible_content = DefaultHasher::new();
        items.len().hash(&mut visible_content);
        for item in &items {
            item.visible_identity(item.range).hash(&mut visible_content);
        }
        let result_id = snapshot.result_id(visible_content.finish());
        Self {
            snapshot,
            result_id,
            items,
            diagnostic_stage,
            context_completeness,
            provenance: None,
        }
    }

    pub(crate) fn same_visible_content(&self, other: &Self) -> bool {
        self.items.len() == other.items.len()
            && self.items.iter().zip(&other.items).all(|(left, right)| {
                left.visible_identity(left.range) == right.visible_identity(right.range)
            })
    }

    pub(crate) fn to_lsp_diagnostics(&self) -> Vec<Diagnostic> {
        self.items.iter().map(DiagnosticItem::to_lsp).collect()
    }

    pub(crate) fn items_overlapping(&self, range: Range) -> impl Iterator<Item = &DiagnosticItem> {
        self.items.iter().filter(move |item| {
            // Empty ranges represent cursor positions or missing tokens.
            range.start == item.range.start
                || (range.start < item.range.end && item.range.start < range.end)
        })
    }
}

fn hash(value: impl Hash) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic::Engine;
    use crate::semantic::diagnostics::snapshot_key_for_path;
    use crate::test_support::TestProject;

    fn snapshot_key() -> DiagnosticSnapshotKey {
        DiagnosticSnapshotKey::new(
            "sample.go".into(),
            DiagnosticDocumentRevision::Open {
                version: 7,
                open_generation: 1,
                fingerprint: 1,
                content_revision: 1,
            },
            Some(2),
        )
    }

    fn columns(start: u32, end: u32) -> Range {
        Range::new(Position::new(0, start), Position::new(0, end))
    }

    fn syntax(range: Range, message: &str) -> DiagnosticItem {
        DiagnosticItem::new(
            DiagnosticCode::SyntaxError,
            DiagnosticSeverity::Error,
            range,
            message,
        )
    }

    #[test]
    fn sorts_and_deduplicates_diagnostics_independently_of_input_order() {
        let range = columns(3, 7);
        let untagged = DiagnosticItem::new(
            DiagnosticCode::ImportUnused,
            DiagnosticSeverity::Error,
            range,
            "unused import",
        );
        let mut unused = untagged.clone();
        unused.tags.push(DiagnosticTag::Unnecessary);
        unused.fix_data = DiagnosticFixData::RemoveUnusedImport {
            import_path: "fmt".into(),
            spec_range: range,
        };
        let expected = vec![
            syntax(columns(1, 8), "earlier start"),
            syntax(columns(3, 5), "earlier end"),
            unused,
            syntax(range, "missing ')'"),
            syntax(range, "unexpected token"),
            syntax(
                Range::new(Position::new(1, 0), Position::new(1, 1)),
                "later line",
            ),
        ];
        let mut input = expected.clone();
        input.rotate_left(2);
        input.extend([untagged, expected[3].clone()]);

        let report = DiagnosticReport::new(snapshot_key(), input.clone(), false);
        assert_eq!(report.items, expected);
        input.reverse();
        assert_eq!(report, DiagnosticReport::new(snapshot_key(), input, false));
        assert_eq!(
            report,
            DiagnosticReport::new(snapshot_key(), expected, false)
        );
    }

    #[test]
    fn result_id_includes_snapshot_and_report_content() {
        let snapshot = snapshot_key();
        let items = vec![syntax(columns(0, 1), "unexpected token")];
        let report = DiagnosticReport::new(snapshot.clone(), items.clone(), false);

        let partial = DiagnosticReport::new(snapshot.clone(), items.clone(), true);
        assert_eq!(report.result_id, partial.result_id);
        assert_ne!(report.diagnostic_stage, partial.diagnostic_stage);

        let mut changed = items.clone();
        changed[0].severity = DiagnosticSeverity::Warning;
        let changed = DiagnosticReport::new(snapshot.clone(), changed, false);
        assert_ne!(report.result_id, changed.result_id);

        let next = DiagnosticSnapshotKey {
            semantic_inputs: 3,
            ..snapshot
        };
        let next = DiagnosticReport::new(next, items, false);
        assert_ne!(report.result_id, next.result_id);
    }

    #[test]
    fn snapshot_key_checks_document_identity_and_revision() {
        let project = TestProject::new();
        let text = "package sample\n";
        let path = project.write("sample.go", text);
        let other_path = project.write("other.go", text);
        let mut engine = Engine::new(project.root.clone());
        engine.open(path.clone(), text.into(), 7).unwrap();
        engine.open(other_path.clone(), text.into(), 7).unwrap();

        let key = {
            let snapshot = engine.snapshot();
            let key = snapshot_key_for_path(&snapshot, &path).unwrap();
            assert_eq!(key, snapshot_key_for_path(&snapshot, &path).unwrap());
            assert_ne!(key, snapshot_key_for_path(&snapshot, &other_path).unwrap());
            key
        };

        for (version, text) in [(8, text), (7, "package sample\nvar changed int\n")] {
            engine.open(path.clone(), text.into(), version).unwrap();
            assert_ne!(
                key,
                snapshot_key_for_path(&engine.snapshot(), &path).unwrap()
            );
        }

        engine.close(&path);
        let snapshot = engine.snapshot();
        let disk_key = snapshot_key_for_path(&snapshot, &path).unwrap();
        assert_ne!(key, disk_key);
        assert_eq!(disk_key.version, None);
        assert_eq!(disk_key, snapshot_key_for_path(&snapshot, &path).unwrap());
        project.write("sample.go", "package changed\n");
        assert_ne!(disk_key, snapshot_key_for_path(&snapshot, &path).unwrap());
    }

    #[test]
    fn converts_model_items_to_lsp_diagnostics() {
        let range = columns(8, 13);
        let mut item = DiagnosticItem::new(
            DiagnosticCode::ImportUnused,
            DiagnosticSeverity::Warning,
            range,
            "unused import",
        );
        assert_eq!(item.to_lsp().tags, None);
        item.tags = vec![DiagnosticTag::Unnecessary, DiagnosticTag::Deprecated];
        item.fix_data = DiagnosticFixData::RemoveUnusedImport {
            import_path: "fmt".into(),
            spec_range: range,
        };
        let expected = Diagnostic {
            range,
            severity: Some(lsp_types::DiagnosticSeverity::WARNING),
            code: Some(NumberOrString::String("import.unused".into())),
            source: Some("rs-gopls".into()),
            message: "unused import".into(),
            tags: Some(vec![
                lsp_types::DiagnosticTag::UNNECESSARY,
                lsp_types::DiagnosticTag::DEPRECATED,
            ]),
            ..Diagnostic::default()
        };
        assert_eq!(item.to_lsp(), expected);
        assert_eq!(
            serde_json::to_string(&item.code).unwrap(),
            "\"import.unused\""
        );
        let report = DiagnosticReport::new(snapshot_key(), vec![item], false);
        assert_eq!(report.to_lsp_diagnostics(), vec![expected]);
    }

    #[test]
    fn overlap_respects_exclusive_ends_and_matches_zero_width_ranges() {
        let report = DiagnosticReport::new(
            snapshot_key(),
            vec![
                syntax(columns(2, 5), "span"),
                syntax(columns(6, 6), "point"),
            ],
            false,
        );
        for (selection, expected) in [
            (columns(0, 1), &[][..]),
            (columns(0, 2), &[]),
            (columns(2, 2), &["span"]),
            (columns(3, 3), &["span"]),
            (columns(4, 7), &["span", "point"]),
            (columns(5, 5), &[]),
            (columns(5, 6), &[]),
            (columns(6, 6), &["point"]),
            (columns(6, 7), &["point"]),
            (columns(7, 8), &[]),
        ] {
            let matches: Vec<_> = report
                .items_overlapping(selection)
                .map(|item| item.message.as_str())
                .collect();
            assert_eq!(matches, expected, "{selection:?}");
        }
    }
}
