use std::collections::{BTreeMap, HashSet};
use std::env;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::{
    go_environment::GoEnvironment,
    workspace::{CommandTrace, path_key},
};

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

pub(crate) fn folder_groups(folders: &[PathBuf], environment: &GoEnvironment) -> Vec<FolderGroup> {
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
            match folder_context(&folder, environment) {
                Ok(context) => groups.entry(context).or_default().push(folder),
                Err(error) => failures.push(FolderGroup::failed(folder, error)),
            }
        }
    }
    groups
        .into_values()
        .map(|folders| {
            let mut member_scopes = Vec::new();
            let roots = list_roots_for_folders(&folders, &mut member_scopes, environment);
            FolderGroup {
                folders,
                member_scopes,
                roots,
            }
        })
        .chain(failures)
        .collect()
}

fn folder_context(folder: &Path, environment: &GoEnvironment) -> Result<PathBuf> {
    let build: GoBuildEnvironment = go_json(
        folder,
        &["env", "-json", "GO111MODULE", "GOWORK", "GOMOD"],
        environment,
    )?;
    let work = Path::new(&build.work_file);
    let module = Path::new(&build.module_file);
    let context = if build.module_mode == "off" {
        folder
    } else if work.is_absolute() {
        work
    } else if module.is_absolute() && module.is_file() {
        module
    } else {
        folder
    };
    Ok(path_key(context))
}

fn list_roots_for_folders(
    folders: &[PathBuf],
    member_scopes: &mut Vec<PathBuf>,
    environment: &GoEnvironment,
) -> Result<Vec<PathBuf>> {
    if let [folder] = folders {
        return list_roots(folder, member_scopes, environment);
    }
    let mut seen = HashSet::new();
    let mut roots = Vec::new();
    for folder in folders {
        for root in list_roots(folder, member_scopes, environment)? {
            if seen.insert(path_key(&canonical_directory(&root)?)) {
                roots.push(root);
            }
        }
    }
    Ok(roots)
}

fn list_roots(
    root: &Path,
    member_scopes: &mut Vec<PathBuf>,
    environment: &GoEnvironment,
) -> Result<Vec<PathBuf>> {
    ensure!(
        root.is_dir(),
        "Go workspace root is not a directory: {}",
        root.display()
    );
    let single_root = || vec![root.to_owned()];
    let workspace_override = environment.get("GOWORK").unwrap_or_default();
    if environment
        .get("GO111MODULE")
        .is_some_and(|mode| mode == "off")
        || workspace_override == "off"
    {
        return Ok(single_root());
    }

    // A module directory stays scoped to itself even under an ancestor go.work.
    if (workspace_override.is_empty() || workspace_override == "auto")
        && root.join("go.mod").is_file()
        && !root.join("go.work").is_file()
    {
        return Ok(single_root());
    }

    let build: GoBuildEnvironment = go_json(
        root,
        &["env", "-json", "GO111MODULE", "GOWORK"],
        environment,
    )?;
    if build.module_mode == "off" || build.work_file.is_empty() || build.work_file == "off" {
        return Ok(single_root());
    }

    let work_file = Path::new(&build.work_file);
    let work_dir = work_file
        .parent()
        .with_context(|| format!("cannot determine the directory of {}", work_file.display()))?;
    let root_key = canonical_directory(root)?;
    let at_work_root = root_key == canonical_directory(work_dir)?;
    if !at_work_root && root.join("go.mod").is_file() {
        return Ok(single_root());
    }

    let workspace: GoWorkspace = go_json(root, &["work", "edit", "-json"], environment)?;
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

pub(crate) fn use_module_auto(root: &Path, environment: &GoEnvironment) -> Result<bool> {
    if !environment.allows_module_auto() {
        return Ok(false);
    }
    let build: GoBuildEnvironment = go_json(
        root,
        &["env", "-json", "GO111MODULE", "GOWORK", "GOMOD", "GOPATH"],
        environment,
    )?;
    Ok(build.use_module_auto(root))
}

fn go_json<T: DeserializeOwned>(
    root: &Path,
    args: &[&str],
    environment: &GoEnvironment,
) -> Result<T> {
    let command = format!("go {}", args.join(" "));
    let starting_go = || format!("starting {command} in {}", root.display());
    let mut trace = CommandTrace::new(
        if args.first() == Some(&"env") {
            "go env"
        } else {
            "go work"
        },
        root,
    );
    let mut process = environment.command("go").with_context(starting_go)?;
    process.args(args).current_dir(root).stdin(Stdio::null());
    let output = CommandTrace::output(&mut process, trace.as_mut()).with_context(starting_go)?;
    ensure!(
        output.status.success(),
        "{command} failed in {} ({}): {}",
        root.display(),
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let decoded = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("decoding {command} output in {}", root.display()))?;
    if let Some(trace) = &mut trace {
        trace.parsed(0, 0, 0);
    }
    Ok(decoded)
}

#[derive(Default, Deserialize)]
struct GoBuildEnvironment {
    #[serde(rename = "GO111MODULE")]
    module_mode: String,
    #[serde(rename = "GOWORK")]
    work_file: String,
    #[serde(default, rename = "GOMOD")]
    module_file: String,
    #[serde(default, rename = "GOPATH")]
    gopath: String,
}

impl GoBuildEnvironment {
    fn use_module_auto(&self, root: &Path) -> bool {
        if !self.module_mode.is_empty()
            || !(self.module_file.is_empty()
                || self.module_file.eq_ignore_ascii_case("NUL")
                || self.module_file == "/dev/null")
            || !(self.work_file.is_empty() || self.work_file == "off")
        {
            return false;
        }
        let root = directory_key(root);
        env::split_paths(&self.gopath)
            .filter(|entry| entry.is_absolute())
            .any(|entry| {
                let entry = directory_key(&entry);
                root.starts_with(&entry) || entry.parent() == Some(root.as_path())
            })
    }
}

fn directory_key(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return path_key(&canonical);
    }
    // GOPATH entries need not exist yet. Normalize their absolute path components.
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    path_key(&normalized)
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
        let groups = folder_groups(
            &[
                broken.clone(),
                healthy.clone(),
                absent.clone(),
                member.clone(),
            ],
            &GoEnvironment::capture(),
        );
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

    #[test]
    fn workspace_gopath_fallback_compares_path_components_across_entries() {
        let project = TestProject::new();
        let first = project.root.join("first");
        let second = project.root.join("second");
        let build = GoBuildEnvironment {
            gopath: env::join_paths([&first, &second])
                .unwrap()
                .into_string()
                .unwrap(),
            ..GoBuildEnvironment::default()
        };
        for matched in [&first, &second, &second.join("src/pkg"), &project.root] {
            assert!(build.use_module_auto(matched), "{}", matched.display());
        }
        for unmatched in [
            &project.root.join("first-extra"),
            project.root.parent().unwrap(),
        ] {
            assert!(!build.use_module_auto(unmatched), "{}", unmatched.display());
        }
        assert!(build.use_module_auto(&first.join("missing/../src/")));
    }

    #[test]
    fn workspace_gopath_fallback_requires_no_effective_module_or_workspace() {
        let project = TestProject::new();
        let mut build = GoBuildEnvironment {
            gopath: project.root.to_str().unwrap().to_owned(),
            ..GoBuildEnvironment::default()
        };
        for no_module in ["", "NUL", "nuL", "/dev/null"] {
            build.module_file = no_module.into();
            for no_workspace in ["", "off"] {
                build.work_file = no_workspace.into();
                assert!(build.use_module_auto(&project.root));
            }
        }
        for mode in ["on", "off", "auto"] {
            build.module_mode = mode.into();
            assert!(!build.use_module_auto(&project.root));
        }
        build.module_mode.clear();
        build.module_file = project.root.join("go.mod").to_str().unwrap().into();
        assert!(!build.use_module_auto(&project.root));
        build.module_file.clear();
        build.work_file = project.root.join("go.work").to_str().unwrap().into();
        assert!(!build.use_module_auto(&project.root));
    }

    #[test]
    fn workspace_gopath_fallback_honors_persisted_goenv_module_mode() {
        let project = TestProject::new();
        let goenv = project.write("goenv", "GO111MODULE=on\n");
        let environment = GoEnvironment::capture().with_overrides(BTreeMap::from([
            ("GO111MODULE".into(), String::new()),
            ("GOENV".into(), goenv.to_str().unwrap().into()),
            ("GOPATH".into(), project.root.to_str().unwrap().into()),
            ("GOWORK".into(), "off".into()),
        ]));
        assert!(environment.allows_module_auto());
        assert!(!use_module_auto(&project.root, &environment).unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn workspace_gopath_fallback_splits_windows_paths_and_normalizes_case() {
        let build = GoBuildEnvironment {
            gopath: "C:\\mock\\first;D:\\mock\\second\\".into(),
            ..GoBuildEnvironment::default()
        };
        assert!(build.use_module_auto(Path::new(r"c:\MOCK\FIRST\src")));
        assert!(build.use_module_auto(Path::new(r"d:\mock\SECOND")));
        assert!(!build.use_module_auto(Path::new(r"d:\mock\second-extra")));
    }
}
