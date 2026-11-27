use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use crossbeam_channel::Sender;
use serde::{Deserialize, Serialize};

use crate::go_environment::GoEnvironment;
use crate::semantic::source::is_test_source;
use crate::workspace::{is_symbol_source, path_key};

use super::imports::valid_package;
use super::prefilter::FileNames;
use super::sdk_names::{Token, Tokens, next_nonnewline};

#[derive(Clone)]
pub(super) struct Inventory {
    state: Arc<Mutex<InventoryState>>,
    wake: Sender<()>,
}

#[derive(Default)]
struct InventoryState {
    loading: bool,
    refresh_requested: bool,
    root: Option<PathBuf>,
    ready: Option<Arc<Catalog>>,
    inputs: Option<ProbeInputs>,
}

pub(super) struct Catalog {
    key: Option<BuildKey>,
    pub(super) packages: Vec<Package>,
    pub(super) names: HashMap<PathBuf, FileNames>,
    pub(super) stats: InventoryStats,
}

impl Catalog {
    pub(super) fn root_key(&self) -> Option<&Path> {
        self.key.as_ref().map(|key| key.root.as_path())
    }

    pub(super) fn same_sdk(&self, other: &Self) -> bool {
        self.key
            .as_ref()
            .zip(other.key.as_ref())
            .is_some_and(|(left, right)| left.root == right.root && left.version == right.version)
    }
}

#[derive(Clone, Serialize)]
pub(super) struct InventoryStats {
    pub(super) started_unix_ms: u64,
    pub(super) ready_unix_ms: u64,
    pub(super) context_ms: f64,
    pub(super) discovery_ms: f64,
    pub(super) method: &'static str,
    pub(super) reused: bool,
    pub(super) fallback: Option<String>,
    pub(super) prewarmed_names: usize,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct Package {
    pub(super) path: String,
    pub(super) name: String,
    pub(super) dir: PathBuf,
    pub(super) files: Vec<PathBuf>,
}

#[derive(Default)]
struct NativeInventory {
    packages: Vec<Package>,
    names: HashMap<PathBuf, FileNames>,
}

#[derive(Debug, Eq, PartialEq)]
struct BuildKey {
    root: PathBuf,
    version: (Option<SystemTime>, Vec<u8>),
    context: BuildContext,
    env: BTreeMap<String, String>,
    environment: Arc<BTreeMap<OsString, OsString>>,
}

#[derive(Debug, Eq, PartialEq)]
struct BuildContext {
    os: String,
    arch: String,
    compiler: String,
    cgo: bool,
    tags: HashSet<String>,
}

#[derive(PartialEq, Eq)]
struct ProbeInputs {
    environment: Arc<BTreeMap<OsString, OsString>>,
    files: Vec<(PathBuf, Option<Vec<u8>>)>,
}

impl ProbeInputs {
    fn capture(project: &Path, root: &Path, key: &BuildKey, environment: &GoEnvironment) -> Self {
        let files = std::iter::once(root.join("VERSION"))
            .chain(["GOENV", "GOWORK"].into_iter().filter_map(|name| {
                let path = Path::new(key.env.get(name)?);
                path.is_absolute().then(|| path.to_owned())
            }))
            .chain(
                project
                    .ancestors()
                    .flat_map(|dir| ["go.work", "go.mod"].map(|name| dir.join(name))),
            )
            .map(|path| {
                let bytes = fs::read(&path).ok();
                (path, bytes)
            })
            .collect();
        Self {
            environment: Arc::clone(environment.variables()),
            files,
        }
    }
}

impl Inventory {
    pub(super) fn new(wake: Sender<()>) -> Self {
        Self {
            state: Arc::new(Mutex::new(InventoryState::default())),
            wake,
        }
    }

    pub(super) fn start(&self, project: &Path, environment: &GoEnvironment, refresh: bool) {
        let mut state = self.state.lock().unwrap();
        if state.loading {
            state.refresh_requested |= refresh;
            return;
        }
        if let Some(ready) = &state.ready
            && (!refresh
                || ready.stats.method != "unavailable"
                    && state
                        .root
                        .as_ref()
                        .zip(ready.key.as_ref())
                        .is_some_and(|(root, key)| {
                            state.inputs.as_ref()
                                == Some(&ProbeInputs::capture(project, root, key, environment))
                        }))
        {
            return;
        }
        state.loading = true;
        drop(state);

        let project = project.to_owned();
        let environment = environment.clone();
        let inventory = self.clone();
        if thread::Builder::new()
            .name("go-completion-sdk".into())
            .spawn(move || {
                loop {
                    inventory.load(&project, &environment);
                    let mut state = inventory.state.lock().unwrap();
                    if !std::mem::take(&mut state.refresh_requested) {
                        state.loading = false;
                        let _ = inventory.wake.try_send(());
                        break;
                    }
                }
            })
            .is_err()
        {
            self.state.lock().unwrap().loading = false;
        }
    }

    pub(super) fn root(&self) -> Option<PathBuf> {
        self.state.lock().unwrap().root.clone()
    }

    pub(super) fn is_loading(&self) -> bool {
        self.state.lock().unwrap().loading
    }

    pub(super) fn ready(&self) -> Option<Arc<Catalog>> {
        let state = self.state.lock().unwrap();
        (!state.loading).then(|| state.ready.clone()).flatten()
    }

    fn load(&self, project: &Path, environment: &GoEnvironment) {
        let started = Instant::now();
        let mut stats = InventoryStats {
            started_unix_ms: unix_ms(),
            ready_unix_ms: 0,
            context_ms: 0.0,
            discovery_ms: 0.0,
            method: "unavailable",
            reused: false,
            fallback: None,
            prewarmed_names: 0,
        };
        let probed = build_context(project, environment).ok();
        stats.context_ms = started.elapsed().as_secs_f64() * 1000.0;
        self.publish(project, environment, probed, stats);
    }

    fn publish(
        &self,
        project: &Path,
        environment: &GoEnvironment,
        probed: Option<(PathBuf, BuildContext, BTreeMap<String, String>)>,
        mut stats: InventoryStats,
    ) {
        let discovery = Instant::now();
        let mut key = None;
        let mut packages = Vec::new();
        let mut names = HashMap::new();
        if let Some((root, context, env)) = probed {
            key = Some(BuildKey::new(&root, context, env, environment));
            let inputs = ProbeInputs::capture(project, &root, key.as_ref().unwrap(), environment);
            let mut state = self.state.lock().unwrap();
            state.root = Some(root.clone());
            state.inputs = Some(inputs);
            if let Some(previous) = &state.ready
                && previous.key == key
                && previous.stats.method != "unavailable"
            {
                return;
            }
            drop(state);
            let build = key.as_ref().unwrap();
            let src = root.join("src");
            if native_supported(build) {
                match native_inventory(&src, &build.context) {
                    Ok(native) => {
                        packages = native.packages;
                        names = native.names;
                        stats.method = "native";
                    }
                    Err(error) => stats.fallback = Some(format!("{error:#}")),
                }
            }
            if stats.method != "native"
                && let Ok(entries) =
                    command_inventory(project, &root, environment, Some(&build.env))
            {
                packages = entries;
                stats.method = "go list std";
            }
        }
        stats.discovery_ms = discovery.elapsed().as_secs_f64() * 1000.0;
        stats.ready_unix_ms = unix_ms();
        stats.prewarmed_names = names.len();
        let mut state = self.state.lock().unwrap();
        if key.is_none() {
            state.root = None;
            state.inputs = None;
        }
        state.ready = Some(Arc::new(Catalog {
            key,
            packages,
            names,
            stats,
        }));
    }
}

pub(super) fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

impl BuildKey {
    fn new(
        root: &Path,
        context: BuildContext,
        env: BTreeMap<String, String>,
        environment: &GoEnvironment,
    ) -> Self {
        let version = root.join("VERSION");
        Self {
            root: path_key(&root.canonicalize().unwrap_or_else(|_| root.to_owned())),
            version: (
                fs::metadata(&version).and_then(|file| file.modified()).ok(),
                fs::read(version).unwrap_or_default(),
            ),
            context,
            env,
            environment: Arc::clone(environment.variables()),
        }
    }
}

fn go_output(mut go: Command, project: &Path, args: &[&str]) -> Result<Output> {
    let output = go
        .args(args)
        .current_dir(project)
        .stdin(Stdio::null())
        .output()?;
    ensure!(
        output.status.success(),
        "Go SDK probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}

fn sdk_go(
    project: &Path,
    root: &Path,
    environment: &GoEnvironment,
    env: &BTreeMap<String, String>,
    args: &[&str],
) -> Result<Output> {
    let installed = root
        .join("bin")
        .join(format!("go{}", std::env::consts::EXE_SUFFIX));
    let executable = if installed.is_file() {
        &installed
    } else {
        Path::new("go")
    };
    let mut go = environment.command(executable)?;
    // Keep cmd/go's effective build settings even when PATH first resolves
    // to a launcher. Graph loads continue to use the configured launcher.
    go.envs(env.iter().filter(|(name, _)| {
        !matches!(name.as_str(), "GOVERSION" | "GOTOOLDIR")
            && environment.get(name).is_none_or(|value| !value.is_empty())
    }))
    .env("GOTOOLCHAIN", "local");
    go_output(go, project, args)
}

fn build_context(
    project: &Path,
    environment: &GoEnvironment,
) -> Result<(PathBuf, BuildContext, BTreeMap<String, String>)> {
    let env = go_output(
        environment.command("go")?,
        project,
        &[
            "env",
            "-json",
            "GOROOT",
            "GOOS",
            "GOARCH",
            "CGO_ENABLED",
            "GOFLAGS",
            "GOEXPERIMENT",
            "GOFIPS140",
            "GOTOOLCHAIN",
            "GOTOOLDIR",
            "GOVERSION",
            "GOENV",
            "GOWORK",
            "GO111MODULE",
            "GOAMD64",
            "GO386",
            "GOARM",
            "GOARM64",
            "GOLOONG64",
            "GOMIPS",
            "GOMIPS64",
            "GOPPC64",
            "GORISCV64",
            "GOWASM",
        ],
    )?;
    let env: BTreeMap<String, String> = serde_json::from_slice(&env.stdout)?;
    let root = PathBuf::from(env.get("GOROOT").map_or("", String::as_str));
    ensure!(root.is_absolute(), "Go SDK probe did not return GOROOT");

    // Build, tool and release tags include defaults that vary with Go
    // versions, architectures and compiler experiments.
    let output = sdk_go(
        project,
        &root,
        environment,
        &env,
        &[
            "list",
            "-e",
            "-find",
            "-f",
            concat!(
                "{{println context.GOOS}}{{println context.GOARCH}}",
                "{{println context.Compiler}}{{println context.CgoEnabled}}",
                "{{range context.BuildTags}}{{println .}}{{end}}",
                "{{range context.ToolTags}}{{println .}}{{end}}",
                "{{range context.ReleaseTags}}{{println .}}{{end}}"
            ),
            "unsafe",
        ],
    )?;
    let mut lines = std::str::from_utf8(&output.stdout)?.lines();
    let context = BuildContext {
        os: lines.next().unwrap_or_default().into(),
        arch: lines.next().unwrap_or_default().into(),
        compiler: lines.next().unwrap_or_default().into(),
        cgo: lines.next().unwrap_or_default().parse()?,
        tags: lines
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect(),
    };
    Ok((root, context, env))
}

fn native_supported(build: &BuildKey) -> bool {
    if build.context.compiler != "gc"
        || build
            .env
            .get("GOFIPS140")
            .is_some_and(|value| !matches!(value.as_str(), "" | "off"))
    {
        return false;
    }
    // An overlay, alternate compiler, or other build option can select
    // sources outside this tree. Leave unfamiliar flags to cmd/go.
    let flags = build.env.get("GOFLAGS").map_or("", String::as_str);
    flags.split_ascii_whitespace().all(|flag| {
        flag.starts_with("-tags=")
            || matches!(
                flag,
                "-race" | "-msan" | "-asan" | "-mod=vendor" | "-mod=readonly" | "-mod=mod"
            )
    })
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
struct ListedPackage {
    dir: PathBuf,
    import_path: String,
    name: String,
    go_files: Vec<PathBuf>,
    cgo_files: Vec<PathBuf>,
}

fn command_inventory(
    project: &Path,
    root: &Path,
    environment: &GoEnvironment,
    env: Option<&BTreeMap<String, String>>,
) -> Result<Vec<Package>> {
    let args = &[
        "list",
        "-e",
        "-json=Dir,ImportPath,Name,GoFiles,CgoFiles",
        "std",
    ];
    let output = if let Some(env) = env {
        sdk_go(project, root, environment, env, args)?
    } else {
        go_output(environment.command("go")?, project, args)?
    };
    let src = path_key(&root.join("src"));
    let entries = serde_json::Deserializer::from_slice(&output.stdout).into_iter::<ListedPackage>();
    let mut packages = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !path_key(&entry.dir).starts_with(&src)
            || entry.import_path == "cmd"
            || entry.import_path.starts_with("cmd/")
            || !valid_package(&entry.import_path, &entry.name)
        {
            continue;
        }
        let files = entry
            .go_files
            .into_iter()
            .chain(entry.cgo_files)
            .filter(|file| is_symbol_source(file) && !is_test_source(file))
            .map(|file| entry.dir.join(file))
            .collect::<Vec<_>>();
        if !files.is_empty() {
            packages.push(Package {
                path: entry.import_path,
                name: entry.name,
                dir: entry.dir,
                files,
            });
        }
    }
    Ok(packages)
}

fn native_inventory(src: &Path, build: &BuildContext) -> Result<NativeInventory> {
    let mut inputs = Vec::new();
    let mut dirs = vec![(String::new(), src.to_owned())];
    while let Some((path, dir)) = dirs.pop() {
        let mut files = Vec::new();
        for entry in dir.read_dir()? {
            let entry = entry?;
            let file = entry.file_name();
            let Some(file) = file.to_str() else {
                continue;
            };
            if file.starts_with(['.', '_']) {
                continue;
            }
            let kind = entry.file_type()?;
            if kind.is_dir() {
                if matches!(file, "testdata" | "vendor")
                    || path.is_empty() && (matches!(file, "cmd" | "builtin") || file.contains('.'))
                {
                    continue;
                }
                let path = if path.is_empty() {
                    file.into()
                } else {
                    format!("{path}/{file}")
                };
                if path != "runtime/cgo" || build.cgo {
                    dirs.push((path, entry.path()));
                }
            } else if !path.is_empty()
                && file.ends_with(".go")
                && !file.ends_with("_test.go")
                && build.matches_filename(file)
            {
                files.push(entry.path());
            }
        }
        if !files.is_empty() {
            inputs.push((path, dir, files));
        }
    }
    let next = AtomicUsize::new(0);
    let count = thread::available_parallelism()
        .map_or(1, usize::from)
        .min(4);
    let mut inventory = thread::scope(|scope| -> Result<NativeInventory> {
        let workers: Vec<_> = (0..count)
            .map(|_| {
                scope.spawn(|| -> Result<NativeInventory> {
                    let mut output = NativeInventory::default();
                    while let Some((path, dir, files)) =
                        inputs.get(next.fetch_add(1, Ordering::Relaxed))
                    {
                        output.packages.extend(read_sdk_package(
                            path,
                            dir,
                            files,
                            build,
                            &mut output.names,
                        )?);
                    }
                    Ok(output)
                })
            })
            .collect();
        let mut output = NativeInventory::default();
        for worker in workers {
            let worker = worker.join().unwrap()?;
            output.packages.extend(worker.packages);
            output.names.extend(worker.names);
        }
        Ok(output)
    })?;
    inventory
        .packages
        .sort_unstable_by(|left, right| left.path.cmp(&right.path));
    Ok(inventory)
}

fn read_sdk_package(
    path: &str,
    dir: &Path,
    files: &[PathBuf],
    build: &BuildContext,
    cached_names: &mut HashMap<PathBuf, FileNames>,
) -> Result<Option<Package>> {
    let mut go_files = Vec::new();
    let mut cgo_files = Vec::new();
    let mut name = String::new();
    for file in files {
        let Some((declared, cgo, names)) =
            sdk_file(file, build).with_context(|| format!("checking {}", file.display()))?
        else {
            continue;
        };
        if declared == "documentation" || cgo && !build.cgo {
            continue;
        }
        ensure!(
            name.is_empty() || name == declared,
            "mixed SDK package {path}"
        );
        if name.is_empty() {
            name = declared;
        }
        if cgo {
            cgo_files.push(file.clone());
        } else {
            go_files.push(file.clone());
        }
        if let Some(names) = names {
            cached_names.insert(path_key(file), names);
        }
    }
    if !valid_package(path, &name) {
        return Ok(None);
    }
    go_files.sort_unstable();
    cgo_files.sort_unstable();
    go_files.extend(cgo_files);
    Ok(Some(Package {
        path: path.to_owned(),
        name,
        dir: dir.to_owned(),
        files: go_files,
    }))
}

fn sdk_file(
    path: &Path,
    build: &BuildContext,
) -> Result<Option<(String, bool, Option<FileNames>)>> {
    let mut file = File::open(path)?;
    let mut bytes = [0; 8192];
    let n = file.read(&mut bytes)?;
    let truncated = n == bytes.len();
    let text = match std::str::from_utf8(&bytes[..n]) {
        Ok(text) => text,
        Err(error) if truncated && error.error_len().is_none() => {
            std::str::from_utf8(&bytes[..error.valid_up_to()])?
        }
        Err(error) => return Err(error.into()),
    };
    let check = |text: &str, truncated| -> Result<Option<(String, bool)>> {
        if !build.matches_header(text)? {
            return Ok(None);
        }
        let (name, cgo) = package_header(text, truncated)?;
        Ok(Some((name.to_owned(), cgo)))
    };
    let mut long_header = None;
    let selected = match check(text, truncated) {
        Err(_) if truncated => {
            let mut header = bytes.to_vec();
            file.read_to_end(&mut header)?;
            let selected = check(std::str::from_utf8(&header)?, false);
            long_header = Some(header);
            selected
        }
        result => result,
    }?;
    let Some((name, cgo)) = selected else {
        return Ok(None);
    };
    // Reuse the header read while the SDK is being inventoried. Keep only
    // the scanned names and positions, even for files that will later match.
    let mut names = None;
    if valid_package("sdk", &name)
        && (!cgo || build.cgo)
        && let Ok(metadata) = file.metadata()
        && metadata.len() <= 2 * 1024 * 1024
    {
        let mut contents = long_header.unwrap_or_default();
        if !truncated {
            names = Some(FileNames::new(metadata.into(), text, true));
        } else {
            if contents.is_empty() {
                contents.reserve_exact(metadata.len() as usize);
                contents.extend_from_slice(&bytes);
                file.read_to_end(&mut contents)?;
            }
            if let Ok(text) = std::str::from_utf8(&contents) {
                names = Some(FileNames::new(metadata.into(), text, true));
            }
        }
    }
    Ok(Some((name, cgo, names)))
}

fn package_header(text: &str, truncated: bool) -> Result<(&str, bool)> {
    let mut tokens = Tokens::new(text);
    ensure!(
        matches!(next_nonnewline(&mut tokens), Some(Token::Word("package"))),
        "invalid SDK package clause"
    );
    let Some(Token::Word(name)) = tokens.next() else {
        anyhow::bail!("missing SDK package name");
    };
    ensure!(
        name.chars()
            .next()
            .is_some_and(|ch| ch == '_' || ch.is_alphabetic()),
        "invalid SDK package name"
    );
    let mut cgo = false;
    let mut next = tokens.next();
    while matches!(next, Some(Token::Newline | Token::Punct(b';', _))) {
        next = next_nonnewline(&mut tokens);
        if !matches!(next, Some(Token::Word("import"))) {
            break;
        }
        let mut first = next_nonnewline(&mut tokens);
        let group = matches!(first, Some(Token::Punct(b'(', _)));
        if group {
            first = next_nonnewline(&mut tokens);
        }
        loop {
            if group && matches!(first, Some(Token::Punct(b')', _))) {
                break;
            }
            if matches!(first, Some(Token::Word(_) | Token::Punct(b'.', _))) {
                first = tokens.next();
            }
            let Some(Token::Literal(path)) = first else {
                anyhow::bail!("invalid SDK import");
            };
            ensure!(
                path.as_bytes()[0] != b'\'' && !path.contains('\\'),
                "unusual SDK import path"
            );
            cgo |= &path[1..path.len() - 1] == "C";
            if !group {
                break;
            }
            first = tokens.find(|token| !matches!(token, Token::Newline | Token::Punct(b';', _)));
        }
        next = tokens.next();
    }
    ensure!(
        next.is_some() || !truncated,
        "SDK header exceeds read limit"
    );
    Ok((name, cgo))
}

const KNOWN_OS: &[&str] = &[
    "aix",
    "android",
    "darwin",
    "dragonfly",
    "freebsd",
    "hurd",
    "illumos",
    "ios",
    "js",
    "linux",
    "nacl",
    "netbsd",
    "openbsd",
    "plan9",
    "solaris",
    "wasip1",
    "windows",
    "zos",
];
const KNOWN_ARCH: &[&str] = &[
    "386",
    "amd64",
    "amd64p32",
    "arm",
    "armbe",
    "arm64",
    "arm64be",
    "loong64",
    "mips",
    "mipsle",
    "mips64",
    "mips64le",
    "mips64p32",
    "mips64p32le",
    "ppc",
    "ppc64",
    "ppc64le",
    "riscv",
    "riscv64",
    "s390",
    "s390x",
    "sparc",
    "sparc64",
    "wasm",
];

impl BuildContext {
    fn matches_tag(&self, tag: &str) -> bool {
        tag == self.os
            || tag == self.arch
            || tag == self.compiler
            || tag == "cgo" && self.cgo
            || matches!(
                (self.os.as_str(), tag),
                ("android", "linux") | ("illumos", "solaris") | ("ios", "darwin")
            )
            || tag == "unix"
                && matches!(
                    self.os.as_str(),
                    "aix"
                        | "android"
                        | "darwin"
                        | "dragonfly"
                        | "freebsd"
                        | "hurd"
                        | "illumos"
                        | "ios"
                        | "linux"
                        | "netbsd"
                        | "openbsd"
                        | "solaris"
                )
            || self.tags.contains(if tag == "boringcrypto" {
                "goexperiment.boringcrypto"
            } else {
                tag
            })
    }

    fn matches_filename(&self, filename: &str) -> bool {
        let stem = filename.split('.').next().unwrap_or_default();
        let Some((_, suffix)) = stem.split_once('_') else {
            return true;
        };
        let mut parts = suffix.rsplit('_');
        let last = parts.next().unwrap_or_default();
        if KNOWN_ARCH.contains(&last) {
            self.matches_tag(last)
                && parts
                    .next()
                    .is_none_or(|os| !KNOWN_OS.contains(&os) || self.matches_tag(os))
        } else {
            !KNOWN_OS.contains(&last) || self.matches_tag(last)
        }
    }

    fn matches_header(&self, text: &str) -> Result<bool> {
        let mut pending_legacy = true;
        let mut legacy = true;
        let mut legacy_ended = false;
        let mut modern = None;
        let mut in_block = false;
        'lines: for line in text.trim_start_matches('\u{feff}').lines() {
            let mut line = line.trim();
            if line.is_empty() && !legacy_ended {
                legacy = pending_legacy;
                continue;
            }
            legacy_ended |= !line.starts_with("//");
            if !in_block {
                if let Some(directive) = directive(line, "//go:build") {
                    ensure!(modern.is_none(), "duplicate SDK build constraint");
                    let mut parser = Constraint {
                        rest: directive,
                        build: self,
                    };
                    let value = parser.or(0);
                    ensure!(
                        parser.rest.trim().is_empty() && value.is_some(),
                        "invalid SDK build constraint"
                    );
                    modern = value;
                }
                if !legacy_ended
                    && let Some(tags) = line
                        .strip_prefix("//")
                        .and_then(|line| directive(line.trim_start(), "+build"))
                {
                    pending_legacy &= tags.split_whitespace().any(|option| {
                        option.split(',').all(|tag| {
                            tag.strip_prefix('!')
                                .map_or_else(|| self.matches_tag(tag), |tag| !self.matches_tag(tag))
                        })
                    });
                }
            }
            while !line.is_empty() {
                if in_block {
                    let Some((_, remaining)) = line.split_once("*/") else {
                        continue 'lines;
                    };
                    line = remaining.trim_start();
                    in_block = false;
                } else if line.starts_with("//") {
                    continue 'lines;
                } else if let Some(remaining) = line.strip_prefix("/*") {
                    line = remaining.trim_start();
                    in_block = true;
                } else {
                    break 'lines;
                }
            }
        }
        Ok(modern.unwrap_or(legacy))
    }
}

fn directive<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(prefix)?;
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then(|| rest.trim())
}

#[cfg(test)]
pub(super) fn compare_current_sdk(project: &Path) -> Result<()> {
    let environment = GoEnvironment::capture();
    let (root, context, env) = build_context(project, &environment)?;
    let build = BuildKey::new(&root, context, env, &environment);
    if native_supported(&build) {
        let src = root.join("src");
        let actual = native_inventory(&src, &build.context)?.packages;
        let mut expected = command_inventory(project, &root, &environment, None)?;
        expected.sort_unstable_by(|left, right| left.path.cmp(&right.path));
        let actual_paths: HashSet<_> = actual.iter().map(|pkg| pkg.path.as_str()).collect();
        let expected_paths: HashSet<_> = expected.iter().map(|pkg| pkg.path.as_str()).collect();
        ensure!(
            actual_paths == expected_paths,
            "extra packages: {:?}, missing packages: {:?}",
            actual_paths.difference(&expected_paths).collect::<Vec<_>>(),
            expected_paths.difference(&actual_paths).collect::<Vec<_>>(),
        );
        for (actual, expected) in actual.iter().zip(&expected) {
            ensure!(
                actual == expected,
                "SDK inventory differs in {}\nnative: {actual:?}\ngo list: {expected:?}",
                actual.path
            );
        }
    }
    Ok(())
}

struct Constraint<'a> {
    rest: &'a str,
    build: &'a BuildContext,
}

impl Constraint<'_> {
    fn take(&mut self, token: &str) -> bool {
        if let Some(rest) = self.rest.trim_start().strip_prefix(token) {
            self.rest = rest;
            true
        } else {
            false
        }
    }

    fn or(&mut self, depth: usize) -> Option<bool> {
        let mut result = self.and(depth)?;
        while self.take("||") {
            result |= self.and(depth)?;
        }
        Some(result)
    }

    fn and(&mut self, depth: usize) -> Option<bool> {
        let mut result = self.term(depth)?;
        while self.take("&&") {
            result &= self.term(depth)?;
        }
        Some(result)
    }

    fn term(&mut self, depth: usize) -> Option<bool> {
        if depth >= 64 {
            return None;
        }
        if self.take("!") {
            return Some(!self.term(depth + 1)?);
        }
        if self.take("(") {
            let result = self.or(depth + 1)?;
            return self.take(")").then_some(result);
        }
        self.rest = self.rest.trim_start();
        let end = self
            .rest
            .find(|ch: char| !(ch == '.' || ch == '_' || ch.is_alphanumeric()))
            .unwrap_or(self.rest.len());
        let (tag, rest) = self.rest.split_at(end);
        self.rest = rest;
        (!tag.is_empty()).then(|| self.build.matches_tag(tag))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestProject;

    #[test]
    fn inventory_reuses_a_matching_build_key_and_replaces_changed_inputs() {
        let project = TestProject::new();
        let version = project.write("sdk/VERSION", "go1.test");
        let sdk = version.parent().unwrap();
        project.write("sdk/src/p/a_windows_amd64.go", "package p\n");
        project.write("sdk/src/p/ignored_linux.go", "package p\n");
        project.write("sdk/src/p/ignored_test.go", "package p\n");
        project.write(
            "sdk/src/p/b.go",
            "//go:build amd64.v3 && (gc || arm)\n\npackage p\n",
        );
        project.write("sdk/src/p/c.go", "// +build legacy\n\npackage p\n");
        project.write("sdk/src/p/z.go", "package p\nimport \"C\"\n");
        project.write("sdk/src/p/testdata/q/hidden.go", "package q\n");
        project.write("sdk/src/vendor/q/hidden.go", "package q\n");
        project.write("sdk/src/p/internal/q/visible.go", "package q\n");
        let (wake, _) = crossbeam_channel::bounded(1);
        let inventory = Inventory::new(wake);
        let startup = GoEnvironment::capture();
        let configured = |value: &str| {
            startup.with_overrides(BTreeMap::from([("GOFLAGS".to_owned(), value.to_owned())]))
        };
        let environment = configured("");
        let changed = configured("-tags=extra");
        let set = |environment: &GoEnvironment, tags: &[&str], cgo| {
            let build = BuildContext {
                os: "windows".into(),
                arch: "amd64".into(),
                compiler: "gc".into(),
                cgo,
                tags: tags.iter().map(|tag| (*tag).into()).collect(),
            };
            inventory.publish(
                &project.root,
                environment,
                Some((sdk.to_owned(), build, BTreeMap::new())),
                InventoryStats {
                    started_unix_ms: 0,
                    ready_unix_ms: 0,
                    context_ms: 0.0,
                    discovery_ms: 0.0,
                    method: "unavailable",
                    reused: false,
                    fallback: None,
                    prewarmed_names: 0,
                },
            );
            inventory.ready().unwrap()
        };
        let base = set(&environment, &[], false);
        assert_eq!(base.packages.len(), 2);
        assert_eq!(base.packages[0].path, "p");
        assert_eq!(
            base.packages[0].files,
            [sdk.join("src/p/a_windows_amd64.go")]
        );
        let key = base.key.as_ref().unwrap();
        let inputs = ProbeInputs::capture(&project.root, sdk, key, &environment);
        assert!(inputs == ProbeInputs::capture(&project.root, sdk, key, &configured("")));
        assert!(inputs != ProbeInputs::capture(&project.root, sdk, key, &changed));
        assert!(Arc::ptr_eq(&base, &set(&configured(""), &[], false)));

        let replacement = set(&changed, &[], false);
        assert!(!Arc::ptr_eq(&base, &replacement));
        assert!(Arc::ptr_eq(&replacement, &set(&changed, &[], false)));

        let tagged = set(&changed, &["amd64.v3", "legacy"], true);
        assert!(!Arc::ptr_eq(&replacement, &tagged));
        assert_eq!(tagged.packages[0].files.len(), 4);
        std::fs::write(&version, "go1.next").unwrap();
        assert!(!Arc::ptr_eq(
            &tagged,
            &set(&changed, &["amd64.v3", "legacy"], true)
        ));
    }
}
