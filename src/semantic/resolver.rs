use std::collections::{BTreeMap, HashMap, HashSet, btree_map::Entry};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use lsp_types::{Position, Range};
use tree_sitter::Node;

use super::index::IndexReader;
use super::navigation_trace::record;
use super::source::{
    Import, ReferenceQuery, Source, is_declaration_name, is_type_declaration, node_text,
    read_navigation_source,
};
use crate::scope::{
    declaration_binding, lexical_binding, receiver_type_parameters, reference_binding,
};
use crate::symbols::{
    self, Container, FunctionSignature, GenericParameter, InlineType, Symbol, SymbolKind,
    TypeDetails, TypeRef, TypeRefKind,
};
use crate::workspace::{Package, Workspace, path_key};

pub(super) const MAX_TYPE_DEPTH: usize = 32;

#[derive(Clone)]
pub(crate) struct TypeIdentity<'w> {
    pub(super) package: &'w Package,
    pub(super) kind: TypeKind<'w>,
    pub(super) pointers: u8,
    pub(super) addressable: bool,
    pub(super) type_expression: bool,
}

#[derive(Clone)]
pub(super) enum TypeKind<'w> {
    Named {
        name: String,
        arguments: Vec<Option<TypeIdentity<'w>>>,
    },
    Container(Box<Container<TypeIdentity<'w>>>, Option<(PathBuf, Range)>),
    Function {
        file: PathBuf,
        signature: Arc<FunctionSignature>,
        substitutions: Substitutions<'w>,
        method_receiver: Option<Box<TypeIdentity<'w>>>,
    },
    Parameter(usize),
    Inline {
        file: PathBuf,
        shape: Arc<InlineType>,
        substitutions: Substitutions<'w>,
    },
}

#[derive(Clone, Eq, PartialEq)]
pub(super) enum TypeKey {
    Named(*const Package, String),
    Inline(PathBuf, Position),
}

impl<'w> TypeIdentity<'w> {
    pub(super) fn new(package: &'w Package, kind: TypeKind<'w>, pointers: u8) -> Self {
        Self {
            package,
            kind,
            pointers,
            addressable: false,
            type_expression: false,
        }
    }

    pub(super) fn named(package: &'w Package, name: &str) -> Self {
        Self::new(
            package,
            TypeKind::Named {
                name: name.to_owned(),
                arguments: Vec::new(),
            },
            0,
        )
    }

    pub(super) fn key(&self) -> Option<TypeKey> {
        match &self.kind {
            TypeKind::Named { name, .. } => Some(TypeKey::Named(
                std::ptr::from_ref(self.package),
                name.clone(),
            )),
            TypeKind::Inline { file, shape, .. } => {
                Some(TypeKey::Inline(file.clone(), shape.origin.start))
            }
            TypeKind::Container(..) | TypeKind::Function { .. } | TypeKind::Parameter(_) => None,
        }
    }

    fn project_properties(&mut self, from: &Self) {
        self.pointers = self.pointers.saturating_add(from.pointers);
        self.addressable = from.addressable;
        self.type_expression = from.type_expression;
    }
}

#[derive(Clone)]
pub(super) struct ResolvedSymbol<'w> {
    pub(super) package: &'w Package,
    pub(super) file: PathBuf,
    pub(super) symbol: Symbol,
    pub(super) receiver: Option<TypeIdentity<'w>>,
    pub(super) method_receiver: Option<Box<TypeIdentity<'w>>>,
}

impl ResolvedSymbol<'_> {
    pub(super) fn location(self) -> (PathBuf, Range) {
        (self.file, self.symbol.range)
    }
}

#[allow(dead_code, reason = "consumed by completion in Wave 2")]
pub(super) struct VisibleMember<'w> {
    pub(super) name: String,
    pub(super) resolved: ResolvedSymbol<'w>,
    /// Embedded fields from the queried type to the owner. Empty for direct members.
    pub(super) embedded_path: Vec<String>,
}

#[allow(dead_code, reason = "consumed by completion in Wave 2")]
pub(super) struct StructField<'w> {
    pub(super) member: VisibleMember<'w>,
    /// Whether this entire path can be written as nested literal keys.
    pub(super) visible: bool,
    pub(super) ty: Option<TypeIdentity<'w>>,
}

struct MemberPath<'w> {
    owner: TypeIdentity<'w>,
    ancestors: Vec<TypeKey>,
    embedded_path: Vec<String>,
    indirect: bool,
    fields_only: bool,
}

// Unexported names of different packages are independent selector names.
type MemberKey<'w> = (String, Option<&'w str>);

impl<'w> VisibleMember<'w> {
    fn key(&self) -> MemberKey<'w> {
        (
            self.name.clone(),
            (!is_exported(&self.name)).then(|| package_identity(self.resolved.package)),
        )
    }

    fn visible_from(&self, from: &Package) -> bool {
        is_exported(&self.name) || package_identity(self.resolved.package) == package_identity(from)
    }
}

fn package_identity(package: &Package) -> &str {
    package
        .id
        .split_once(" [")
        .map_or(&package.id, |(id, _)| id)
}

pub(super) type Substitutions<'w> = Vec<(String, Option<TypeIdentity<'w>>)>;

enum DeclarationSelection<'a> {
    All,
    Exports(&'a dyn Fn(&str) -> bool),
    Local {
        matches: &'a dyn Fn(&str) -> bool,
        skipped: &'a mut HashSet<String>,
    },
}

pub(super) struct Resolver<'w, 'idx> {
    pub(super) workspace: &'w Workspace,
    pub(super) indexes: IndexReader<'idx>,
    diagnostic_symbols: Option<SymbolMemo<'w>>,
}

type SymbolMemo<'w> = HashMap<(&'w str, String, Option<String>), Option<ResolvedSymbol<'w>>>;

impl<'w, 'idx> Resolver<'w, 'idx> {
    pub(super) fn new(
        workspace: &'w Workspace,
        indexes: IndexReader<'idx>,
        diagnostics: bool,
    ) -> Self {
        Self {
            workspace,
            indexes,
            diagnostic_symbols: diagnostics.then(HashMap::new),
        }
    }
}

impl<'w> Resolver<'w, '_> {
    pub(super) fn declaration_location(
        &mut self,
        from: &'w Package,
        source: &Source,
        reference: ReferenceQuery<'_>,
        root: &Path,
    ) -> Result<Option<(PathBuf, Range)>> {
        match reference {
            ReferenceQuery::Name(name) => self.name_location(from, source, name, root),
            ReferenceQuery::Member { receiver, name } => {
                let member =
                    self.selector(from, source, receiver, node_text(name, &source.text), 0)?;
                if let Some(member) = &member {
                    record(self.indexes.navigation, |counts| {
                        counts.definition_route = if member.receiver.is_some() {
                            "member"
                        } else {
                            "cross_package"
                        };
                    });
                }
                Ok(member.map(ResolvedSymbol::location))
            }
            ReferenceQuery::LiteralKey { value, name } => {
                let Some(ty) = self.literal_type(from, source, value, 0)? else {
                    return Ok(None);
                };
                if self.container(ty.clone())?.is_some() {
                    return self.name_location(from, source, name, root);
                }
                let member = self.member(from, &ty, node_text(name, &source.text), false)?;
                let member =
                    member.filter(|found| matches!(found.symbol.kind, SymbolKind::Value(_)));
                if member.is_some() {
                    record(self.indexes.navigation, |counts| {
                        counts.definition_route = "member"
                    });
                }
                Ok(member.map(ResolvedSymbol::location))
            }
        }
    }

    pub(super) fn reference_location(
        &mut self,
        from: &'w Package,
        source: &Source,
        reference: ReferenceQuery<'_>,
        root: &Path,
    ) -> Result<Option<(PathBuf, Range)>> {
        if let ReferenceQuery::Name(name) = reference {
            if let Some(binding) = reference_binding(name, &source.text) {
                return Ok(Some((source.path.clone(), source.range(binding))));
            }
            if is_declaration_name(name) {
                return Ok(Some((source.path.clone(), source.range(name))));
            }
        }
        self.declaration_location(from, source, reference, root)
    }
    pub(super) fn type_definition(
        &mut self,
        from: &'w Package,
        source: &Source,
        reference: ReferenceQuery<'_>,
        root: &Path,
    ) -> Result<Option<(PathBuf, Range)>> {
        let ty = match reference {
            ReferenceQuery::Name(name) => return self.name_type_location(from, source, name, root),
            ReferenceQuery::Member { receiver, name } => {
                let spelling = node_text(name, &source.text);
                let Some(symbol) = self.selector(from, source, receiver, spelling, 0)? else {
                    return Ok(None);
                };
                if symbol.receiver.is_some() {
                    record(self.indexes.navigation, |counts| {
                        counts.definition_route = "member"
                    });
                }
                if let Some(location) = self.field_type_parameter(&symbol)? {
                    return Ok(Some(location));
                }
                self.symbol_type(source, &symbol, spelling, 0)?
            }
            ReferenceQuery::LiteralKey { value, name } => {
                let Some(owner) = self.literal_type(from, source, value, 0)? else {
                    return Ok(None);
                };
                if self.container(owner.clone())?.is_some() {
                    self.infer_type(from, source, name, 0, 0)?
                } else {
                    let spelling = node_text(name, &source.text);
                    let Some(symbol) = self.member(from, &owner, spelling, false)? else {
                        return Ok(None);
                    };
                    record(self.indexes.navigation, |counts| {
                        counts.definition_route = "member"
                    });
                    self.symbol_type(source, &symbol, spelling, 0)?
                }
            }
        };
        match ty {
            Some(ty) => self.type_location(ty, root),
            None => Ok(None),
        }
    }

    fn field_type_parameter(
        &mut self,
        field: &ResolvedSymbol<'w>,
    ) -> Result<Option<(PathBuf, Range)>> {
        let SymbolKind::Value(Some(TypeRef {
            kind: TypeRefKind::Named(named),
            ..
        })) = &field.symbol.kind
        else {
            return Ok(None);
        };
        let Some(receiver) = &field.receiver else {
            return Ok(None);
        };
        let TypeKind::Named { name, .. } = &receiver.kind else {
            return Ok(None);
        };
        if named.qualifier.is_some() || !named.arguments.is_empty() {
            return Ok(None);
        }
        let Some((file, details)) = self.type_info(receiver.package, name)? else {
            return Ok(None);
        };
        Ok(details
            .parameters
            .iter()
            .find(|parameter| parameter.name == named.name)
            .map(|parameter| (file, parameter.name_range)))
    }

    fn name_type_location(
        &mut self,
        from: &'w Package,
        source: &Source,
        name: Node<'_>,
        root: &Path,
    ) -> Result<Option<(PathBuf, Range)>> {
        let owner = name.parent();
        if name.kind() == "package_identifier"
            || owner.is_some_and(|node| node.kind() == "import_spec")
        {
            return Ok(None);
        }
        let spelling = node_text(name, &source.text);
        let location = |node| Some((source.path.clone(), source.range(node)));
        let binding = declaration_binding(name, &source.text)
            .or_else(|| lexical_binding(name, spelling, &source.text));
        let ty = if let Some(binding) = binding {
            if binding.name.parent().is_some_and(is_lexical_type_owner) {
                record(self.indexes.navigation, |counts| {
                    counts.definition_route = "local"
                });
                return Ok(location(binding.name));
            }
            if let Some(explicit) = binding.explicit_type {
                if let Some(local) = lexical_type_declaration(source, explicit) {
                    record(self.indexes.navigation, |counts| {
                        counts.definition_route = "local"
                    });
                    return Ok(location(local));
                }
                self.source_type(from, source, explicit)
            } else if let Some(initializer) = binding.initializer {
                if binding.result_index == 0
                    && let Some(local) = lexical_expression_type(source, initializer)
                {
                    record(self.indexes.navigation, |counts| {
                        counts.definition_route = "local"
                    });
                    return Ok(location(local));
                }
                let ty = self.infer_type(from, source, initializer, 0, binding.result_index)?;
                if ty.is_none()
                    && binding.result_index == 0
                    && let Some(builtin) = untyped_default(initializer)
                {
                    return self.builtin_type_location(builtin, root);
                }
                ty
            } else {
                None
            }
        } else if let Some(owner) = owner.filter(|node| {
            matches!(
                node.kind(),
                "function_declaration" | "method_declaration" | "method_elem"
            ) && node.child_by_field_name("name") == Some(name)
        }) {
            self.source_type(from, source, owner)
        } else if source
            .imported_package(self.workspace, from, spelling)
            .is_some()
        {
            None
        } else {
            self.infer_type(from, source, name, 0, 0)?.or_else(|| {
                (name.kind() == "type_identifier")
                    .then(|| self.source_type(from, source, name))
                    .flatten()
            })
        };
        match ty {
            Some(ty) => self.type_location(ty, root),
            None => Ok(None),
        }
    }

    fn builtin_type_location(
        &mut self,
        name: &str,
        root: &Path,
    ) -> Result<Option<(PathBuf, Range)>> {
        let file = self.workspace.builtin_source(root)?;
        let probe = self.indexes.navigation;
        let found = self.indexes.file_index(file)?.symbols.declaration(name);
        if found.is_some() {
            record(probe, |counts| counts.definition_route = "builtin");
        }
        Ok(found.map(|symbol| (file.to_owned(), symbol.range)))
    }

    fn type_location(
        &mut self,
        mut ty: TypeIdentity<'w>,
        root: &Path,
    ) -> Result<Option<(PathBuf, Range)>> {
        for _ in 0..MAX_TYPE_DEPTH {
            match ty.kind {
                TypeKind::Named { name, .. } => {
                    if let Some(symbol) = self.find_symbol(ty.package, &name, None)? {
                        return Ok(matches!(symbol.symbol.kind, SymbolKind::Type)
                            .then(|| symbol.location()));
                    }
                    if !is_predeclared_type(&name) {
                        return Ok(None);
                    }
                    return self.builtin_type_location(&name, root);
                }
                TypeKind::Container(container, location) => {
                    // GoLand follows the direct type reference in []T or [N]T.
                    // A nested type expression retains the array/slice syntax as the target.
                    if let Container::Array { element, .. } | Container::Slice(element) = *container
                        && element.pointers == 0
                        && matches!(element.kind, TypeKind::Named { .. })
                        && let Some(target) = self.type_location(element, root)?
                    {
                        return Ok(Some(target));
                    }
                    return Ok(location);
                }
                TypeKind::Inline { file, shape, .. } => return Ok(Some((file, shape.origin))),
                TypeKind::Function {
                    file,
                    signature,
                    substitutions,
                    ..
                } => {
                    if let Some(range) = signature.origin.function_type().or_else(|| {
                        (signature.results.len() > 1)
                            .then_some(signature.result_tuple)
                            .flatten()
                    }) {
                        return Ok(Some((file, range)));
                    }
                    let [Some(result)] = signature.results.as_ref() else {
                        return Ok(None);
                    };
                    if let TypeRefKind::Named(named) = &result.kind
                        && named.qualifier.is_none()
                        && let Some(parameter) = signature
                            .type_parameters
                            .iter()
                            .find(|parameter| parameter.name == named.name)
                    {
                        return Ok(Some((file, parameter.name_range)));
                    }
                    let imports = &self.indexes.file_index(&file)?.imports;
                    let Some(result) = materialize_type(
                        self.workspace,
                        ty.package,
                        &file,
                        imports,
                        result.clone(),
                        &substitutions,
                    ) else {
                        return Ok(None);
                    };
                    ty = result;
                }
                TypeKind::Parameter(_) => return Ok(None),
            }
        }
        Ok(None)
    }

    fn name_location(
        &mut self,
        from: &'w Package,
        source: &Source,
        name: Node<'_>,
        root: &Path,
    ) -> Result<Option<(PathBuf, Range)>> {
        let location = self.source_name_location(from, source, name)?;
        let identifier = node_text(name, &source.text);
        if location.is_some() || !is_predeclared(identifier) {
            return Ok(location);
        }
        let file = self.workspace.builtin_source(root)?;
        let probe = self.indexes.navigation;
        let definition = self
            .indexes
            .file_index(file)?
            .symbols
            .declaration(identifier);
        if definition.is_some() {
            record(probe, |counts| counts.definition_route = "builtin");
        }
        Ok(definition.map(|symbol| (file.to_owned(), symbol.range)))
    }

    pub(super) fn source_name_location(
        &mut self,
        from: &'w Package,
        source: &Source,
        name: Node<'_>,
    ) -> Result<Option<(PathBuf, Range)>> {
        let identifier = node_text(name, &source.text);
        if let Some(binding) = lexical_binding(name, identifier, &source.text) {
            record(self.indexes.navigation, |counts| {
                counts.definition_route = "local"
            });
            return Ok(Some((source.path.clone(), source.range(binding.name))));
        }
        if let Some((import, _)) = source.imported_package(self.workspace, from, identifier) {
            return Ok(Some((source.path.clone(), import.binding)));
        }
        if let Some(location) = self.indexes.find(from, |index| {
            index
                .symbols
                .declaration(identifier)
                .map(|symbol| symbol.range)
        })? {
            return Ok(Some(location));
        }
        if is_exported(identifier) {
            return self
                .indexes
                .find_dot_import(self.workspace, from, &source.imports, identifier);
        }
        Ok(None)
    }

    pub(super) fn literal_type(
        &mut self,
        from: &'w Package,
        source: &Source,
        value: Node<'_>,
        depth: usize,
    ) -> Result<Option<TypeIdentity<'w>>> {
        if depth >= MAX_TYPE_DEPTH {
            return Ok(None);
        }
        let Some(parent) = value.parent() else {
            return Ok(None);
        };
        if parent.kind() == "composite_literal" {
            return Ok(parent
                .child_by_field_name("type")
                .and_then(|ty| self.source_type(from, source, ty)));
        }
        let Some(container_element) = parent.parent() else {
            return Ok(None);
        };
        let (surrounding, map_key) = if container_element.kind() == "keyed_element" {
            (
                container_element.parent(),
                container_element.child_by_field_name("key") == Some(parent),
            )
        } else {
            (Some(container_element), false)
        };
        let Some(surrounding) = surrounding else {
            return Ok(None);
        };
        if surrounding.kind() != "literal_value" {
            return Ok(None);
        }
        let Some(ty) = self.literal_type(from, source, surrounding, depth + 1)? else {
            return Ok(None);
        };
        Ok(match self.container(ty)? {
            Some((Container::Slice(ty) | Container::Array { element: ty, .. }, 0)) => Some(ty),
            Some((Container::Map { key, value }, 0)) => Some(if map_key { key } else { value }),
            _ => None,
        })
    }

    pub(super) fn selector(
        &mut self,
        from: &'w Package,
        source: &Source,
        receiver: Node<'_>,
        name: &str,
        depth: usize,
    ) -> Result<Option<ResolvedSymbol<'w>>> {
        if matches!(receiver.kind(), "identifier" | "package_identifier") {
            let qualifier = node_text(receiver, &source.text);
            if lexical_binding(receiver, qualifier, &source.text).is_none()
                && let Some((_, package)) = source.imported_package(self.workspace, from, qualifier)
            {
                return if is_exported(name) {
                    self.find_symbol(package, name, None)
                } else {
                    Ok(None)
                };
            }
        }

        let Some(ty) = self.infer_type(from, source, receiver, depth + 1, 0)? else {
            return Ok(None);
        };
        self.member_selector(from, ty, name)
    }

    pub(super) fn member_selector(
        &mut self,
        from: &Package,
        ty: TypeIdentity<'w>,
        name: &str,
    ) -> Result<Option<ResolvedSymbol<'w>>> {
        let mut member = self.member(from, &ty, name, true)?;
        if ty.type_expression
            && let Some(member) = &mut member
        {
            member.method_receiver = Some(Box::new(ty));
        }
        Ok(member)
    }

    pub(super) fn member(
        &mut self,
        from: &Package,
        ty: &TypeIdentity<'w>,
        name: &str,
        promote: bool,
    ) -> Result<Option<ResolvedSymbol<'w>>> {
        if ty.pointers > 1 {
            return Ok(None);
        }
        let mut frontier: Vec<(TypeIdentity<'w>, Vec<TypeKey>)> = vec![(ty.clone(), Vec::new())];

        // A unique match at the shallowest embedding depth wins. Distinct
        // paths to the same declaration are still ambiguous for struct fields.
        for _ in 0..MAX_TYPE_DEPTH {
            let mut next = Vec::new();
            let mut found = None;
            let mut blocked = false;
            for (mut owner, mut visited) in frontier {
                let mut inherited = false;
                while let Some(key) = owner.key() {
                    if owner.pointers > 1 || visited.contains(&key) {
                        break;
                    }
                    visited.push(key);

                    if (is_exported(name) || std::ptr::eq(from, owner.package))
                        && let Some(mut symbol) = self.direct_member(&owner, name, inherited)?
                    {
                        if found.is_some() || blocked {
                            return Ok(None);
                        }
                        let method = matches!(symbol.symbol.kind, SymbolKind::Function(_));
                        blocked = (owner.type_expression && !method)
                            || (symbol.symbol.pointer_receiver
                                && owner.pointers == 0
                                && !owner.addressable);
                        if !blocked {
                            symbol.receiver = Some(owner);
                            found = Some(symbol);
                        }
                        break;
                    }

                    let Some((file, details, substitutions)) = self.type_context(&owner)? else {
                        break;
                    };
                    let index = self.indexes.file_index(&file)?;
                    if let Some(base) = details.alias.or_else(|| {
                        inherited = true;
                        details.underlying
                    }) {
                        let Some(mut target) = materialize_type(
                            self.workspace,
                            owner.package,
                            &file,
                            &index.imports,
                            base,
                            &substitutions,
                        ) else {
                            break;
                        };
                        target.project_properties(&owner);
                        owner = target;
                        continue;
                    }
                    if promote && found.is_none() && !blocked {
                        for embedded in details.embedded {
                            let Some(mut target) = materialize_type(
                                self.workspace,
                                owner.package,
                                &file,
                                &index.imports,
                                embedded,
                                &substitutions,
                            ) else {
                                continue;
                            };
                            target.addressable = owner.addressable && !owner.type_expression;
                            target.pointers = target.pointers.max(owner.pointers);
                            target.type_expression = owner.type_expression;
                            next.push((target, visited.clone()));
                        }
                    }
                    break;
                }
            }
            if blocked || found.is_some() || next.is_empty() {
                return Ok(found);
            }
            frontier = next;
        }
        Ok(None)
    }

    /// Enumerates declarations in the supplied package view. An imported
    /// package contributes exported names; a local package contributes its
    /// package scope. The result is ordered by name.
    #[allow(dead_code, reason = "consumed by completion in Wave 2")]
    pub(super) fn package_declarations(
        &mut self,
        from: &Package,
        target: &'w Package,
    ) -> Result<Vec<(String, ResolvedSymbol<'w>)>> {
        self.package_declarations_where(from, target, DeclarationSelection::All)
    }

    pub(super) fn matching_exports(
        &mut self,
        from: &Package,
        target: &'w Package,
        matches: impl Fn(&str) -> bool,
    ) -> Result<Vec<(String, ResolvedSymbol<'w>)>> {
        self.package_declarations_where(from, target, DeclarationSelection::Exports(&matches))
    }

    pub(super) fn local_declarations(
        &mut self,
        target: &'w Package,
        matches: impl Fn(&str) -> bool,
        skipped: &mut HashSet<String>,
    ) -> Result<Vec<(String, ResolvedSymbol<'w>)>> {
        self.package_declarations_where(
            target,
            target,
            DeclarationSelection::Local {
                matches: &matches,
                skipped,
            },
        )
    }

    fn package_declarations_where(
        &mut self,
        from: &Package,
        target: &'w Package,
        mut selection: DeclarationSelection<'_>,
    ) -> Result<Vec<(String, ResolvedSymbol<'w>)>> {
        let mut declarations = BTreeMap::new();
        let local = !matches!(selection, DeclarationSelection::Exports(_))
            && package_identity(from) == package_identity(target);
        let project = self.workspace.is_project_package(target);
        let external = target.is_external();
        let can_prefilter = self.indexes.completion.is_some() && !target.load_error;
        for file in self.indexes.files(target).iter() {
            let index = match &mut selection {
                DeclarationSelection::Exports(matches) if can_prefilter => self
                    .indexes
                    .filtered_index(file, project, external, |names| names.may_export(*matches))?,
                DeclarationSelection::Local { matches, skipped } if can_prefilter => {
                    self.indexes
                        .filtered_index(file, project, external, |names| {
                            names.local_matches(*matches, skipped)
                        })?
                }
                _ => Some(self.indexes.file_index(file)?),
            };
            let Some(index) = index else {
                continue;
            };
            for (name, symbol) in index.symbols.declarations() {
                if name != "init"
                    && (local || is_exported(name))
                    && !matches!(selection, DeclarationSelection::Exports(matches) if !matches(name))
                {
                    declarations
                        .entry(name.to_owned())
                        .or_insert_with(|| ResolvedSymbol {
                            package: target,
                            file: file.clone(),
                            symbol: symbol.clone(),
                            receiver: None,
                            method_receiver: None,
                        });
                }
            }
        }
        Ok(declarations.into_iter().collect())
    }

    /// Enumerates selectors at the shallowest embedding depth for each name.
    /// A field or an unavailable pointer method can still shadow deeper names.
    #[allow(dead_code, reason = "consumed by completion in Wave 2")]
    pub(super) fn visible_members(
        &mut self,
        from: &Package,
        ty: &TypeIdentity<'w>,
    ) -> Result<Vec<VisibleMember<'w>>> {
        let mut names = BTreeMap::new();
        self.walk_member_paths(ty, false, |depth, mut member, pointer_allowed| {
            if !member.visible_from(from) {
                return;
            }
            match names.entry(member.key()) {
                Entry::Vacant(entry) => {
                    let method = matches!(member.resolved.symbol.kind, SymbolKind::Function(_));
                    let eligible = (!ty.type_expression || method)
                        && (!member.resolved.symbol.pointer_receiver || pointer_allowed);
                    if ty.type_expression && eligible {
                        member.resolved.method_receiver = Some(Box::new(ty.clone()));
                    }
                    entry.insert((depth, eligible.then_some(member)));
                }
                Entry::Occupied(mut entry) if entry.get().0 == depth => {
                    entry.get_mut().1 = None;
                }
                Entry::Occupied(_) => {}
            }
        })?;
        Ok(names
            .into_values()
            .filter_map(|(_, member)| member)
            .collect())
    }

    /// Enumerates the individual paths to fields, starting with direct fields.
    /// A caller can use the path to construct nested keyed literals and skip
    /// descendants of an embedded field that is already assigned.
    #[allow(dead_code, reason = "consumed by completion in Wave 2")]
    pub(super) fn struct_fields(
        &mut self,
        from: &Package,
        ty: &TypeIdentity<'w>,
    ) -> Result<Vec<StructField<'w>>> {
        let mut members = Vec::new();
        self.walk_member_paths(ty, true, |_, member, _| members.push(member))?;
        let mut hidden_paths = HashSet::new();
        members
            .into_iter()
            .map(|member| {
                let visible =
                    member.visible_from(from) && !hidden_paths.contains(&member.embedded_path);
                if !visible {
                    let mut path = member.embedded_path.clone();
                    path.push(member.name.clone());
                    hidden_paths.insert(path);
                }
                let ty = match &member.resolved.symbol.kind {
                    SymbolKind::Value(Some(reference)) => {
                        self.type_from_symbol(&member.resolved, reference.clone(), false)?
                    }
                    _ => None,
                };
                Ok(StructField {
                    member,
                    visible,
                    ty,
                })
            })
            .collect()
    }

    fn walk_member_paths(
        &mut self,
        ty: &TypeIdentity<'w>,
        fields_only: bool,
        mut visit: impl FnMut(usize, VisibleMember<'w>, bool),
    ) -> Result<()> {
        let mut frontier = vec![MemberPath {
            owner: ty.clone(),
            ancestors: Vec::new(),
            embedded_path: Vec::new(),
            indirect: false,
            fields_only,
        }];
        for depth in 0..MAX_TYPE_DEPTH {
            if frontier.is_empty() {
                break;
            }
            let mut next = Vec::new();
            for path in frontier {
                for (member, pointer_allowed) in self.members_at_path(path, &mut next)? {
                    visit(depth, member, pointer_allowed);
                }
            }
            frontier = next;
        }
        Ok(())
    }

    fn members_at_path(
        &mut self,
        path: MemberPath<'w>,
        next: &mut Vec<MemberPath<'w>>,
    ) -> Result<Vec<(VisibleMember<'w>, bool)>> {
        let MemberPath {
            mut owner,
            mut ancestors,
            embedded_path,
            indirect,
            mut fields_only,
        } = path;
        let mut members = BTreeMap::new();
        let mut inherited = false;
        while let Some(key) = owner.key() {
            if owner.pointers > 1 || ancestors.len() >= MAX_TYPE_DEPTH || ancestors.contains(&key) {
                break;
            }
            let Some((file, details, substitutions)) = self.type_context(&owner)? else {
                break;
            };
            if details.interface {
                if !fields_only && owner.pointers == 0 {
                    self.interface_members(owner, &mut ancestors, &embedded_path, &mut members)?;
                }
                break;
            }
            ancestors.push(key);
            let pointer_allowed =
                indirect || owner.pointers == 1 || (!owner.type_expression && owner.addressable);
            self.add_direct_members(
                &owner,
                &embedded_path,
                inherited || fields_only,
                pointer_allowed,
                &mut members,
            )?;

            let imports = &self.indexes.file_index(&file)?.imports;
            let resolve = |reference| {
                materialize_type(
                    self.workspace,
                    owner.package,
                    &file,
                    imports,
                    reference,
                    &substitutions,
                )
            };
            if let Some(base) = details.alias.or_else(|| {
                inherited = true;
                details.underlying
            }) {
                let Some(mut target) = resolve(base) else {
                    break;
                };
                // A defined pointer permits field selectors through one
                // dereference. It does not inherit pointer methods.
                fields_only |= inherited && target.pointers != 0;
                target.project_properties(&owner);
                owner = target;
                continue;
            }

            for embedded in details.embedded {
                let TypeRefKind::Named(named) = &embedded.kind else {
                    continue;
                };
                let mut member_path = embedded_path.clone();
                member_path.push(named.name.clone());
                let Some(mut target) = resolve(embedded) else {
                    continue;
                };
                target.type_expression = owner.type_expression;
                target.addressable = !owner.type_expression && pointer_allowed;
                next.push(MemberPath {
                    owner: target,
                    ancestors: ancestors.clone(),
                    embedded_path: member_path,
                    indirect: indirect || owner.pointers == 1,
                    fields_only,
                });
            }
            break;
        }
        Ok(members.into_values().collect())
    }

    fn interface_members(
        &mut self,
        ty: TypeIdentity<'w>,
        ancestors: &mut Vec<TypeKey>,
        embedded_path: &[String],
        members: &mut BTreeMap<MemberKey<'w>, (VisibleMember<'w>, bool)>,
    ) -> Result<()> {
        if ancestors.len() >= MAX_TYPE_DEPTH {
            return Ok(());
        }
        let Some((owner, file, details, substitutions)) = self.constraint_interface(ty)? else {
            return Ok(());
        };
        let Some(key) = owner.key().filter(|key| !ancestors.contains(key)) else {
            return Ok(());
        };
        ancestors.push(key);
        self.add_direct_members(&owner, embedded_path, false, false, members)?;
        let imports = self.indexes.file_index(&file)?.imports.clone();
        // Interface embedding contributes to one method set. The embedding
        // depth for selectors counts struct fields, not interface elements.
        for embedded in details.embedded {
            let Some(embedded) = materialize_type(
                self.workspace,
                owner.package,
                &file,
                &imports,
                embedded,
                &substitutions,
            ) else {
                continue;
            };
            self.interface_members(embedded, ancestors, embedded_path, members)?;
        }
        ancestors.pop();
        Ok(())
    }

    fn add_direct_members(
        &mut self,
        owner: &TypeIdentity<'w>,
        embedded_path: &[String],
        fields_only: bool,
        pointer_allowed: bool,
        members: &mut BTreeMap<MemberKey<'w>, (VisibleMember<'w>, bool)>,
    ) -> Result<()> {
        let mut add = |file: &Path, name: &str, symbol: &Symbol| {
            if name.is_empty()
                || name == "_"
                || (fields_only && matches!(symbol.kind, SymbolKind::Function(_)))
            {
                return;
            }
            let member = VisibleMember {
                name: name.to_owned(),
                resolved: ResolvedSymbol {
                    package: owner.package,
                    file: file.to_owned(),
                    symbol: symbol.clone(),
                    receiver: Some(owner.clone()),
                    method_receiver: None,
                },
                embedded_path: embedded_path.to_vec(),
            };
            members
                .entry(member.key())
                .or_insert((member, pointer_allowed));
        };
        match &owner.kind {
            TypeKind::Named { name, .. } => {
                for file in self.indexes.files(owner.package).iter() {
                    for (name, symbol) in self.indexes.file_index(file)?.symbols.members_of(name) {
                        add(file, name, symbol);
                    }
                }
            }
            TypeKind::Inline { file, shape, .. } => {
                for (name, symbol) in &shape.members {
                    add(file, name, symbol);
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn direct_member(
        &mut self,
        ty: &TypeIdentity<'w>,
        name: &str,
        inherited: bool,
    ) -> Result<Option<ResolvedSymbol<'w>>> {
        match &ty.kind {
            TypeKind::Named { name: owner, .. } => {
                let member = self.find_symbol(ty.package, name, Some(owner))?;
                if inherited
                    && member
                        .as_ref()
                        .is_some_and(|member| matches!(member.symbol.kind, SymbolKind::Function(_)))
                    && !self
                        .type_info(ty.package, owner)?
                        .is_some_and(|(_, details)| details.interface)
                {
                    return Ok(None);
                }
                Ok(member)
            }
            TypeKind::Inline { file, shape, .. } => {
                Ok(shape.members.get(name).map(|symbol| ResolvedSymbol {
                    package: ty.package,
                    file: file.clone(),
                    symbol: symbol.clone(),
                    receiver: None,
                    method_receiver: None,
                }))
            }
            TypeKind::Container(..) | TypeKind::Function { .. } | TypeKind::Parameter(_) => {
                Ok(None)
            }
        }
    }

    pub(super) fn type_context(
        &mut self,
        ty: &TypeIdentity<'w>,
    ) -> Result<Option<(PathBuf, TypeDetails, Substitutions<'w>)>> {
        match &ty.kind {
            TypeKind::Named { name, .. } => {
                Ok(self.type_info(ty.package, name)?.map(|(file, details)| {
                    let substitutions = type_substitutions(ty, &details.parameters, &[]);
                    (file, details, substitutions)
                }))
            }
            TypeKind::Inline {
                file,
                shape,
                substitutions,
            } => Ok(Some((
                file.clone(),
                TypeDetails {
                    interface: shape.interface,
                    structure: !shape.interface,
                    method_names: if shape.interface {
                        shape.members.keys().cloned().collect()
                    } else {
                        Vec::new()
                    },
                    embedded: shape.embedded.clone(),
                    ..TypeDetails::default()
                },
                substitutions.clone(),
            ))),
            TypeKind::Container(..) | TypeKind::Function { .. } | TypeKind::Parameter(_) => {
                Ok(None)
            }
        }
    }

    pub(super) fn type_info(
        &mut self,
        package: &Package,
        name: &str,
    ) -> Result<Option<(PathBuf, TypeDetails)>> {
        self.indexes
            .type_details(package, name, self.workspace.is_project_package(package))
    }

    pub(super) fn container(
        &mut self,
        ty: TypeIdentity<'w>,
    ) -> Result<Option<(Container<TypeIdentity<'w>>, u8)>> {
        if ty.type_expression {
            return Ok(None);
        }
        Ok(match self.underlying_type(ty)? {
            Some(TypeIdentity {
                kind: TypeKind::Container(container, _),
                pointers,
                ..
            }) => Some((*container, pointers)),
            _ => None,
        })
    }

    pub(super) fn underlying_type(
        &mut self,
        mut ty: TypeIdentity<'w>,
    ) -> Result<Option<TypeIdentity<'w>>> {
        for _ in 0..MAX_TYPE_DEPTH {
            if !matches!(ty.kind, TypeKind::Named { .. }) {
                return Ok(Some(ty));
            }
            let Some((file, details, substitutions)) = self.type_context(&ty)? else {
                return Ok(Some(ty));
            };
            let Some(reference) = details.alias.or(details.underlying) else {
                return Ok(Some(ty));
            };
            let imports = &self.indexes.file_index(&file)?.imports;
            let Some(mut underlying) = materialize_type(
                self.workspace,
                ty.package,
                &file,
                imports,
                reference,
                &substitutions,
            ) else {
                return Ok(None);
            };
            underlying.project_properties(&ty);
            ty = underlying;
        }
        Ok(None)
    }

    pub(super) fn find_symbol(
        &mut self,
        package: &'w Package,
        name: &str,
        receiver: Option<&str>,
    ) -> Result<Option<ResolvedSymbol<'w>>> {
        if self.diagnostic_symbols.is_some() {
            let key = (
                package.id.as_str(),
                name.to_owned(),
                receiver.map(str::to_owned),
            );
            if let Some(cached) = self.diagnostic_symbols.as_ref().unwrap().get(&key) {
                return Ok(cached.clone());
            }
            let result = self.find_symbol_in_files(package, name, receiver)?;
            self.diagnostic_symbols
                .as_mut()
                .unwrap()
                .insert(key, result.clone());
            return Ok(result);
        }
        self.find_symbol_in_files(package, name, receiver)
    }

    fn find_symbol_in_files(
        &mut self,
        package: &'w Package,
        name: &str,
        receiver: Option<&str>,
    ) -> Result<Option<ResolvedSymbol<'w>>> {
        for file in self.indexes.files(package).iter() {
            let index =
                if receiver.is_none() && self.indexes.completion.is_some() && !package.load_error {
                    self.indexes.filtered_index(
                        file,
                        self.workspace.is_project_package(package),
                        package.is_external(),
                        |names| names.may_declare(name),
                    )?
                } else {
                    Some(self.indexes.file_index(file)?)
                };
            let Some(index) = index.map(|index| &index.symbols) else {
                continue;
            };
            let symbol = match receiver {
                Some(ty) => index.member(ty, name),
                None => index.declaration(name),
            };
            if let Some(symbol) = symbol {
                return Ok(Some(ResolvedSymbol {
                    package,
                    file: file.clone(),
                    symbol: symbol.clone(),
                    receiver: None,
                    method_receiver: None,
                }));
            }
        }
        Ok(None)
    }

    pub(super) fn infer_type(
        &mut self,
        from: &'w Package,
        source: &Source,
        expression: Node<'_>,
        depth: usize,
        result_index: usize,
    ) -> Result<Option<TypeIdentity<'w>>> {
        if depth >= MAX_TYPE_DEPTH {
            return Ok(None);
        }
        if expression.kind() == "call_expression" {
            return self.call_result(from, source, expression, depth + 1, result_index);
        }
        if expression.kind() == "type_conversion_expression" {
            return self.conversion_result(from, source, expression, depth + 1, result_index);
        }
        if expression.kind() == "range_clause" {
            let Some(operand) = expression.child_by_field_name("right") else {
                return Ok(None);
            };
            let Some(ty) = self.infer_type(from, source, operand, depth + 1, 0)? else {
                return Ok(None);
            };
            return Ok(match (self.container(ty)?, result_index) {
                (Some((Container::Array { element, .. }, 0 | 1)), 1)
                | (Some((Container::Slice(element), 0)), 1)
                | (Some((Container::Map { value: element, .. }, 0)), 1)
                | (
                    Some((
                        Container::Channel {
                            element,
                            receive: true,
                            ..
                        },
                        0,
                    )),
                    0,
                ) => Some(element),
                (Some((Container::Map { key, .. }, 0)), 0) => Some(key),
                _ => None,
            });
        }
        if result_index != 0 {
            return Ok(None);
        }
        match expression.kind() {
            "identifier" | "type_identifier" | "package_identifier" | "true" | "false" => {
                let name = node_text(expression, &source.text);
                if let Some(binding) = lexical_binding(expression, name, &source.text) {
                    let addressable = binding
                        .name
                        .parent()
                        .is_none_or(|node| node.kind() != "const_spec");
                    let result = if let Some(ty) = binding.explicit_type {
                        self.source_type(from, source, ty)
                    } else if let Some(initializer) = binding.initializer {
                        self.infer_type(from, source, initializer, depth + 1, binding.result_index)?
                    } else {
                        None
                    };
                    return Ok(result.map(|mut ty| {
                        ty.type_expression = binding.name.parent().is_some_and(is_type_declaration);
                        ty.addressable = addressable && !ty.type_expression;
                        ty
                    }));
                }
                let Some(symbol) = self.find_symbol(from, name, None)? else {
                    return Ok(None);
                };
                self.symbol_type(source, &symbol, name, depth + 1)
            }
            "func_literal" => Ok(self.source_type(from, source, expression)),
            "composite_literal" | "type_assertion_expression" => Ok(expression
                .child_by_field_name("type")
                .and_then(|ty| self.source_type(from, source, ty))),
            "index_expression" | "slice_expression" => {
                let Some(operand) = expression.child_by_field_name("operand") else {
                    return Ok(None);
                };
                let Some(ty) = self.infer_type(from, source, operand, depth + 1, 0)? else {
                    return Ok(None);
                };
                if expression.kind() == "index_expression" {
                    if let Some(argument) = expression.child_by_field_name("index") {
                        if ty.type_expression {
                            return self.instantiate_type_expression(from, source, ty, [argument]);
                        }
                        if matches!(&ty.kind, TypeKind::Function { signature, .. } if !signature.type_parameters.is_empty())
                        {
                            return self.specialize_function(from, source, ty, [argument]);
                        }
                    }
                    return self.indexed_element(ty);
                }
                let Some((container, pointers)) = self.container(ty.clone())? else {
                    return Ok(None);
                };
                Ok(match (container, pointers) {
                    (Container::Slice(_), 0) => Some(TypeIdentity {
                        addressable: false,
                        ..ty
                    }),
                    (Container::Array { element, .. }, 0 | 1)
                        if ty.addressable || pointers == 1 =>
                    {
                        Some(TypeIdentity::new(
                            ty.package,
                            TypeKind::Container(Box::new(Container::Slice(element)), None),
                            0,
                        ))
                    }
                    _ => None,
                })
            }
            "type_instantiation_expression" => {
                let Some(function) = expression.child_by_field_name("type") else {
                    return Ok(None);
                };
                let Some(ty) = self.infer_type(from, source, function, depth + 1, 0)? else {
                    return Ok(None);
                };
                let mut cursor = expression.walk();
                let arguments = expression
                    .named_children(&mut cursor)
                    .filter(|node| !node.is_extra() && *node != function);
                if ty.type_expression {
                    self.instantiate_type_expression(from, source, ty, arguments)
                } else {
                    self.specialize_function(from, source, ty, arguments)
                }
            }
            "parenthesized_expression" => match expression.named_child(0) {
                Some(inner) => self.infer_type(from, source, inner, depth + 1, 0),
                None => Ok(None),
            },
            "unary_expression" => {
                let (Some(operator), Some(operand)) = (
                    expression.child_by_field_name("operator"),
                    expression.child_by_field_name("operand"),
                ) else {
                    return Ok(None);
                };
                if !matches!(operator.kind(), "&" | "*" | "<-") {
                    return Ok(None);
                }
                let Some(mut ty) = self.infer_type(from, source, operand, depth + 1, 0)? else {
                    return Ok(None);
                };
                if operator.kind() == "<-" {
                    return Ok(match self.container(ty)? {
                        Some((
                            Container::Channel {
                                element,
                                receive: true,
                                ..
                            },
                            0,
                        )) => Some(element),
                        _ => None,
                    });
                }
                if operator.kind() == "&" || (operator.kind() == "*" && ty.type_expression) {
                    ty.pointers = ty.pointers.saturating_add(1);
                    ty.addressable = false;
                } else if operator.kind() == "*" && ty.pointers > 0 {
                    ty.pointers -= 1;
                    ty.addressable = !ty.type_expression;
                } else {
                    return Ok(None);
                }
                Ok(Some(ty))
            }
            "selector_expression" | "qualified_type" => {
                let fields = if expression.kind() == "qualified_type" {
                    ("package", "name")
                } else {
                    ("operand", "field")
                };
                let Some(receiver) = expression.child_by_field_name(fields.0) else {
                    return Ok(None);
                };
                let Some(name) = expression.child_by_field_name(fields.1) else {
                    return Ok(None);
                };
                let name = node_text(name, &source.text);
                let Some(symbol) = self.selector(from, source, receiver, name, depth + 1)? else {
                    return Ok(None);
                };
                self.symbol_type(source, &symbol, name, depth + 1)
            }
            _ => Ok(None),
        }
    }

    fn call_result(
        &mut self,
        from: &'w Package,
        source: &Source,
        call: Node<'_>,
        depth: usize,
        result_index: usize,
    ) -> Result<Option<TypeIdentity<'w>>> {
        let Some(function) = call.child_by_field_name("function") else {
            return Ok(None);
        };
        let spelling = node_text(function, &source.text);
        if result_index == 0
            && function.kind() == "identifier"
            && matches!(spelling, "new" | "make")
            && lexical_binding(function, spelling, &source.text).is_none()
            && self.find_symbol(from, spelling, None)?.is_none()
        {
            let Some(argument) = call
                .child_by_field_name("arguments")
                .and_then(|args| args.named_child(0))
                .and_then(|node| self.source_type(from, source, node))
            else {
                return Ok(None);
            };
            if spelling == "new" {
                return Ok(argument
                    .pointers
                    .checked_add(1)
                    .map(|pointers| TypeIdentity {
                        pointers,
                        ..argument
                    }));
            }
            return Ok(matches!(
                self.container(argument.clone())?,
                Some((
                    Container::Slice(_) | Container::Map { .. } | Container::Channel { .. },
                    0
                ))
            )
            .then_some(argument));
        }
        let Some(mut ty) = self.infer_type(from, source, function, depth, 0)? else {
            return Ok(None);
        };
        if let Some(explicit) = call.child_by_field_name("type_arguments") {
            let Some(specialized) =
                self.specialize_function(from, source, ty, explicit_type_arguments(explicit))?
            else {
                return Ok(None);
            };
            ty = specialized;
        }
        let arguments = CallArguments::List(call.child_by_field_name("arguments"));
        self.result_of_call(from, source, ty, arguments, depth, result_index)
    }

    fn conversion_result(
        &mut self,
        from: &'w Package,
        source: &Source,
        expression: Node<'_>,
        depth: usize,
        result_index: usize,
    ) -> Result<Option<TypeIdentity<'w>>> {
        let Some(syntax) = expression.child_by_field_name("type") else {
            return Ok(None);
        };
        let mut ty = syntax;
        while ty.kind() == "parenthesized_type" {
            let Some(inner) = ty.named_child(0) else {
                return Ok(None);
            };
            ty = inner;
        }
        // The grammar can parse F[T](x), Box[T](x), and callbacks[i](x)
        // using the same node. The binding determines which operation it is.
        if ty.kind() == "generic_type"
            && let (Some(base), Some(explicit), Some(argument)) = (
                ty.child_by_field_name("type"),
                ty.child_by_field_name("type_arguments"),
                expression.child_by_field_name("operand"),
            )
        {
            let Some(callee) = self.infer_type(from, source, base, depth, 0)? else {
                return Ok(None);
            };
            if !callee.type_expression {
                let callee = if matches!(&callee.kind, TypeKind::Function { signature, .. } if !signature.type_parameters.is_empty())
                {
                    self.specialize_function(
                        from,
                        source,
                        callee,
                        explicit_type_arguments(explicit),
                    )?
                } else if explicit_type_arguments(explicit).len() == 1 {
                    self.indexed_element(callee)?
                } else {
                    None
                };
                let Some(callee) = callee else {
                    return Ok(None);
                };
                return self.result_of_call(
                    from,
                    source,
                    callee,
                    CallArguments::Single(argument),
                    depth,
                    result_index,
                );
            }
        }
        Ok((result_index == 0)
            .then(|| self.source_type(from, source, syntax))
            .flatten())
    }

    pub(super) fn indexed_element(
        &mut self,
        ty: TypeIdentity<'w>,
    ) -> Result<Option<TypeIdentity<'w>>> {
        let addressable = ty.addressable;
        let Some((container, pointers)) = self.container(ty)? else {
            return Ok(None);
        };
        let (mut element, addressable) = match (container, pointers) {
            (Container::Slice(element), 0) => (element, true),
            (Container::Array { element, .. }, 0 | 1) => (element, addressable || pointers == 1),
            (Container::Map { value, .. }, 0) => (value, false),
            _ => return Ok(None),
        };
        element.addressable = addressable;
        Ok(Some(element))
    }

    pub(super) fn specialize_function<'a>(
        &mut self,
        caller: &'w Package,
        source: &Source,
        mut ty: TypeIdentity<'w>,
        arguments: impl IntoIterator<Item = Node<'a>>,
    ) -> Result<Option<TypeIdentity<'w>>> {
        let TypeKind::Function {
            signature,
            substitutions,
            ..
        } = &mut ty.kind
        else {
            return Ok(None);
        };
        let mut parameters = signature.type_parameters.iter();
        for argument in arguments {
            let Some(parameter) = parameters.next() else {
                return Ok(None);
            };
            let Some(actual) = self.type_argument(caller, source, argument)? else {
                return Ok(None);
            };
            substitutions.push((parameter.name.clone(), Some(actual)));
        }
        Ok(Some(ty))
    }

    fn instantiate_type_expression<'a>(
        &mut self,
        caller: &'w Package,
        source: &Source,
        mut ty: TypeIdentity<'w>,
        arguments: impl IntoIterator<Item = Node<'a>>,
    ) -> Result<Option<TypeIdentity<'w>>> {
        let TypeKind::Named {
            name,
            arguments: actual,
        } = &mut ty.kind
        else {
            return Ok(None);
        };
        let Some((_, details)) = self.type_info(ty.package, name)? else {
            return Ok(None);
        };
        for argument in arguments {
            if let Some(argument) = self.type_argument(caller, source, argument)? {
                actual.push(Some(argument));
            } else {
                return Ok(None);
            }
        }
        Ok((!actual.is_empty() && actual.len() == details.parameters.len()).then_some(ty))
    }

    fn type_argument(
        &mut self,
        caller: &'w Package,
        source: &Source,
        argument: Node<'_>,
    ) -> Result<Option<TypeIdentity<'w>>> {
        if argument.has_error() {
            return Ok(None);
        }
        let Some(ty) = self.source_type(caller, source, argument) else {
            return Ok(None);
        };
        if let TypeKind::Named { name, .. } = &ty.kind
            && !is_predeclared_type(name)
            && self.type_info(ty.package, name)?.is_none()
        {
            return Ok(None);
        }
        Ok(Some(ty))
    }

    fn result_of_call(
        &mut self,
        caller: &'w Package,
        source: &Source,
        mut ty: TypeIdentity<'w>,
        arguments: CallArguments<'_>,
        depth: usize,
        result_index: usize,
    ) -> Result<Option<TypeIdentity<'w>>> {
        if ty.type_expression {
            ty.type_expression = false;
            ty.addressable = false;
            return Ok((result_index == 0).then_some(ty));
        }
        let Some(TypeIdentity {
            package,
            pointers: 0,
            kind:
                TypeKind::Function {
                    file,
                    signature,
                    mut substitutions,
                    ..
                },
            ..
        }) = self.underlying_type(ty)?
        else {
            return Ok(None);
        };
        let Some(Some(reference)) = signature.results.get(result_index) else {
            return Ok(None);
        };
        if !signature.type_parameters.is_empty() {
            let arguments = arguments.expressions();
            let call = GenericCall {
                caller,
                source,
                arguments: &arguments,
                depth,
                allow_partial: false,
            };
            let origin = TypeOrigin {
                package,
                file: &file,
            };
            let Some(inferred) =
                self.infer_type_arguments(call, origin, &signature, &substitutions)?
            else {
                return Ok(None);
            };
            substitutions = inferred;
        }
        let imports = &self.indexes.file_index(&file)?.imports;
        Ok(materialize_type(
            self.workspace,
            package,
            &file,
            imports,
            reference.clone(),
            &substitutions,
        ))
    }

    pub(super) fn infer_type_arguments(
        &mut self,
        call: GenericCall<'w, '_, '_>,
        origin: TypeOrigin<'w, '_>,
        signature: &FunctionSignature,
        explicit: &Substitutions<'w>,
    ) -> Result<Option<Substitutions<'w>>> {
        let count = signature.parameters.len();
        let variadic = signature.variadic && count > 0;
        if !call.allow_partial
            && if variadic {
                call.arguments.len() < count - 1
            } else {
                call.arguments.len() != count
            }
        {
            return Ok(None);
        }
        let parameters = &signature.type_parameters;
        let placeholders: Substitutions<'w> = parameters
            .iter()
            .enumerate()
            .map(|(i, param)| {
                (
                    param.name.clone(),
                    Some(TypeIdentity::new(origin.package, TypeKind::Parameter(i), 0)),
                )
            })
            .collect();
        let mut bindings: Vec<_> = parameters
            .iter()
            .map(|param| {
                explicit
                    .iter()
                    .find(|(name, _)| name == &param.name)
                    .and_then(|(_, ty)| ty.clone())
            })
            .collect();

        let imports = self.indexes.file_index(origin.file)?.imports.clone();
        let workspace = self.workspace;
        let pattern = |reference| {
            materialize_type(
                workspace,
                origin.package,
                origin.file,
                &imports,
                reference,
                &placeholders,
            )
        };
        let mut untyped = Vec::new();
        for (index, argument) in call.arguments.iter().copied().enumerate() {
            let Some(parameter) = signature
                .parameters
                .get(index)
                .or_else(|| variadic.then(|| &signature.parameters[count - 1]))
            else {
                break;
            };
            let Some(expected) = parameter.clone().and_then(pattern) else {
                return Ok(None);
            };
            let (argument, expected) = if argument.kind() == "variadic_argument"
                && let Some(operand) = argument
                    .named_children(&mut argument.walk())
                    .find(|node| !node.is_extra())
            {
                (
                    operand,
                    TypeIdentity::new(
                        origin.package,
                        TypeKind::Container(Box::new(Container::Slice(expected)), None),
                        0,
                    ),
                )
            } else {
                (argument, expected)
            };
            if argument.has_error() {
                if call.allow_partial {
                    continue;
                }
                return Ok(None);
            }
            if let Some(actual) =
                self.infer_type(call.caller, call.source, argument, call.depth + 1, 0)?
            {
                if actual.type_expression
                    || !self.unify_types(expected, actual, &mut bindings, true)?
                {
                    return Ok(None);
                }
            } else if let Some(default) = untyped_default(argument) {
                untyped.push((expected, default));
            } else if !call.allow_partial {
                return Ok(None);
            }
        }

        let approximations: Vec<_> = parameters
            .iter()
            .map(|param| param.constraint.approximation.clone().and_then(pattern))
            .collect();
        if !self.apply_approximations(&approximations, &mut bindings)? {
            return Ok(None);
        }
        for (expected, default) in untyped {
            if let TypeKind::Parameter(index) = expected.kind
                && let Some(bound) = bindings.get(index).and_then(Clone::clone)
                && expected.pointers == 0
                && bound.pointers == 0
                && let Some(underlying) = self.underlying_type(bound)?
                && let TypeKind::Named { name, .. } = underlying.kind
                && name == default
                && self.type_info(underlying.package, &name)?.is_none()
            {
                continue;
            }
            let actual = TypeIdentity::named(call.caller, default);
            if !self.unify_types(expected, actual, &mut bindings, true)? {
                return Ok(None);
            }
        }
        if !self.apply_approximations(&approximations, &mut bindings)?
            || (!call.allow_partial && bindings.iter().any(Option::is_none))
        {
            return Ok(None);
        }
        let substitutions = parameters
            .iter()
            .zip(&bindings)
            .filter(|(_, ty)| ty.is_some())
            .map(|(param, ty)| (param.name.clone(), ty.clone()))
            .collect::<Vec<_>>();
        for (param, actual) in parameters.iter().zip(bindings) {
            if let (Some(reference), Some(mut actual)) = (param.constraint.methods.clone(), actual)
                && let Some(required) = materialize_type(
                    workspace,
                    origin.package,
                    origin.file,
                    &imports,
                    reference,
                    &substitutions,
                )
            {
                let mut names = Vec::new();
                self.collect_constraint_methods(required, &mut names, &[])?;
                // A pointer method on an addressable variable alone does not
                // satisfy a constraint for the variable's non-pointer type.
                actual.addressable = false;
                actual.type_expression = true;
                for (package, name) in names {
                    if self.member(package, &actual, &name, true)?.is_none() {
                        return Ok(None);
                    }
                }
            }
        }
        Ok(Some(substitutions))
    }

    fn apply_approximations(
        &mut self,
        approximations: &[Option<TypeIdentity<'w>>],
        bindings: &mut [Option<TypeIdentity<'w>>],
    ) -> Result<bool> {
        // A typed S with S ~[]E (or ~map[K]V) supplies the remaining
        // arguments through its underlying container shape.
        for _ in 0..bindings.len() {
            let previous_len = bindings.iter().filter(|slot| slot.is_some()).count();
            for (i, expected) in approximations.iter().enumerate() {
                if let (Some(expected), Some(actual)) = (expected.clone(), bindings[i].clone()) {
                    let Some(underlying) = self.underlying_type(actual)? else {
                        return Ok(false);
                    };
                    if !self.unify_types(expected, underlying, bindings, false)? {
                        return Ok(false);
                    }
                }
            }
            if previous_len == bindings.iter().filter(|slot| slot.is_some()).count() {
                break;
            }
        }
        Ok(true)
    }

    fn constraint_interface(
        &mut self,
        mut ty: TypeIdentity<'w>,
    ) -> Result<Option<(TypeIdentity<'w>, PathBuf, TypeDetails, Substitutions<'w>)>> {
        for _ in 0..MAX_TYPE_DEPTH {
            let Some((file, details, substitutions)) = self.type_context(&ty)? else {
                return Ok(None);
            };
            if details.interface && ty.pointers == 0 {
                return Ok(Some((ty, file, details, substitutions)));
            }
            let Some(reference) = details.alias.or(details.underlying) else {
                return Ok(None);
            };
            let imports = &self.indexes.file_index(&file)?.imports;
            let Some(mut next) = materialize_type(
                self.workspace,
                ty.package,
                &file,
                imports,
                reference,
                &substitutions,
            ) else {
                return Ok(None);
            };
            next.project_properties(&ty);
            ty = next;
        }
        Ok(None)
    }

    fn collect_constraint_methods(
        &mut self,
        constraint: TypeIdentity<'w>,
        names: &mut Vec<(&'w Package, String)>,
        ancestors: &[TypeKey],
    ) -> Result<()> {
        if ancestors.len() >= MAX_TYPE_DEPTH {
            return Ok(());
        }
        let Some((ty, file, details, substitutions)) = self.constraint_interface(constraint)?
        else {
            return Ok(());
        };
        let Some(key) = ty.key().filter(|key| !ancestors.contains(key)) else {
            return Ok(());
        };
        names.extend(
            details
                .method_names
                .into_iter()
                .map(|name| (ty.package, name)),
        );
        let mut ancestors = ancestors.to_vec();
        ancestors.push(key);
        let imports = self.indexes.file_index(&file)?.imports.clone();
        for embedded in details.embedded {
            if let Some(interface) = materialize_type(
                self.workspace,
                ty.package,
                &file,
                &imports,
                embedded,
                &substitutions,
            ) {
                self.collect_constraint_methods(interface, names, &ancestors)?;
            }
        }
        Ok(())
    }

    fn unify_types(
        &mut self,
        expected: TypeIdentity<'w>,
        mut actual: TypeIdentity<'w>,
        bindings: &mut [Option<TypeIdentity<'w>>],
        loose: bool,
    ) -> Result<bool> {
        if let TypeKind::Parameter(index) = expected.kind {
            let Some(pointers) = actual.pointers.checked_sub(expected.pointers) else {
                return Ok(false);
            };
            actual.pointers = pointers;
            actual.addressable = false;
            let Some(bound) = bindings.get_mut(index) else {
                return Ok(false);
            };
            if let Some(previous) = bound.clone() {
                return self.unify_types(previous, actual, bindings, false);
            }
            *bound = Some(actual);
            return Ok(true);
        }
        let Some(expected) = self.resolve_alias(expected)? else {
            return Ok(false);
        };
        let Some(actual) = self.resolve_alias(actual)? else {
            return Ok(false);
        };
        let actual = if loose
            && matches!(
                &expected.kind,
                TypeKind::Container(..) | TypeKind::Function { .. }
            ) {
            self.underlying_type(actual)?
        } else {
            Some(actual)
        };
        let Some(actual) = actual.filter(|actual| actual.pointers == expected.pointers) else {
            return Ok(false);
        };
        match (expected.kind, actual.kind) {
            (
                TypeKind::Named {
                    name: left,
                    arguments: left_args,
                },
                TypeKind::Named {
                    name: right,
                    arguments: right_args,
                },
            ) => {
                if left != right || left_args.len() != right_args.len() {
                    return Ok(false);
                }
                if !std::ptr::eq(expected.package, actual.package)
                    && (!is_predeclared_type(&left)
                        || self.type_info(expected.package, &left)?.is_some()
                        || self.type_info(actual.package, &right)?.is_some())
                {
                    return Ok(false);
                }
                for (left, right) in left_args.into_iter().zip(right_args) {
                    if let (Some(left), Some(right)) = (left, right)
                        && self.unify_types(left, right, bindings, false)?
                    {
                        continue;
                    }
                    return Ok(false);
                }
                Ok(true)
            }
            (TypeKind::Container(left, _), TypeKind::Container(right, _)) => {
                match (*left, *right) {
                    (Container::Slice(left), Container::Slice(right)) => {
                        self.unify_types(left, right, bindings, false)
                    }
                    (
                        Container::Array {
                            element: left,
                            length: Some(n),
                        },
                        Container::Array {
                            element: right,
                            length: Some(m),
                        },
                    ) if n == m => self.unify_types(left, right, bindings, false),
                    (
                        Container::Map { key: lk, value: lv },
                        Container::Map { key: rk, value: rv },
                    ) => Ok(self.unify_types(lk, rk, bindings, false)?
                        && self.unify_types(lv, rv, bindings, false)?),
                    (
                        Container::Channel {
                            element: left,
                            receive: a,
                            ..
                        },
                        Container::Channel {
                            element: right,
                            receive: b,
                            ..
                        },
                    ) if a == b => self.unify_types(left, right, bindings, false),
                    _ => Ok(false),
                }
            }
            (left @ TypeKind::Function { .. }, right @ TypeKind::Function { .. }) => self
                .unify_function_types(
                    TypeIdentity {
                        kind: left,
                        ..expected
                    },
                    TypeIdentity {
                        kind: right,
                        ..actual
                    },
                    bindings,
                ),
            (
                TypeKind::Inline {
                    file: a,
                    shape: left,
                    ..
                },
                TypeKind::Inline {
                    file: b,
                    shape: right,
                    ..
                },
            ) => Ok(a == b && left.origin == right.origin),
            _ => Ok(false),
        }
    }

    pub(super) fn resolve_alias(
        &mut self,
        mut ty: TypeIdentity<'w>,
    ) -> Result<Option<TypeIdentity<'w>>> {
        for _ in 0..MAX_TYPE_DEPTH {
            let TypeKind::Named { name, .. } = &ty.kind else {
                return Ok(Some(ty));
            };
            let Some((file, details)) = self.type_info(ty.package, name)? else {
                return Ok(Some(ty));
            };
            let Some(reference) = details.alias else {
                return Ok(Some(ty));
            };
            let substitutions = type_substitutions(&ty, &details.parameters, &[]);
            let imports = &self.indexes.file_index(&file)?.imports;
            let Some(mut underlying) = materialize_type(
                self.workspace,
                ty.package,
                &file,
                imports,
                reference,
                &substitutions,
            ) else {
                return Ok(None);
            };
            underlying.project_properties(&ty);
            ty = underlying;
        }
        Ok(None)
    }

    pub(super) fn source_type(
        &mut self,
        from: &'w Package,
        source: &Source,
        ty: Node<'_>,
    ) -> Option<TypeIdentity<'w>> {
        self.source_type_with_scope(from, source, ty, true)
    }

    pub(super) fn source_type_unscoped(
        &mut self,
        from: &'w Package,
        source: &Source,
        ty: Node<'_>,
    ) -> Option<TypeIdentity<'w>> {
        self.source_type_with_scope(from, source, ty, false)
    }

    fn source_type_with_scope(
        &mut self,
        from: &'w Package,
        source: &Source,
        ty: Node<'_>,
        lexical: bool,
    ) -> Option<TypeIdentity<'w>> {
        let parse = |node| symbols::type_ref(node, &source.text, &|node| source.range(node));
        let mut reference = parse(ty)?;
        let mut constrained = false;
        if lexical
            && let TypeRefKind::Named(named) = &reference.kind
            && named.qualifier.is_none()
            && let Some(binding) = lexical_binding(ty, &named.name, &source.text)
        {
            let owner = binding.name.parent()?;
            if owner.kind() == "type_parameter_declaration" && reference.pointers == 0 {
                constrained = true;
                reference =
                    symbols::type_constraint(binding.explicit_type?, &source.text, &|node| {
                        source.range(node)
                    })
                    .methods?;
            } else if owner.kind() == "type_elem" && reference.pointers == 0 {
                return self.receiver_constraint(from, source, binding.name);
            } else {
                if !is_type_declaration(owner) {
                    return None;
                }
                let mut underlying = parse(binding.explicit_type?)?;
                if !matches!(
                    underlying.kind,
                    TypeRefKind::Function(_) | TypeRefKind::Inline(_)
                ) {
                    return None;
                }
                underlying.pointers = underlying.pointers.checked_add(reference.pointers)?;
                reference = underlying;
            }
        }
        let ty = materialize_type(
            self.workspace,
            from,
            &source.path,
            &source.imports,
            reference,
            &[],
        )?;
        if !constrained {
            return Some(ty);
        }
        Some(self.constraint_interface(ty).ok()??.0)
    }

    fn receiver_constraint(
        &mut self,
        from: &'w Package,
        source: &Source,
        parameter: Node<'_>,
    ) -> Option<TypeIdentity<'w>> {
        let mut method = parameter.parent()?;
        while method.kind() != "method_declaration" {
            method = method.parent()?;
        }
        let (base, arguments) = receiver_type_parameters(method)?;
        let mut cursor = arguments.walk();
        let index = arguments
            .named_children(&mut cursor)
            .filter(|node| node.kind() == "type_elem")
            .position(|node| node.named_child(0) == Some(parameter))?;
        let name = node_text(base, &source.text);
        let (file, details) = self.type_info(from, name).ok()??;
        let reference = details.parameters.get(index)?.constraint.methods.clone()?;
        let imports = &self.indexes.file_index(&file).ok()?.imports;
        let ty = materialize_type(self.workspace, from, &file, imports, reference, &[])?;
        Some(self.constraint_interface(ty).ok()??.0)
    }

    pub(super) fn symbol_type(
        &mut self,
        source: &Source,
        resolved: &ResolvedSymbol<'w>,
        name: &str,
        depth: usize,
    ) -> Result<Option<TypeIdentity<'w>>> {
        match &resolved.symbol.kind {
            SymbolKind::Value(Some(reference)) => {
                let addressable = resolved.symbol.addressable
                    && resolved
                        .receiver
                        .as_ref()
                        .is_none_or(|ty| ty.addressable || ty.pointers > 0);
                self.type_from_symbol(resolved, reference.clone(), addressable)
            }
            SymbolKind::InferredValue {
                initializer,
                result_index,
            } => {
                let documents = self.indexes.documents;
                let disk_source;
                let cached;
                let source = if source.path == resolved.file {
                    source
                } else if let Some(document) = documents.get(&path_key(&resolved.file)) {
                    document
                } else if self.indexes.completion.is_some() {
                    cached = self
                        .indexes
                        .declaration_source(&resolved.file)?
                        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
                    cached.as_ref()
                } else {
                    let probe = self.indexes.navigation;
                    record(probe, |counts| {
                        counts.scanned_files += 1;
                        counts.ast_access(false);
                    });
                    disk_source =
                        read_navigation_source(self.indexes.parser, &resolved.file, probe)?;
                    &disk_source
                };
                let Some(expression) = source
                    .tree
                    .root_node()
                    .named_descendant_for_byte_range(initializer.start, initializer.end)
                    .filter(|node| node.byte_range() == *initializer)
                else {
                    return Ok(None);
                };
                let mut ty =
                    self.infer_type(resolved.package, source, expression, depth, *result_index)?;
                if let Some(ty) = &mut ty {
                    ty.addressable = true;
                    ty.type_expression = false;
                }
                Ok(ty)
            }
            SymbolKind::Function(signature) => self.type_from_symbol(
                resolved,
                TypeRef {
                    pointers: 0,
                    kind: TypeRefKind::Function(signature.clone()),
                },
                false,
            ),
            SymbolKind::Type => {
                let mut ty = TypeIdentity::named(resolved.package, name);
                ty.type_expression = true;
                Ok(Some(ty))
            }
            _ => Ok(None),
        }
    }

    pub(super) fn type_from_symbol(
        &mut self,
        resolved: &ResolvedSymbol<'w>,
        reference: TypeRef,
        addressable: bool,
    ) -> Result<Option<TypeIdentity<'w>>> {
        let substitutions = match resolved.receiver.as_ref().map(|receiver| &receiver.kind) {
            Some(TypeKind::Named { name, arguments }) if !arguments.is_empty() => {
                let receiver = resolved.receiver.as_ref().expect("named receiver");
                match self.type_info(receiver.package, name)? {
                    Some((_, details)) => type_substitutions(
                        receiver,
                        &details.parameters,
                        &resolved.symbol.receiver_parameters,
                    ),
                    None => Vec::new(),
                }
            }
            Some(TypeKind::Inline { substitutions, .. }) => substitutions.clone(),
            _ => Vec::new(),
        };
        let index = self.indexes.file_index(&resolved.file)?;
        Ok(materialize_type(
            self.workspace,
            resolved.package,
            &resolved.file,
            &index.imports,
            reference,
            &substitutions,
        )
        .map(|mut ty| {
            ty.addressable = addressable;
            if let TypeKind::Function {
                method_receiver, ..
            } = &mut ty.kind
            {
                *method_receiver = resolved.method_receiver.clone();
            }
            ty
        }))
    }
}
pub(super) struct TypeOrigin<'w, 'path> {
    pub(super) package: &'w Package,
    pub(super) file: &'path Path,
}

pub(super) fn explicit_type_arguments(list: Node<'_>) -> Vec<Node<'_>> {
    list.named_children(&mut list.walk())
        .filter(|element| element.kind() == "type_elem")
        .filter_map(|element| element.named_child(0))
        .collect()
}

pub(super) struct GenericCall<'w, 's, 'tree> {
    pub(super) caller: &'w Package,
    pub(super) source: &'s Source,
    pub(super) arguments: &'s [Node<'tree>],
    pub(super) depth: usize,
    pub(super) allow_partial: bool,
}

#[derive(Clone, Copy)]
pub(super) enum CallArguments<'tree> {
    List(Option<Node<'tree>>),
    Single(Node<'tree>),
}

impl<'tree> CallArguments<'tree> {
    pub(super) fn expressions(self) -> Vec<Node<'tree>> {
        match self {
            Self::List(Some(list)) => list
                .named_children(&mut list.walk())
                .filter(|node| !node.is_extra())
                .collect(),
            Self::List(None) => Vec::new(),
            Self::Single(node) => vec![node],
        }
    }
}

pub(super) fn untyped_default(mut argument: Node<'_>) -> Option<&'static str> {
    loop {
        argument = match argument.kind() {
            "parenthesized_expression" => argument
                .named_children(&mut argument.walk())
                .find(|node| !node.is_extra())?,
            "unary_expression"
                if argument
                    .child_by_field_name("operator")
                    .is_some_and(|operator| matches!(operator.kind(), "+" | "-" | "^")) =>
            {
                argument.child_by_field_name("operand")?
            }
            _ => break,
        };
    }
    match argument.kind() {
        "int_literal" => Some("int"),
        "rune_literal" => Some("rune"),
        "float_literal" => Some("float64"),
        "imaginary_literal" => Some("complex128"),
        "interpreted_string_literal" | "raw_string_literal" => Some("string"),
        "true" | "false" => Some("bool"),
        _ => None,
    }
}

pub(super) fn is_predeclared_type(name: &str) -> bool {
    matches!(
        name,
        "any"
            | "bool"
            | "byte"
            | "comparable"
            | "complex64"
            | "complex128"
            | "error"
            | "float32"
            | "float64"
            | "int"
            | "int8"
            | "int16"
            | "int32"
            | "int64"
            | "rune"
            | "string"
            | "uint"
            | "uint8"
            | "uint16"
            | "uint32"
            | "uint64"
            | "uintptr"
    )
}

fn type_substitutions<'w>(
    owner: &TypeIdentity<'w>,
    parameters: &[GenericParameter],
    receiver_parameters: &[Option<String>],
) -> Substitutions<'w> {
    let TypeKind::Named { arguments, .. } = &owner.kind else {
        return Vec::new();
    };
    arguments
        .iter()
        .enumerate()
        .filter_map(|(index, argument)| {
            let name = if receiver_parameters.is_empty() {
                &parameters.get(index)?.name
            } else {
                receiver_parameters.get(index)?.as_ref()?
            };
            Some((name.clone(), argument.clone()))
        })
        .collect()
}

pub(super) fn materialize_type<'w>(
    workspace: &'w Workspace,
    from: &'w Package,
    file: &Path,
    imports: &[Import],
    reference: TypeRef,
    substitutions: &[(String, Option<TypeIdentity<'w>>)],
) -> Option<TypeIdentity<'w>> {
    let resolve =
        |reference| materialize_type(workspace, from, file, imports, reference, substitutions);
    let (package, kind) = match reference.kind {
        TypeRefKind::Named(named) => {
            if named.qualifier.is_none()
                && let Some((_, argument)) =
                    substitutions.iter().find(|(name, _)| *name == named.name)
            {
                let mut ty = argument.clone()?;
                ty.pointers = ty.pointers.saturating_add(reference.pointers);
                return Some(ty);
            }
            let package = match named.qualifier {
                Some(qualifier) => imported_package(imports, workspace, from, &qualifier)?.1,
                None => from,
            };
            let arguments = named
                .arguments
                .into_iter()
                .map(|arg| arg.and_then(resolve))
                .collect();
            (
                package,
                TypeKind::Named {
                    name: named.name,
                    arguments,
                },
            )
        }
        TypeRefKind::Container(container, origin) => {
            let container = match *container {
                Container::Slice(element) => Container::Slice(resolve(element)?),
                Container::Array { element, length } => Container::Array {
                    element: resolve(element)?,
                    length,
                },
                Container::Map { key, value } => Container::Map {
                    key: resolve(key)?,
                    value: resolve(value)?,
                },
                Container::Channel {
                    element,
                    receive,
                    send,
                } => Container::Channel {
                    element: resolve(element)?,
                    receive,
                    send,
                },
            };
            (
                from,
                TypeKind::Container(Box::new(container), Some((file.to_owned(), origin))),
            )
        }
        TypeRefKind::Function(signature) => (
            from,
            TypeKind::Function {
                file: file.to_owned(),
                signature,
                substitutions: substitutions.to_vec(),
                method_receiver: None,
            },
        ),
        TypeRefKind::Inline(shape) => (
            from,
            TypeKind::Inline {
                file: file.to_owned(),
                shape,
                substitutions: substitutions.to_vec(),
            },
        ),
    };
    Some(TypeIdentity::new(package, kind, reference.pointers))
}

impl Source {
    pub(super) fn imported_package<'a>(
        &self,
        workspace: &'a Workspace,
        from: &Package,
        qualifier: &str,
    ) -> Option<(&Import, &'a Package)> {
        imported_package(&self.imports, workspace, from, qualifier)
    }
}

pub(super) fn imported_package<'a, 'w>(
    imports: &'a [Import],
    workspace: &'w Workspace,
    from: &Package,
    qualifier: &str,
) -> Option<(&'a Import, &'w Package)> {
    imports.iter().find_map(|import| {
        let package = workspace.resolve_import(from, &import.path)?;
        let name = import.alias.as_deref().unwrap_or(&package.name);
        (name == qualifier).then_some((import, package))
    })
}

fn lexical_expression_type<'tree>(
    source: &Source,
    mut expression: Node<'tree>,
) -> Option<Node<'tree>> {
    for _ in 0..MAX_TYPE_DEPTH {
        expression = match expression.kind() {
            "composite_literal" | "type_assertion_expression" | "type_conversion_expression" => {
                return lexical_type_declaration(source, expression.child_by_field_name("type")?);
            }
            "parenthesized_expression" => expression.named_child(0)?,
            "unary_expression"
                if expression
                    .child_by_field_name("operator")
                    .is_some_and(|operator| matches!(operator.kind(), "&" | "*")) =>
            {
                expression.child_by_field_name("operand")?
            }
            "identifier" => {
                let binding = lexical_binding(
                    expression,
                    node_text(expression, &source.text),
                    &source.text,
                )?;
                if let Some(explicit) = binding.explicit_type {
                    return lexical_type_declaration(source, explicit);
                }
                if binding.result_index != 0 {
                    return None;
                }
                binding.initializer?
            }
            _ => return None,
        };
    }
    None
}

fn lexical_type_declaration<'tree>(source: &Source, mut node: Node<'tree>) -> Option<Node<'tree>> {
    loop {
        node = match node.kind() {
            "pointer_type" | "parenthesized_type" => node.named_child(0)?,
            "generic_type" => node.child_by_field_name("type")?,
            "type_identifier" => {
                let binding = lexical_binding(node, node_text(node, &source.text), &source.text)?;
                return binding
                    .name
                    .parent()
                    .filter(|owner| is_lexical_type_owner(*owner))
                    .map(|_| binding.name);
            }
            _ => return None,
        };
    }
}

fn is_lexical_type_owner(node: Node<'_>) -> bool {
    is_type_declaration(node) || matches!(node.kind(), "type_parameter_declaration" | "type_elem")
}

pub(super) fn is_predeclared(name: &str) -> bool {
    matches!(
        name,
        "any"
            | "bool"
            | "byte"
            | "comparable"
            | "complex64"
            | "complex128"
            | "error"
            | "float32"
            | "float64"
            | "int"
            | "int8"
            | "int16"
            | "int32"
            | "int64"
            | "rune"
            | "string"
            | "uint"
            | "uint8"
            | "uint16"
            | "uint32"
            | "uint64"
            | "uintptr"
            | "true"
            | "false"
            | "iota"
            | "nil"
            | "append"
            | "cap"
            | "clear"
            | "close"
            | "complex"
            | "copy"
            | "delete"
            | "imag"
            | "len"
            | "make"
            | "max"
            | "min"
            | "new"
            | "panic"
            | "print"
            | "println"
            | "real"
            | "recover"
    )
}

pub(super) fn is_exported(symbol: &str) -> bool {
    symbol.chars().next().is_some_and(char::is_uppercase)
}

impl<'w> Resolver<'w, '_> {
    fn unify_function_types(
        &mut self,
        expected: TypeIdentity<'w>,
        actual: TypeIdentity<'w>,
        bindings: &mut [Option<TypeIdentity<'w>>],
    ) -> Result<bool> {
        let (Some(expected), Some(actual)) = (
            self.callable_components(expected)?,
            self.callable_components(actual)?,
        ) else {
            return Ok(false);
        };
        if expected.variadic != actual.variadic
            || expected.parameters.len() != actual.parameters.len()
            || expected.results.len() != actual.results.len()
        {
            return Ok(false);
        }
        for (left, right) in expected
            .parameters
            .into_iter()
            .chain(expected.results)
            .zip(actual.parameters.into_iter().chain(actual.results))
        {
            if !self.unify_types(left, right, bindings, false)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn callable_components(
        &mut self,
        ty: TypeIdentity<'w>,
    ) -> Result<Option<CallableComponents<'w>>> {
        let TypeKind::Function {
            file,
            signature,
            substitutions,
            method_receiver,
        } = ty.kind
        else {
            return Ok(None);
        };
        let imports = &self.indexes.file_index(&file)?.imports;
        let materialize = |reference: &Option<TypeRef>| {
            materialize_type(
                self.workspace,
                ty.package,
                &file,
                imports,
                reference.clone()?,
                &substitutions,
            )
        };
        let parameters = method_receiver
            .into_iter()
            .map(|receiver| Some(*receiver))
            .chain(signature.parameters.iter().map(materialize))
            .collect::<Option<Vec<_>>>();
        let results = signature
            .results
            .iter()
            .map(materialize)
            .collect::<Option<Vec<_>>>();
        Ok(parameters
            .zip(results)
            .map(|(parameters, results)| CallableComponents {
                parameters,
                results,
                variadic: signature.variadic,
            }))
    }
}

struct CallableComponents<'w> {
    parameters: Vec<TypeIdentity<'w>>,
    results: Vec<TypeIdentity<'w>>,
    variadic: bool,
}

#[cfg(test)]
mod enumeration_tests {
    use serde_json::json;

    use super::*;
    use crate::semantic::Engine;
    use crate::semantic::snapshot::DocumentSnapshot;
    use crate::test_support::TestProject;

    fn with_resolver(
        text: &str,
        test: impl for<'w> FnOnce(&mut Resolver<'w, '_>, &'w Package, &DocumentSnapshot<'_>),
    ) {
        let project = TestProject::new();
        let path = project.write("types.go", text);
        let mut engine = Engine::new(project.root.clone());
        assert!(engine.install_workspace(
            engine.workspace_load(),
            project.workspace(&["types.go"], false)
        ));
        let mut snapshot = engine.snapshot();
        let workspace = snapshot.workspace().unwrap();
        let package = workspace.package_of_file(&path).unwrap();
        let document = snapshot.document(&path).unwrap().unwrap();
        test(&mut snapshot.resolver(workspace), package, &document);
    }

    fn names<'a>(members: &'a [VisibleMember<'_>]) -> Vec<&'a str> {
        members.iter().map(|member| member.name.as_str()).collect()
    }

    #[test]
    fn enumerates_value_pointer_and_method_expression_selectors() {
        with_resolver(
            concat!(
                "package sample\n",
                "type T struct { Field int; Callback func() }\n",
                "func (T) Value() {}\nfunc (*T) Pointer() {}\n",
                "type Alias = T\ntype PtrAlias = *T\ntype Defined T\ntype DefinedPtr *T\n",
                "type I interface { Invoke() }\ntype IAlias = I\ntype DerivedI I\n",
                "type WithI struct { I }\n",
                "type WithT struct { T }\ntype WithPtr struct { *T }\n",
            ),
            |resolver, package, source| {
                for (name, pointers, addressable, type_expression, expected) in [
                    ("T", 0, false, false, &["Callback", "Field", "Value"][..]),
                    (
                        "T",
                        0,
                        true,
                        false,
                        &["Callback", "Field", "Pointer", "Value"],
                    ),
                    (
                        "T",
                        1,
                        false,
                        false,
                        &["Callback", "Field", "Pointer", "Value"],
                    ),
                    ("T", 2, true, false, &[]),
                    ("T", 0, false, true, &["Value"]),
                    ("T", 1, false, true, &["Pointer", "Value"]),
                    ("Alias", 0, false, true, &["Value"]),
                    ("PtrAlias", 0, false, true, &["Pointer", "Value"]),
                    ("PtrAlias", 1, false, false, &[]),
                    ("Defined", 0, true, false, &["Callback", "Field"]),
                    ("DefinedPtr", 0, true, false, &["Callback", "Field"]),
                    ("I", 0, false, false, &["Invoke"]),
                    ("I", 1, true, false, &[]),
                    ("IAlias", 1, false, true, &[]),
                    ("DerivedI", 0, false, true, &["Invoke"]),
                    ("WithI", 1, false, false, &["I", "Invoke"]),
                    ("WithI", 1, false, true, &["Invoke"]),
                    ("WithT", 0, false, true, &["Value"]),
                    ("WithT", 1, false, true, &["Pointer", "Value"]),
                    ("WithPtr", 0, false, true, &["Pointer", "Value"]),
                ] {
                    let ty = TypeIdentity {
                        pointers,
                        addressable,
                        type_expression,
                        ..TypeIdentity::named(package, name)
                    };
                    let members = resolver.visible_members(package, &ty).unwrap();
                    assert_eq!(
                        names(&members),
                        expected,
                        "{name}, {pointers}, {addressable}, {type_expression}"
                    );
                    if type_expression {
                        for member in &members {
                            let function = resolver
                                .symbol_type(source.indexed(), &member.resolved, &member.name, 0)
                                .unwrap()
                                .unwrap();
                            assert!(matches!(
                                function.kind,
                                TypeKind::Function { method_receiver: Some(receiver), .. }
                                    if receiver.pointers == pointers
                                        && matches!(&receiver.kind, TypeKind::Named { name: actual, .. } if actual == name)
                            ));
                        }
                    }
                }
            },
        );
    }

    #[test]
    fn shadows_deeper_members_and_cancels_distinct_same_depth_paths() {
        with_resolver(
            concat!(
                "package sample\n",
                "type Shared struct { Collision int; Closest int }\n",
                "func (Shared) Blocked() {}\nfunc (Shared) Ambiguous() {}\n",
                "type Left struct{ Shared }\ntype Right struct{ Shared }\n",
                "type Both struct { Left; Right; Closest bool }\n",
                "type Block struct { Shared; Blocked bool }\n",
                "type PointerBlock struct { Shared }\nfunc (*PointerBlock) Blocked() {}\n",
                "type Cycle struct{ *Cycle; Own int }\n",
            ),
            |resolver, package, _| {
                for (name, type_expression, expected) in [
                    ("Both", false, &["Closest", "Left", "Right"][..]),
                    (
                        "Block",
                        false,
                        &["Ambiguous", "Blocked", "Closest", "Collision", "Shared"],
                    ),
                    ("Block", true, &["Ambiguous"]),
                    (
                        "PointerBlock",
                        false,
                        &["Ambiguous", "Closest", "Collision", "Shared"],
                    ),
                    ("Cycle", false, &["Cycle", "Own"]),
                ] {
                    let ty = TypeIdentity {
                        type_expression,
                        ..TypeIdentity::named(package, name)
                    };
                    let members = resolver.visible_members(package, &ty).unwrap();
                    assert_eq!(names(&members), expected, "{name}");
                    if let Some(member) = members.iter().find(|member| member.name == "Closest") {
                        assert_eq!(member.embedded_path.is_empty(), name == "Both");
                    }
                }
            },
        );
    }

    #[test]
    fn combines_interface_methods_and_enumerates_type_parameter_constraints() {
        with_resolver(
            concat!(
                "package sample\n",
                "type Base interface { Shared(); BaseOnly() }\n",
                "type Other interface { Shared(); OtherOnly() }\n",
                "type Combined interface { Base; Other }\n",
                "type Pair struct { Base; Other }\n",
                "type Term struct { Field int }\nfunc (Term) TermOnly() {}\n",
                "type Constraint interface { Term; Combined; Extra() }\n",
                "func use[T Constraint](value T) { value.Probe() }\n",
            ),
            |resolver, package, document| {
                for (ty, expected) in [
                    ("Combined", &["BaseOnly", "OtherOnly", "Shared"][..]),
                    ("Pair", &["Base", "BaseOnly", "Other", "OtherOnly"]),
                ] {
                    let members = resolver
                        .visible_members(package, &TypeIdentity::named(package, ty))
                        .unwrap();
                    assert_eq!(names(&members), expected);
                }
                let start = document.text().find("value.Probe").unwrap();
                let receiver = document
                    .root_node()
                    .named_descendant_for_byte_range(start, start + "value".len())
                    .unwrap();
                let ty = resolver
                    .infer_type(package, document.indexed(), receiver, 0, 0)
                    .unwrap()
                    .unwrap();
                let members = resolver.visible_members(package, &ty).unwrap();
                assert_eq!(
                    names(&members),
                    ["BaseOnly", "Extra", "OtherOnly", "Shared"]
                );
                assert!(members.iter().all(|member| member.embedded_path.is_empty()));
            },
        );
    }

    #[test]
    fn retains_generic_field_types_owners_paths_and_package_visibility() {
        let project = TestProject::new();
        let decl = project.write(
            "sample.go",
            concat!(
                "package sample\nimport foreign \"example.test/foreign\"\n",
                "type Wrap struct { foreign.Box[string]; Direct bool }\n",
            ),
        );
        let dependency = project.write(
            "foreign/types.go",
            concat!(
                "package foreign\n",
                "type Leaf[T any] struct { Value T; private T }\n",
                "type hidden struct{ Exported int }\n",
                "type Box[T any] struct { Leaf[T]; Callback func(T); hidden }\n",
                "func (Box[U]) Get() U { panic(\"stub\") }\n",
                "func (Box[U]) privateMethod() {}\n",
            ),
        );
        let listing = format!(
            "{}\n{}",
            json!({"Dir": project.root, "ImportPath": "example.test/sample", "Name": "sample", "GoFiles": ["sample.go"]}),
            json!({"Dir": dependency.parent(), "ImportPath": "example.test/foreign", "Name": "foreign", "GoFiles": ["types.go"]})
        );
        let workspace =
            Workspace::from_test_listing(std::slice::from_ref(&project.root), &listing, false);
        let mut engine = Engine::new(project.root.clone());
        assert!(engine.install_workspace(engine.workspace_load(), workspace));
        let mut snapshot = engine.snapshot();
        let workspace = snapshot.workspace().unwrap();
        let package = workspace.package_of_file(&decl).unwrap();
        let dependency = workspace.package_of_file(&dependency).unwrap();
        let source = snapshot.document(&decl).unwrap().unwrap();
        let mut resolver = snapshot.resolver(workspace);
        let ty = TypeIdentity::named(package, "Wrap");
        let members = resolver.visible_members(package, &ty).unwrap();
        assert_eq!(
            names(&members),
            [
                "Box", "Callback", "Direct", "Exported", "Get", "Leaf", "Value"
            ]
        );
        let get = members.iter().find(|member| member.name == "Get").unwrap();
        assert_eq!(get.embedded_path, ["Box"]);
        let method_ty = resolver
            .symbol_type(source.indexed(), &get.resolved, &get.name, 0)
            .unwrap()
            .unwrap();
        let components = resolver.callable_components(method_ty).unwrap().unwrap();
        assert!(
            matches!(&components.results[0].kind, TypeKind::Named { name, .. } if name == "string")
        );

        let fields = resolver.struct_fields(package, &ty).unwrap();
        assert_eq!(
            fields
                .iter()
                .map(|field| field.member.name.as_str())
                .collect::<Vec<_>>(),
            [
                "Box", "Direct", "Callback", "Leaf", "hidden", "Value", "private", "Exported"
            ]
        );
        let value = &fields[5];
        assert_eq!(value.member.embedded_path, ["Box", "Leaf"]);
        assert!(value.visible);
        assert!(!fields[6].visible);
        assert_eq!(fields[7].member.embedded_path, ["Box", "hidden"]);
        assert!(!fields[7].visible);
        let owner = value.member.resolved.receiver.as_ref().unwrap();
        assert!(std::ptr::eq(owner.package, dependency));
        assert!(matches!(&owner.kind, TypeKind::Named { name, .. } if name == "Leaf"));
        assert!(
            matches!(&value.ty.as_ref().unwrap().kind, TypeKind::Named { name, .. } if name == "string")
        );
    }

    #[test]
    fn enumerates_package_declarations_from_the_selected_test_view_and_overlay() {
        let project = TestProject::new();
        let regular = project.write("sample.go", "package sample\nvar Disk int\n");
        let internal = project.write("sample_test.go", "package sample\nvar InternalTest int\n");
        let external = project.write(
            "external_test.go",
            "package sample_test\nvar ExternalTest int\n",
        );
        let listing = [
            json!({"Dir": project.root, "Name": "sample", "ImportPath": "example.test/sample", "GoFiles": ["sample.go"]}),
            json!({"Dir": project.root, "Name": "sample", "ImportPath": "example.test/sample [example.test/sample.test]", "ForTest": "example.test/sample", "GoFiles": ["sample.go", "sample_test.go"]}),
            json!({"Dir": project.root, "Name": "sample_test", "ImportPath": "example.test/sample_test [example.test/sample.test]", "ForTest": "example.test/sample", "GoFiles": ["external_test.go"]}),
        ].into_iter().map(|value| value.to_string()).collect::<Vec<_>>().join("\n");
        let mut engine = Engine::new(project.root.clone());
        engine
            .open(
                regular.clone(),
                "package sample\nvar Overlay int\nvar private int\nfunc init() {}\n".into(),
                1,
            )
            .unwrap();
        assert!(engine.install_workspace(
            engine.workspace_load(),
            Workspace::from_test_listing(std::slice::from_ref(&project.root), &listing, true)
        ));
        let mut snapshot = engine.snapshot();
        let workspace = snapshot.workspace().unwrap();
        let regular = workspace.package_of_file(&regular).unwrap();
        let internal = workspace.package_of_file(&internal).unwrap();
        let external = workspace.package_of_file(&external).unwrap();
        let mut resolver = snapshot.resolver(workspace);
        for (from, target, expected) in [
            (regular, regular, &["Overlay", "private"][..]),
            (internal, internal, &["InternalTest", "Overlay", "private"]),
            (external, external, &["ExternalTest"]),
            (external, regular, &["Overlay"]),
        ] {
            let declarations = resolver.package_declarations(from, target).unwrap();
            assert_eq!(
                declarations
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>(),
                expected
            );
            assert!(
                declarations
                    .iter()
                    .all(|(_, declaration)| std::ptr::eq(declaration.package, target))
            );
        }
    }
}
