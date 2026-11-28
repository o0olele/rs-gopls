use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result};

use super::{check_load_cancelled, path_key};
use crate::go_environment::GoEnvironment;

// A rename needs a complete reference set even when the editor supplies no
// file watcher. Capture before go list so a newly added importer, package or
// build-selected source cannot silently fall outside the installed graph.
pub(super) struct PackageInputs {
    folders: Vec<PathBuf>,
    configuration: BTreeMap<PathBuf, u64>,
    files: BTreeMap<PathBuf, u64>,
}

impl PackageInputs {
    // Module and work files can live above an editor folder. Take their
    // fingerprints before discovering the build groups and Go project roots.
    pub(super) fn configuration(
        folders: &[PathBuf],
        environment: &GoEnvironment,
        cancelled: Option<&AtomicBool>,
    ) -> Result<Self> {
        let folders = folders
            .iter()
            .map(std::path::absolute)
            .collect::<std::io::Result<Vec<_>>>()?;
        let mut configuration = BTreeMap::new();
        for dir in folders.iter().flat_map(|folder| folder.ancestors()) {
            check_load_cancelled(cancelled)?;
            for name in ["go.work", "go.work.sum", "go.mod", "go.sum"] {
                let path = dir.join(name);
                if path.is_file() {
                    record(&mut configuration, &path)?;
                }
            }
        }
        if let Some(path) = environment.get("GOWORK").map(PathBuf::from)
            && path.is_absolute()
            && path.is_file()
        {
            record(&mut configuration, &path)?;
            let mut sum = path.into_os_string();
            sum.push(".sum");
            let sum = PathBuf::from(sum);
            if sum.is_file() {
                record(&mut configuration, &sum)?;
            }
        }
        Ok(Self {
            folders,
            configuration,
            files: BTreeMap::new(),
        })
    }

    pub(super) fn with_sources(
        mut self,
        roots: &[PathBuf],
        cancelled: Option<&AtomicBool>,
    ) -> Result<Self> {
        let mut pending = roots.to_vec();
        let mut seen = HashSet::new();
        while let Some(dir) = pending.pop() {
            check_load_cancelled(cancelled)?;
            if !seen.insert(path_key(&dir)) {
                continue;
            }
            let entries =
                fs::read_dir(&dir).with_context(|| format!("listing {}", dir.display()))?;
            for entry in entries {
                check_load_cancelled(cancelled)?;
                let entry = entry?;
                let path = entry.path();
                let filename = entry.file_name();
                let name = filename.to_string_lossy();
                if name.starts_with(['.', '_']) {
                    continue;
                }
                if entry.file_type()?.is_dir() {
                    if matches!(name.as_ref(), "vendor" | "testdata") {
                        continue;
                    }
                    pending.push(path);
                } else if path.extension().is_some_and(|x| x == "go")
                    || matches!(
                        name.as_ref(),
                        "go.mod" | "go.sum" | "go.work" | "go.work.sum"
                    )
                {
                    record(&mut self.files, &path)?;
                }
            }
        }
        Ok(self)
    }

    pub(super) fn current(&self, roots: &[PathBuf], environment: &GoEnvironment) -> Result<bool> {
        let current = Self::configuration(&self.folders, environment, None)?;
        Ok(current.configuration == self.configuration
            && current.with_sources(roots, None)?.files == self.files)
    }
}

fn record(files: &mut BTreeMap<PathBuf, u64>, path: &Path) -> Result<()> {
    anyhow::ensure!(path.is_file(), "not a regular Go input: {}", path.display());
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut hash = DefaultHasher::new();
    bytes.hash(&mut hash);
    files.insert(path_key(path), hash.finish());
    Ok(())
}
