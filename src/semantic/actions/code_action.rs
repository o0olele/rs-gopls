use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;
use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, Range, TextEdit, Uri, WorkspaceEdit,
};

use super::{ImportEditError, ImportOperation, import_edits};
use crate::semantic::diagnostics::{
    DiagnosticCode, DiagnosticFixData, DiagnosticItem, DiagnosticReport, snapshot_key,
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
) -> Result<Vec<CodeActionOrCommand>> {
    let organize = wants_kind(only, &CodeActionKind::SOURCE_ORGANIZE_IMPORTS);
    let quickfix = wants_kind(only, &CodeActionKind::QUICKFIX);
    if !organize && (!quickfix || report.is_none()) {
        return Ok(Vec::new());
    }

    let source = document.version().is_some().then(|| &**document.indexed());
    let workspace = snapshot
        .diagnostic_workspace(document.path(), source)
        .filter(|workspace| workspace.total_load_failure().is_none());
    let report = report.filter(|report| {
        snapshot_key(snapshot, document).is_ok_and(|current| current == report.snapshot)
    });
    let local_prefixes: Vec<_> = workspace
        .into_iter()
        .flat_map(Workspace::module_paths)
        .map(String::as_str)
        .collect();
    let organize_edits = |unused_paths: &[&str]| {
        action_import_edits(
            document,
            ImportOperation::OrganizeImports {
                unused_paths,
                local_prefixes: &local_prefixes,
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
        if !edits.is_empty() && seen.insert((kind.clone(), title.clone())) {
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
        let unused_paths: Vec<_> = report
            .into_iter()
            .flat_map(|r| &r.items)
            .filter_map(|item| match (item.code, &item.fix_data) {
                (
                    DiagnosticCode::ImportUnused,
                    DiagnosticFixData::RemoveUnusedImport { import_path, .. },
                ) => Some(import_path.as_str()),
                _ => None,
            })
            .collect();
        add(
            "Organize Imports".into(),
            organize_edits(&unused_paths)?,
            None,
        );
    }
    if quickfix && let Some(report) = report {
        let owner = workspace
            .and_then(|workspace| snapshot.diagnostic_package(workspace, document.path(), source));
        let mut searched = HashSet::new();
        for item in report.items_overlapping(range) {
            let mut fix = |title, edits| add(title, edits, Some(item));
            match (item.code, &item.fix_data) {
                (
                    DiagnosticCode::ImportUnused,
                    DiagnosticFixData::RemoveUnusedImport { import_path, .. },
                ) => {
                    fix(
                        format!("Remove unused import \"{import_path}\""),
                        organize_edits(&[import_path])?,
                    );
                }
                (
                    DiagnosticCode::NameUnresolved,
                    DiagnosticFixData::ImportMissingPackage { qualifier, .. },
                ) if searched.insert(qualifier) => {
                    for path in import_candidates(qualifier, workspace, owner, document.path()) {
                        let edits = action_import_edits(
                            document,
                            ImportOperation::AddImport { path, alias: None },
                        )?;
                        fix(format!("Import \"{path}\""), edits);
                    }
                }
                (
                    DiagnosticCode::VarUnused,
                    DiagnosticFixData::ReplaceUnusedVarWithBlank { name, ident_range },
                ) => {
                    fix(
                        format!("Replace unused variable '{name}' with '_'"),
                        replace_unused_variable(document, *ident_range),
                    );
                }
                (
                    DiagnosticCode::ShortVarNoNewVars,
                    DiagnosticFixData::ChangeShortDeclToAssign { operator_range },
                ) => {
                    fix(
                        "Replace ':=' with '='".into(),
                        vec![TextEdit::new(*operator_range, "=".into())],
                    );
                }
                _ => {}
            }
        }
    }
    Ok(actions)
}

fn replace_unused_variable(document: &DocumentSnapshot<'_>, range: Range) -> Vec<TextEdit> {
    let mut edits = vec![TextEdit::new(range, "_".into())];
    if let Some(name) = node_at_range(document.indexed(), range)
        && let Some(left) = name.parent()
        && let Some(decl) = left.parent()
        && decl.kind() == "short_var_declaration"
        && decl.child_by_field_name("left") == Some(left)
        && left.named_children(&mut left.walk()).all(|other| {
            other == name || other.is_extra() || node_text(other, document.text()) == "_"
        })
        && let Some(operator) = decl.children(&mut decl.walk()).find(|n| n.kind() == ":=")
    {
        edits.push(TextEdit::new(
            document.byte_range(operator.byte_range()),
            "=".into(),
        ));
    }
    edits
}

fn wants_kind(only: Option<&[CodeActionKind]>, target: &CodeActionKind) -> bool {
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

pub(crate) fn needs_diagnostics(only: Option<&[CodeActionKind]>) -> bool {
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
