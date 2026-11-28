use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, TryLockError};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::{
    go_environment::GoEnvironment,
    workspace::{CommandTrace, check_load_cancelled, command_output, path_key},
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

pub(crate) fn folder_groups(
    folders: &[PathBuf],
    environment: &GoEnvironment,
    cancelled: Option<&AtomicBool>,
) -> Result<Vec<FolderGroup>> {
    let mut unique = Vec::new();
    let mut failures = Vec::new();
    let mut seen = HashSet::new();
    for folder in folders {
        check_load_cancelled(cancelled)?;
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
            let context = folder_context(&folder, environment, cancelled);
            check_load_cancelled(cancelled)?;
            match context {
                Ok(context) => groups.entry(context).or_default().push(folder),
                Err(error) => failures.push(FolderGroup::failed(folder, error)),
            }
        }
    }
    let mut discovered = Vec::with_capacity(groups.len() + failures.len());
    for folders in groups.into_values() {
        let mut member_scopes = Vec::new();
        let roots = list_roots_for_folders(&folders, &mut member_scopes, environment, cancelled);
        check_load_cancelled(cancelled)?;
        discovered.push(FolderGroup {
            folders,
            member_scopes,
            roots,
        });
    }
    discovered.extend(failures);
    Ok(discovered)
}

fn folder_context(
    folder: &Path,
    environment: &GoEnvironment,
    cancelled: Option<&AtomicBool>,
) -> Result<PathBuf> {
    let build = go_build_environment(folder, environment, cancelled)?;
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
    cancelled: Option<&AtomicBool>,
) -> Result<Vec<PathBuf>> {
    if let [folder] = folders {
        return list_roots(folder, member_scopes, environment, cancelled);
    }
    let mut seen = HashSet::new();
    let mut roots = Vec::new();
    for folder in folders {
        for root in list_roots(folder, member_scopes, environment, cancelled)? {
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
    cancelled: Option<&AtomicBool>,
) -> Result<Vec<PathBuf>> {
    check_load_cancelled(cancelled)?;
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

    let build = go_build_environment(root, environment, cancelled)?;
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

    let workspace: GoWorkspace = go_json(root, &["work", "edit", "-json"], environment, cancelled)?;
    let mut seen = HashSet::new();
    let mut roots = Vec::new();
    let mut failure = None;
    for member in workspace.members.unwrap_or_default() {
        check_load_cancelled(cancelled)?;
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

pub(crate) fn use_module_auto(
    root: &Path,
    environment: &GoEnvironment,
    cancelled: Option<&AtomicBool>,
) -> Result<bool> {
    if !environment.allows_module_auto() {
        return Ok(false);
    }
    let build = go_build_environment(root, environment, cancelled)?;
    Ok(build.use_module_auto(root))
}

type GoBuildEnvironmentSlot = Arc<Mutex<Option<CachedGoBuildEnvironment>>>;

#[derive(Default)]
pub(crate) struct GoBuildEnvironmentCache {
    directories: Mutex<HashMap<PathBuf, GoBuildEnvironmentSlot>>,
    #[cfg(test)]
    probes: std::sync::atomic::AtomicUsize,
}

struct CachedGoBuildEnvironment {
    inputs: GoBuildEnvironmentInputs,
    build: Arc<GoBuildEnvironment>,
}

fn go_build_environment(
    root: &Path,
    environment: &GoEnvironment,
    cancelled: Option<&AtomicBool>,
) -> Result<Arc<GoBuildEnvironment>> {
    check_load_cancelled(cancelled)?;
    let cache = &environment.workspace_build_environments;
    let query = || {
        #[cfg(test)]
        cache
            .probes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        go_json(
            root,
            &["env", "-json", "GO111MODULE", "GOWORK", "GOMOD", "GOPATH"],
            environment,
            cancelled,
        )
        .map(Arc::new)
    };
    let Some(directory) = effective_directory(root, environment) else {
        // Let Go report an invalid or inaccessible command directory as usual.
        return query();
    };
    let slot = Arc::clone(
        cache
            .directories
            .lock()
            .unwrap()
            .entry(path_key(&directory))
            .or_default(),
    );
    // Share the command for this directory without holding the cache-wide lock
    // or making another workspace generation wait on a cancelled generation.
    let mut cached = loop {
        match slot.try_lock() {
            Ok(cached) => break cached,
            Err(TryLockError::WouldBlock) => {
                check_load_cancelled(cancelled)?;
                thread::sleep(Duration::from_millis(5));
            }
            Err(TryLockError::Poisoned(error)) => break error.into_inner(),
        }
    };
    check_load_cancelled(cancelled)?;
    let inputs = GoBuildEnvironmentInputs::capture(&directory, environment);
    if let Some(cached) = cached.as_ref()
        && inputs.as_ref() == Some(&cached.inputs)
    {
        return Ok(Arc::clone(&cached.build));
    }
    *cached = None;
    let build = query()?;
    check_load_cancelled(cancelled)?;
    if let Some(inputs) = inputs
        && inputs.covers(&build)
        && effective_directory(root, environment).as_ref() == Some(&directory)
        && GoBuildEnvironmentInputs::capture(&directory, environment).as_ref() == Some(&inputs)
    {
        *cached = Some(CachedGoBuildEnvironment {
            inputs,
            build: Arc::clone(&build),
        });
    }
    Ok(build)
}

fn effective_directory(root: &Path, environment: &GoEnvironment) -> Option<PathBuf> {
    let canonical = root.canonicalize().ok()?;
    if cfg!(windows) {
        // Go uses the logical command directory on Windows.
        return std::path::absolute(root).ok();
    }
    // On Unix Go honors PWD if it refers to the command directory.
    let pwd = environment.get("PWD").map(Path::new);
    if let Some(pwd) = pwd.filter(|pwd| pwd.is_absolute())
        && pwd.canonicalize().ok().as_ref() == Some(&canonical)
    {
        return Some(pwd.to_owned());
    }
    Some(canonical)
}

#[derive(PartialEq, Eq)]
struct GoBuildEnvironmentInputs(BTreeMap<PathBuf, Option<Vec<u8>>>);

impl GoBuildEnvironmentInputs {
    fn capture(directory: &Path, environment: &GoEnvironment) -> Option<Self> {
        let mut inputs = Self(BTreeMap::new());
        let user_config = match user_go_config(environment) {
            Some(path) => inputs.read(directory.join(path))?,
            None => None,
        };
        let configured = |name| {
            environment
                .get(name)
                .filter(|value| !value.is_empty())
                .map(Path::new)
                .or_else(|| go_config_value(user_config, name).map(Path::new))
        };
        let work_override = configured("GOWORK").map(Path::to_owned);
        let sdk_config = match configured("GOROOT").map(|path| directory.join(path).join("go.env"))
        {
            Some(path) => inputs.read(path)?,
            None => None,
        };
        if let Some(work_file) = work_override
            .as_deref()
            .or_else(|| go_config_value(sdk_config, "GOWORK").map(Path::new))
            .filter(|path| path.is_absolute())
            .map(Path::to_owned)
        {
            inputs.read(work_file)?;
        }
        // An ancestor manifest can be added, removed or rewritten without
        // changing the configured Go environment or the command directory.
        for ancestor in directory.ancestors() {
            for name in ["go.mod", "go.work"] {
                inputs.read(ancestor.join(name))?;
            }
        }
        Some(inputs)
    }

    fn read(&mut self, path: PathBuf) -> Option<Option<&[u8]>> {
        let bytes = match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => Some(fs::read(&path).ok()?),
            Ok(metadata) if metadata.is_dir() => None,
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            _ => return None,
        };
        Some(self.0.entry(path_key(&path)).or_insert(bytes).as_deref())
    }

    fn covers(&self, build: &GoBuildEnvironment) -> bool {
        [&build.work_file, &build.module_file]
            .into_iter()
            .map(Path::new)
            .filter(|path| path.is_absolute() && *path != Path::new("/dev/null"))
            .all(|path| self.0.contains_key(&path_key(path)))
    }
}

fn user_go_config(environment: &GoEnvironment) -> Option<PathBuf> {
    if let Some(path) = environment.get("GOENV").filter(|path| !path.is_empty()) {
        let disabled = path == "off"
            || path == "/dev/null" && cfg!(unix)
            || path.eq_ignore_ascii_case(std::ffi::OsStr::new("NUL")) && cfg!(windows);
        return (!disabled).then(|| PathBuf::from(path));
    }
    let get = |key| {
        environment
            .get(key)
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
    };
    let directory = if cfg!(windows) {
        get("APPDATA")
    } else if cfg!(any(target_os = "macos", target_os = "ios")) {
        get("HOME").map(|home| home.join("Library/Application Support"))
    } else {
        match get("XDG_CONFIG_HOME") {
            Some(path) => path.is_absolute().then_some(path),
            None => get("HOME").map(|home| home.join(".config")),
        }
    }?;
    Some(directory.join("go/env"))
}

fn go_config_value<'a>(bytes: Option<&'a [u8]>, name: &str) -> Option<&'a str> {
    std::str::from_utf8(bytes?)
        .ok()?
        .split_terminator('\n')
        .rev()
        .filter_map(|line| line.split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value)
        .filter(|value| !value.is_empty())
}

fn go_json<T: DeserializeOwned>(
    root: &Path,
    args: &[&str],
    environment: &GoEnvironment,
    cancelled: Option<&AtomicBool>,
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
    let output =
        command_output(&mut process, trace.as_mut(), cancelled).with_context(starting_go)?;
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
            None,
        )
        .unwrap();
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
    fn workspace_go_env_is_shared_by_folder_discovery_and_package_listing() {
        let project = TestProject::new();
        project.write("go.work", "go 1.22\nuse ./member\n");
        project.write("member/go.mod", "module example.test/member\n\ngo 1.22\n");
        project.write(
            "member/main.go",
            "package member\nfunc Value() int { return 1 }\n",
        );
        let environment = GoEnvironment::capture().with_overrides(BTreeMap::from([
            ("GO111MODULE".into(), String::new()),
            ("GOENV".into(), "off".into()),
            (
                "GOPATH".into(),
                project.root.join("gopath").to_str().unwrap().into(),
            ),
            ("GOTOOLCHAIN".into(), "local".into()),
            ("GOWORK".into(), String::new()),
        ]));
        let folders = [project.root.clone(), project.root.join("member")];
        let load = |environment: &GoEnvironment| {
            crate::workspace::Workspace::load_roots_in_environment(&folders, false, environment)
                .unwrap()
        };
        let probes = || {
            environment
                .workspace_build_environments
                .probes
                .load(std::sync::atomic::Ordering::Relaxed)
        };

        let workspace = load(&environment);
        assert!(
            workspace
                .package_of_file(&project.root.join("member/main.go"))
                .is_some()
        );
        // Both folders get a context, and root discovery and listing reuse it.
        assert_eq!(probes(), 2);
        project.write(
            "member/main.go",
            "package member\nfunc Value() int { return 2 }\n",
        );
        load(&environment.clone());
        assert_eq!(probes(), 2);

        project.write("go.work", "go 1.22\nuse ./member\n// changed\n");
        load(&environment);
        assert_eq!(probes(), 4);

        fs::remove_file(project.root.join("go.work")).unwrap();
        let groups = folder_groups(&folders, &environment, None).unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(probes(), 6);
    }

    #[test]
    fn workspace_go_env_reloads_changed_gopath_and_persisted_goenv() {
        let project = TestProject::new();
        let goenv = project.write("settings", "GO111MODULE=on\n");
        let overrides = BTreeMap::from([
            ("GO111MODULE".into(), String::new()),
            ("GOENV".into(), goenv.to_str().unwrap().into()),
            ("GOPATH".into(), project.root.to_str().unwrap().into()),
            ("GOTOOLCHAIN".into(), "local".into()),
            ("GOWORK".into(), "off".into()),
        ]);
        let environment = GoEnvironment::capture().with_overrides(overrides.clone());
        let allows_auto = |environment: &GoEnvironment| {
            use_module_auto(&project.root, environment, None).unwrap()
        };
        assert!(!allows_auto(&environment));
        project.write("settings", "GO111MODULE=\n");
        assert!(allows_auto(&environment));

        let other = TestProject::new();
        let mut changed = overrides;
        changed.insert("GOPATH".into(), other.root.to_str().unwrap().into());
        let changed_environment = environment.with_overrides(changed);
        assert!(!allows_auto(&changed_environment));
        assert!(allows_auto(&environment));
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
        assert!(!use_module_auto(&project.root, &environment, None).unwrap());
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
