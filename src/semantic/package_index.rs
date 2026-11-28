use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Result;

use super::diagnostics::DiagnosticPackageNames;
use super::index::DeclarationSummary;
use super::shared_disk::SharedDeclarations;
use super::source::check_cancelled;
use crate::workspace::{Package, Workspace};

pub(super) struct PackageDeclFile {
    pub(super) path: PathBuf,
    pub(super) key: PathBuf,
    pub(super) declarations: Arc<SharedDeclarations>,
}

#[derive(Eq, Hash, PartialEq)]
pub(super) struct PackageDeclKey {
    pub(super) id: String,
    directory: PathBuf,
    name: String,
    private_name_owner: String,
    for_test: Option<String>,
    load_error: bool,
    includes_tests: bool,
    groups: Vec<usize>,
    environment: Arc<BTreeMap<OsString, OsString>>,
    allows_module_auto: bool,
    imports: BTreeMap<String, Option<String>>,
    files: Vec<(PathBuf, PackageFileRevision)>,
}

#[derive(Eq, Hash, PartialEq)]
pub(super) struct PackageFileRevision {
    pub(super) overlay: Option<(u64, u64)>,
    pub(super) reliable_declarations: bool,
    pub(super) reliable_package_scope: bool,
    pub(super) content: DeclarationIdentity,
}

#[derive(Eq, PartialEq)]
pub(super) enum DeclarationIdentity {
    Summary(Arc<DeclarationSummary>),
    Exact(u64, Arc<String>),
}

impl Hash for DeclarationIdentity {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            Self::Summary(summary) => summary.hash(state),
            Self::Exact(fingerprint, _) => fingerprint.hash(state),
        }
    }
}

impl PackageDeclKey {
    pub(super) fn new(
        workspace: &Workspace,
        package: &Package,
        files: &[PackageDeclFile],
        revisions: Vec<(PathBuf, PackageFileRevision)>,
    ) -> Self {
        let (directory, name) = package.directory_and_name();
        let imports = package.diagnostic_import_paths().chain(
            files
                .iter()
                .flat_map(|file| file.declarations.index.imports.iter())
                .map(|import| import.path.as_str()),
        );
        let imports = imports
            .map(|path| {
                let target = workspace.resolve_import(package, path);
                (path.to_owned(), target.map(|target| target.id.clone()))
            })
            .collect();
        let environment = workspace.build_environment();
        Self {
            id: package.id.clone(),
            directory: directory.to_owned(),
            name: name.to_owned(),
            private_name_owner: package.private_name_owner().to_owned(),
            for_test: package.test_of().map(str::to_owned),
            load_error: package.load_error,
            includes_tests: workspace.includes_tests(),
            groups: package.groups().to_owned(),
            environment: Arc::clone(environment.variables()),
            allows_module_auto: environment.allows_module_auto(),
            imports,
            files: revisions,
        }
    }

    pub(super) fn cache_weight(&self) -> usize {
        size_of::<Self>()
            + self.id.capacity()
            + self.directory.capacity()
            + self.name.capacity()
            + self.private_name_owner.capacity()
            + self.for_test.as_ref().map_or(0, String::capacity)
            + self.groups.capacity() * size_of::<usize>()
            + self.environment.len() * (size_of::<(OsString, OsString)>() + 4 * size_of::<usize>())
            + self
                .environment
                .iter()
                .map(|(name, value)| name.capacity() + value.capacity())
                .sum::<usize>()
            + self.imports.len() * (size_of::<(String, Option<String>)>() + 4 * size_of::<usize>())
            + self
                .imports
                .iter()
                .map(|(path, id)| path.capacity() + id.as_ref().map_or(0, String::capacity))
                .sum::<usize>()
            + self.files.capacity() * size_of::<(PathBuf, PackageFileRevision)>()
            + self
                .files
                .iter()
                .map(|(path, revision)| {
                    path.capacity()
                        + match &revision.content {
                            DeclarationIdentity::Summary(summary) => {
                                size_of::<DeclarationSummary>() + summary.heap_weight()
                            }
                            DeclarationIdentity::Exact(_, text) => {
                                size_of::<String>() + text.capacity()
                            }
                        }
                })
                .sum::<usize>()
    }
}

/// Candidate ordinals preserve package file order, including draft files.
/// Locations and semantic payloads come from the chosen file's current revision.
pub(super) struct PackageDeclIndex {
    files: Vec<PathBuf>,
    declarations: FileCandidates,
    types: FileCandidates,
    members: HashMap<String, ReceiverCandidates>,
    pub(super) diagnostics: DiagnosticPackageNames,
}

type FileCandidates = HashMap<String, Vec<usize>>;

pub(super) enum PackageLookup<'a> {
    Symbol(&'a str, Option<&'a str>),
    Type(&'a str),
    Receiver(&'a str),
}

pub(super) enum PackageLookupFiles<'a> {
    Indexed(Arc<PackageDeclIndex>),
    All(Cow<'a, [PathBuf]>),
}

impl PackageLookupFiles<'_> {
    pub(super) fn iter(&self, lookup: PackageLookup<'_>) -> impl Iterator<Item = &Path> {
        let (candidates, paths, fallback) = match self {
            Self::Indexed(index) => (index.candidates(lookup), index.files.as_slice(), &[][..]),
            Self::All(files) => (&[][..], &[][..], files.as_ref()),
        };
        candidates
            .iter()
            .map(move |&ordinal| paths[ordinal].as_path())
            .chain(fallback.iter().map(PathBuf::as_path))
    }
}

#[derive(Default)]
struct ReceiverCandidates {
    files: Vec<usize>,
    names: FileCandidates,
}

impl PackageDeclIndex {
    pub(super) fn collect(
        files: &[PackageDeclFile],
        package_name: &str,
        cancelled: &AtomicBool,
    ) -> Result<Self> {
        let mut declarations = HashMap::<String, Vec<usize>>::new();
        let mut types = HashMap::<String, Vec<usize>>::new();
        let mut members = HashMap::<String, ReceiverCandidates>::new();
        let mut paths = Vec::with_capacity(files.len());
        for (ordinal, file) in files.iter().enumerate() {
            check_cancelled(cancelled)?;
            paths.push(file.path.clone());
            let symbols = &file.declarations.index.symbols;
            for (name, _) in symbols.declarations() {
                check_cancelled(cancelled)?;
                declarations
                    .entry(name.to_owned())
                    .or_default()
                    .push(ordinal);
            }
            for (name, _) in symbols.type_declarations() {
                check_cancelled(cancelled)?;
                types.entry(name.to_owned()).or_default().push(ordinal);
            }
            for (owner, group) in symbols.member_groups() {
                check_cancelled(cancelled)?;
                let receiver = members.entry(owner.to_owned()).or_default();
                receiver.files.push(ordinal);
                for name in group.keys() {
                    check_cancelled(cancelled)?;
                    receiver
                        .names
                        .entry(name.clone())
                        .or_default()
                        .push(ordinal);
                }
            }
        }
        Ok(Self {
            files: paths,
            declarations,
            types,
            members,
            diagnostics: DiagnosticPackageNames::collect(files, package_name, cancelled)?,
        })
    }

    fn candidates(&self, lookup: PackageLookup<'_>) -> &[usize] {
        match lookup {
            PackageLookup::Symbol(name, None) => self.declarations.get(name),
            PackageLookup::Symbol(name, Some(owner)) => self
                .members
                .get(owner)
                .and_then(|group| group.names.get(name)),
            PackageLookup::Type(name) => self.types.get(name),
            PackageLookup::Receiver(owner) => self.members.get(owner).map(|group| &group.files),
        }
        .map_or(&[], Vec::as_slice)
    }

    pub(super) fn cache_weight(&self) -> usize {
        fn candidates_weight(candidates: &FileCandidates) -> usize {
            map_weight(candidates)
                + candidates
                    .iter()
                    .map(|(name, files)| name.capacity() + files.capacity() * size_of::<usize>())
                    .sum::<usize>()
        }

        size_of::<Self>()
            + self.files.capacity() * size_of::<PathBuf>()
            + self.files.iter().map(PathBuf::capacity).sum::<usize>()
            + candidates_weight(&self.declarations)
            + candidates_weight(&self.types)
            + map_weight(&self.members)
            + self
                .members
                .iter()
                .map(|(name, receiver)| {
                    name.capacity()
                        + receiver.files.capacity() * size_of::<usize>()
                        + candidates_weight(&receiver.names)
                })
                .sum::<usize>()
            + self.diagnostics.cache_weight()
    }
}

fn map_weight<K, V>(map: &HashMap<K, V>) -> usize {
    (map.capacity() * (size_of::<(K, V)>() + 1) * 8).div_ceil(7)
}

#[cfg(test)]
thread_local! {
    static ENABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

pub(super) fn enabled() -> bool {
    #[cfg(test)]
    return ENABLED.get();
    #[cfg(not(test))]
    true
}

#[cfg(test)]
pub(super) fn without_index<T>(run: impl FnOnce() -> T) -> T {
    struct Restore(bool);

    impl Drop for Restore {
        fn drop(&mut self) {
            ENABLED.set(self.0);
        }
    }

    let _restore = Restore(ENABLED.replace(false));
    run()
}

#[cfg(test)]
mod tests {
    use lsp_types::{Position, TextDocumentContentChangeEvent};

    use super::*;
    use crate::semantic::Engine;
    use crate::test_support::TestProject;

    fn index(engine: &mut Engine, path: &Path) -> Arc<PackageDeclIndex> {
        let mut snapshot = engine.snapshot();
        let workspace = snapshot.workspace().unwrap();
        let package = workspace.package_of_file(path).unwrap();
        snapshot
            .diagnostic_resolver(workspace, None)
            .indexes
            .prepare_package_index(package)
            .unwrap()
            .unwrap()
    }

    #[test]
    fn declaration_candidates_reuse_indexes_while_current_ranges_follow_edits() {
        let project = TestProject::new();
        let declaration = "package sample\ntype Shape struct { Field int }\n\
                           func (Shape) Method() {}\n";
        let path = project.write("api.go", declaration);
        let sibling = project.write("other.go", "package sample\nvar Other bool\n");
        let mut engine = Engine::new(project.root.clone());
        assert!(engine.install_workspace(
            engine.workspace_load(),
            project.workspace(&["api.go", "other.go"], false),
        ));

        let disk = index(&mut engine, &path);
        assert!(Arc::ptr_eq(&disk, &index(&mut engine, &path)));
        let candidates = PackageLookupFiles::Indexed(disk.clone());
        for lookup in [
            PackageLookup::Type("Shape"),
            PackageLookup::Symbol("Field", Some("Shape")),
            PackageLookup::Symbol("Method", Some("Shape")),
            PackageLookup::Receiver("Shape"),
        ] {
            assert_eq!(
                candidates.iter(lookup).collect::<Vec<_>>(),
                [path.as_path()]
            );
        }
        assert_eq!(
            candidates
                .iter(PackageLookup::Symbol("Other", None))
                .collect::<Vec<_>>(),
            [sibling.as_path()]
        );
        assert_eq!(candidates.iter(PackageLookup::Type("Other")).count(), 0);

        engine.open(path.clone(), declaration.into(), 1).unwrap();
        let overlay = index(&mut engine, &path);
        assert!(!Arc::ptr_eq(&disk, &overlay));
        let moved = "package sample\n\n/*😀*/ type Shape struct { Field int }\n\
                     func (Shape) Method() { _ = 1 }\n";
        engine
            .edit(
                &path,
                &[TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: moved.into(),
                }],
                2,
            )
            .unwrap();
        assert!(Arc::ptr_eq(&overlay, &index(&mut engine, &path)));
        {
            let mut snapshot = engine.snapshot();
            let workspace = snapshot.workspace().unwrap();
            let package = workspace.package_of_file(&path).unwrap();
            let mut resolver = snapshot.resolver(workspace);
            let PackageLookupFiles::Indexed(warm) = resolver.indexes.lookup_files(package).unwrap()
            else {
                panic!("warm navigation should reuse the verified package index");
            };
            assert!(Arc::ptr_eq(&overlay, &warm));
            let found = resolver
                .find_symbol(package, "Shape", None)
                .unwrap()
                .unwrap();
            assert_eq!(found.symbol.range.start, Position::new(2, 12));
            assert_eq!(found.symbol.range.end, Position::new(2, 17));
        }

        engine
            .edit(
                &path,
                &[TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: moved.replace("Field int", "Renamed int"),
                }],
                3,
            )
            .unwrap();
        let signature = index(&mut engine, &path);
        assert!(!Arc::ptr_eq(&signature, &overlay));
        let candidates = PackageLookupFiles::Indexed(signature);
        assert_eq!(
            candidates
                .iter(PackageLookup::Symbol("Field", Some("Shape")))
                .count(),
            0
        );
        assert_eq!(
            candidates
                .iter(PackageLookup::Symbol("Renamed", Some("Shape")))
                .count(),
            1
        );

        engine.close(&path);
        assert!(Arc::ptr_eq(&disk, &index(&mut engine, &path)));
        let moved_disk = moved.replace("_ = 1", "_ = 2");
        project.write("api.go", &moved_disk);
        assert!(Arc::ptr_eq(&disk, &index(&mut engine, &path)));
        project.write("api.go", &moved_disk.replace("Shape", "Circle"));
        {
            let mut snapshot = engine.snapshot();
            let workspace = snapshot.workspace().unwrap();
            let package = workspace.package_of_file(&path).unwrap();
            let mut resolver = snapshot.resolver(workspace);
            assert!(matches!(
                resolver.indexes.lookup_files(package).unwrap(),
                PackageLookupFiles::All(_),
            ));
            assert!(
                resolver
                    .find_symbol(package, "Circle", None)
                    .unwrap()
                    .is_some()
            );
        }
        assert!(!Arc::ptr_eq(&disk, &index(&mut engine, &path)));
    }
}
