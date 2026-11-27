use std::collections::{BTreeMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::workspace::path_key;

pub(crate) struct FolderGroup {
    pub folders: Vec<PathBuf>,
    pub member_scopes: Vec<PathBuf>,
    pub roots: Result<Vec<PathBuf>>,
}

impl FolderGroup {
    fn failed(folder: PathBuf, error: anyhow::Error) -> Self {
        Self {
            folders: vec![folder],
            member_scopes: Vec::new(),
            roots: Err(error),
        }
    }
}

pub(crate) fn folder_groups(folders: &[PathBuf]) -> Vec<FolderGroup> {
    let mut unique = Vec::new();
    let mut failures = Vec::new();
    let mut seen = HashSet::new();
    for folder in folders {
        let absolute = std::path::absolute(folder)
            .with_context(|| format!("locating Go workspace folder {}", folder.display()));
        if !seen.insert(path_key(absolute.as_deref().unwrap_or(folder))) {
            continue;
        }
        match absolute {
            Ok(folder) => unique.push(folder),
            Err(error) => failures.push(FolderGroup::failed(folder.clone(), error)),
        }
    }

    let mut groups = BTreeMap::<PathBuf, Vec<PathBuf>>::new();
    // Only explicit folders and their go.work members participate in discovery.
    if let [folder] = unique.as_slice() {
        groups.insert(path_key(folder), unique);
    } else {
        for folder in unique {
            match folder_context(&folder) {
                Ok(context) => groups.entry(context).or_default().push(folder),
                Err(error) => failures.push(FolderGroup::failed(folder, error)),
            }
        }
    }
    groups
        .into_values()
        .map(|folders| {
            let mut member_scopes = Vec::new();
            let roots = list_roots_for_folders(&folders, &mut member_scopes);
            FolderGroup {
                folders,
                member_scopes,
                roots,
            }
        })
        .chain(failures)
        .collect()
}

fn folder_context(folder: &Path) -> Result<PathBuf> {
    let environment: GoEnvironment =
        go_json(folder, &["env", "-json", "GO111MODULE", "GOWORK", "GOMOD"])?;
    let work = Path::new(&environment.work_file);
    let context = if environment.module_mode == "off" {
        folder
    } else if work.is_absolute() {
        work
    } else if environment.module_file.is_absolute() && environment.module_file.is_file() {
        &environment.module_file
    } else {
        folder
    };
    Ok(path_key(context))
}

fn list_roots_for_folders(
    folders: &[PathBuf],
    member_scopes: &mut Vec<PathBuf>,
) -> Result<Vec<PathBuf>> {
    if let [folder] = folders {
        return list_roots(folder, member_scopes);
    }
    let mut seen = HashSet::new();
    let mut roots = Vec::new();
    for folder in folders {
        for root in list_roots(folder, member_scopes)? {
            if seen.insert(path_key(&canonical_directory(&root)?)) {
                roots.push(root);
            }
        }
    }
    Ok(roots)
}

fn list_roots(root: &Path, member_scopes: &mut Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    ensure!(
        root.is_dir(),
        "Go workspace root is not a directory: {}",
        root.display()
    );
    let single_root = || vec![root.to_owned()];
    let workspace_override = env::var_os("GOWORK").unwrap_or_default();
    if env::var_os("GO111MODULE").is_some_and(|mode| mode == "off") || workspace_override == "off" {
        return Ok(single_root());
    }

    // A module directory stays scoped to itself even under an ancestor go.work.
    if (workspace_override.is_empty() || workspace_override == "auto")
        && root.join("go.mod").is_file()
        && !root.join("go.work").is_file()
    {
        return Ok(single_root());
    }

    let environment: GoEnvironment = go_json(root, &["env", "-json", "GO111MODULE", "GOWORK"])?;
    if environment.module_mode == "off"
        || environment.work_file.is_empty()
        || environment.work_file == "off"
    {
        return Ok(single_root());
    }

    let work_file = Path::new(&environment.work_file);
    let work_dir = work_file
        .parent()
        .with_context(|| format!("cannot determine the directory of {}", work_file.display()))?;
    let root_key = canonical_directory(root)?;
    let at_work_root = root_key == canonical_directory(work_dir)?;
    if !at_work_root && root.join("go.mod").is_file() {
        return Ok(single_root());
    }

    let workspace: GoWorkspace = go_json(root, &["work", "edit", "-json"])?;
    let mut seen = HashSet::new();
    let mut roots = Vec::new();
    let mut failure = None;
    for member in workspace.members.unwrap_or_default() {
        let directory = work_dir.join(member.disk_path);
        let key = match canonical_directory(&directory) {
            Ok(key) => key,
            Err(error) => {
                failure.get_or_insert(error);
                continue;
            }
        };
        if !at_work_root && root_key.starts_with(&key) {
            // Opening a subdirectory of a used module limits discovery to that subtree.
            return failure.map_or_else(|| Ok(single_root()), Err);
        }
        if (at_work_root || key.starts_with(&root_key)) && seen.insert(key) {
            roots.push(directory);
        }
    }

    if let Some(error) = failure {
        // A valid member may be outside the explicit folder containing go.work.
        // Its source documents can carry this group's discovery error.
        member_scopes.extend(seen);
        return Err(error);
    }
    if roots.is_empty() {
        ensure!(
            !at_work_root,
            "Go workspace {} has no used modules",
            work_file.display()
        );
        return Ok(single_root());
    }
    Ok(roots)
}

fn canonical_directory(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("locating Go workspace directory {}", path.display()))
}

fn go_json<T: DeserializeOwned>(root: &Path, args: &[&str]) -> Result<T> {
    let command = format!("go {}", args.join(" "));
    let output = Command::new("go")
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("starting {command} in {}", root.display()))?;
    ensure!(
        output.status.success(),
        "{command} failed in {} ({}): {}",
        root.display(),
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    serde_json::from_slice(&output.stdout)
        .with_context(|| format!("decoding {command} output in {}", root.display()))
}

#[derive(Deserialize)]
struct GoEnvironment {
    #[serde(rename = "GO111MODULE")]
    module_mode: String,
    #[serde(rename = "GOWORK")]
    work_file: String,
    #[serde(default, rename = "GOMOD")]
    module_file: PathBuf,
}

#[derive(Deserialize)]
struct GoWorkspace {
    #[serde(rename = "Use")]
    members: Option<Vec<WorkspaceMember>>,
}

#[derive(Deserialize)]
struct WorkspaceMember {
    #[serde(rename = "DiskPath")]
    disk_path: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestProject;

    #[test]
    fn workspace_discovery_preserves_healthy_groups_and_failed_folder_membership() {
        let project = TestProject::new();
        project.write("healthy/go.mod", "module example.test/healthy\n\ngo 1.22\n");
        project.write(
            "broken/go.work",
            "go 1.22\n\nuse (\n./member\n./missing\n../outside\n)\n",
        );
        project.write(
            "broken/member/go.mod",
            "module example.test/member\n\ngo 1.22\n",
        );
        project.write("outside/go.mod", "module example.test/outside\n\ngo 1.22\n");
        let [broken, healthy, absent, member] =
            ["broken", "healthy", "absent", "broken/member"].map(|name| project.root.join(name));
        let groups = folder_groups(&[
            broken.clone(),
            healthy.clone(),
            absent.clone(),
            member.clone(),
        ]);
        assert_eq!(groups.len(), 3);

        let group = |folder: &Path| {
            groups
                .iter()
                .find(|group| group.folders[0] == folder)
                .unwrap()
        };
        assert_eq!(group(&healthy).roots.as_deref().unwrap(), [healthy.clone()]);
        let broken_group = group(&broken);
        assert_eq!(broken_group.folders, [broken, member]);
        assert!(
            broken_group
                .member_scopes
                .iter()
                .any(|dir| path_key(dir) == path_key(&project.root.join("outside")))
        );
        let error = broken_group.roots.as_ref().unwrap_err().to_string();
        assert!(error.contains("locating Go workspace directory") && error.ends_with("missing"));
        let error = group(&absent).roots.as_ref().unwrap_err().to_string();
        assert!(error.contains("starting go env") && error.ends_with("absent"));
    }
}
