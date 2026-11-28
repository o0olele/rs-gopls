use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, Diagnostic, NumberOrString, Range, TextEdit,
    Uri, WorkspaceEdit,
};

use super::{ImportEditError, ImportOperation, import_edits};
use crate::semantic::diagnostics::{
    DiagnosticCode, DiagnosticFixData, DiagnosticItem, DiagnosticReport, DiagnosticStage,
    local_imports, local_variable_fixes, snapshot_key,
};
use crate::semantic::snapshot::{DocumentSnapshot, EngineSnapshot};
use crate::semantic::source::{node_at_range, node_text};
use crate::workspace::{Package, Workspace, path_key};

pub(crate) fn code_actions(
    snapshot: &mut EngineSnapshot<'_>,
    document: &DocumentSnapshot<'_>,
    uri: &Uri,
    range: Range,
    only: Option<&[CodeActionKind]>,
    report: Option<&DiagnosticReport>,
    context: &[Diagnostic],
) -> Result<Vec<CodeActionOrCommand>> {
    let organize = wants_kind(only, &CodeActionKind::SOURCE_ORGANIZE_IMPORTS);
    let quickfix = wants_kind(only, &CodeActionKind::QUICKFIX);
    if (!organize && !quickfix) || exact_range(document, range).is_none() {
        return Ok(Vec::new());
    }

    let source = document.version().is_some().then(|| &**document.indexed());
    let workspace = snapshot
        .diagnostic_workspace(document.path(), source)
        .filter(|workspace| workspace.total_load_failure().is_none());
    let report = report.filter(|report| {
        snapshot_key(snapshot, document).is_ok_and(|current| current == report.snapshot)
    });
    let mut selected: Vec<_> = report
        .filter(|_| quickfix)
        .into_iter()
        .flat_map(|r| r.items_overlapping(range))
        .filter(|item| exact_range(document, item.range).is_some())
        .collect();
    let hints: Vec<_> = context
        .iter()
        .filter(|hint| {
            quickfix && overlaps(range, hint.range) && exact_range(document, hint.range).is_some()
        })
        .filter_map(|hint| hint_code(hint).map(|code| (code, hint)))
        .filter(|(code, hint)| {
            !selected.iter().any(|item| {
                item.code == *code
                    && item.range == hint.range
                    && item.message == hint.message
                    && item.fix_data != DiagnosticFixData::None
            })
        })
        .collect();
    if !organize && selected.is_empty() && hints.is_empty() {
        return Ok(Vec::new());
    }
    let complete = report.filter(|r| r.diagnostic_stage == DiagnosticStage::Stage2);
    let import_hint = hints
        .iter()
        .any(|(code, _)| *code == DiagnosticCode::ImportUnused);
    let check_import_usage = organize && complete.is_none()
        || import_hint
        || selected.iter().any(|item| {
            item.code == DiagnosticCode::ImportUnused && item.fix_data == DiagnosticFixData::None
        });
    let imports = if organize
        || import_hint
        || selected
            .iter()
            .any(|item| item.code == DiagnosticCode::ImportUnused)
    {
        local_imports(snapshot, document, check_import_usage)?
    } else {
        Default::default()
    };
    let variable_hints: Vec<_> = selected
        .iter()
        .filter_map(|item| variable_target(document, item))
        .chain(hints.iter().filter_map(|(code, hint)| {
            matches!(
                code,
                DiagnosticCode::VarUnused | DiagnosticCode::ShortVarNoNewVars
            )
            .then_some((*code, hint.range))
        }))
        .collect();
    let variables = local_variable_fixes(document, &variable_hints)?;
    let unused_by_range: HashMap<_, _> = imports
        .unused
        .iter()
        .map(|item| (item.range, item))
        .collect();
    selected.extend(hints.into_iter().filter_map(|(code, hint)| {
        let verified = if code == DiagnosticCode::ImportUnused {
            unused_by_range.get(&hint.range).copied()
        } else {
            variables.get(&(code, hint.range)).map(|(item, _)| item)
        };
        verified.filter(|item| item.message == hint.message)
    }));
    let local_prefixes: Vec<_> = workspace
        .into_iter()
        .flat_map(Workspace::module_paths)
        .map(String::as_str)
        .collect();
    let organize_edits = |unused_specs: &[usize]| {
        action_import_edits(
            document,
            ImportOperation::OrganizeImports {
                unused_specs,
                local_prefixes: &local_prefixes,
                package_names: &imports.package_names,
            },
        )
    };
    let mut actions = Vec::new();
    let mut seen = HashSet::new();
    let mut add = |title: String, edits: Vec<TextEdit>, diagnostic: Option<&DiagnosticItem>| {
        let kind = if diagnostic.is_some() {
            CodeActionKind::QUICKFIX
        } else {
            CodeActionKind::SOURCE_ORGANIZE_IMPORTS
        };
        let site = diagnostic
            .filter(|item| {
                matches!(
                    item.code,
                    DiagnosticCode::ImportUnused
                        | DiagnosticCode::VarUnused
                        | DiagnosticCode::ShortVarNoNewVars
                )
            })
            .map(|item| item.range);
        if !edits.is_empty() && seen.insert((kind.clone(), title.clone(), site)) {
            actions.push(
                CodeAction {
                    title,
                    kind: Some(kind),
                    diagnostics: diagnostic.map(|item| vec![item.to_lsp()]),
                    edit: Some(WorkspaceEdit {
                        changes: Some([(uri.clone(), edits)].into()),
                        ..WorkspaceEdit::default()
                    }),
                    ..CodeAction::default()
                }
                .into(),
            );
        }
    };

    if organize {
        let unused_specs: Vec<_> = complete
            .into_iter()
            .flat_map(|report| &report.items)
            .chain(&imports.unused)
            .filter_map(|item| unused_spec(document, item))
            .collect();
        add(
            "Organize Imports".into(),
            organize_edits(&unused_specs)?,
            None,
        );
    }
    if quickfix {
        let owner = workspace
            .and_then(|workspace| snapshot.diagnostic_package(workspace, document.path(), source));
        let mut searched = HashSet::new();
        for item in selected {
            let mut fix = |title, edits| add(title, edits, Some(item));
            match (item.code, &item.fix_data) {
                (DiagnosticCode::ImportUnused, _) => {
                    let verified = unused_by_range
                        .get(&item.range)
                        .copied()
                        .filter(|found| found.message == item.message);
                    if let Some((path, offset)) = unused_import_fix(document, item)
                        .or_else(|| verified.and_then(|found| unused_import_fix(document, found)))
                    {
                        fix(
                            format!("Remove unused import \"{path}\""),
                            organize_edits(&[offset])?,
                        );
                    }
                }
                (
                    DiagnosticCode::NameUnresolved,
                    DiagnosticFixData::ImportMissingPackage {
                        qualifier,
                        ident_range,
                    },
                ) if overlaps(item.range, *ident_range)
                    && selector_qualifier(document, *ident_range, qualifier)
                    && searched.insert(qualifier) =>
                {
                    for path in import_candidates(qualifier, workspace, owner, document.path()) {
                        let edits = action_import_edits(
                            document,
                            ImportOperation::AddImport { path, alias: None },
                        )?;
                        fix(format!("Import \"{path}\""), edits);
                    }
                }
                (DiagnosticCode::VarUnused | DiagnosticCode::ShortVarNoNewVars, _) => {
                    if let Some((verified, edits)) =
                        variable_target(document, item).and_then(|key| variables.get(&key))
                        && (item.fix_data != DiagnosticFixData::None
                            || item.message == verified.message)
                    {
                        fix(variable_fix_title(verified, edits), edits.clone());
                    }
                }
                _ => {}
            }
        }
    }
    Ok(actions)
}

fn hint_code(hint: &Diagnostic) -> Option<DiagnosticCode> {
    if hint.source.as_deref() != Some("rs-gopls") {
        return None;
    }
    let Some(NumberOrString::String(code)) = &hint.code else {
        return None;
    };
    [
        DiagnosticCode::ImportUnused,
        DiagnosticCode::VarUnused,
        DiagnosticCode::ShortVarNoNewVars,
    ]
    .into_iter()
    .find(|known| known.as_str() == code)
}

fn variable_target(
    document: &DocumentSnapshot<'_>,
    item: &DiagnosticItem,
) -> Option<(DiagnosticCode, Range)> {
    let target = match (item.code, &item.fix_data) {
        (
            DiagnosticCode::VarUnused,
            DiagnosticFixData::ReplaceUnusedVarWithBlank { ident_range, .. },
        ) => *ident_range,
        (
            DiagnosticCode::ShortVarNoNewVars,
            DiagnosticFixData::ChangeShortDeclToAssign { operator_range },
        ) => *operator_range,
        (
            DiagnosticCode::VarUnused | DiagnosticCode::ShortVarNoNewVars,
            DiagnosticFixData::None,
        ) => item.range,
        _ => return None,
    };
    (overlaps(item.range, target) && exact_range(document, target).is_some())
        .then_some((item.code, target))
}

fn variable_fix_title(item: &DiagnosticItem, edits: &[TextEdit]) -> String {
    match &item.fix_data {
        DiagnosticFixData::ReplaceUnusedVarWithBlank { name, .. }
            if edits.first().is_some_and(|edit| edit.new_text.is_empty()) =>
        {
            format!("Remove unused variable '{name}'")
        }
        DiagnosticFixData::ReplaceUnusedVarWithBlank { name, .. } => {
            format!("Replace unused variable '{name}' with '_'")
        }
        _ => "Replace ':=' with '='".into(),
    }
}

fn unused_spec(document: &DocumentSnapshot<'_>, item: &DiagnosticItem) -> Option<usize> {
    unused_import_fix(document, item).map(|(_, offset)| offset)
}

fn unused_import_fix<'a>(
    document: &DocumentSnapshot<'_>,
    item: &'a DiagnosticItem,
) -> Option<(&'a str, usize)> {
    let (
        DiagnosticCode::ImportUnused,
        DiagnosticFixData::RemoveUnusedImport {
            import_path,
            spec_range,
        },
    ) = (item.code, &item.fix_data)
    else {
        return None;
    };
    let bytes = exact_range(document, *spec_range)?;
    let diagnostic = exact_range(document, item.range)?;
    document
        .indexed()
        .diagnostic_imports
        .iter()
        .find(|spec| {
            spec.spec == bytes
                && spec.path.as_ref() == Some(import_path)
                && spec.spec.start <= diagnostic.start
                && diagnostic.end <= spec.spec.end
                && !matches!(spec.alias.as_deref(), Some("." | "_"))
                && import_path != "C"
        })
        .map(|spec| (import_path.as_str(), spec.spec.start))
}

fn selector_qualifier(document: &DocumentSnapshot<'_>, range: Range, qualifier: &str) -> bool {
    exact_range(document, range).is_some()
        && node_at_range(document.indexed(), range).is_some_and(|node| {
            node_text(node, document.text()) == qualifier
                && node.parent().is_some_and(|parent| match parent.kind() {
                    "qualified_type" => parent.child_by_field_name("package") == Some(node),
                    "selector_expression" => parent.child_by_field_name("operand") == Some(node),
                    _ => false,
                })
        })
}

fn overlaps(range: Range, diagnostic: Range) -> bool {
    range.start == diagnostic.start
        || (range.start < diagnostic.end && diagnostic.start < range.end)
}

pub(crate) fn exact_range(
    document: &DocumentSnapshot<'_>,
    range: Range,
) -> Option<std::ops::Range<usize>> {
    let start = document.offset(range.start)?;
    let end = document.offset(range.end)?;
    (start <= end && document.byte_range(start..end) == range).then_some(start..end)
}

pub(crate) fn wants_kind(only: Option<&[CodeActionKind]>, target: &CodeActionKind) -> bool {
    only.is_none_or(|kinds| {
        kinds.iter().any(|kind| {
            kind.as_str().is_empty()
                || target
                    .as_str()
                    .strip_prefix(kind.as_str())
                    .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with('.'))
        })
    })
}

pub(crate) fn supports_code_actions(only: Option<&[CodeActionKind]>) -> bool {
    wants_kind(only, &CodeActionKind::QUICKFIX)
        || wants_kind(only, &CodeActionKind::SOURCE_ORGANIZE_IMPORTS)
}

fn action_import_edits(
    document: &DocumentSnapshot<'_>,
    operation: ImportOperation<'_>,
) -> Result<Vec<TextEdit>> {
    match import_edits(document, operation) {
        Ok(edits) => Ok(edits),
        Err(
            ImportEditError::UnsafeSyntaxTree
            | ImportEditError::MissingPackageClause
            | ImportEditError::InvalidImportPath
            | ImportEditError::ConflictingBinding { .. },
        ) => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

// go list -deps only includes standard packages already reachable from the workspace.
const COMMON_STANDARD_IMPORTS: &[&str] = &[
    "bufio",
    "bytes",
    "cmp",
    "context",
    "crypto/rand",
    "crypto/sha256",
    "database/sql",
    "embed",
    "encoding/base64",
    "encoding/binary",
    "encoding/csv",
    "encoding/hex",
    "encoding/json",
    "encoding/xml",
    "errors",
    "flag",
    "fmt",
    "go/ast",
    "go/format",
    "go/parser",
    "go/token",
    "go/types",
    "html/template",
    "io",
    "io/fs",
    "io/ioutil",
    "log",
    "log/slog",
    "maps",
    "math",
    "math/big",
    "math/rand",
    "net",
    "net/http",
    "net/http/httptest",
    "net/url",
    "os",
    "os/exec",
    "os/signal",
    "path",
    "path/filepath",
    "reflect",
    "regexp",
    "runtime",
    "slices",
    "sort",
    "strconv",
    "strings",
    "sync",
    "sync/atomic",
    "testing",
    "text/template",
    "time",
    "unicode",
    "unicode/utf8",
    "unsafe",
];

fn import_candidates<'w>(
    qualifier: &str,
    workspace: Option<&'w Workspace>,
    owner: Option<&'w Package>,
    file: &Path,
) -> Vec<&'w str> {
    if matches!(qualifier, "_" | "main") || qualifier.ends_with("_test") {
        return Vec::new();
    }
    let mut candidates: Vec<_> = COMMON_STANDARD_IMPORTS
        .iter()
        .copied()
        .filter(|path| path.rsplit('/').next() == Some(qualifier))
        .map(|path| (0, path))
        .collect();
    let mut blocked = HashSet::new();
    if let Some(workspace) = workspace {
        // Importing the current package or one of its transitive importers creates a cycle.
        let mut importers: Vec<_> = owner.into_iter().collect();
        while let Some(package) = importers.pop() {
            if blocked.insert(package.id.as_str()) {
                importers.extend(workspace.direct_importers(package));
            }
        }
        let owner_path = owner.map(Package::import_path);
        let file = path_key(file);
        for package in workspace
            .navigation_packages()
            .filter(|p| p.name == qualifier)
        {
            let path = package.import_path();
            if owner.is_some_and(|from| !workspace.matches_import_variant(from, path, &package.id))
                || package.load_error
                || package.id.contains(" [")
                || path == "builtin"
                || path == "C"
                || path.split('/').any(|part| part == "vendor")
                || Some(path) == owner_path
                || blocked.contains(package.id.as_str())
            {
                continue;
            }
            let local = workspace
                .module_paths()
                .iter()
                .any(|prefix| Path::new(path).starts_with(prefix))
                || (workspace.module_paths().is_empty() && workspace.is_project_package(package));
            let standard = !local && !path.split('/').next().unwrap_or(path).contains('.');
            if internal_visible_from(path, package, owner_path, &file, standard) {
                let rank = if standard {
                    0
                } else if local {
                    1
                } else {
                    2
                };
                candidates.push((rank, path));
            }
        }
        if let Some(path) = owner_path {
            blocked.insert(path);
        }
    }
    candidates.sort_unstable();
    let mut seen = HashSet::new();
    candidates
        .into_iter()
        .map(|(_, path)| path)
        .filter(|path| !blocked.contains(path) && seen.insert(*path))
        .take(8)
        .collect()
}

fn internal_visible_from(
    path: &str,
    package: &Package,
    owner: Option<&str>,
    file: &Path,
    standard: bool,
) -> bool {
    let path = Path::new(path);
    let Some(internal) = path
        .ancestors()
        .find(|ancestor| ancestor.ends_with("internal"))
    else {
        return true;
    };
    let parent = internal.parent().expect("internal has a relative parent");
    if !standard
        && !parent.as_os_str().is_empty()
        && let Some(owner) = owner
    {
        return Path::new(owner).starts_with(parent);
    }
    package
        .directory_and_name()
        .0
        .ancestors()
        .find(|dir| dir.ends_with("internal"))
        .and_then(Path::parent)
        .is_some_and(|root| file.starts_with(root))
}
