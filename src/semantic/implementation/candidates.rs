use super::super::*;
use super::{
    interface_probe::{Arity, InterfaceSpellings, interface_spellings},
    method_sets::MethodShape,
    stats::IndexStats,
};

#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) enum PackageProbeKey {
    Implementations(Vec<MethodShape>),
    InterfaceMethod(String),
    Interface(Vec<MethodShape>),
}

#[derive(Clone, Eq, Hash, PartialEq)]
struct TypeKey {
    package: String,
    name: String,
}

impl TypeKey {
    fn new(package: &Package, name: &str) -> Self {
        Self {
            package: package.id.clone(),
            name: name.to_owned(),
        }
    }
}

type Postings = HashMap<String, HashMap<Option<Arity>, HashSet<TypeKey>>>;

#[derive(Default)]
struct TypePostings {
    receivers: Postings,
    interfaces: Postings,
    declarations: HashMap<TypeKey, Vec<PathBuf>>,
    derived: HashMap<TypeKey, HashSet<TypeKey>>,
    implicit_error: HashSet<TypeKey>,
    unknown_types: HashSet<TypeKey>,
    uncertain: HashSet<String>,
    local_types: HashSet<PathBuf>,
}

#[derive(Default)]
pub(in crate::semantic) struct InterfaceSources {
    files: HashMap<PathBuf, Option<Rc<InterfaceSpellings>>>,
    packages: HashMap<String, PackageSpellings>,
    owners: HashMap<PathBuf, Vec<String>>,
    directories: HashMap<PathBuf, Vec<String>>,
    pending: VecDeque<String>,
    partial: Option<PackageSpellings>,
    postings: Option<TypePostings>,
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
        self.files.remove(key);
        let owners = self
            .owners
            .get(key)
            .or_else(|| key.parent().and_then(|dir| self.directories.get(dir)));
        for id in owners.into_iter().flatten() {
            if self.pending.front() == Some(id) {
                self.partial = None;
            } else if self.packages.remove(id).is_some() {
                self.pending.push_back(id.clone());
            }
            self.postings = None;
        }
    }

    pub(in crate::semantic) fn retain_workspace(&mut self, workspace: &Workspace) {
        self.owners.clear();
        self.directories.clear();
        self.pending.clear();
        self.partial = None;
        self.postings = None;
        self.stats = IndexStats::new();
        self.packages.retain(|id, summary| {
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
        self.files.retain(|key, _| self.owners.contains_key(key));
    }

    pub(in crate::semantic) fn pending(&self) -> bool {
        self.postings.is_none()
    }

    pub(in crate::semantic) fn advance(&mut self, resolver: &mut Resolver<'_, '_>) -> bool {
        if !self.pending() {
            return true;
        }
        let started = Instant::now();
        self.stats.begin();
        for _ in 0..32 {
            let Some(id) = self.pending.front() else {
                self.postings = Some(TypePostings::build(
                    resolver.workspace,
                    &self.packages,
                    &self.files,
                ));
                self.stats.add_build(started);
                self.stats.ready(self.packages.len(), self.files.len());
                return true;
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
                self.files.entry(key.clone()).or_insert_with(|| {
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
                    spellings.map(Rc::new)
                });
            } else {
                let id = self.pending.pop_front().expect("completed package probe");
                self.packages.insert(id, self.partial.take().unwrap());
            }
            if started.elapsed() >= Duration::from_millis(2) {
                break;
            }
        }
        self.stats.add_build(started);
        false
    }

    pub(in crate::semantic) fn embedding_files(
        &self,
        workspace: &Workspace,
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
                workspace
                    .package(id)
                    .map(|package| TypeKey::new(package, name))
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
                        .filter_map(|id| self.packages.get(id))
                        .flat_map(|p| p.files.iter().cloned()),
                )
                .chain(postings.local_types.iter().cloned())
                .map(|file| path_key(&file))
                .collect(),
        )
    }
}

impl TypePostings {
    fn build(
        workspace: &Workspace,
        packages: &HashMap<String, PackageSpellings>,
        files: &HashMap<PathBuf, Option<Rc<InterfaceSpellings>>>,
    ) -> Self {
        let mut index = Self::default();
        for (id, summary) in packages {
            let package = workspace.package(id).expect("active package");
            if summary.load_error {
                index.uncertain.insert(id.clone());
            }
            for file in &summary.files {
                let key = path_key(file);
                let Some(source) = files.get(&key).and_then(Option::as_deref) else {
                    index.uncertain.insert(id.clone());
                    continue;
                };
                index.add_file(workspace, package, file, source);
            }
        }
        index
    }

    fn add_file(
        &mut self,
        workspace: &Workspace,
        package: &Package,
        file: &Path,
        source: &InterfaceSpellings,
    ) {
        if source.has_local_types {
            self.local_types.insert(file.to_owned());
        }
        for (methods, postings) in [
            (&source.receiver_methods, &mut self.receivers),
            (&source.interface_methods, &mut self.interfaces),
        ] {
            for method in methods {
                postings
                    .entry(method.name.clone())
                    .or_default()
                    .entry(method.arity)
                    .or_default()
                    .insert(TypeKey::new(package, &method.receiver));
            }
        }
        for name in &source.type_names {
            self.declarations
                .entry(TypeKey::new(package, name))
                .or_default()
                .push(file.to_owned());
        }
        self.unknown_types.extend(
            source
                .uncertain_types
                .iter()
                .map(|name| TypeKey::new(package, name)),
        );

        // Resolve qualifiers using the package's import map so each edge uses the
        // same build group and test view as full method-set resolution.
        let mut qualified: HashMap<&str, Vec<&Package>> = HashMap::new();
        for path in &source.default_imports {
            if let Some(target) = workspace.resolve_import(package, path) {
                qualified.entry(&target.name).or_default().push(target);
            }
        }
        for (name, path) in &source.alias_imports {
            if let Some(target) = workspace.resolve_import(package, path) {
                qualified.entry(name).or_default().push(target);
            }
        }
        let dots: Vec<_> = source
            .dot_imports
            .iter()
            .filter_map(|path| workspace.resolve_import(package, path))
            .collect();
        for reference in &source.type_references {
            let owner = TypeKey::new(package, &reference.owner);
            if let Some(qualifier) = &reference.qualifier {
                if let Some(targets) = qualified.get(qualifier.as_str()) {
                    for target in targets {
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
                for target in std::iter::once(package).chain(dots.iter().copied()) {
                    self.add_reference(target, &reference.target, &owner, reference.alias);
                }
            }
        }
    }

    fn add_reference(&mut self, package: &Package, name: &str, owner: &TypeKey, alias: bool) {
        let target = TypeKey::new(package, name);
        self.derived
            .entry(target.clone())
            .or_default()
            .insert(owner.clone());
        if alias {
            self.derived
                .entry(owner.clone())
                .or_default()
                .insert(target);
        }
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
        let in_group = |id: &str| {
            workspace
                .package(id)
                .is_some_and(|p| groups.iter().any(|group| p.groups().contains(group)))
        };
        self.eligible = index
            .uncertain
            .iter()
            .filter(|id| in_group(id))
            .cloned()
            .collect();
        let mut seeds = Vec::new();
        match &self.key {
            PackageProbeKey::Implementations(shapes) => {
                // Select the rarest required shape among this group's packages.
                if let Some(anchor) = shapes.iter().min_by_key(|shape| {
                    index
                        .shape_matches(shape)
                        .filter(|ty| in_group(&ty.package))
                        .map(|ty| ty.package.as_str())
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
        for ty in index.inherited(seeds.into_iter().filter(|ty| in_group(&ty.package))) {
            if !in_group(&ty.package) {
                continue;
            }
            if forward {
                if let Some(files) = index.declarations.get(&ty) {
                    let types = self.types.entry(ty.package).or_default();
                    types.names.insert(ty.name);
                    types.files.extend(files.iter().cloned());
                }
            } else {
                self.eligible.insert(ty.package);
            }
        }
        for id in &self.eligible {
            self.types.remove(id);
        }
        self.eligible.extend(self.types.keys().cloned());
    }
}
