use std::collections::{BTreeMap, HashMap};
use std::env;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::thread;

use anyhow::{Context, Result, bail, ensure};
use lsp_types::TextEdit;

use crate::go_environment::GoEnvironment;
use crate::semantic::snapshot::DocumentSnapshot;
use crate::workspace::path_key;

pub(crate) fn format_document(
    document: &DocumentSnapshot<'_>,
    workdir: Option<&Path>,
    environment: &GoEnvironment,
    cache: &mut GofmtCache,
) -> Result<Vec<TextEdit>> {
    if document.root_node().has_error() {
        return Ok(Vec::new());
    }
    let executable = cache.resolve(workdir, environment, |dir| sdk_root(dir, environment));
    let mut command = environment
        .command(&executable)
        .with_context(|| format!("finding gofmt ({})", executable.display()))?;
    if let Some(dir) = workdir {
        command.current_dir(dir);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting gofmt ({})", executable.display()))?;
    let mut stdin = child.stdin.take().expect("gofmt stdin is piped");
    let text = document.text();
    let (written, output) = thread::scope(|scope| {
        let writer = scope.spawn(move || stdin.write_all(text.as_bytes()));
        let output = child.wait_with_output();
        (writer.join().expect("gofmt stdin writer panicked"), output)
    });
    let output = output.context("waiting for gofmt")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // A Go line directive can give a stdin parse error a different filename.
        if stderr.lines().any(|line| {
            line.split_once(": ")
                .and_then(|(position, _)| position.rsplit_once(':'))
                .is_some_and(|(_, number)| number.parse::<u32>().is_ok())
        }) {
            return Ok(Vec::new());
        }
        bail!("gofmt failed ({}): {}", output.status, stderr.trim_end());
    }
    written.context("writing document to gofmt")?;
    formatting_edits(
        document,
        String::from_utf8(output.stdout).context("decoding gofmt output")?,
    )
}

#[derive(Eq, Hash, PartialEq)]
struct GofmtContext {
    workdir: PathBuf,
    environment: Arc<BTreeMap<OsString, OsString>>,
}

#[derive(Default)]
pub(crate) struct GofmtCache {
    paths: HashMap<GofmtContext, PathBuf>,
}

impl GofmtCache {
    pub(crate) fn clear(&mut self) {
        self.paths.clear();
    }

    fn resolve(
        &mut self,
        workdir: Option<&Path>,
        environment: &GoEnvironment,
        query_root: impl FnOnce(&Path) -> Option<PathBuf>,
    ) -> PathBuf {
        if let Some(path) = environment.get("GOROOT").map(Path::new).and_then(sdk_gofmt) {
            return path;
        }
        let dir = workdir.unwrap_or(Path::new("."));
        let context = GofmtContext {
            workdir: path_key(&dir.canonicalize().unwrap_or_else(|_| dir.to_owned())),
            environment: Arc::clone(environment.variables()),
        };
        self.paths
            .entry(context)
            .or_insert_with_key(|context| {
                query_root(&context.workdir)
                    .as_deref()
                    .and_then(sdk_gofmt)
                    .unwrap_or_else(|| PathBuf::from("gofmt"))
            })
            .clone()
    }
}

fn sdk_gofmt(root: &Path) -> Option<PathBuf> {
    let path = root
        .join("bin")
        .join(format!("gofmt{}", env::consts::EXE_SUFFIX));
    (root.is_absolute() && path.is_file()).then_some(path)
}

fn sdk_root(dir: &Path, environment: &GoEnvironment) -> Option<PathBuf> {
    let output = environment
        .command("go")
        .ok()?
        .args(["env", "GOROOT"])
        .current_dir(dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = std::str::from_utf8(&output.stdout)
        .ok()?
        .trim_end_matches(['\r', '\n']);
    Some(PathBuf::from(root))
}

pub(super) fn formatting_edits(
    document: &DocumentSnapshot<'_>,
    mut formatted: String,
) -> Result<Vec<TextEdit>> {
    let original = document.text();
    if formatted == original {
        return Ok(Vec::new());
    }
    ensure!(
        !formatted.trim().is_empty() || original.trim().is_empty(),
        "gofmt returned an empty result for a non-empty document"
    );
    if original.matches("\r\n").count() > original.bytes().filter(|&b| b == b'\n').count() / 2 {
        formatted = formatted.replace("\r\n", "\n").replace('\n', "\r\n");
        if formatted == original {
            return Ok(Vec::new());
        }
    }

    let mut start = matching_bytes(original.chars(), formatted.chars());
    if splits_crlf(original, start) || splits_crlf(&formatted, start) {
        start -= 1;
    }
    let mut suffix = matching_bytes(
        original[start..].chars().rev(),
        formatted[start..].chars().rev(),
    );
    // LSP has no position between the two bytes of a CRLF.
    if splits_crlf(original, original.len() - suffix)
        || splits_crlf(&formatted, formatted.len() - suffix)
    {
        suffix -= 1;
    }
    Ok(vec![TextEdit::new(
        document.byte_range(start..original.len() - suffix),
        formatted[start..formatted.len() - suffix].to_owned(),
    )])
}

fn matching_bytes(left: impl Iterator<Item = char>, right: impl Iterator<Item = char>) -> usize {
    left.zip(right)
        .take_while(|(a, b)| a == b)
        .map(|(c, _)| c.len_utf8())
        .sum()
}

fn splits_crlf(text: &str, offset: usize) -> bool {
    text[..offset].ends_with('\r') && text[offset..].starts_with('\n')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestProject;

    #[test]
    fn gofmt_resolution_is_cached_per_directory_and_effective_environment() {
        let project = TestProject::new();
        let tool = format!("bin/gofmt{}", env::consts::EXE_SUFFIX);
        let first = project.write(&format!("first/{tool}"), "");
        let second = project.write(&format!("second/{tool}"), "");
        let startup = GoEnvironment::capture();
        let configured = |overrides: &[(&str, &str)]| {
            startup.with_overrides(
                std::iter::once(("GOROOT", ""))
                    .chain(overrides.iter().copied())
                    .map(|(key, value)| (key.to_owned(), value.to_owned()))
                    .collect(),
            )
        };
        let environment = configured(&[]);
        let dir = Some(project.root.as_path());
        let mut cache = GofmtCache::default();
        let resolve_once = |_: &Path| -> Option<PathBuf> { panic!("unexpected go env GOROOT") };

        assert_eq!(
            cache.resolve(dir, &environment, |dir| {
                assert_eq!(dir, path_key(&project.root));
                Some(project.root.join("first"))
            }),
            first
        );
        assert_eq!(
            cache.resolve(
                Some(&project.root.join("first/..")),
                &configured(&[]),
                resolve_once
            ),
            first
        );

        let explicit = configured(&[("GOROOT", project.root.join("second").to_str().unwrap())]);
        assert_eq!(cache.resolve(dir, &explicit, resolve_once), second);
        let missing = configured(&[("GOROOT", project.root.join("missing").to_str().unwrap())]);
        assert_eq!(cache.resolve(dir, &missing, |_| None), Path::new("gofmt"));
        assert_eq!(
            cache.resolve(dir, &missing, resolve_once),
            Path::new("gofmt")
        );

        assert_eq!(
            cache.resolve(Some(&project.root.join("second")), &environment, |_| Some(
                project.root.join("second")
            )),
            second
        );
        for (key, value) in [
            ("PATH", project.root.to_str().unwrap()),
            ("GOTOOLCHAIN", "test-toolchain"),
            ("GOENV", "test-config"),
            ("GOWORK", "test-workspace"),
        ] {
            let changed = configured(&[(key, value)]);
            assert_eq!(
                cache.resolve(dir, &changed, |_| Some(project.root.join("second"))),
                second
            );
            assert_eq!(cache.resolve(dir, &changed, resolve_once), second);
        }
        assert_eq!(cache.resolve(dir, &environment, resolve_once), first);
        cache.clear();
        assert_eq!(
            cache.resolve(dir, &environment, |_| Some(project.root.join("second"))),
            second
        );
    }
}
