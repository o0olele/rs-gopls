use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use lsp_types::{Position, Range};

use super::index::active_package;
use super::query::{QueryProgress, SemanticTask};
use super::resolver::{Resolver, TypeIdentity, TypeKind, is_exported};
use super::source::{ReferenceQuery, Source, TextEditDelta, is_type_declaration, node_text};
use super::{Engine, Rc, VecDeque};
use crate::symbols::{self, TypeRef, TypeRefKind};
use crate::workspace::{Package, path_key};

mod array_length;
mod candidates;
mod interface_index;
mod interface_probe;
mod method_sets;

pub(super) use candidates::InterfaceSources;
use candidates::{CandidatePackages, PackageProbeKey};
use interface_index::{IndexedInterface, InterfaceIndex};
pub(in crate::semantic) use method_sets::RenameMethod;
use method_sets::{InterfaceMethods, MethodKey, MethodShape};

type Location = (PathBuf, Range);
type QueryKey = (PathBuf, Position);

#[derive(Default)]
pub(super) struct ImplementationCache {
    results: HashMap<QueryKey, Vec<Location>>,
    packages: HashMap<PackageProbeKey, HashSet<String>>,
    interfaces: HashMap<String, InterfaceIndex>,
}

impl ImplementationCache {
    pub(super) fn is_empty(&self) -> bool {
        self.results.is_empty() && self.packages.is_empty() && self.interfaces.is_empty()
    }

    pub(super) fn clear(&mut self) {
        self.results.clear();
        self.packages.clear();
        self.interfaces.clear();
    }

    fn remember(&mut self, key: QueryKey, result: Vec<Location>) {
        if self.results.len() >= 128 {
            self.results.clear();
        }
        self.results.insert(key, result);
    }

    fn remember_packages(&mut self, candidates: CandidatePackages) {
        if self.packages.len() >= 32 {
            self.packages.clear();
        }
        self.packages.insert(candidates.key, candidates.eligible);
    }

    fn shift(&mut self, path: &Path, shift: LineShift) {
        if shift.old_end == shift.new_end {
            return;
        }
        // Method signatures in this index can contain declaration ranges.
        self.interfaces.clear();
        self.results = std::mem::take(&mut self.results)
            .into_iter()
            .filter_map(|((query_path, mut position), mut locations)| {
                if query_path == path {
                    position = shift.position(position)?;
                }
                for (file, range) in &mut locations {
                    if file == path || path_key(file) == path {
                        range.start = shift.position(range.start)?;
                        range.end = shift.position(range.end)?;
                    }
                }
                Some(((query_path, position), locations))
            })
            .collect();
    }
}

pub(super) struct ObservedEdit {
    shifts: Option<Vec<LineShift>>,
}

impl ObservedEdit {
    pub(super) fn new() -> Self {
        Self {
            shifts: Some(Vec::new()),
        }
    }

    pub(super) fn observe(&mut self, delta: TextEditDelta<'_>) -> bool {
        if let Some(shift) = LineShift::for_edit(delta) {
            self.shifts.as_mut().expect("valid edit batch").push(shift);
        } else {
            self.shifts = None;
        }
        self.shifts.is_some()
    }

    pub(super) fn apply(self, cache: &mut ImplementationCache, path: &Path) {
        if let Some(shifts) = self.shifts {
            for shift in shifts {
                cache.shift(path, shift);
            }
        } else {
            cache.clear();
        }
    }
}

struct LineShift {
    start: u32,
    old_end: u32,
    new_end: u32,
}

impl LineShift {
    fn for_edit(delta: TextEditDelta<'_>) -> Option<Self> {
        let TextEditDelta {
            range,
            bytes,
            inserted,
            new_end_line,
            text,
            tree,
        } = delta;
        let range = range?;
        let root = tree.root_node();
        if range.start.character != 0
            || range.end.character != 0
            || root.has_error()
            || (bytes.start == 0 && text.starts_with('\u{feff}'))
            || !top_level_trivia(&text[bytes.clone()])
            || !top_level_trivia(inserted)
        {
            return None;
        }
        let mut cursor = root.walk();
        let unaffected = root.named_children(&mut cursor).all(|node| {
            let content = node_text(node, text);
            if node.kind() == "comment" {
                if ordinary_comment(content) {
                    return true;
                }
                // Keep directives and block comments on the conservative invalidation path.
                return false;
            }
            node.end_byte() <= bytes.start || node.start_byte() >= bytes.end
        });
        unaffected.then_some(Self {
            start: range.start.line,
            old_end: range.end.line,
            new_end: u32::try_from(new_end_line).ok()?,
        })
    }

    fn position(&self, mut position: Position) -> Option<Position> {
        if position.line >= self.start {
            position.line = position
                .line
                .checked_sub(self.old_end)?
                .checked_add(self.new_end)?;
        }
        Some(position)
    }
}

fn top_level_trivia(text: &str) -> bool {
    (text.is_empty() || text.ends_with('\n'))
        && text.lines().all(|line| {
            let line = line.trim_matches([' ', '\t', '\r']);
            line.is_empty() || ordinary_comment(line)
        })
}

fn ordinary_comment(text: &str) -> bool {
    text.strip_prefix("//").is_some_and(|line| {
        !line.contains('\n')
            && !["go:", "+build", "line "]
                .iter()
                .any(|prefix| line.trim_start().starts_with(prefix))
    })
}

pub(crate) struct ImplementationSearch {
    key: QueryKey,
    direction: Direction,
    selected: Option<MethodKey>,
    declaration: Location,
    packages: VecDeque<String>,
    package_filter: Option<CandidatePackages>,
    current: Option<PackageSearch>,
    matches: Vec<Location>,
}

enum Direction {
    Implementations {
        required: InterfaceMethods,
        anchor: String,
    },
    Interfaces {
        package_id: String,
        name: String,
        shapes: Option<HashSet<MethodShape>>,
    },
}

impl Direction {
    fn receiver_shapes(&self) -> Option<&HashSet<MethodShape>> {
        match self {
            Self::Interfaces { shapes, .. } => shapes.as_ref(),
            Self::Implementations { .. } => None,
        }
    }
}

struct PackageSearch {
    id: String,
    files: Vec<PathBuf>,
    direct_owners: HashSet<String>,
    candidates: Vec<Candidate>,
    interface_index: Option<InterfaceIndex>,
    indexed_interfaces: Vec<Rc<IndexedInterface>>,
}

struct Candidate {
    name: String,
    location: Location,
    inherits: bool,
    may_be_interface: bool,
}

impl ImplementationSearch {
    fn complete(&mut self, cache: &mut ImplementationCache) {
        self.matches
            .sort_by_cached_key(|(file, range)| (path_key(file), range.start, range.end));
        self.matches
            .dedup_by(|left, right| same_location(left, right));
        cache.remember(self.key.clone(), self.matches.clone());
    }

    fn indexes_interfaces(&self) -> bool {
        self.selected.is_none() && matches!(self.direction, Direction::Interfaces { .. })
    }
}

struct Subject<'w> {
    owner: TypeIdentity<'w>,
    method: Option<MethodKey>,
    declaration: Location,
}

impl Engine {
    pub(crate) fn implementations(
        &mut self,
        path: &Path,
        position: Position,
    ) -> Result<QueryProgress<Vec<Location>, ImplementationSearch>> {
        if path.extension().is_none_or(|extension| extension != "go") {
            return Ok(QueryProgress::Ready(Vec::new()));
        }
        self.include_tests = true;
        let mut snapshot = self.state.snapshot(self.include_tests);
        let workspace = match snapshot.active_graph(true) {
            QueryProgress::Ready(workspace) => workspace,
            QueryProgress::Pending(load) => return Ok(QueryProgress::Pending(load)),
            QueryProgress::Searching(task) => match task {},
        };
        let key = (path_key(path), position);
        if let Some(cached) = self.implementation_cache.results.get(&key) {
            return Ok(QueryProgress::Ready(cached.clone()));
        }

        let root = snapshot.root();
        let new_documents = snapshot.new_documents();
        let mut resolver = snapshot.resolver(workspace);
        let Some(source) = resolver.indexes.reference_source(path)? else {
            return Ok(QueryProgress::Ready(Vec::new()));
        };
        let source = source.as_ref();
        let Some(from) = active_package(workspace, source, new_documents) else {
            return Ok(QueryProgress::Ready(Vec::new()));
        };
        let Some(subject) = resolver.implementation_subject(from, source, position, root)? else {
            return Ok(QueryProgress::Ready(Vec::new()));
        };
        let mut matches = Vec::new();
        let direction = if let Some(required) = resolver.interface_methods(&subject.owner)? {
            let same_file = path_key(path) == path_key(&subject.declaration.0);
            let range = subject.declaration.1;
            if !same_file || position < range.start || position >= range.end {
                matches.push(subject.declaration.clone());
            }
            let Some(anchor) = required
                .method_names()
                .max_by_key(|name| (name.len(), *name))
                .map(str::to_owned)
            else {
                return Ok(QueryProgress::Ready(matches));
            };
            Direction::Implementations { required, anchor }
        } else if let TypeKind::Named { name, .. } = &subject.owner.kind {
            let concrete = TypeIdentity::named(subject.owner.package, name);
            let shapes = if subject.method.is_none() {
                resolver.concrete_method_shapes(&concrete).unwrap_or(None)
            } else {
                None
            };
            if shapes.as_ref().is_some_and(|shapes| shapes.is_empty()) {
                return Ok(QueryProgress::Ready(matches));
            }
            Direction::Interfaces {
                package_id: subject.owner.package.id.clone(),
                name: name.clone(),
                shapes,
            }
        } else {
            return Ok(QueryProgress::Ready(matches));
        };

        let package = subject.owner.package;
        let package_only = subject
            .method
            .as_ref()
            .is_some_and(|method| !method.is_exported());
        // The predeclared error interface can supply Error outside the go list graph.
        let probe = match &direction {
            Direction::Implementations { anchor, .. } if !package_only && anchor != "Error" => {
                Some(PackageProbeKey::Method(anchor.clone()))
            }
            Direction::Interfaces {
                shapes: Some(shapes),
                ..
            } => {
                let mut shapes: Vec<_> = shapes.iter().cloned().collect();
                shapes.sort_unstable();
                Some(PackageProbeKey::Interface(shapes))
            }
            Direction::Interfaces { .. } => subject
                .method
                .as_ref()
                .filter(|method| method.is_exported())
                .map(|method| PackageProbeKey::InterfaceMethod(method.name().to_owned())),
            _ => None,
        };
        let (packages, package_filter) = match probe {
            Some(key) => match self.implementation_cache.packages.get(&key) {
                Some(eligible) => (eligible.iter().cloned().collect(), None),
                None => (
                    VecDeque::new(),
                    Some(CandidatePackages::new(key, workspace)),
                ),
            },
            None => {
                let packages = workspace
                    .navigation_packages()
                    .filter(|candidate| {
                        !package_only
                            || candidate.directory_and_name() == package.directory_and_name()
                    })
                    .map(|candidate| candidate.id.clone())
                    .collect();
                (packages, None)
            }
        };
        Ok(QueryProgress::Searching(ImplementationSearch {
            key,
            direction,
            selected: subject.method,
            declaration: subject.declaration,
            packages,
            package_filter,
            current: None,
            matches,
        }))
    }
}

impl SemanticTask<Vec<Location>> for ImplementationSearch {
    fn advance(&mut self, engine: &mut Engine) -> Result<bool> {
        let mut snapshot = engine.state.snapshot(engine.include_tests);
        let workspace = snapshot
            .workspace()
            .context("implementation search needs a workspace")?;
        ensure!(
            matches!(snapshot.active_graph(false), QueryProgress::Ready(_)),
            "workspace changed during implementation search"
        );
        let mut resolver = snapshot.resolver(workspace);
        if let Some(filter) = &mut self.package_filter {
            if filter.advance(&resolver, &mut engine.implementation_sources) {
                self.packages.extend(filter.eligible.iter().cloned());
                if let Some(filter) = self.package_filter.take() {
                    engine.implementation_cache.remember_packages(filter);
                }
            }
            return Ok(false);
        }
        let started = Instant::now();
        let index_interfaces = self.indexes_interfaces();
        let receiver_shapes = self.direction.receiver_shapes();
        for _ in 0..32 {
            if self.current.is_none() {
                let Some(id) = self.packages.pop_front() else {
                    break;
                };
                let Some(package) = workspace.package(&id) else {
                    continue;
                };
                let existing = index_interfaces
                    .then(|| engine.implementation_cache.interfaces.get(&id))
                    .flatten();
                self.current = Some(PackageSearch {
                    id,
                    files: if existing.is_some() {
                        Vec::new()
                    } else {
                        resolver.indexes.files(package).into_owned()
                    },
                    direct_owners: HashSet::new(),
                    candidates: Vec::new(),
                    interface_index: (index_interfaces && existing.is_none())
                        .then(InterfaceIndex::default),
                    indexed_interfaces: existing
                        .map_or_else(Vec::new, |index| index.candidates(receiver_shapes)),
                });
            }
            let current = self.current.as_mut().expect("initialized package search");
            if let Some(file) = current.files.pop() {
                resolver.collect_implementation_candidates(
                    current,
                    &file,
                    &self.direction,
                    index_interfaces,
                )?;
            } else if let Some(candidate) = current.candidates.pop() {
                let eligible = match self.direction {
                    Direction::Implementations { .. } => {
                        candidate.inherits || current.direct_owners.contains(&candidate.name)
                    }
                    Direction::Interfaces { .. } => candidate.may_be_interface,
                };
                if eligible && let Some(package) = workspace.package(&current.id) {
                    if let Some(index) = &mut current.interface_index {
                        let ty = TypeIdentity::named(package, &candidate.name);
                        if let Some(required) = resolver.interface_methods(&ty)? {
                            index.insert(candidate.location, required);
                        }
                    } else if let Some(target) =
                        resolver.match_implementation(package, self, candidate)?
                    {
                        self.matches.push(target);
                    }
                }
            } else if let Some(index) = current.interface_index.take() {
                current.indexed_interfaces = index.candidates(receiver_shapes);
                engine
                    .implementation_cache
                    .interfaces
                    .insert(current.id.clone(), index);
            } else if let Some(candidate) = current.indexed_interfaces.pop() {
                if let Some(package) = workspace.package(&current.id)
                    && let Some(target) = resolver.match_interface(
                        package,
                        self,
                        &candidate.methods,
                        &candidate.location,
                    )?
                {
                    self.matches.push(target);
                }
            } else {
                self.current = None;
            }
            if started.elapsed() >= Duration::from_millis(2) {
                break;
            }
        }
        let done = self.current.is_none() && self.packages.is_empty();
        if done {
            self.complete(&mut engine.implementation_cache);
        }
        Ok(done)
    }

    fn finish(self) -> Vec<Location> {
        self.matches
    }
}

impl<'w> Resolver<'w, '_> {
    fn implementation_subject(
        &mut self,
        from: &'w Package,
        source: &Source,
        position: Position,
        root: &Path,
    ) -> Result<Option<Subject<'w>>> {
        let Some(reference) = source.reference_at(position) else {
            return Ok(None);
        };
        let Some(declaration) = self.reference_location(from, source, reference, root)? else {
            return Ok(None);
        };
        let Some(origin) = self.indexes.reference_source(&declaration.0)? else {
            return Ok(None);
        };
        let origin = origin.as_ref();
        let same_package = self
            .indexes
            .files(from)
            .iter()
            .any(|path| path == &origin.path);
        let Some(package) = same_package
            .then_some(from)
            .or_else(|| active_package(self.workspace, origin, &HashSet::new()))
        else {
            return Ok(None);
        };
        let Some(name) = origin
            .reference_at(declaration.1.start)
            .map(ReferenceQuery::name)
        else {
            return Ok(None);
        };
        let Some(parent) = name.parent() else {
            return Ok(None);
        };

        let method = matches!(parent.kind(), "method_declaration" | "method_elem")
            .then(|| MethodKey::new(node_text(name, &origin.text), package));
        let owner = match parent.kind() {
            "method_declaration" => parent
                .child_by_field_name("receiver")
                .and_then(|receiver| receiver.named_child(0))
                .and_then(|parameter| parameter.child_by_field_name("type"))
                .and_then(|ty| symbols::type_ref(ty, &origin.text, &|node| origin.range(node)))
                .and_then(|ty| match ty.kind {
                    TypeRefKind::Named(named) => Some(TypeIdentity::named(package, &named.name)),
                    _ => None,
                }),
            "method_elem" => parent.parent().and_then(|interface| {
                let declaration = interface.parent().filter(|node| is_type_declaration(*node));
                if let Some(declaration) = declaration {
                    let name = declaration.child_by_field_name("name")?;
                    Some(TypeIdentity::named(package, node_text(name, &origin.text)))
                } else {
                    self.source_type(package, origin, interface)
                }
            }),
            "type_spec" | "type_alias" => {
                let ty = TypeIdentity::named(package, node_text(name, &origin.text));
                let mut use_node = reference.name();
                if let Some(parent) = use_node
                    .parent()
                    .filter(|node| node.kind() == "qualified_type")
                {
                    use_node = parent;
                }
                if let Some(parent) = use_node
                    .parent()
                    .filter(|node| node.kind() == "generic_type")
                {
                    self.source_type(from, source, parent).or(Some(ty))
                } else {
                    Some(ty)
                }
            }
            _ => None,
        };
        Ok(owner.map(|owner| Subject {
            owner,
            method,
            declaration,
        }))
    }

    fn collect_implementation_candidates(
        &mut self,
        package: &mut PackageSearch,
        file: &Path,
        direction: &Direction,
        index_interfaces: bool,
    ) -> Result<()> {
        let index = if index_interfaces {
            self.indexes.candidate_index(file)
        } else {
            self.indexes.file_index(file).map(Some)
        };
        let index = match index {
            Ok(Some(index)) => index,
            Ok(None) => return Ok(()),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if let Direction::Implementations { anchor, .. } = direction {
            package
                .direct_owners
                .extend(index.symbols.member_owners(anchor).map(str::to_owned));
        }
        for (name, details) in index.symbols.type_declarations() {
            if let Some(symbol) = index.symbols.declaration(name) {
                let named = |reference: &TypeRef| matches!(reference.kind, TypeRefKind::Named(_));
                let alias_or_base = details.alias.as_ref().is_some_and(|alias| {
                    named(alias) || matches!(alias.kind, TypeRefKind::Inline(_))
                }) || details.underlying.as_ref().is_some_and(named);
                package.candidates.push(Candidate {
                    name: name.to_owned(),
                    location: (file.to_owned(), symbol.range),
                    inherits: alias_or_base || !details.embedded.is_empty(),
                    may_be_interface: details.interface || alias_or_base,
                });
            }
        }
        Ok(())
    }

    fn match_implementation(
        &mut self,
        package: &'w Package,
        search: &ImplementationSearch,
        candidate: Candidate,
    ) -> Result<Option<Location>> {
        if same_location(&candidate.location, &search.declaration) {
            return Ok(None);
        }
        let ty = TypeIdentity::named(package, &candidate.name);
        let selected = search.selected.as_ref();
        let target = match &search.direction {
            Direction::Implementations { required, .. } => {
                let Some(matched) = self.satisfies_interface(required, &ty, selected, true)? else {
                    return Ok(None);
                };
                if selected.is_some() {
                    matched.location
                } else {
                    Some(candidate.location)
                }
            }
            Direction::Interfaces { .. } => {
                let Some(required) = self.interface_methods(&ty)? else {
                    return Ok(None);
                };
                return self.match_interface(package, search, &required, &candidate.location);
            }
        };
        Ok(target.filter(|target| !same_location(target, &search.declaration)))
    }

    fn match_interface(
        &mut self,
        package: &'w Package,
        search: &ImplementationSearch,
        required: &InterfaceMethods,
        location: &Location,
    ) -> Result<Option<Location>> {
        let Direction::Interfaces {
            package_id, name, ..
        } = &search.direction
        else {
            return Ok(None);
        };
        let Some(owner) = self.workspace.package(package_id) else {
            return Ok(None);
        };
        if same_location(location, &search.declaration)
            || (owner.directory_and_name() != package.directory_and_name()
                && !required.method_names().any(is_exported))
        {
            return Ok(None);
        }
        let concrete = TypeIdentity::named(owner, name);
        let selected = search.selected.as_ref();
        if self
            .satisfies_interface(required, &concrete, selected, true)?
            .is_none()
        {
            return Ok(None);
        }
        let target = match selected {
            Some(method) => required.location(method),
            None => Some(location.clone()),
        };
        Ok(target.filter(|target| !same_location(target, &search.declaration)))
    }
}

fn same_location(left: &Location, right: &Location) -> bool {
    left.1 == right.1 && path_key(&left.0) == path_key(&right.0)
}

#[cfg(test)]
mod tests {
    use lsp_types::TextDocumentContentChangeEvent;
    use serde_json::json;

    use super::*;
    use crate::test_support::TestProject;
    use crate::workspace::Workspace;

    const RECEIVERS: &str = "package receivers\ntype Starter struct{}\nfunc (Starter) Start() {}\ntype Reader struct{}\nfunc (Reader) Read(int) {}\n";

    fn install_packages(engine: &mut Engine, root: &Path, packages: &[(&str, &[&str], &[&str])]) {
        let listing = packages
            .iter()
            .map(|(id, files, imports)| {
                json!({"Dir": root.join(id), "ImportPath": id, "Name": id,
                   "GoFiles": files, "Imports": imports})
                .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let workspace = Workspace::from_test_listing(&[root.to_owned()], &listing, true);
        assert!(engine.install_workspace(engine.workspace_load(), workspace));
    }

    fn reverse(engine: &mut Engine, path: &Path, line: u32) -> Vec<Location> {
        match engine
            .implementations(path, Position::new(line, 6))
            .unwrap()
        {
            QueryProgress::Ready(found) => found,
            QueryProgress::Searching(mut search) => {
                while !search.advance(engine).unwrap() {}
                search.finish()
            }
            QueryProgress::Pending(_) => panic!("expected an installed test graph"),
        }
    }

    fn type_at(path: &Path, line: u32, name: &str) -> Location {
        (
            path.to_owned(),
            Range::new(
                Position::new(line, 5),
                Position::new(line, 5 + name.len() as u32),
            ),
        )
    }

    #[test]
    fn array_length_uses_the_type_alias_declaration_scope() {
        let project = TestProject::new();
        project.write(
            "formats/block.go",
            "package formats\nconst (\n _ = 1 << iota\n _\n Width\n)\ntype Block = [Width]byte\n",
        );
        let api = project.write(
            "api/sink.go",
            "package api\nimport f \"formats\"\nconst Width = 9\ntype Sink interface { Push(f.Block) }\n",
        );
        let impls = project.write(
            "impl/sinks.go",
            "package impl\ntype Four struct{}\nfunc (Four) Push([2 + 2]byte) {}\ntype Nine struct{}\nfunc (Nine) Push([9]byte) {}\n",
        );
        let mut engine = Engine::new(project.root.clone());
        install_packages(
            &mut engine,
            &project.root,
            &[
                ("formats", &["block.go"], &[]),
                ("api", &["sink.go"], &["formats"]),
                ("impl", &["sinks.go"], &[]),
            ],
        );
        assert_eq!(reverse(&mut engine, &api, 3), [type_at(&impls, 1, "Four")]);
        assert_eq!(reverse(&mut engine, &impls, 1), [type_at(&api, 3, "Sink")]);
        assert!(reverse(&mut engine, &impls, 3).is_empty());
    }

    #[test]
    fn array_lengths_match_for_typed_and_converted_integer_constants() {
        let project = TestProject::new();
        let api = project.write(
            "api/sink.go",
            "package api\nconst Width uint8 = 4\ntype Sink interface { Push([Width]byte) }\n",
        );
        let impls = project.write(
            "impl/sinks.go",
            concat!(
                "package impl\nconst Width = uint8(uint32(4))\n",
                "type Four struct{}\nfunc (Four) Push([Width]byte) {}\n",
                "const OtherWidth = ^Width\ntype Other struct{}\nfunc (Other) Push([OtherWidth]byte) {}\n",
            ),
        );
        let mut engine = Engine::new(project.root.clone());
        install_packages(
            &mut engine,
            &project.root,
            &[("api", &["sink.go"], &[]), ("impl", &["sinks.go"], &[])],
        );
        assert_eq!(reverse(&mut engine, &api, 2), [type_at(&impls, 2, "Four")]);
    }

    #[test]
    fn a_leading_zero_in_an_array_length_expression_is_not_an_octal_prefix() {
        let project = TestProject::new();
        let api = project.write(
            "api/sink.go",
            "package api\ntype Sink interface { Push([0+10]byte) }\n",
        );
        let impls = project.write(
            "impl/sinks.go",
            "package impl\ntype Eight struct{}\nfunc (Eight) Push([8]byte) {}\ntype Ten struct{}\nfunc (Ten) Push([10]byte) {}\n",
        );
        let mut engine = Engine::new(project.root.clone());
        install_packages(
            &mut engine,
            &project.root,
            &[("api", &["sink.go"], &[]), ("impl", &["sinks.go"], &[])],
        );
        assert_eq!(reverse(&mut engine, &api, 1), [type_at(&impls, 3, "Ten")]);
    }

    #[test]
    fn unresolved_array_lengths_never_match_a_known_length() {
        let project = TestProject::new();
        let api = project.write(
            "api/sink.go",
            "package api\ntype Sink interface { Push([0]byte) }\n",
        );
        let impls = project.write(
            "impl/sinks.go",
            concat!(
                "package impl\ntype Zero struct{}\nfunc (Zero) Push([0]byte) {}\n",
                "const Cycle = Cycle\ntype Cyclic struct{}\nfunc (Cyclic) Push([Cycle]byte) {}\n",
                "var Size = 0\ntype Variable struct{}\nfunc (Variable) Push([Size]byte) {}\n",
            ),
        );
        let mut engine = Engine::new(project.root.clone());
        install_packages(
            &mut engine,
            &project.root,
            &[("api", &["sink.go"], &[]), ("impl", &["sinks.go"], &[])],
        );
        assert_eq!(reverse(&mut engine, &api, 1), [type_at(&impls, 1, "Zero")]);
    }

    #[test]
    fn disjoint_reverse_queries_reuse_every_file_of_a_probed_package() {
        let project = TestProject::new();
        let origin = project.write("receivers/r.go", RECEIVERS);
        let first = project.write(
            "both/start.go",
            "package both\ntype A interface { Start() }\n",
        );
        let second = project.write(
            "both/read.go",
            "package both\ntype B interface { Read(int) }\n",
        );
        let unrelated = project.write(
            "unrelated/use.go",
            "package unrelated\nimport _ \"both\"\ntype C interface { Other() }\n",
        );
        let mut engine = Engine::new(project.root.clone());
        install_packages(
            &mut engine,
            &project.root,
            &[
                ("both", &["read.go", "start.go"], &[]),
                ("receivers", &["r.go"], &[]),
                ("unrelated", &["use.go"], &["both"]),
            ],
        );
        assert_eq!(engine.implementation_sources.file_probes, 0);
        assert_eq!(reverse(&mut engine, &origin, 1), [type_at(&first, 1, "A")]);
        assert_eq!(engine.implementation_sources.file_probes, 4);
        engine.implementation_cache.clear();
        assert_eq!(reverse(&mut engine, &origin, 3), [type_at(&second, 1, "B")]);
        assert_eq!(engine.implementation_sources.file_probes, 4);
        assert!(!engine.state.disk_index.contains_key(&path_key(&unrelated)));
    }

    #[test]
    fn overlay_in_a_pruned_package_bypasses_disk_candidates_until_close() {
        let project = TestProject::new();
        let origin = project.write("receivers/r.go", RECEIVERS);
        let disk = "package edited\ntype Other interface { Other() }\n";
        let edited = project.write("edited/i.go", disk);
        let mut engine = Engine::new(project.root.clone());
        install_packages(
            &mut engine,
            &project.root,
            &[("edited", &["i.go"], &[]), ("receivers", &["r.go"], &[])],
        );
        assert!(reverse(&mut engine, &origin, 3).is_empty());
        assert!(
            !engine
                .implementation_cache
                .interfaces
                .contains_key("edited")
        );
        engine.open(edited.clone(), disk.into(), 1).unwrap();
        engine
            .edit(
                &edited,
                &[edit_lines(2, 2, "type Added interface { Read(int) }\n")],
                2,
            )
            .unwrap();
        assert_eq!(
            reverse(&mut engine, &origin, 3),
            [type_at(&edited, 2, "Added")]
        );
        assert_eq!(engine.implementation_sources.file_probes, 2);
        engine.close(&edited);
        assert!(reverse(&mut engine, &origin, 3).is_empty());
        assert!(
            !engine
                .implementation_cache
                .interfaces
                .contains_key("edited")
        );
        assert_eq!(engine.implementation_sources.file_probes, 3);
    }

    #[test]
    fn watched_changes_and_new_build_files_replace_only_affected_postings() {
        let project = TestProject::new();
        let origin = project.write("receivers/r.go", RECEIVERS);
        let name = "contracts/i.go";
        let disk = project.write(name, "package contracts\ntype A interface { Start() }\n");
        let mut engine = Engine::new(project.root.clone());
        let disk_packages: &[(&str, &[&str], &[&str])] =
            &[("contracts", &["i.go"], &[]), ("receivers", &["r.go"], &[])];
        install_packages(&mut engine, &project.root, disk_packages);
        assert_eq!(reverse(&mut engine, &origin, 1), [type_at(&disk, 1, "A")]);
        project.write(name, "package contracts\ntype B interface { Read(int) }\n");
        engine.file_changed(&disk, lsp_types::FileChangeType::CHANGED);
        assert!(reverse(&mut engine, &origin, 1).is_empty());
        assert!(
            !engine
                .implementation_cache
                .interfaces
                .contains_key("contracts")
        );
        assert_eq!(reverse(&mut engine, &origin, 3), [type_at(&disk, 1, "B")]);
        assert_eq!(engine.implementation_sources.file_probes, 3);

        let alternate = project.write(
            "contracts/alt.go",
            "package contracts\ntype Alt interface { Start() }\n",
        );
        install_packages(
            &mut engine,
            &project.root,
            &[
                ("contracts", &["alt.go"], &[]),
                ("receivers", &["r.go"], &[]),
            ],
        );
        assert_eq!(
            reverse(&mut engine, &origin, 1),
            [type_at(&alternate, 1, "Alt")]
        );
        assert!(reverse(&mut engine, &origin, 3).is_empty());
        assert_eq!(engine.implementation_sources.file_probes, 4);
    }

    #[test]
    fn reverse_index_follows_generic_interface_aliases_and_transitive_embedding() {
        let project = TestProject::new();
        let origin = project.write("receivers/r.go", RECEIVERS);
        let base = project.write(
            "base/i.go",
            "package base\ntype I[T any] interface { Read(T) }\n",
        );
        let alias = project.write(
            "alias/i.go",
            "package alias\nimport renamed \"base\"\ntype A = renamed.I[int]\n",
        );
        let embedded = project.write(
            "embed/i.go",
            "package embed\nimport . \"alias\"\ntype E interface { A }\n",
        );
        let mut engine = Engine::new(project.root.clone());
        install_packages(
            &mut engine,
            &project.root,
            &[
                ("receivers", &["r.go"], &[]),
                ("base", &["i.go"], &[]),
                ("alias", &["i.go"], &["base"]),
                ("embed", &["i.go"], &["alias"]),
            ],
        );
        assert!(reverse(&mut engine, &origin, 1).is_empty());
        assert_eq!(
            reverse(&mut engine, &origin, 3),
            [
                type_at(&alias, 2, "A"),
                type_at(&base, 1, "I"),
                type_at(&embedded, 2, "E"),
            ]
        );
        assert_eq!(engine.implementation_sources.file_probes, 4);
    }

    #[test]
    fn invalidating_a_shared_file_requeues_every_test_view() {
        let project = TestProject::new();
        let origin = project.write("receivers/r.go", RECEIVERS);
        let contract = project.write(
            "contracts/i.go",
            "package contracts\ntype I interface { Start() }\n",
        );
        project.write("contracts/i_test.go", "package contracts\n");
        let dir = project.root.join("contracts");
        let listing = [
            json!({"Dir": project.root.join("receivers"), "ImportPath": "receivers", "Name": "receivers", "GoFiles": ["r.go"]}),
            json!({"Dir": dir, "ImportPath": "contracts", "Name": "contracts", "GoFiles": ["i.go"]}),
            json!({"Dir": dir, "ImportPath": "contracts [contracts.test]", "ForTest": "contracts", "Name": "contracts", "GoFiles": ["i.go", "i_test.go"]}),
        ].map(|package| package.to_string()).join("\n");
        let mut engine = Engine::new(project.root.clone());
        let workspace =
            Workspace::from_test_listing(std::slice::from_ref(&project.root), &listing, true);
        assert!(engine.install_workspace(engine.workspace_load(), workspace));
        assert_eq!(
            reverse(&mut engine, &origin, 1),
            [type_at(&contract, 1, "I")]
        );
        assert_eq!(engine.implementation_sources.file_probes, 4);
        project.write(
            "contracts/i.go",
            "package contracts\ntype I interface { Read(int) }\n",
        );
        engine.file_changed(&contract, lsp_types::FileChangeType::CHANGED);
        assert_eq!(
            reverse(&mut engine, &origin, 3),
            [type_at(&contract, 1, "I")]
        );
        assert_eq!(engine.implementation_sources.file_probes, 7);
    }

    #[test]
    fn error_receivers_keep_aliases_of_the_predeclared_interface() {
        let project = TestProject::new();
        let origin = project.write("receivers/r.go", "package receivers\ntype Failure struct{}\nfunc (Failure) Error() string { return \"\" }\n");
        let reporter = project.write(
            "reports/i.go",
            "package reports\ntype Report interface { Error() string }\n",
        );
        project.write("implicit/i.go", "package implicit\ntype E = error\n");
        project.write(
            "other/i.go",
            "package other\ntype I interface { Other() }\n",
        );
        let mut engine = Engine::new(project.root.clone());
        install_packages(
            &mut engine,
            &project.root,
            &[
                ("receivers", &["r.go"], &[]),
                ("reports", &["i.go"], &[]),
                ("implicit", &["i.go"], &[]),
                ("other", &["i.go"], &[]),
            ],
        );
        assert!(reverse(&mut engine, &origin, 1).contains(&type_at(&reporter, 1, "Report")));
        let candidates = &engine.implementation_cache.interfaces;
        assert!(candidates.contains_key("implicit"));
        assert!(!candidates.contains_key("other"));
    }

    #[test]
    fn cancelled_partial_probes_recover_after_file_invalidation() {
        let project = TestProject::new();
        let origin = project.write("receivers/r.go", RECEIVERS);
        let files: Vec<_> = (0..40).map(|index| format!("i{index:02}.go")).collect();
        for file in &files {
            project.write(&format!("contracts/{file}"), "package contracts\n");
        }
        let contract = project.write(
            "contracts/i00.go",
            "package contracts\ntype I interface { Start() }\n",
        );
        let files: Vec<_> = files.iter().map(String::as_str).collect();
        let mut engine = Engine::new(project.root.clone());
        install_packages(
            &mut engine,
            &project.root,
            &[("contracts", &files, &[]), ("receivers", &["r.go"], &[])],
        );
        let QueryProgress::Searching(mut search) = engine
            .implementations(&origin, Position::new(1, 6))
            .unwrap()
        else {
            panic!("expected a batched reverse search");
        };
        assert!(!search.advance(&mut engine).unwrap());
        assert!((1..40).contains(&engine.implementation_sources.file_probes));
        drop(search);
        project.write(
            "contracts/i00.go",
            "package contracts\ntype I interface { Read(int) }\n",
        );
        engine
            .implementation_sources
            .invalidate(&path_key(&contract));
        assert!(reverse(&mut engine, &origin, 1).is_empty());
        assert_eq!(
            reverse(&mut engine, &origin, 3),
            [type_at(&contract, 1, "I")]
        );
    }

    fn edit_lines(start: u32, end: u32, text: &str) -> TextDocumentContentChangeEvent {
        TextDocumentContentChangeEvent {
            range: Some(Range::new(Position::new(start, 0), Position::new(end, 0))),
            range_length: None,
            text: text.into(),
        }
    }

    fn location(path: &Path, line: u32) -> Location {
        (
            path.to_owned(),
            Range::new(Position::new(line, 5), Position::new(line, 9)),
        )
    }

    fn opened(project: &TestProject, text: &str) -> (Engine, PathBuf) {
        let path = project.write("item.go", text);
        let mut engine = Engine::new(project.root.clone());
        engine.open(path.clone(), text.into(), 1).unwrap();
        (engine, path)
    }

    fn seed_cache(engine: &mut Engine, path: &Path, line: u32) -> QueryKey {
        let key = (path_key(path), Position::new(line, 5));
        let cache = &mut engine.implementation_cache;
        cache.remember(key.clone(), vec![location(path, line)]);
        cache.packages.insert(
            PackageProbeKey::Method("Run".into()),
            HashSet::from(["example.test/sample".into()]),
        );
        cache
            .interfaces
            .insert("example.test/sample".into(), InterfaceIndex::default());
        key
    }

    #[test]
    fn shifts_queries_and_results_in_batch_order_and_drops_deleted_ranges() {
        let project = TestProject::new();
        let text = "package sample\r\n// item\r\n\r\ntype Item struct{}\r\n";
        let (mut engine, path) = opened(&project, text);
        let other = project.root.join("other.go");
        let old_key = seed_cache(&mut engine, &path, 3);
        let cache = &mut engine.implementation_cache;
        cache.remember(
            (path_key(&other), Position::new(5, 0)),
            vec![location(&other, 5), location(&path, 3)],
        );
        cache.remember(
            (path_key(&path), Position::new(2, 0)),
            vec![location(&other, 5)],
        );
        cache.remember(
            (path_key(&other), Position::new(6, 0)),
            vec![(
                path.clone(),
                Range::new(Position::new(1, 0), Position::new(2, 0)),
            )],
        );

        engine
            .edit(
                &path,
                &[
                    edit_lines(1, 1, "// inserted\r\n\r\n"),
                    edit_lines(4, 5, ""),
                ],
                2,
            )
            .unwrap();
        let cache = &engine.implementation_cache;
        let shifted_key = (path_key(&path), Position::new(4, 5));
        assert_eq!(
            cache.results.get(&shifted_key),
            Some(&vec![location(&path, 4)])
        );
        assert_eq!(
            cache.results.get(&(path_key(&other), Position::new(5, 0))),
            Some(&vec![location(&other, 5), location(&path, 4)])
        );
        assert_eq!(cache.results.len(), 2);
        assert!(!cache.results.contains_key(&old_key));
        assert_eq!(cache.packages.len(), 1);
        assert!(cache.interfaces.is_empty());
        let mut snapshot = engine.snapshot();
        let document = snapshot.document(&path).unwrap().unwrap();
        assert_eq!(document.version(), Some(2));
        assert_eq!(
            document.text(),
            "package sample\r\n// inserted\r\n\r\n// item\r\ntype Item struct{}\r\n"
        );
    }

    #[test]
    fn retains_every_cache_for_ordinary_comment_replacement_with_equal_line_count() {
        let project = TestProject::new();
        let (mut engine, path) = opened(&project, "package sample\n// old\ntype Item struct{}\n");
        let key = seed_cache(&mut engine, &path, 2);
        engine
            .edit(&path, &[edit_lines(1, 2, "// revised 😀\n")], 2)
            .unwrap();
        let cache = &engine.implementation_cache;
        assert_eq!(cache.results.get(&key), Some(&vec![location(&path, 2)]));
        assert_eq!(cache.packages.len(), 1);
        assert_eq!(cache.interfaces.len(), 1);
    }

    #[test]
    fn clears_all_caches_for_directives_bom_and_semantic_edits() {
        const TEXT: &str = "package sample\n// old\ntype Item struct{}\n";
        for (label, text, changes) in [
            ("directive", TEXT, vec![edit_lines(1, 2, "//go:noinline\n")]),
            (
                "build constraint",
                TEXT,
                vec![edit_lines(0, 0, "// +build example\n\n")],
            ),
            (
                "line directive",
                TEXT,
                vec![edit_lines(1, 2, "//line mapped.go:42\n")],
            ),
            (
                "block comment",
                TEXT,
                vec![edit_lines(1, 2, "/* block */\n")],
            ),
            (
                "BOM",
                "\u{feff}package sample\n// old\ntype Item struct{}\n",
                vec![edit_lines(0, 0, "\n")],
            ),
            (
                "semantic edit after trivia",
                TEXT,
                vec![edit_lines(1, 1, "\n"), edit_lines(3, 4, "type Item int\n")],
            ),
            (
                "full replacement",
                TEXT,
                vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: TEXT.into(),
                }],
            ),
        ] {
            let project = TestProject::new();
            let (mut engine, path) = opened(&project, text);
            seed_cache(&mut engine, &path, 2);
            engine.edit(&path, &changes, 2).unwrap();
            assert!(engine.implementation_cache.is_empty(), "{label}");
        }
    }

    #[test]
    fn failed_edit_keeps_version_declarations_and_implementation_cache() {
        let project = TestProject::new();
        let text = "package sample\n\ntype Item struct{}\n";
        let (mut engine, path) = opened(&project, text);
        let key = seed_cache(&mut engine, &path, 2);
        let before = std::ptr::from_ref(engine.state.documents[&key.0].declarations());
        let changes = [edit_lines(1, 1, "// inserted\n"), edit_lines(99, 99, "\n")];
        assert_eq!(
            engine.edit(&path, &changes, 2).unwrap_err().to_string(),
            "invalid edit start"
        );
        let cache = &engine.implementation_cache;
        assert_eq!(cache.results.get(&key), Some(&vec![location(&path, 2)]));
        assert_eq!(cache.packages.len(), 1);
        assert_eq!(cache.interfaces.len(), 1);
        let mut snapshot = engine.snapshot();
        let document = snapshot.document(&path).unwrap().unwrap();
        assert_eq!(document.version(), Some(1));
        assert_eq!(document.text(), text);
        assert_eq!(
            std::ptr::from_ref(document.indexed().declarations()),
            before
        );
    }
}
