use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, Metadata};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{OnceLock, atomic::AtomicBool};
use std::time::{Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::Deserialize;

use crate::go_environment::GoEnvironment;
use crate::workspace_roots::{folder_groups, use_module_auto};

mod inputs;
mod load_trace;
mod process;
use inputs::PackageInputs;
pub(crate) use load_trace::{CommandTrace, LoadTrace};
pub(crate) use process::{check_load_cancelled, command_output};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkspaceLoad {
    pub generation: u64,
    pub include_tests: bool,
}

impl WorkspaceLoad {
    pub(crate) fn covers(self, other: Self) -> bool {
        self.generation == other.generation && (self.include_tests || !other.include_tests)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum PackageErrorKind {
    Direct,
    Dependency,
    Group,
}

/// Source positions use Go's one-based lines and byte columns.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct PackageLoadError {
    pub group_id: usize,
    pub import_path: String,
    pub package_id: Option<String>,
    pub package_dir: Option<PathBuf>,
    pub package_name: String,
    pub for_test: String,
    pub kind: PackageErrorKind,
    pub message: String,
    pub import_stack: Vec<String>,
    pub path: Option<PathBuf>,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

pub struct Package {
    pub id: String,
    pub name: String,
    pub files: Vec<PathBuf>,
    pub(crate) load_error: bool,
    directory: PathBuf,
    import_map: HashMap<String, Option<String>>,
    for_test: String,
    external: bool,
    groups: Vec<usize>,
    standard: bool,
    module_path: Option<String>,
    direct_dependency: bool,
}

pub struct Workspace {
    environment: GoEnvironment,
    rename_inputs: Option<Result<PackageInputs, String>>,
    roots: Vec<PathBuf>,
    module_paths: Vec<String>,
    package_errors: Vec<PackageLoadError>,
    group_scopes: Vec<GroupScope>,
    group_imports: Vec<HashMap<String, String>>,
    packages: BTreeMap<String, Package>,
    file_packages: HashMap<PathBuf, String>,
    importers: HashMap<String, Vec<String>>,
    diagnostic_file_stamps: HashMap<PathBuf, DiagnosticFileStamp>,
    diagnostic_observed_at: Instant,
    includes_tests: bool,
    sdk_root: OnceLock<PathBuf>,
    builtin_source: OnceLock<Result<PathBuf, String>>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct DiagnosticFileStamp {
    modified: Option<SystemTime>,
    len: u64,
}

#[derive(Clone)]
pub(crate) struct DiagnosticFileObservation {
    pub(crate) epoch: u64,
    observed_at: Instant,
    pub(crate) stamp: Option<DiagnosticFileStamp>,
    semantic: (Option<DiagnosticFileStamp>, u64),
}

impl DiagnosticFileObservation {
    pub(crate) fn new(stamp: Option<DiagnosticFileStamp>, epoch: u64) -> Self {
        Self {
            epoch,
            observed_at: Instant::now(),
            stamp,
            semantic: (stamp, epoch),
        }
    }

    pub(crate) fn retain_semantics(&mut self, previous: (Option<DiagnosticFileStamp>, u64)) {
        self.semantic = previous;
    }
}

impl From<Metadata> for DiagnosticFileStamp {
    fn from(metadata: Metadata) -> Self {
        Self {
            modified: metadata.modified().ok(),
            len: metadata.len(),
        }
    }
}

struct GroupScope {
    folders: Vec<PathBuf>,
    roots: Vec<PathBuf>,
}

impl Workspace {
    #[cfg(test)]
    pub fn load(root: &Path, include_tests: bool) -> Result<Self> {
        Self::load_roots(&[root.to_owned()], include_tests)
    }

    #[cfg(test)]
    pub(crate) fn load_roots(folders: &[PathBuf], include_tests: bool) -> Result<Self> {
        Self::load_roots_in_environment(folders, include_tests, &GoEnvironment::capture())
    }

    pub(crate) fn load_roots_in_environment(
        folders: &[PathBuf],
        include_tests: bool,
        environment: &GoEnvironment,
    ) -> Result<Self> {
        Self::load_graph(folders, include_tests, false, environment, None)
    }

    pub(crate) fn load_roots_for_rename(
        folders: &[PathBuf],
        include_tests: bool,
        environment: &GoEnvironment,
    ) -> Result<Self> {
        Self::load_graph(folders, include_tests, true, environment, None)
    }

    pub(crate) fn prewarm_tests(
        folders: &[PathBuf],
        environment: &GoEnvironment,
        track_inputs: bool,
        cancelled: &AtomicBool,
    ) -> Result<Self> {
        Self::load_graph(folders, true, track_inputs, environment, Some(cancelled))
    }

    fn load_graph(
        folders: &[PathBuf],
        include_tests: bool,
        track_inputs: bool,
        environment: &GoEnvironment,
        cancelled: Option<&AtomicBool>,
    ) -> Result<Self> {
        check_load_cancelled(cancelled)?;
        let configuration =
            track_inputs.then(|| PackageInputs::configuration(folders, environment));
        let groups = folder_groups(folders, environment, cancelled)?;
        load_trace::groups_discovered(groups.len());
        let inputs = configuration.map(|config| {
            let roots: Vec<_> = groups
                .iter()
                .filter_map(|group| group.roots.as_ref().ok())
                .flatten()
                .map(|dir| path_key(&dir.canonicalize().unwrap_or_else(|_| dir.clone())))
                .collect();
            config
                .and_then(|inputs| inputs.with_sources(&roots))
                .map_err(|err| err.to_string())
        });
        let mut roots = BTreeSet::new();
        let mut scopes = Vec::with_capacity(groups.len());
        let mut listing = PackageListing::default();
        for group in groups {
            check_load_cancelled(cancelled)?;
            let _trace = LoadTrace::group(scopes.len());
            let folder = &group.folders[0];
            let mut scope = GroupScope {
                folders: group.folders.iter().map(|dir| path_key(dir)).collect(),
                roots: group
                    .member_scopes
                    .iter()
                    .map(|dir| path_key(dir))
                    .collect(),
            };
            scope.folders.sort_unstable();
            let packages = group.roots.and_then(|directories| {
                let keys: Vec<_> = directories
                    .iter()
                    .map(|dir| path_key(&dir.canonicalize().unwrap_or_else(|_| dir.clone())))
                    .collect();
                scope.roots.extend(keys.iter().cloned());
                let packages =
                    list_packages(folder, &directories, include_tests, environment, cancelled)?;
                roots.extend(keys);
                Ok(packages)
            });
            match packages {
                Ok(packages) => listing.add_group(folder, packages),
                Err(error) => listing.add_failed_group(error),
            }
            scope.roots.sort_unstable();
            scope.roots.dedup();
            scopes.push(scope);
        }
        check_load_cancelled(cancelled)?;
        let mut workspace = Self::from_listing(
            roots.into_iter().collect(),
            scopes,
            listing,
            include_tests,
            environment,
        );
        workspace.rename_inputs = inputs;
        check_load_cancelled(cancelled)?;
        load_trace::graph_built(&workspace);
        Ok(workspace)
    }

    fn from_listing(
        roots: Vec<PathBuf>,
        group_scopes: Vec<GroupScope>,
        listing: PackageListing,
        includes_tests: bool,
        environment: &GoEnvironment,
    ) -> Self {
        let PackageListing {
            packages,
            mut importers,
            sdk_root,
            module_paths,
            mut errors,
            group_imports,
            ..
        } = listing;
        errors.sort_unstable();
        errors.dedup();
        for importers in importers.values_mut() {
            importers.sort_unstable();
            importers.dedup();
        }
        let mut file_packages = HashMap::<PathBuf, String>::new();
        // Ordinary files belong to their normal package, even when the Go
        // command also reports an in-package test compilation of those files.
        let ordinary = packages
            .iter()
            .filter(|(_, package)| package.for_test.is_empty());
        let testing = packages
            .iter()
            .filter(|(_, package)| !package.for_test.is_empty());
        for (id, package) in ordinary.chain(testing) {
            for file in &package.files {
                let key = path_key(file);
                match file_packages.entry(key) {
                    std::collections::hash_map::Entry::Vacant(slot) => {
                        slot.insert(id.clone());
                    }
                    std::collections::hash_map::Entry::Occupied(mut slot) => {
                        let previous = &packages[slot.get()];
                        if package.is_production() == previous.is_production()
                            && package.groups != previous.groups
                            && scoped_group(&group_scopes, slot.key(), None).is_some_and(|group| {
                                package.groups.contains(&group) && !previous.groups.contains(&group)
                            })
                        {
                            slot.insert(id.clone());
                        }
                    }
                }
            }
        }
        let diagnostic_observed_at = Instant::now();
        let diagnostic_file_stamps = file_packages
            .keys()
            .filter_map(|path| {
                fs::metadata(path)
                    .ok()
                    .map(|metadata| (path.clone(), metadata.into()))
            })
            .collect();
        Self {
            environment: environment.clone(),
            rename_inputs: None,
            roots,
            module_paths: module_paths.into_iter().collect(),
            package_errors: errors,
            group_scopes,
            group_imports,
            packages,
            file_packages,
            importers,
            diagnostic_file_stamps,
            diagnostic_observed_at,
            includes_tests,
            sdk_root: sdk_root.map_or_else(OnceLock::new, OnceLock::from),
            builtin_source: OnceLock::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn from_test_listing(
        roots: &[PathBuf],
        listing: &str,
        includes_tests: bool,
    ) -> Self {
        let mut packages = PackageListing::default();
        packages.add_group(
            roots.first().map_or(Path::new("."), PathBuf::as_path),
            read_packages(listing.as_bytes()).unwrap(),
        );
        let keys: Vec<_> = roots.iter().map(|root| path_key(root)).collect();
        let mut folders = keys.clone();
        folders.sort_unstable();
        let scopes = vec![GroupScope {
            folders,
            roots: Vec::new(),
        }];
        let environment = GoEnvironment::capture();
        let mut workspace =
            Self::from_listing(keys, scopes, packages, includes_tests, &environment);
        workspace.rename_inputs = Some(
            PackageInputs::configuration(roots, &environment)
                .and_then(|inputs| inputs.with_sources(roots))
                .map_err(|err| err.to_string()),
        );
        workspace
    }

    pub(crate) fn rename_inputs_current(&self, environment: &GoEnvironment) -> Result<bool> {
        if self.environment != *environment {
            return Ok(false);
        }
        if let Some(error) = self
            .package_errors
            .iter()
            .find(|error| error.kind == PackageErrorKind::Group)
        {
            bail!("cannot verify a complete package graph: {}", error.message);
        }
        match &self.rename_inputs {
            None => Ok(false),
            Some(Ok(inputs)) => inputs.current(&self.roots, environment),
            Some(Err(reason)) => bail!("cannot verify the package inputs: {reason}"),
        }
    }

    pub(crate) fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    pub(crate) fn module_paths(&self) -> &[String] {
        &self.module_paths
    }

    pub(crate) fn package_errors(&self) -> &[PackageLoadError] {
        &self.package_errors
    }

    pub(crate) fn diagnostic_group_inputs(&self, group: usize) -> Option<(&[PathBuf], &[PathBuf])> {
        let scope = self.group_scopes.get(group)?;
        Some((&scope.folders, &scope.roots))
    }

    pub(crate) fn diagnostic_disk_input(
        &self,
        key: &Path,
        change: Option<&DiagnosticFileObservation>,
    ) -> (Option<DiagnosticFileStamp>, u64) {
        let stamp = self.diagnostic_disk_stamp(key, change);
        change
            .filter(|change| change.stamp == stamp)
            .map_or((stamp, 0), |change| change.semantic)
    }

    pub(crate) fn diagnostic_disk_stamp(
        &self,
        key: &Path,
        change: Option<&DiagnosticFileObservation>,
    ) -> Option<DiagnosticFileStamp> {
        match change {
            Some(change) if change.observed_at >= self.diagnostic_observed_at => change.stamp,
            _ => self.diagnostic_file_stamps.get(key).copied(),
        }
    }

    pub(crate) fn total_load_failure(&self) -> Option<&str> {
        let first = self.package_errors.first()?;
        (self.roots.is_empty() && first.kind == PackageErrorKind::Group)
            .then_some(first.message.as_str())
    }

    pub(crate) fn covers_production_groups(&self, previous: &Self) -> bool {
        if self
            .package_errors
            .iter()
            .any(|error| error.kind == PackageErrorKind::Group)
        {
            return false;
        }
        previous
            .group_scopes
            .iter()
            .enumerate()
            .all(|(index, scope)| {
                if previous
                    .package_errors
                    .iter()
                    .any(|error| error.group_id == index && error.kind == PackageErrorKind::Group)
                {
                    return true;
                }
                let Some(group) = self.group_scopes.iter().position(|candidate| {
                    candidate.folders == scope.folders
                        && scope
                            .roots
                            .iter()
                            .all(|root| candidate.roots.contains(root))
                }) else {
                    return false;
                };
                previous.group_imports[index].iter().all(|(import, id)| {
                    previous.package(id).is_none_or(|old| {
                        !old.is_production()
                            || old.load_error
                            || self
                                .package_in_group_by_import(group, import)
                                .is_some_and(|new| {
                                    new.is_production()
                                        && !new.load_error
                                        && new.directory == old.directory
                                        && new.files == old.files
                                })
                    })
                })
            })
    }

    pub(crate) fn group_for_path(&self, path: &Path) -> Option<usize> {
        let package = self.package_of_file(path);
        scoped_group(
            &self.group_scopes,
            &path_key(path),
            package.map(|package| package.groups.as_slice()),
        )
        .or_else(|| package.map(Package::group_id))
    }

    pub(crate) fn group_scope_for_path(&self, path: &Path) -> Option<usize> {
        scoped_group(&self.group_scopes, &path_key(path), None)
    }

    pub(crate) fn package_in_group(&self, package: &Package, group: usize) -> bool {
        package.groups.contains(&group)
    }

    pub(crate) fn package_in_group_by_import(
        &self,
        group: usize,
        import_path: &str,
    ) -> Option<&Package> {
        self.package(self.group_imports.get(group)?.get(import_path)?)
    }

    pub(crate) fn builtin_source(&self, root: &Path) -> Result<&Path> {
        self.builtin_source
            .get_or_init(|| {
                if let Some(sdk) = self.sdk_root() {
                    Ok(sdk.join("src").join("builtin").join("builtin.go"))
                } else {
                    find_builtin_source(root, &self.environment)
                        .map_err(|error| format!("{error:#}"))
                }
            })
            .as_ref()
            .map(PathBuf::as_path)
            .map_err(|error| anyhow!("{error}"))
    }

    pub(crate) fn sdk_root(&self) -> Option<&Path> {
        self.sdk_root.get().map(PathBuf::as_path)
    }

    pub(crate) fn share_sdk_root(&self, root: &Path) {
        let _ = self.sdk_root.set(root.to_owned());
    }

    pub fn includes_tests(&self) -> bool {
        self.includes_tests
    }

    pub(crate) fn selected_file_stamp(&self, key: &Path) -> Option<Option<DiagnosticFileStamp>> {
        self.file_packages
            .contains_key(key)
            .then(|| self.diagnostic_file_stamps.get(key).copied())
    }

    pub fn package_of_file(&self, path: &Path) -> Option<&Package> {
        self.package(self.file_packages.get(&path_key(path))?)
    }

    pub fn package(&self, import_path: &str) -> Option<&Package> {
        self.packages.get(import_path)
    }

    pub(crate) fn navigation_packages(&self) -> impl Iterator<Item = &Package> {
        self.packages.values()
    }

    pub fn resolve_import(&self, from: &Package, import_path: &str) -> Option<&Package> {
        if let Some(target) = from.import_map.get(import_path) {
            return self.package(target.as_deref()?);
        }
        // A shared package can resolve a new import when its groups agree on the target.
        let (first, others) = from.groups.split_first()?;
        let id = self.group_imports[*first].get(import_path)?;
        if others
            .iter()
            .all(|&group| self.group_imports[group].get(import_path) == Some(id))
        {
            return self.package(id);
        }
        None
    }

    pub(crate) fn resolve_import_in_group(
        &self,
        from: &Package,
        group: usize,
        import_path: &str,
    ) -> Option<&Package> {
        if !self.package_in_group(from, group) {
            return None;
        }
        match from.import_map.get(import_path) {
            Some(id) => self.package(id.as_deref()?),
            None => self.package_in_group_by_import(group, import_path),
        }
    }

    pub(crate) fn matches_import_variant(
        &self,
        from: &Package,
        import_path: &str,
        id: &str,
    ) -> bool {
        from.groups
            .iter()
            .any(|&group| match self.group_imports[group].get(import_path) {
                Some(selected) => selected == id,
                None => self
                    .package(id)
                    .is_some_and(|package| self.package_in_group(package, group)),
            })
    }

    pub(crate) fn package_of_draft(&self, path: &Path, name: &str) -> Option<&Package> {
        let directory = path_key(path.parent()?);
        let mut packages = self.packages.values().filter(|package| {
            package.for_test.is_empty() && package.directory == directory && package.name == name
        });
        let package = packages.next()?;
        packages.next().is_none().then_some(package)
    }

    pub(crate) fn same_package_variants<'w>(
        &'w self,
        owner: &'w Package,
    ) -> impl Iterator<Item = &'w Package> {
        let id = if owner.for_test.is_empty() {
            &owner.id
        } else {
            &owner.for_test
        };
        self.packages.values().filter(move |package| {
            package.directory == owner.directory && (&package.id == id || &package.for_test == id)
        })
    }

    pub(crate) fn is_project_package(&self, package: &Package) -> bool {
        self.contains_project_path(&package.directory)
    }

    pub(crate) fn contains_project_path(&self, key: &Path) -> bool {
        self.roots().iter().any(|root| key.starts_with(root))
    }

    pub(crate) fn contains_external_symbol_path(&self, key: &Path) -> bool {
        self.navigation_packages()
            .any(|package| package.external && key.starts_with(&package.directory))
    }

    pub(crate) fn project_symbol_files(&self) -> HashMap<PathBuf, PathBuf> {
        let mut files = HashMap::new();
        for package in self
            .navigation_packages()
            .filter(|package| !package.external && self.is_project_package(package))
        {
            for path in &package.files {
                let key = path_key(path);
                // The synthetic test main may have an absolute Go build-cache
                // path in GoFiles. A package's source files share its directory.
                if key.parent() == Some(package.directory.as_path()) && is_symbol_source(&key) {
                    files.entry(key).or_insert_with(|| path.clone());
                }
            }
        }
        files
    }

    pub(crate) fn direct_importers<'a>(
        &'a self,
        package: &Package,
    ) -> impl Iterator<Item = &'a Package> {
        self.importers
            .get(&package.id)
            .into_iter()
            .flatten()
            .filter_map(|id| self.package(id))
    }

    pub(crate) fn affected_packages(&self, path: &Path) -> Option<HashSet<String>> {
        let directory = path_key(path.parent()?);
        // A known test file can change the in-package or external test
        // compilation in this directory. The production selection excludes it.
        let only_tests = path
            .file_stem()
            .is_some_and(|name| name.to_string_lossy().ends_with("_test"));
        let mut queue: Vec<_> = self
            .packages
            .values()
            .filter(|package| {
                package.directory == directory && (!only_tests || !package.is_production())
            })
            .map(|package| package.id.as_str())
            .collect();
        if queue.is_empty() && !only_tests {
            return None;
        }

        let mut dirty = HashSet::new();
        while let Some(id) = queue.pop() {
            if dirty.insert(id.to_owned())
                && let Some(importers) = self.importers.get(id)
            {
                queue.extend(importers.iter().map(String::as_str));
            }
        }
        Some(dirty)
    }
}

fn scoped_group(scopes: &[GroupScope], key: &Path, groups: Option<&[usize]>) -> Option<usize> {
    scopes
        .iter()
        .enumerate()
        .filter(|(group, _)| groups.is_none_or(|groups| groups.contains(group)))
        .flat_map(|(group, scope)| {
            scope
                .folders
                .iter()
                .map(|path| (path, true))
                .chain(scope.roots.iter().map(|path| (path, false)))
                .map(move |(path, explicit)| (group, path, explicit))
        })
        .filter(|(_, scope, _)| key.starts_with(scope))
        .max_by_key(|(_, scope, explicit)| (scope.components().count(), *explicit))
        .map(|(group, _, _)| group)
}

impl Package {
    pub(crate) fn diagnostic_import_paths(&self) -> impl Iterator<Item = &str> {
        self.import_map.keys().map(String::as_str)
    }

    fn remap_imports(&mut self, aliases: &HashMap<String, String>) {
        if let Some(id) = aliases.get(&self.for_test) {
            self.for_test.clone_from(id);
        }
        for target in self.import_map.values_mut().flatten() {
            if let Some(id) = aliases.get(target) {
                target.clone_from(id);
            }
        }
    }

    fn same_build(&self, other: &Self) -> bool {
        self.directory == other.directory
            && self.files == other.files
            && self.import_map == other.import_map
            && self.for_test == other.for_test
            && self.name == other.name
            && self.load_error == other.load_error
            && self.external == other.external
            && self.standard == other.standard
            && self.module_path == other.module_path
    }

    pub(crate) fn directory_and_name(&self) -> (&Path, &str) {
        (&self.directory, &self.name)
    }

    pub(crate) fn import_path(&self) -> &str {
        let path = self
            .id
            .split_once(':')
            .map_or(self.id.as_str(), |(_, path)| path);
        path.split_once(" [").map_or(path, |(path, _)| path)
    }

    pub(crate) fn private_name_owner(&self) -> &str {
        // An in-package test view shares the regular package's private names.
        // Its own display ID may lack the build-group prefix when only this
        // module has tests; ForTest retains the remapped production ID.
        let tested_path = self
            .for_test
            .split_once(':')
            .map_or(self.for_test.as_str(), |(_, path)| path);
        if !self.is_production() && self.import_path() == tested_path {
            &self.for_test
        } else {
            &self.id
        }
    }

    pub(crate) fn is_production(&self) -> bool {
        self.for_test.is_empty()
    }

    pub(crate) fn test_of(&self) -> Option<&str> {
        (!self.is_production()).then(|| {
            self.for_test
                .split_once(':')
                .map_or(self.for_test.as_str(), |(_, path)| path)
        })
    }

    pub(crate) fn group_id(&self) -> usize {
        self.groups[0]
    }

    pub(crate) fn groups(&self) -> &[usize] {
        &self.groups
    }

    pub(crate) fn is_standard(&self) -> bool {
        self.standard
    }

    pub(crate) fn is_external(&self) -> bool {
        self.external
    }

    pub(crate) fn module_path(&self) -> Option<&str> {
        self.module_path.as_deref()
    }

    pub(crate) fn is_direct_dependency(&self) -> bool {
        self.direct_dependency
    }
}

fn find_builtin_source(root: &Path, environment: &GoEnvironment) -> Result<PathBuf> {
    let starting_go = || format!("starting go env GOROOT in {}", root.display());
    let mut trace = CommandTrace::new("go env", root);
    let mut command = environment.command("go").with_context(starting_go)?;
    command
        .args(["env", "GOROOT"])
        .current_dir(root)
        .stdin(Stdio::null());
    let output = CommandTrace::output(&mut command, trace.as_mut()).with_context(starting_go)?;
    ensure!(
        output.status.success(),
        "go env GOROOT failed in {} ({}): {}",
        root.display(),
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let goroot = std::str::from_utf8(&output.stdout).context("decoding go env GOROOT")?;
    let goroot = Path::new(goroot.trim_end_matches(['\r', '\n']));
    ensure!(
        goroot.is_absolute(),
        "go env GOROOT did not return an absolute path"
    );
    if let Some(trace) = &mut trace {
        trace.parsed(0, 0, 0);
    }
    Ok(goroot.join("src").join("builtin").join("builtin.go"))
}

fn list_packages(
    root: &Path,
    directories: &[PathBuf],
    include_tests: bool,
    environment: &GoEnvironment,
    cancelled: Option<&AtomicBool>,
) -> Result<Vec<ListedPackage>> {
    check_load_cancelled(cancelled)?;
    let patterns = directories.iter().map(|directory| {
        if directory == root {
            "./...".to_owned()
        } else {
            directory.join("...").to_string_lossy().replace('\\', "/")
        }
    });
    let starting_go = || format!("starting go list in {}", root.display());
    let mut command = environment.command("go").with_context(starting_go)?;
    if use_module_auto(root, environment, cancelled)? {
        command.env("GO111MODULE", "auto");
    }
    let mut trace = CommandTrace::new("go list", root);
    command
        .args([
            "list",
            "-e",
            "-json=Dir,ImportPath,Name,GoFiles,CgoFiles,Imports,ImportMap,Error,DepsErrors,ForTest,Goroot,Module",
            "-deps",
        ])
        .args(include_tests.then_some("-test"))
        .args(patterns)
        .current_dir(root)
        .stdin(Stdio::null());
    let (status, packages, stderr) =
        process::read_command(&mut command, trace.as_mut(), cancelled, |stdout| {
            read_packages(BufReader::new(stdout))
        })
        .with_context(starting_go)?;
    if let (Some(trace), Ok(packages)) = (&mut trace, &packages) {
        trace_listed_packages(trace, packages, directories);
    }
    if !status.success() {
        bail!(
            "go list failed in {} ({status}): {}",
            root.display(),
            String::from_utf8_lossy(&stderr).trim()
        );
    }
    packages.with_context(|| format!("reading Go packages in {}", root.display()))
}

fn trace_listed_packages(trace: &mut CommandTrace, packages: &[ListedPackage], roots: &[PathBuf]) {
    let roots: Vec<_> = roots.iter().map(|root| path_key(root)).collect();
    let mut project_files = HashSet::new();
    let mut errors = 0;
    for package in packages {
        errors +=
            usize::from(package.error.is_some()) + package.deps_errors.as_ref().map_or(0, Vec::len);
        if !package.has_sources() || package.is_external() {
            continue;
        }
        let directory = path_key(&package.dir);
        if roots.iter().any(|root| directory.starts_with(root)) {
            for path in package.go_files.iter().chain(&package.cgo_files) {
                let key = path_key(&directory.join(path));
                if key.parent() == Some(directory.as_path()) && is_symbol_source(&key) {
                    project_files.insert(key);
                }
            }
        }
    }
    trace.parsed(packages.len(), project_files.len(), errors);
}

fn read_packages(reader: impl Read) -> Result<Vec<ListedPackage>> {
    serde_json::Deserializer::from_reader(reader)
        .into_iter::<ListedPackage>()
        .collect::<serde_json::Result<_>>()
        .context("decoding go list package metadata")
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
struct ListedPackage {
    dir: PathBuf,
    import_path: String,
    for_test: String,
    name: String,
    go_files: Vec<PathBuf>,
    cgo_files: Vec<PathBuf>,
    imports: Vec<String>,
    import_map: HashMap<String, String>,
    error: Option<ListedError>,
    deps_errors: Option<Vec<ListedError>>,
    goroot: bool,
    module: Option<ListedModule>,
}

impl ListedPackage {
    fn has_sources(&self) -> bool {
        !self.dir.as_os_str().is_empty()
            && !self.import_path.is_empty()
            && !self.name.is_empty()
            && !(self.go_files.is_empty() && self.cgo_files.is_empty())
    }

    fn is_external(&self) -> bool {
        self.goroot
            || self
                .module
                .as_ref()
                .is_some_and(ListedModule::in_module_cache)
    }
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
struct ListedModule {
    path: String,
    main: bool,
    indirect: bool,
    version: String,
    replace: Option<Box<ListedModule>>,
}

impl ListedModule {
    fn in_module_cache(&self) -> bool {
        !self.main && !self.replace.as_deref().unwrap_or(self).version.is_empty()
    }
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
struct ListedError {
    import_stack: Option<Vec<String>>,
    pos: String,
    err: String,
}

impl ListedError {
    fn resolve(
        self,
        group_id: usize,
        kind: PackageErrorKind,
        package: &ListedPackage,
        root: &Path,
    ) -> PackageLoadError {
        let (path, line, column) = error_position(&self.pos, root, &package.dir);
        PackageLoadError {
            group_id,
            import_path: package.import_path.clone(),
            package_id: None,
            package_dir: (!package.dir.as_os_str().is_empty()).then(|| root.join(&package.dir)),
            package_name: package.name.clone(),
            for_test: package.for_test.clone(),
            kind,
            message: self.err,
            import_stack: self.import_stack.unwrap_or_default(),
            path,
            line,
            column,
        }
    }
}

fn error_position(
    pos: &str,
    root: &Path,
    dir: &Path,
) -> (Option<PathBuf>, Option<u32>, Option<u32>) {
    fn split_number(value: &str) -> Option<(&str, u32)> {
        let (prefix, number) = value.rsplit_once(':')?;
        Some((prefix, number.parse().ok()?))
    }
    let (file, line, column) = match split_number(pos) {
        Some((prefix, last)) => match split_number(prefix) {
            Some((file, line)) => (file, Some(line), Some(last)),
            None => (prefix, Some(last), None),
        },
        None => (pos, None, None),
    };
    (
        error_path(file, root, dir),
        line.filter(|&n| n > 0),
        column.filter(|&n| n > 0),
    )
}

fn error_path(file: &str, root: &Path, dir: &Path) -> Option<PathBuf> {
    if file.is_empty() || file == "-" {
        return None;
    }
    let path = Path::new(file);
    let windows_absolute = matches!(file.as_bytes(), [drive, b':', b'/' | b'\\', ..] if drive.is_ascii_alphabetic())
        || file.starts_with(r"\\");
    if file.starts_with('/') || path.is_absolute() || windows_absolute {
        return Some(path.to_owned());
    }
    // cmd/go can shorten a dependency's position relative to its working directory.
    let from_root = root.join(path);
    let absolute = if dir.as_os_str().is_empty() || from_root.is_file() {
        from_root
    } else {
        root.join(dir).join(path)
    };
    if path
        .components()
        .any(|part| part == std::path::Component::ParentDir)
    {
        return Some(absolute.canonicalize().unwrap_or(absolute));
    }
    Some(absolute)
}

#[derive(Default)]
struct PackageListing {
    packages: BTreeMap<String, Package>,
    sdk_root: Option<PathBuf>,
    group_imports: Vec<HashMap<String, String>>,
    alternate_ids: HashMap<String, Vec<String>>,
    importers: HashMap<String, Vec<String>>,
    module_paths: BTreeSet<String>,
    errors: Vec<PackageLoadError>,
}

impl PackageListing {
    fn add_failed_group(&mut self, error: anyhow::Error) {
        self.errors.push(PackageLoadError {
            group_id: self.group_imports.len(),
            import_path: String::new(),
            package_id: None,
            package_dir: None,
            package_name: String::new(),
            for_test: String::new(),
            kind: PackageErrorKind::Group,
            message: format!("{error:#}"),
            import_stack: Vec::new(),
            path: None,
            line: None,
            column: None,
        });
        self.group_imports.push(HashMap::new());
    }

    fn add_group(&mut self, root: &Path, packages: Vec<ListedPackage>) {
        let group = self.group_imports.len();
        let mut aliases: HashMap<_, _> = packages
            .iter()
            .filter(|package| package.has_sources())
            .map(|package| {
                let raw = &package.import_path;
                let id = if self.packages.contains_key(raw) {
                    format!("{group}:{raw}")
                } else {
                    raw.clone()
                };
                (raw.clone(), id)
            })
            .collect();
        let first_error = self.errors.len();
        let mut reused = HashMap::new();
        let mut added = Vec::new();
        let mut seen = HashSet::new();
        // go list -deps normally emits dependencies first. Provisional aliases keep
        // forward edges (such as import cycles) inside the group during insertion.
        for mut package in packages {
            if package.goroot && self.sdk_root.is_none() {
                let import = package.import_path.split(" [").next().unwrap_or_default();
                let relative = Path::new(import);
                if !import.is_empty()
                    && package.dir.is_absolute()
                    && package.dir.ends_with(relative)
                {
                    self.sdk_root = package
                        .dir
                        .ancestors()
                        .nth(relative.components().count())
                        .filter(|src| src.file_name().is_some_and(|name| name == "src"))
                        .and_then(Path::parent)
                        .map(Path::to_owned);
                }
            }
            let load_error = package.error.is_some();
            if let Some(error) = package.error.take() {
                self.errors
                    .push(error.resolve(group, PackageErrorKind::Direct, &package, root));
            }
            for error in package.deps_errors.take().unwrap_or_default() {
                self.errors.push(error.resolve(
                    group,
                    PackageErrorKind::Dependency,
                    &package,
                    root,
                ));
            }
            if let Some(module) = &package.module
                && module.main
                && !module.path.is_empty()
            {
                self.module_paths.insert(module.path.clone());
            }
            if !package.has_sources() || !seen.insert(package.import_path.clone()) {
                continue;
            }

            let external = package.is_external();
            let mut files: Vec<_> = package
                .go_files
                .into_iter()
                .chain(package.cgo_files)
                .map(|file| package.dir.join(file))
                .collect();
            files.sort_unstable();
            files.dedup();
            let raw = package.import_path;
            let mut import_map: HashMap<_, _> = package
                .import_map
                .into_iter()
                .map(|(path, id)| (path, aliases.get(&id).cloned()))
                .collect();
            for import in &package.imports {
                import_map
                    .entry(import.clone())
                    .or_insert_with(|| aliases.get(import).cloned());
            }
            let for_test = if package.for_test.is_empty() {
                package.for_test
            } else {
                // A test-only directory can omit its production graph node.
                // Its private names still belong to this build group.
                aliases
                    .get(&package.for_test)
                    .cloned()
                    .unwrap_or_else(|| format!("{group}:{}", package.for_test))
            };
            let owner = Package {
                id: aliases[&raw].clone(),
                name: package.name,
                files,
                load_error,
                directory: path_key(&package.dir),
                import_map,
                for_test,
                external,
                groups: vec![group],
                standard: package.goroot,
                direct_dependency: package
                    .module
                    .as_ref()
                    .is_some_and(|m| !m.main && !m.indirect),
                module_path: package.module.map(|m| m.path).filter(|p| !p.is_empty()),
            };
            if let Some(id) = self.reuse_package(&raw, &owner, group) {
                reused.insert(owner.id, id.clone());
                aliases.insert(raw, id);
            } else {
                added.push((owner.id.clone(), package.imports));
                self.packages.insert(owner.id.clone(), owner);
            }
        }
        for (id, imports) in added {
            let owner = self.packages.get_mut(&id).expect("newly inserted package");
            owner.remap_imports(&reused);
            for import in &imports {
                let selected = owner.import_map.get(import).and_then(Option::as_ref);
                // Keep reverse edges both to the selected test variant and to
                // the ordinary imported package used by project-wide searches.
                for dependency in selected.into_iter().chain(aliases.get(import)) {
                    self.importers
                        .entry(dependency.clone())
                        .or_default()
                        .push(id.clone());
                }
            }
        }
        for error in &mut self.errors[first_error..] {
            error.package_id = aliases.get(&error.import_path).cloned();
        }
        self.group_imports.push(aliases);
    }

    fn reuse_package(&mut self, raw: &str, owner: &Package, group: usize) -> Option<String> {
        if raw == owner.id {
            return None;
        }
        // Independent modules may bind the same Go import path to different sources.
        let variants = self.alternate_ids.entry(raw.to_owned()).or_default();
        let selected = std::iter::once(raw)
            .chain(variants.iter().map(String::as_str))
            .find(|id| self.packages[*id].same_build(owner));
        if let Some(id) = selected {
            let existing = self.packages.get_mut(id).expect("existing package variant");
            existing.direct_dependency |= owner.direct_dependency;
            existing.groups.push(group);
            return Some(id.to_owned());
        }
        variants.push(owner.id.clone());
        None
    }
}

pub(crate) fn is_symbol_source(key: &Path) -> bool {
    key.extension().is_some_and(|extension| extension == "go")
        && key
            .file_name()
            .is_some_and(|name| !name.to_string_lossy().starts_with(['.', '_']))
        && !key
            .components()
            .any(|component| component.as_os_str() == "vendor")
}

pub(crate) fn is_go_manifest(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "go.mod" | "go.sum" | "go.work" | "go.work.sum"))
}

pub(crate) fn is_watched_go_file(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension == "go") || is_go_manifest(path)
}

pub(crate) fn path_key(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let path = path.to_string_lossy().replace('/', "\\").to_lowercase();
        if let Some(unc) = path.strip_prefix("\\\\?\\unc\\") {
            return PathBuf::from(format!("\\\\{unc}"));
        }
        if let Some(disk) = path.strip_prefix("\\\\?\\") {
            return PathBuf::from(disk);
        }
        PathBuf::from(path)
    }
    #[cfg(not(windows))]
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic::{DefinitionKind, Engine, QueryProgress};
    use crate::test_support::TestProject;
    use lsp_types::{Position, Range};
    use serde_json::json;

    #[test]
    fn rename_detects_parent_module_changes_for_each_independent_folder() {
        let project = TestProject::new();
        for name in ["left", "right"] {
            project.write(
                &format!("{name}/go.mod"),
                &format!("module example.test/{name}\n\ngo 1.22\n"),
            );
            project.write(
                &format!("{name}/pkg/source.go"),
                "package pkg\nconst Value = 1\n",
            );
        }
        let folders = ["left/pkg", "right/pkg"].map(|name| project.root.join(name));
        let environment = GoEnvironment::capture();
        let workspace = Workspace::load_roots_for_rename(&folders, false, &environment).unwrap();
        assert!(workspace.rename_inputs_current(&environment).unwrap());
        assert!(workspace.package("example.test/right/pkg").is_some());
        project.write("right/go.mod", "module example.test/renamed\n\ngo 1.22\n");
        assert!(
            !workspace.rename_inputs_current(&environment).unwrap(),
            "the second folder's parent go.mod changed"
        );
    }

    #[test]
    fn workspace_rename_tracks_external_work_file_in_the_session_environment() {
        let project = TestProject::new();
        let folder = project.root.join("folder");
        project.write("folder/go.mod", "module example.test/folder\n\ngo 1.22\n");
        project.write("folder/example.go", "package folder\n");
        let work = "outside/custom.work";
        let work_file = project.write(work, "go 1.22\n\nuse ../folder\n");
        let startup = GoEnvironment::capture();
        let overrides = BTreeMap::from([
            ("GO111MODULE".into(), "on".into()),
            ("GOWORK".into(), work_file.to_str().unwrap().into()),
        ]);
        let environment = startup.with_overrides(overrides.clone());
        let folders = [folder];
        let workspace = Workspace::load_roots_for_rename(&folders, false, &environment).unwrap();
        assert!(workspace.package("example.test/folder").is_some());
        assert!(workspace.rename_inputs_current(&environment).unwrap());

        let mut changed = overrides;
        changed.insert("GOFLAGS".into(), "-tags=changed".into());
        let changed = startup.with_overrides(changed);
        assert!(!workspace.rename_inputs_current(&changed).unwrap());

        project.write(work, "go 1.22\n\nuse ../folder\n\n// changed\n");
        assert!(!workspace.rename_inputs_current(&environment).unwrap());
    }

    #[test]
    fn workspace_loads_canonical_imports_from_two_session_gopath_entries() {
        let project = TestProject::new();
        let server = project.root.join("server");
        let base = project.root.join("serverbase");
        let caller = project.write(
            "server/src/example.test/server/caller.go",
            "package server\nimport \"example.test/serverbase\"\nconst Use = serverbase.Value\n",
        );
        project.write(
            "serverbase/src/example.test/serverbase/base.go",
            "package serverbase\nconst Value = 1\n",
        );
        let environment = GoEnvironment::capture().with_overrides(BTreeMap::from([
            ("GO111MODULE".into(), String::new()),
            ("GOENV".into(), "off".into()),
            ("GOWORK".into(), String::new()),
            (
                "GOPATH".into(),
                std::env::join_paths([&server, &base])
                    .unwrap()
                    .into_string()
                    .unwrap(),
            ),
        ]));
        let single_package = caller.parent().unwrap().to_path_buf();
        for folders in [
            vec![single_package],
            vec![server.clone()],
            vec![server, base],
            vec![project.root.clone()],
        ] {
            let workspace =
                Workspace::load_roots_in_environment(&folders, false, &environment).unwrap();
            assert!(workspace.package_errors().is_empty(), "{folders:?}");
            let owner = workspace.package_of_file(&caller).unwrap();
            assert_eq!(owner.id, "example.test/server");
            assert_eq!(
                workspace
                    .resolve_import(owner, "example.test/serverbase")
                    .unwrap()
                    .id,
                "example.test/serverbase"
            );
        }
    }

    #[test]
    fn parses_package_error_positions_from_the_right() {
        let project = TestProject::new();
        let dir = project.root.join("pkg");
        for (pos, file, line, column) in [
            (r"C:\src\broken.go:12:5", r"C:\src\broken.go", 12, Some(5)),
            (r"C:\src\broken.go:12", r"C:\src\broken.go", 12, None),
            ("/src/broken.go:7:2", "/src/broken.go", 7, Some(2)),
            ("/src/broken.go:7", "/src/broken.go", 7, None),
        ] {
            assert_eq!(
                error_position(pos, &project.root, &dir),
                (Some(PathBuf::from(file)), Some(line), column)
            );
        }
        assert_eq!(
            error_position("broken.go:12", &project.root, &dir),
            (Some(dir.join("broken.go")), Some(12), None)
        );
        assert_eq!(
            error_position("go.mod", &project.root, Path::new("")),
            (Some(project.root.join("go.mod")), None, None)
        );
        let file = project.write("other/broken.go", "package broken\n");
        assert_eq!(
            error_position("other/broken.go:3:1", &project.root, &dir),
            (Some(file), Some(3), Some(1))
        );
        assert_eq!(error_position("", &project.root, &dir), (None, None, None));
    }

    #[test]
    fn retains_direct_and_dependency_errors_even_without_navigable_files() {
        let project = TestProject::new();
        let id = "example.test/pkg [example.test/pkg.test]";
        let dep = json!({"ImportStack": ["example.test/pkg", "missing"], "Err": "unknown import"});
        let broken = json!({
            "Dir": project.root.join("pkg"), "ImportPath": id, "ForTest": "example.test/pkg",
            "GoFiles": [], "Error": {"Pos": "broken_test.go:8:3", "Err": "parse failed"},
            "DepsErrors": [dep, dep], "Module": {"Main": true, "Path": "example.test/pkg"},
        });
        let missing = json!({
            "ImportPath": "missing", "Name": "missing", "Error": {
                "ImportStack": null, "Pos": "go.mod:2", "Err": "unknown module"
            }
        });
        let healthy = json!({
            "Dir": project.root, "ImportPath": "example.test/importer", "Name": "importer",
            "GoFiles": ["ok.go"], "DepsErrors": [dep],
        });
        let empty = json!({
            "Dir": project.root.join("empty"), "ImportPath": "z.test/empty", "Name": "empty",
            "GoFiles": [], "Error": { "Err": "no Go files" }
        });
        let records = [&missing, &broken, &healthy, &broken, &empty];
        let load = |records: &[&serde_json::Value]| {
            let listing = records
                .iter()
                .map(|record| record.to_string())
                .collect::<Vec<_>>()
                .join("\n");
            Workspace::from_test_listing(std::slice::from_ref(&project.root), &listing, true)
        };
        let workspace = load(&records);
        assert_eq!(
            workspace.package_errors(),
            load(&[&empty, &healthy, &broken, &missing]).package_errors()
        );
        assert_eq!(workspace.package_errors().len(), 5);
        assert!(workspace.package(id).is_none());
        assert!(
            !workspace
                .package("example.test/importer")
                .unwrap()
                .load_error
        );
        assert_eq!(workspace.module_paths(), ["example.test/pkg"]);
        let errors = workspace.package_errors();
        assert_eq!(
            errors[1],
            PackageLoadError {
                group_id: 0,
                import_path: id.into(),
                package_id: None,
                package_dir: Some(project.root.join("pkg")),
                package_name: String::new(),
                for_test: "example.test/pkg".into(),
                kind: PackageErrorKind::Direct,
                message: "parse failed".into(),
                import_stack: Vec::new(),
                path: Some(project.root.join("pkg/broken_test.go")),
                line: Some(8),
                column: Some(3),
            }
        );
        assert_eq!(
            errors[2],
            PackageLoadError {
                group_id: 0,
                import_path: id.into(),
                package_id: None,
                package_dir: Some(project.root.join("pkg")),
                package_name: String::new(),
                for_test: "example.test/pkg".into(),
                kind: PackageErrorKind::Dependency,
                message: "unknown import".into(),
                import_stack: vec!["example.test/pkg".into(), "missing".into()],
                path: None,
                line: None,
                column: None,
            }
        );
        assert_eq!(errors[3].path, Some(project.root.join("go.mod")));
        assert_eq!((errors[3].line, errors[3].column), (Some(2), None));
        assert_eq!(errors[4].message, "no Go files");
    }

    #[test]
    fn merges_independent_module_graphs_and_resolves_each_folders_replacements() {
        let project = TestProject::new();
        project.write("common/go.mod", "module example.test/common\n\ngo 1.22\n");
        let common = project.write("common/common.go", "package common\n");
        for folder in ["one", "two"] {
            project.write(&format!("{folder}/go.mod"), &format!(
                "module example.test/{folder}\n\ngo 1.22\n\nrequire (\nexample.test/common v0.0.0\nexample.test/dep v0.0.0\n)\nreplace example.test/common => ../common\nreplace example.test/dep => ../dep-{folder}\n"
            ));
            project.write(&format!("{folder}/app.go"), &format!(
                "package {folder}\nimport (\n_ \"example.test/common\"\n_ \"example.test/dep\"\n_ \"example.test/{folder}/missing\"\n)\n"
            ));
            project.write(
                &format!("dep-{folder}/go.mod"),
                "module example.test/dep\n\ngo 1.22\n",
            );
            project.write(&format!("dep-{folder}/dep.go"), "package dep\n");
        }
        let folders = [project.root.join("two"), project.root.join("one")];
        let workspace = Workspace::load_roots(&folders, false).unwrap();
        assert_eq!(
            workspace.roots(),
            [
                path_key(&folders[1].canonicalize().unwrap()),
                path_key(&folders[0].canonicalize().unwrap())
            ]
        );
        assert_eq!(
            workspace.module_paths(),
            ["example.test/one", "example.test/two"]
        );
        let common = workspace.package_of_file(&common).unwrap();
        assert_eq!(
            workspace
                .direct_importers(common)
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>(),
            ["example.test/one", "example.test/two"]
        );
        for folder in ["one", "two"] {
            let file = project.root.join(folder).join("app.go");
            let owner = workspace.package_of_file(&file).unwrap();
            let dep = workspace.resolve_import(owner, "example.test/dep").unwrap();
            assert_eq!(dep.import_path(), "example.test/dep");
            assert_eq!(dep.module_path(), Some("example.test/dep"));
            assert!(dep.is_direct_dependency());
            let dep_file = project.root.join(format!("dep-{folder}/dep.go"));
            assert_eq!(dep.id, workspace.package_of_file(&dep_file).unwrap().id);
            assert_eq!(
                workspace
                    .direct_importers(dep)
                    .map(|p| &p.id)
                    .collect::<Vec<_>>(),
                [&owner.id]
            );
            assert_eq!(
                workspace
                    .resolve_import(owner, "example.test/common")
                    .unwrap()
                    .id,
                common.id
            );
            assert!(
                workspace
                    .package_errors()
                    .iter()
                    .any(
                        |error| error.package_id.as_deref() == Some(owner.id.as_str())
                            && error.kind == PackageErrorKind::Dependency
                            && error.path.as_ref() == Some(&file)
                            && error.line == Some(5)
                    )
            );
        }
    }

    #[test]
    fn groups_selected_go_work_members_without_loading_unselected_siblings() {
        let project = TestProject::new();
        project.write("go.work", "go 1.22\n\nuse (\n./one\n./two\n./three\n)\n");
        for name in ["one", "two", "three"] {
            project.write(
                &format!("{name}/go.mod"),
                &format!("module example.test/{name}\n\ngo 1.22\n"),
            );
            project.write(&format!("{name}/pkg.go"), &format!("package {name}\n"));
        }
        let folders = [project.root.join("one"), project.root.join("two")];
        project.write("one/pkg.go", "package one\nimport _ \"example.test/two\"\n");
        project.write(
            "two/pkg.go",
            "package two\nimport _ \"example.test/two/missing\"\n",
        );
        let environment = GoEnvironment::capture();
        let groups = folder_groups(&folders, &environment, None).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].folders, folders);
        let workspace = Workspace::load_roots(&folders, false).unwrap();
        assert_eq!(
            workspace.module_paths(),
            ["example.test/one", "example.test/two"]
        );
        assert!(workspace.package("example.test/three").is_none());
        let error = workspace
            .package_errors()
            .iter()
            .find(|error| error.package_id.as_deref() == Some("example.test/one"))
            .unwrap();
        let path = error.path.as_deref().unwrap();
        let owner = workspace.package_of_file(path).unwrap();
        assert_eq!(owner.id, "example.test/two");
        assert_eq!(
            crate::protocol::path_uri(path).unwrap(),
            crate::protocol::path_uri(&owner.files[0]).unwrap()
        );

        let overlapping = [project.root.clone(), folders[0].clone()];
        let groups = folder_groups(&overlapping, &environment, None).unwrap();
        assert_eq!(groups[0].roots.as_ref().unwrap().len(), 3);
        let workspace = Workspace::load_roots(&overlapping, false).unwrap();
        assert_eq!(workspace.roots().len(), 3);
        assert_eq!(workspace.navigation_packages().count(), 3);
    }

    #[test]
    fn workspace_group_failures_preserve_definition_and_block_rename() {
        let project = TestProject::new();
        project.write("healthy/go.mod", "module example.test/healthy\n\ngo 1.22\n");
        let target = project.write("healthy/lib/lib.go", "package lib\nconst Value = 1\n");
        let source = project.write(
            "healthy/app.go",
            "package app\nimport \"example.test/healthy/lib\"\nconst Use = lib.Value\n",
        );
        project.write(
            "broken/go.mod",
            "module example.test/healthy\n\ninvalid directive\n",
        );
        let broken = project.write("broken/app.go", "package app\nconst Use = 2\n");
        let folders = ["healthy", "absent", "broken"].map(|folder| project.root.join(folder));
        let workspace =
            Workspace::load_roots_for_rename(&folders, true, &GoEnvironment::capture()).unwrap();
        assert_eq!(workspace.navigation_packages().count(), 2);
        assert_eq!(workspace.package_errors().len(), 2);
        assert!(workspace.package_of_file(&broken).is_none());
        for (path, command) in [(&folders[1], "go env"), (&folders[2], "go list")] {
            let group = workspace.group_for_path(&path.join("app.go")).unwrap();
            assert_ne!(Some(group), workspace.group_for_path(&source));
            let error = workspace
                .package_errors()
                .iter()
                .find(|error| error.group_id == group)
                .unwrap();
            assert_eq!(error.kind, PackageErrorKind::Group);
            assert!(error.package_id.is_none());
            assert!(error.message.contains(command), "{error:?}");
        }
        let mut engine = Engine::new(folders[0].clone());
        assert!(engine.install_workspace(engine.workspace_load(), workspace));
        let cursor = Position::new(2, 17);
        let QueryProgress::Ready(Some((path, range))) = engine
            .definition(&source, cursor, DefinitionKind::Declaration)
            .unwrap()
        else {
            panic!("definition should resolve in the healthy group");
        };
        assert_eq!(path_key(&path), path_key(&target));
        assert_eq!(range, Range::new(Position::new(1, 6), Position::new(1, 11)));
        let failure = engine.prepare_rename(&source, cursor).err().unwrap();
        assert!(failure.to_string().contains("complete package graph"));
    }

    #[test]
    fn workspace_group_errors_use_canonical_ids_and_deduplicate_within_each_group() {
        let project = TestProject::new();
        let roots: Vec<_> = ["first", "second"]
            .map(|folder| project.root.join(folder))
            .into();
        let raw = "example.test/app";
        let mut listing = PackageListing::default();
        for root in &roots {
            let error =
                json!({"ImportStack": [raw, "example.test/missing"], "Err": "missing package"});
            let owner = json!({
                "Dir": root, "ImportPath": raw, "Name": "app", "GoFiles": ["app.go"],
                "DepsErrors": [error, error],
            });
            let missing = json!({"ImportPath": "example.test/missing", "Error": error});
            let records = format!("{owner}\n{missing}");
            listing.add_group(root, read_packages(records.as_bytes()).unwrap());
        }
        let scopes = roots
            .iter()
            .map(|root| GroupScope {
                folders: vec![path_key(root)],
                roots: Vec::new(),
            })
            .collect();
        let workspace = Workspace::from_listing(
            roots.clone(),
            scopes,
            listing,
            false,
            &GoEnvironment::capture(),
        );
        assert_eq!(workspace.package_errors().len(), 4);
        for (group, root) in roots.iter().enumerate() {
            let package = workspace.package_of_file(&root.join("app.go")).unwrap();
            let mut errors = workspace
                .package_errors()
                .iter()
                .filter(|error| error.group_id == group);
            let attributed = errors.next().unwrap();
            assert_eq!(attributed.package_id.as_deref(), Some(package.id.as_str()));
            assert_eq!(attributed.import_path, raw);
            assert_eq!(attributed.import_stack, [raw, "example.test/missing"]);
            let missing = errors.next().unwrap();
            assert!(missing.package_id.is_none());
            assert_eq!(missing.import_path, "example.test/missing");
            assert_eq!(missing.kind, PackageErrorKind::Direct);
        }
        assert!(workspace.package(&format!("1:{raw}")).is_some());
    }

    #[test]
    fn workspace_test_only_package_has_group_local_private_names() {
        let project = TestProject::new();
        let module = "module example.test/same\n\ngo 1.22\n";
        project.write("first/go.mod", module);
        project.write("second/go.mod", module);
        let production = project.write("first/sample.go", "package sample\nfunc private() {}\n");
        let test_only = project.write(
            "second/sample_test.go",
            "package sample\nfunc private() {}\n",
        );
        let folders = ["first", "second"].map(|folder| project.root.join(folder));
        let workspace = Workspace::load_roots(&folders, true).unwrap();
        let production = workspace.package_of_file(&production).unwrap();
        let test_only = workspace.package_of_file(&test_only).unwrap();
        assert_eq!(test_only.test_of(), Some("example.test/same"));
        assert_ne!(
            test_only.private_name_owner(),
            production.private_name_owner()
        );
        assert!(
            workspace
                .package_in_group_by_import(test_only.group_id(), "example.test/same")
                .is_none()
        );
    }

    #[test]
    fn workspace_does_not_discover_unregistered_nested_modules() {
        let project = TestProject::new();
        project.write("go.mod", "module example.test/outer\n\ngo 1.22\n");
        project.write("outer.go", "package outer\n");
        project.write("nested/go.mod", "module example.test/nested\n\ngo 1.22\n");
        let nested = project.write("nested/nested.go", "package nested\n");
        let workspace = Workspace::load(&project.root, false).unwrap();
        assert!(workspace.package("example.test/outer").is_some());
        assert!(workspace.package("example.test/nested").is_none());
        assert!(workspace.package_of_file(&nested).is_none());
        let workspace =
            Workspace::load_roots(&[project.root.clone(), project.root.join("nested")], false)
                .unwrap();
        assert!(workspace.package_of_file(&nested).is_some());
        let outer = workspace.package("example.test/outer").unwrap();
        assert!(
            workspace
                .resolve_import(outer, "example.test/nested")
                .is_none()
        );
    }
}
