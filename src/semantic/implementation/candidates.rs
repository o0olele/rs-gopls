use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::thread;
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError, bounded};

use super::super::*;
use super::{
    interface_probe::{Arity, InterfaceSpellings, interface_spellings},
    method_sets::MethodShape,
    stats::IndexStats,
};

#[cfg(test)]
mod tests;

#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) enum PackageProbeKey {
    Implementations(Vec<MethodShape>),
    InterfaceMethod(String),
    Interface(Vec<MethodShape>),
}

#[derive(Clone, Eq, Hash, PartialEq)]
struct TypeKey {
    package: usize,
    name: String,
}

impl TypeKey {
    fn new(package: usize, name: &str) -> Self {
        Self {
            package,
            name: name.to_owned(),
        }
    }
}

type Postings = HashMap<String, HashMap<Option<Arity>, HashSet<TypeKey>>>;
type FileSpellings = HashMap<PathBuf, Option<Arc<InterfaceSpellings>>>;
type PackageSummaries = HashMap<String, Arc<PackageSpellings>>;

#[derive(Default)]
struct TypePostings {
    package_ids: Vec<String>,
    package_numbers: HashMap<String, usize>,
    receivers: Postings,
    interfaces: Postings,
    declarations: HashMap<TypeKey, Vec<PathBuf>>,
    derived: HashMap<TypeKey, HashSet<TypeKey>>,
    implicit_error: HashSet<TypeKey>,
    unknown_types: HashSet<TypeKey>,
    uncertain: HashSet<usize>,
    local_types: HashSet<PathBuf>,
}

#[derive(Default)]
pub(in crate::semantic) struct InterfaceSources {
    files: Arc<FileSpellings>,
    packages: Arc<PackageSummaries>,
    owners: HashMap<PathBuf, Vec<String>>,
    directories: HashMap<PathBuf, Vec<String>>,
    pending: VecDeque<String>,
    partial: Option<PackageSpellings>,
    postings: Option<TypePostings>,
    build: Option<PendingPostings>,
    worker: PostingsWorker,
    stats: IndexStats,
    #[cfg(test)]
    pub(super) file_probes: usize,
}

struct PackageSpellings {
    files: Vec<PathBuf>,
    next: usize,
    load_error: bool,
}

impl InterfaceSources {
    pub(in crate::semantic) fn invalidate(&mut self, key: &Path) {
        let owners = self
            .owners
            .get(key)
            .or_else(|| key.parent().and_then(|dir| self.directories.get(dir)));
        if owners.is_some() {
            self.build = None;
            self.postings = None;
        }
        if self.files.contains_key(key) {
            Arc::make_mut(&mut self.files).remove(key);
        }
        for id in owners.into_iter().flatten() {
            if self.pending.front() == Some(id) {
                self.partial = None;
            } else if self.packages.contains_key(id) {
                Arc::make_mut(&mut self.packages).remove(id);
                self.pending.push_back(id.clone());
            }
        }
    }

    pub(in crate::semantic) fn retain_workspace(&mut self, workspace: &Workspace) {
        self.cancel_build();
        self.owners.clear();
        self.directories.clear();
        self.pending.clear();
        self.partial = None;
        self.postings = None;
        self.stats = IndexStats::new();
        Arc::make_mut(&mut self.packages).retain(|id, summary| {
            workspace.package(id).is_some_and(|package| {
                package.files == summary.files && package.load_error == summary.load_error
            })
        });
        for package in workspace.navigation_packages() {
            if !self.packages.contains_key(&package.id) {
                self.pending.push_back(package.id.clone());
            }
            for file in &package.files {
                self.owners
                    .entry(path_key(file))
                    .or_default()
                    .push(package.id.clone());
            }
            self.directories
                .entry(package.directory_and_name().0.to_owned())
                .or_default()
                .push(package.id.clone());
        }
        Arc::make_mut(&mut self.files).retain(|key, _| self.owners.contains_key(key));
    }

    pub(in crate::semantic) fn pending(&self) -> bool {
        self.postings.is_none()
    }

    pub(in crate::semantic) fn ready(&self) -> bool {
        self.build
            .as_ref()
            .is_none_or(|build| build.ready(&self.worker))
    }

    pub(in crate::semantic) fn wake(&self) -> &Receiver<()> {
        &self.worker.wake
    }

    pub(in crate::semantic) fn cancel_build(&mut self) {
        // Each job has its own result channel. Dropping it prevents a previous
        // source revision or package view from publishing, even if it finished.
        self.build = None;
    }

    pub(in crate::semantic) fn advance(
        &mut self,
        workspace: &Arc<Workspace>,
        resolver: &mut Resolver<'_, '_>,
    ) -> Result<bool> {
        if !self.pending() {
            return Ok(true);
        }
        if self.pending.is_empty() {
            return self.advance_build(workspace);
        }
        let started = Instant::now();
        self.stats.begin();
        for _ in 0..32 {
            let Some(id) = self.pending.front() else {
                break;
            };
            let package = resolver.workspace.package(id).expect("active package");
            let summary = self.partial.get_or_insert_with(|| PackageSpellings {
                files: resolver.indexes.files(package).into_owned(),
                next: 0,
                load_error: package.load_error,
            });
            if let Some(file) = summary.files.get(summary.next) {
                summary.next += 1;
                let key = path_key(file);
                let overlay = resolver.indexes.documents.get(&key);
                #[cfg(test)]
                if overlay.is_none() {
                    self.file_probes += 1;
                }
                Arc::make_mut(&mut self.files)
                    .entry(key.clone())
                    .or_insert_with(|| {
                        let spellings = match overlay {
                            Some(document) if document.tree.root_node().has_error() => None,
                            Some(document) => interface_spellings(&document.text),
                            None => {
                                let disk = resolver
                                    .indexes
                                    .disk_index
                                    .get(&key)
                                    .map(|index| index.fingerprint);
                                resolver.indexes.navigation_text(file, &key).ok().and_then(
                                    |(text, read)| {
                                        if read {
                                            self.stats.read(text.len());
                                        }
                                        if disk.is_some_and(|fingerprint| {
                                            fingerprint != text_fingerprint(text)
                                        }) {
                                            return None;
                                        }
                                        interface_spellings(text)
                                    },
                                )
                            }
                        };
                        spellings.map(Arc::new)
                    });
            } else {
                let id = self.pending.pop_front().expect("completed package probe");
                Arc::make_mut(&mut self.packages)
                    .insert(id, Arc::new(self.partial.take().unwrap()));
            }
            if started.elapsed() >= Duration::from_millis(2) {
                break;
            }
        }
        self.stats.add_build(started);
        Ok(false)
    }

    fn advance_build(&mut self, workspace: &Arc<Workspace>) -> Result<bool> {
        if !self.ready() {
            return Ok(false);
        }
        let started = telemetry::now();
        self.stats.begin();
        if let Some(build) = &self.build
            && !Arc::ptr_eq(&build.workspace, workspace)
        {
            self.cancel_build();
        }
        let build = self.build.get_or_insert_with(|| {
            PendingPostings::new(workspace.clone(), self.packages.clone(), self.files.clone())
        });
        let phase = if build.job.is_some() {
            "submit"
        } else {
            "publish"
        };
        let result = match build.poll(&mut self.worker) {
            Ok(result) => result,
            Err(error) => {
                self.cancel_build();
                return Err(error);
            }
        };
        if let Some(result) = &result {
            self.stats.add_duration(result.duration);
            self.stats.ready(self.packages.len(), self.files.len());
        }
        let measurement = result.map(|result| {
            self.postings = Some(result.index);
            self.build = None;
            (result.duration, result.finished)
        });
        if let Some(started) = started {
            telemetry::event!("postings_step", {
                "phase": phase, "step_ms": telemetry::elapsed(started),
            });
            if let Some((duration, finished)) = measurement {
                telemetry::event!("postings_build", {
                    "build_ms": telemetry::millis(duration),
                    "publish_wait_ms": telemetry::millis(started.saturating_duration_since(finished)),
                    "packages": self.packages.len(), "files": self.files.len(),
                    "include_tests": workspace.includes_tests(),
                });
            }
        }
        Ok(self.postings.is_some())
    }

    pub(in crate::semantic) fn embedding_files(
        &self,
        file: &Path,
        name: &str,
        local_only: bool,
    ) -> Option<HashSet<PathBuf>> {
        let postings = self.postings.as_ref()?;
        let types = if local_only {
            HashSet::new()
        } else {
            let ids = self.owners.get(&path_key(file)).or_else(|| {
                let directory = path_key(file.parent()?);
                self.directories.get(&directory)
            });
            let seeds = ids.into_iter().flatten().filter_map(|id| {
                postings
                    .package_numbers
                    .get(id)
                    .map(|&package| TypeKey::new(package, name))
            });
            postings.inherited(seeds.chain(postings.unknown_seeds()))
        };
        Some(
            types
                .iter()
                .filter_map(|ty| postings.declarations.get(ty))
                .flatten()
                .cloned()
                .chain(
                    postings
                        .uncertain
                        .iter()
                        .filter_map(|&number| self.packages.get(postings.package_id(number)))
                        .flat_map(|p| p.files.iter().cloned()),
                )
                .chain(postings.local_types.iter().cloned())
                .map(|file| path_key(&file))
                .collect(),
        )
    }
}

struct PostingsWorker {
    queue: Option<Sender<PostingsJob>>,
    signal: Sender<()>,
    wake: Receiver<()>,
}

impl Default for PostingsWorker {
    fn default() -> Self {
        let (signal, wake) = bounded(1);
        Self {
            queue: None,
            signal,
            wake,
        }
    }
}

impl PostingsWorker {
    fn queue(&mut self) -> Result<&Sender<PostingsJob>> {
        if self.queue.is_none() {
            let (queue, jobs) = bounded::<PostingsJob>(1);
            let signal = self.signal.clone();
            thread::Builder::new()
                .name("go-type-postings".into())
                .spawn(move || {
                    while let Ok(job) = jobs.recv() {
                        let _ = signal.try_send(());
                        job.run();
                        let _ = signal.try_send(());
                    }
                })
                .context("starting Go type postings worker")?;
            self.queue = Some(queue);
        }
        Ok(self.queue.as_ref().unwrap())
    }
}

struct PostingsJob {
    workspace: Arc<Workspace>,
    packages: Arc<PackageSummaries>,
    files: Arc<FileSpellings>,
    cancelled: Arc<AtomicBool>,
    done: Sender<PostingsResult>,
}

struct PostingsResult {
    index: TypePostings,
    duration: Duration,
    finished: Instant,
}

impl PostingsJob {
    fn run(self) {
        let started = Instant::now();
        let index = TypePostings::build(
            &self.workspace,
            &self.packages,
            &self.files,
            &self.cancelled,
        );
        let duration = started.elapsed();
        // Release the read-only snapshots before the main loop resumes writing.
        drop(self.packages);
        drop(self.files);
        if let Some(index) = index
            && !self.cancelled.load(Relaxed)
        {
            let _ = self.done.send(PostingsResult {
                index,
                duration,
                finished: Instant::now(),
            });
        }
    }
}

struct PendingPostings {
    workspace: Arc<Workspace>,
    job: Option<PostingsJob>,
    result: Receiver<PostingsResult>,
    cancelled: Arc<AtomicBool>,
}

impl PendingPostings {
    fn new(
        workspace: Arc<Workspace>,
        packages: Arc<PackageSummaries>,
        files: Arc<FileSpellings>,
    ) -> Self {
        let (done, result) = bounded(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        Self {
            job: Some(PostingsJob {
                workspace: workspace.clone(),
                packages,
                files,
                cancelled: cancelled.clone(),
                done,
            }),
            workspace,
            result,
            cancelled,
        }
    }

    fn ready(&self, worker: &PostingsWorker) -> bool {
        if self.job.is_some() {
            return worker.queue.as_ref().is_none_or(|queue| !queue.is_full());
        }
        !self.result.is_empty()
    }

    fn poll(&mut self, worker: &mut PostingsWorker) -> Result<Option<PostingsResult>> {
        if let Some(job) = self.job.take() {
            match worker.queue()?.try_send(job) {
                Ok(()) => {}
                Err(TrySendError::Full(job)) => self.job = Some(job),
                Err(TrySendError::Disconnected(_)) => {
                    anyhow::bail!("Go type postings worker exited")
                }
            }
            return Ok(None);
        }
        match self.result.try_recv() {
            Ok(result) => Ok(Some(result)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => {
                anyhow::bail!("Go type postings worker dropped its result")
            }
        }
    }
}

impl Drop for PendingPostings {
    fn drop(&mut self) {
        self.cancelled.store(true, Relaxed);
    }
}

impl TypePostings {
    fn build(
        workspace: &Workspace,
        packages: &PackageSummaries,
        files: &FileSpellings,
        cancelled: &AtomicBool,
    ) -> Option<Self> {
        let package_ids: Vec<_> = workspace
            .navigation_packages()
            .map(|package| package.id.clone())
            .collect();
        let package_numbers = package_ids
            .iter()
            .cloned()
            .enumerate()
            .map(|(number, id)| (id, number))
            .collect();
        let mut index = Self {
            package_ids,
            package_numbers,
            ..Self::default()
        };
        for (id, summary) in packages {
            if cancelled.load(Relaxed) {
                return None;
            }
            let package = workspace.package(id).expect("active package");
            let number = index.package_numbers[id];
            if summary.load_error {
                index.uncertain.insert(number);
            }
            for file in &summary.files {
                if cancelled.load(Relaxed) {
                    return None;
                }
                let key = path_key(file);
                let Some(source) = files.get(&key).and_then(Option::as_deref) else {
                    index.uncertain.insert(number);
                    continue;
                };
                index.add_file(workspace, package, number, file, source, cancelled)?;
            }
        }
        Some(index)
    }

    fn package_id(&self, number: usize) -> &str {
        &self.package_ids[number]
    }

    fn add_file(
        &mut self,
        workspace: &Workspace,
        package: &Package,
        number: usize,
        file: &Path,
        source: &InterfaceSpellings,
        cancelled: &AtomicBool,
    ) -> Option<()> {
        if source.has_local_types {
            self.local_types.insert(file.to_owned());
        }
        for (methods, postings) in [
            (&source.receiver_methods, &mut self.receivers),
            (&source.interface_methods, &mut self.interfaces),
        ] {
            for method in methods {
                if cancelled.load(Relaxed) {
                    return None;
                }
                postings
                    .entry(method.name.clone())
                    .or_default()
                    .entry(method.arity)
                    .or_default()
                    .insert(TypeKey::new(number, &method.receiver));
            }
        }
        for name in &source.type_names {
            if cancelled.load(Relaxed) {
                return None;
            }
            self.declarations
                .entry(TypeKey::new(number, name))
                .or_default()
                .push(file.to_owned());
        }
        for name in &source.uncertain_types {
            if cancelled.load(Relaxed) {
                return None;
            }
            self.unknown_types.insert(TypeKey::new(number, name));
        }
        if source.type_references.is_empty() {
            return Some(());
        }

        // Resolve qualifiers using the package's import map so each edge uses the
        // same build group and test view as full method-set resolution.
        let mut qualified: HashMap<&str, Vec<usize>> = HashMap::new();
        if source
            .type_references
            .iter()
            .any(|reference| reference.qualifier.is_some())
        {
            for path in &source.default_imports {
                if cancelled.load(Relaxed) {
                    return None;
                }
                if let Some(target) = workspace.resolve_import(package, path) {
                    qualified
                        .entry(&target.name)
                        .or_default()
                        .push(self.package_numbers[&target.id]);
                }
            }
            for (name, path) in &source.alias_imports {
                if cancelled.load(Relaxed) {
                    return None;
                }
                if let Some(target) = workspace.resolve_import(package, path) {
                    qualified
                        .entry(name)
                        .or_default()
                        .push(self.package_numbers[&target.id]);
                }
            }
        }
        let dots: Vec<_> = source
            .dot_imports
            .iter()
            .filter_map(|path| workspace.resolve_import(package, path))
            .map(|target| self.package_numbers[&target.id])
            .collect();
        for reference in &source.type_references {
            if cancelled.load(Relaxed) {
                return None;
            }
            let owner = TypeKey::new(number, &reference.owner);
            if let Some(qualifier) = &reference.qualifier {
                if let Some(targets) = qualified.get(qualifier.as_str()) {
                    for &target in targets {
                        self.add_reference(target, &reference.target, &owner, reference.alias);
                    }
                } else {
                    // Incomplete imports or source recovery can resolve differently
                    // in the semantic layer. Keep the declaring type and its users.
                    self.unknown_types.insert(owner);
                }
            } else {
                if reference.target == "error" {
                    self.implicit_error.insert(owner.clone());
                }
                for target in std::iter::once(number).chain(dots.iter().copied()) {
                    self.add_reference(target, &reference.target, &owner, reference.alias);
                }
            }
        }
        Some(())
    }

    fn add_reference(&mut self, package: usize, name: &str, owner: &TypeKey, alias: bool) {
        let target = TypeKey::new(package, name);
        if alias {
            self.derived
                .entry(owner.clone())
                .or_default()
                .insert(target.clone());
        }
        self.derived
            .entry(target)
            .or_default()
            .insert(owner.clone());
    }

    fn inherited(&self, seeds: impl IntoIterator<Item = TypeKey>) -> HashSet<TypeKey> {
        let mut pending: Vec<_> = seeds.into_iter().collect();
        let mut types = HashSet::new();
        while let Some(ty) = pending.pop() {
            if types.insert(ty.clone()) {
                pending.extend(self.derived.get(&ty).into_iter().flatten().cloned());
            }
        }
        types
    }

    fn unknown_seeds(&self) -> impl Iterator<Item = TypeKey> + '_ {
        // An unreadable file or incomplete package can supply any named type.
        // Follow every reference to such a package, including references to names
        // missing from its own declaration summary.
        self.derived
            .keys()
            .filter(|ty| self.uncertain.contains(&ty.package))
            .chain(&self.unknown_types)
            .cloned()
    }

    fn matching<'a>(
        postings: &'a Postings,
        name: &str,
        arity: Option<Arity>,
    ) -> impl Iterator<Item = &'a TypeKey> {
        postings.get(name).into_iter().flat_map(move |shapes| {
            shapes
                .iter()
                .filter(move |(candidate, _)| {
                    arity.is_none() || candidate.is_none() || **candidate == arity
                })
                .flat_map(|(_, types)| types)
        })
    }

    fn shape_matches(&self, shape: &MethodShape) -> impl Iterator<Item = &TypeKey> {
        let arity = Some((shape.parameters, shape.results, shape.variadic));
        Self::matching(&self.receivers, &shape.name, arity)
            .chain(Self::matching(&self.interfaces, &shape.name, arity))
            .chain(
                (shape.name == "Error" && arity == Some((0, 1, false)))
                    .then_some(&self.implicit_error)
                    .into_iter()
                    .flatten(),
            )
    }
}

#[derive(Default)]
pub(super) struct CandidateTypes {
    pub(super) names: HashSet<String>,
    pub(super) files: HashSet<PathBuf>,
}

pub(super) struct CandidatePackages {
    pub(super) key: PackageProbeKey,
    pub(super) eligible: HashSet<String>,
    pub(super) types: HashMap<String, CandidateTypes>,
}

impl CandidatePackages {
    pub(super) fn new(key: PackageProbeKey) -> Self {
        Self {
            key,
            eligible: HashSet::new(),
            types: HashMap::new(),
        }
    }

    pub(super) fn collect(
        &mut self,
        workspace: &Workspace,
        sources: &InterfaceSources,
        groups: &[usize],
    ) {
        let index = sources.postings.as_ref().expect("completed index");
        let in_group = |number| {
            workspace
                .package(index.package_id(number))
                .is_some_and(|p| groups.iter().any(|group| p.groups().contains(group)))
        };
        self.eligible = index
            .uncertain
            .iter()
            .copied()
            .filter(|&number| in_group(number))
            .map(|number| index.package_id(number).to_owned())
            .collect();
        let mut seeds = Vec::new();
        match &self.key {
            PackageProbeKey::Implementations(shapes) => {
                // Select the rarest required shape among this group's packages.
                if let Some(anchor) = shapes.iter().min_by_key(|shape| {
                    index
                        .shape_matches(shape)
                        .filter(|ty| in_group(ty.package))
                        .map(|ty| ty.package)
                        .collect::<HashSet<_>>()
                        .len()
                }) {
                    seeds.extend(index.shape_matches(anchor).cloned());
                }
            }
            PackageProbeKey::Interface(shapes) => {
                for shape in shapes {
                    let arity = Some((shape.parameters, shape.results, shape.variadic));
                    seeds.extend(
                        TypePostings::matching(&index.interfaces, &shape.name, arity).cloned(),
                    );
                    if shape.name == "Error" && arity == Some((0, 1, false)) {
                        seeds.extend(index.implicit_error.iter().cloned());
                    }
                }
            }
            PackageProbeKey::InterfaceMethod(name) => {
                seeds.extend(TypePostings::matching(&index.interfaces, name, None).cloned());
                if name == "Error" {
                    seeds.extend(index.implicit_error.iter().cloned());
                }
            }
        }
        seeds.extend(index.unknown_seeds());
        let forward = matches!(self.key, PackageProbeKey::Implementations(_));
        for ty in index.inherited(seeds.into_iter().filter(|ty| in_group(ty.package))) {
            if !in_group(ty.package) {
                continue;
            }
            let id = index.package_id(ty.package);
            if forward {
                if let Some(files) = index.declarations.get(&ty) {
                    let types = self.types.entry(id.to_owned()).or_default();
                    types.names.insert(ty.name);
                    types.files.extend(files.iter().cloned());
                }
            } else {
                self.eligible.insert(id.to_owned());
            }
        }
        for id in &self.eligible {
            self.types.remove(id);
        }
        self.eligible.extend(self.types.keys().cloned());
    }
}
