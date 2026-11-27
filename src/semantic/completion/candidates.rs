use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use lsp_types::CompletionItemKind as Kind;
use tree_sitter::Node;

use crate::scope::{VisibleBinding, VisibleBindingKind, visible_bindings};
use crate::semantic::{
    index::IndexedSource,
    presentation::{builtin_function, parameter_labels_with, result_label_with, syntax_text_with},
    resolver::{
        ResolvedSymbol, Resolver, TypeIdentity, TypeKind, is_predeclared_type, untyped_default,
    },
    snapshot::DocumentSnapshot,
    source::{Source, is_test_source, node_at_range, node_text},
};
use crate::symbols::{Container, Symbol, SymbolKind};
use crate::workspace::{Package, is_symbol_source, path_key};

use super::CompletionClientCapabilities;
use super::cursor::{
    Edit, Expression, LabelKind, LiteralMode, Site, ancestors, identifier_char, identifier_start,
    is_function, is_keyword,
};
use super::expected::{Expected, argument_or_result, is_interface, literal_type};
use super::imports::{FileImport, Selection, SymbolSearch, assumed_package_name, package_weight};
use super::ranking::{
    Candidate, FIELD, Insert, KEYWORD, Match, SYMBOL, match_name, match_start, package_priority,
};
use super::render::{Target, describe, package_name};

const FILE_KEYWORDS: &[&str] = &["package", "import", "func", "type", "const", "var"];
const STMT_KEYWORDS: &[&str] = &[
    "if",
    "for",
    "switch",
    "select",
    "return",
    "go",
    "defer",
    "var",
    "const",
    "type",
    "fallthrough",
    "break",
    "continue",
    "goto",
];
const TYPE_KEYWORDS: &[&str] = &["struct", "interface", "map", "chan", "func"];
const BUILTIN_TYPES: &[&str] = &[
    "any",
    "bool",
    "byte",
    "comparable",
    "complex64",
    "complex128",
    "error",
    "float32",
    "float64",
    "int",
    "int8",
    "int16",
    "int32",
    "int64",
    "rune",
    "string",
    "uint",
    "uint8",
    "uint16",
    "uint32",
    "uint64",
    "uintptr",
];
const BUILTIN_VALUES: &[&str] = &[
    "true", "false", "nil", "append", "cap", "clear", "close", "complex", "copy", "delete", "imag",
    "len", "make", "max", "min", "new", "panic", "print", "println", "real", "recover",
];

pub(super) fn syntax_candidates(
    site: &Site<'_>,
    source: &Source,
    anchor: Node<'_>,
    prefix: &str,
    docs: &HashMap<PathBuf, IndexedSource>,
) -> Option<Vec<Candidate>> {
    let candidates = match site {
        Site::Suppressed => Vec::new(),
        Site::PackageClause => package_names(&source.path, docs, prefix),
        Site::FileTopLevel => keywords(FILE_KEYWORDS, prefix).collect(),
        Site::CaseClause => keywords(&["case", "default"], prefix).collect(),
        Site::Label(kind) => labels(source, anchor, *kind, prefix),
        _ => return None,
    };
    Some(candidates)
}

pub(super) struct Collection {
    pub(super) candidates: Vec<Candidate>,
    pub(super) search: Option<SymbolSearch>,
}

pub(super) fn collect<'w>(
    document: &DocumentSnapshot<'_>,
    source: &Source,
    anchor: Node<'_>,
    site: Site<'_>,
    edit: &Edit,
    graph: Option<(&mut Resolver<'w, '_>, &'w Package)>,
    caps: &CompletionClientCapabilities,
) -> Result<Collection> {
    let prefix = edit.prefix(document.text());
    let bindings = visible_bindings(source.tree.root_node(), anchor.start_byte(), &source.text);
    let mut collector = Collector {
        document,
        source,
        anchor,
        prefix,
        followed_by_selector: document.text()[edit.replace.end..]
            .trim_start_matches([' ', '\t'])
            .starts_with('.'),
        caps,
        bindings,
        items: Vec::new(),
        declared: HashSet::new(),
        skipped_package_names: HashSet::new(),
        imports: Vec::new(),
        expected: None,
    };
    let search = if let Some((resolver, package)) = graph {
        collector.expected = Expected::from_cursor(resolver, package, source, anchor)?;
        collector.semantic(resolver, package, site)?
    } else {
        collector.local_only(site);
        None
    };
    Ok(Collection {
        candidates: collector.items,
        search,
    })
}

struct Collector<'a> {
    document: &'a DocumentSnapshot<'a>,
    source: &'a Source,
    anchor: Node<'a>,
    prefix: &'a str,
    followed_by_selector: bool,
    caps: &'a CompletionClientCapabilities,
    bindings: Vec<VisibleBinding<'a>>,
    items: Vec<Candidate>,
    declared: HashSet<String>,
    skipped_package_names: HashSet<String>,
    imports: Vec<FileImport>,
    expected: Option<Expected>,
}

#[derive(Clone, Copy, Default)]
struct Names {
    types_only: bool,
    receiver: bool,
    literal: bool,
    address_of: bool,
    const_spec: bool,
}

impl Collector<'_> {
    fn semantic<'w>(
        &mut self,
        resolver: &mut Resolver<'w, '_>,
        package: &'w Package,
        site: Site<'_>,
    ) -> Result<Option<SymbolSearch>> {
        self.imports = self
            .document
            .raw_imports()
            .map(|import| {
                let target = resolver.workspace.resolve_import(package, &import.path);
                let qualifier = import
                    .alias
                    .or_else(|| target.map(|package| package.name.clone()))
                    .unwrap_or_else(|| assumed_package_name(&import.path));
                FileImport {
                    path: import.path,
                    qualifier,
                    in_graph: target.is_some(),
                }
            })
            .collect();
        match site {
            Site::Selector {
                receiver,
                types_only,
                type_switch_guard,
                is_address_of,
            } => self.selector(
                resolver,
                package,
                receiver,
                types_only,
                type_switch_guard,
                is_address_of,
            ),
            Site::TypePosition {
                is_receiver_type,
                is_type_arg,
                type_switch_guard,
            } => {
                let receiver = is_receiver_type && !is_type_arg;
                let names = Names {
                    types_only: true,
                    receiver,
                    ..Names::default()
                };
                self.unqualified(resolver, package, names)?;
                if type_switch_guard {
                    self.items.extend(keywords(&["type"], self.prefix));
                }
                let in_case = ancestors(self.anchor).any(|node| node.kind() == "type_case");
                if in_case
                    && !self.declared.contains("nil")
                    && let Some(matched) = match_name(self.prefix, "nil")
                {
                    self.items
                        .push(Candidate::new("nil", Kind::CONSTANT, matched, SYMBOL));
                }
                Ok((!receiver && (!in_case || !self.prefix.is_empty()))
                    .then(|| self.search_names(names, in_case))
                    .flatten())
            }
            Site::StructLiteral { body, mode } => {
                let fields = self.fields(resolver, package, body, mode)?;
                if !fields || mode != LiteralMode::KeysOnly {
                    let names = Names {
                        literal: true,
                        ..Names::default()
                    };
                    self.unqualified(resolver, package, names)?;
                    return Ok(self.search_names(names, false));
                }
                Ok(None)
            }
            Site::Expression(expression) => {
                let names = expression_names(&expression);
                self.unqualified(resolver, package, names)?;
                self.expression_keywords(&expression);
                Ok(self.search_names(names, expression.is_case_value))
            }
            _ => Ok(None),
        }
    }

    fn local_only(&mut self, site: Site<'_>) {
        let names = match &site {
            Site::TypePosition {
                is_receiver_type,
                is_type_arg,
                ..
            } => Names {
                types_only: true,
                receiver: *is_receiver_type && !*is_type_arg,
                ..Names::default()
            },
            Site::Expression(expression) => expression_names(expression),
            Site::StructLiteral { .. } => Names {
                literal: true,
                ..Names::default()
            },
            _ => return,
        };
        if !names.receiver {
            for binding in &self.bindings {
                self.declared.insert(binding.name.clone());
                if (!names.types_only || is_type_binding(binding))
                    && binding.name != "init"
                    && let Some(matched) = match_name(self.prefix, &binding.name)
                {
                    self.items
                        .push(local_candidate(self.source, binding, matched, self.caps));
                }
            }
        }
        for (name, symbol) in self.document.declarations().declarations() {
            if name == "init"
                || name == "_"
                || !self.declared.insert(name.to_owned())
                || (names.types_only && !matches!(symbol.kind, SymbolKind::Type))
            {
                continue;
            }
            if let Some(matched) = match_name(self.prefix, name) {
                self.items.push(indexed_candidate(
                    self.document.indexed(),
                    name,
                    symbol,
                    matched,
                    self.caps,
                ));
            }
        }
        self.builtins(&names);
        if let Site::Expression(expression) = site {
            self.expression_keywords(&expression);
        }
    }

    fn unqualified<'w>(
        &mut self,
        resolver: &mut Resolver<'w, '_>,
        package: &'w Package,
        names: Names,
    ) -> Result<()> {
        for binding in &self.bindings {
            self.declared.insert(binding.name.clone());
            if names.receiver
                || binding.name == "init"
                || (names.types_only && !is_type_binding(binding))
            {
                continue;
            }
            let Some(matched) = match_name(self.prefix, &binding.name) else {
                continue;
            };
            let mut candidate = local_candidate(self.source, binding, matched, self.caps);
            let ty = if let Some(explicit) = binding.binding.explicit_type {
                resolver.source_type(package, self.source, explicit)
            } else if let Some(initializer) = binding.binding.initializer {
                resolver
                    .infer_type(
                        package,
                        self.source,
                        initializer,
                        0,
                        binding.binding.result_index,
                    )?
                    .or_else(|| {
                        untyped_default(initializer).map(|name| TypeIdentity::named(package, name))
                    })
            } else {
                None
            };
            if let Some(ty) = ty {
                if names.literal && is_type_binding(binding) && composite_type(resolver, &ty)? {
                    candidate.insert = Insert::Braces;
                }
                if !is_type_binding(binding)
                    && let Some(type_text) =
                        resolver.type_text(package, self.document.indexed(), &ty)?
                {
                    let declaration = if binding.kind == VisibleBindingKind::Const {
                        "const"
                    } else {
                        "var"
                    };
                    candidate.detail = Some(format!("{declaration} {} {type_text}", binding.name));
                }
                if let Some(expected) = &self.expected {
                    candidate.weight.priority += expected.weight(resolver, &ty)?;
                }
            }
            self.items.push(candidate);
        }

        // Include the current document first, including a new test draft that
        // has not appeared in go list yet. Package declarations are a separate
        // scope, so a value binding can hide a type even in a type position.
        let mut package_names = HashSet::new();
        for (name, symbol) in self.document.declarations().declarations() {
            package_names.insert(name.to_owned());
            let resolved = ResolvedSymbol {
                package,
                file: self.document.path().to_owned(),
                symbol: symbol.clone(),
                receiver: None,
                method_receiver: None,
            };
            self.add_package_symbol(resolver, package, name, &resolved, names)?;
        }
        for (name, resolved) in resolver.local_declarations(
            package,
            |name| match_name(self.prefix, name).is_some(),
            &mut self.skipped_package_names,
        )? {
            if package_names.insert(name.clone())
                && (is_test_source(self.document.path()) || !is_test_source(&resolved.file))
            {
                self.add_package_symbol(resolver, package, &name, &resolved, names)?;
            }
        }
        if names.receiver {
            return Ok(());
        }
        let mut seen = self.declared.clone();
        for import in self
            .imports
            .iter()
            .filter(|import| import.qualifier != ".")
            .chain(self.imports.iter().filter(|import| import.qualifier == "."))
        {
            let target = resolver.workspace.resolve_import(package, &import.path);
            match import.qualifier.as_str() {
                "_" => {}
                "." => {
                    if let Some(target) = target {
                        for (name, resolved) in resolver.package_declarations(package, target)? {
                            if seen.insert(name.clone())
                                && (!names.types_only
                                    || matches!(resolved.symbol.kind, SymbolKind::Type))
                                && let Some(matched) = match_name(self.prefix, &name)
                            {
                                let mut candidate = resolved_candidate(
                                    resolver,
                                    package,
                                    self.document.indexed(),
                                    &name,
                                    &resolved,
                                    matched,
                                    self.expected.as_ref(),
                                    names.literal,
                                    self.caps,
                                )?;
                                candidate.import_path = Some(import.path.clone());
                                self.items.push(candidate);
                            }
                        }
                    }
                }
                name => {
                    if seen.insert(name.to_owned())
                        && let Some(matched) = match_name(self.prefix, name)
                    {
                        let mut candidate = Candidate::new(
                            name,
                            Kind::MODULE,
                            matched,
                            package_priority(package.import_path(), &import.path),
                        );
                        candidate.insert = Insert::Package;
                        candidate.import_path = Some(import.path.clone());
                        if let Some(target) = target {
                            package_weight(
                                resolver.workspace,
                                Some(package),
                                target,
                                &mut candidate.weight,
                            );
                        }
                        self.items.push(candidate);
                    }
                }
            }
        }
        // Dot imports that are already in the graph also shadow predeclared
        // names and participate in collision checks.
        self.declared.extend(
            seen.into_iter()
                .filter(|name| !self.imports.iter().any(|import| import.qualifier == *name)),
        );
        self.builtins(&names);
        Ok(())
    }

    fn add_package_symbol<'w>(
        &mut self,
        resolver: &mut Resolver<'w, '_>,
        package: &'w Package,
        name: &str,
        resolved: &ResolvedSymbol<'w>,
        names: Names,
    ) -> Result<()> {
        if name == "_" || name == "init" {
            return Ok(());
        }
        let unshadowed = self.declared.insert(name.to_owned());
        let ty = matches!(resolved.symbol.kind, SymbolKind::Type);
        if (!names.receiver && !unshadowed) || (names.types_only && !ty) {
            return Ok(());
        }
        let Some(matched) = match_name(self.prefix, name) else {
            return Ok(());
        };
        if names.receiver {
            let ty = TypeIdentity::named(package, name);
            let underlying = resolver.underlying_type(ty)?;
            if underlying.is_none_or(|ty| ty.pointers != 0)
                || resolver
                    .type_info(package, name)?
                    .is_some_and(|(_, details)| details.alias.is_some())
            {
                return Ok(());
            }
            let ty = TypeIdentity::named(package, name);
            if let Some(underlying) = resolver.underlying_type(ty)?
                && is_interface(resolver, &underlying)?
            {
                return Ok(());
            }
        }
        let mut candidate = resolved_candidate(
            resolver,
            package,
            self.document.indexed(),
            name,
            resolved,
            matched,
            self.expected.as_ref(),
            names.literal,
            self.caps,
        )?;
        candidate.weight.same_package = true;
        self.items.push(candidate);
        Ok(())
    }

    fn selector<'w>(
        &mut self,
        resolver: &mut Resolver<'w, '_>,
        package: &'w Package,
        receiver: Node<'_>,
        types_only: bool,
        type_switch_guard: bool,
        address_of: bool,
    ) -> Result<Option<SymbolSearch>> {
        if type_switch_guard && let Some(matched) = match_name(self.prefix, "type") {
            let mut item = Candidate::new("(type)", Kind::KEYWORD, matched, KEYWORD);
            item.filter = Some("type".into());
            self.items.push(item);
        }
        if types_only
            && ancestors(self.anchor).any(|node| {
                node.kind() == "parameter_list"
                    && node
                        .parent()
                        .is_some_and(|owner| owner.child_by_field_name("receiver") == Some(node))
            })
        {
            return Ok(None);
        }
        let qualifier = matches!(
            receiver.kind(),
            "identifier" | "package_identifier" | "type_identifier"
        )
        .then(|| node_text(receiver, &self.source.text));
        let shadowed = if let Some(name) = qualifier {
            self.bindings.iter().any(|binding| binding.name == name)
                || self.document.declarations().declaration(name).is_some()
                || resolver.find_symbol(package, name, None)?.is_some()
        } else {
            false
        };
        if let Some(qualifier) = qualifier.filter(|_| !shadowed)
            && let Some((import, target)) =
                self.source
                    .imported_package(resolver.workspace, package, qualifier)
        {
            for (name, resolved) in resolver.matching_exports(package, target, |name| {
                match_name(self.prefix, name).is_some()
            })? {
                if (types_only && !matches!(resolved.symbol.kind, SymbolKind::Type))
                    || name == "_"
                    || !crate::semantic::resolver::is_exported(&name)
                {
                    continue;
                }
                let Some(matched) = match_name(self.prefix, &name) else {
                    continue;
                };
                let mut candidate = resolved_candidate(
                    resolver,
                    package,
                    self.document.indexed(),
                    &name,
                    &resolved,
                    matched,
                    self.expected.as_ref(),
                    !types_only,
                    self.caps,
                )?;
                candidate.import_path = Some(import.path.clone());
                self.items.push(candidate);
            }
            return Ok(None);
        }
        if !types_only
            && let Some(ty) = resolver.infer_type(package, self.source, receiver, 0, 0)?
        {
            for member in resolver.visible_members(package, &ty)? {
                let Some(matched) = match_name(self.prefix, &member.name) else {
                    continue;
                };
                let candidate = resolved_candidate(
                    resolver,
                    package,
                    self.document.indexed(),
                    &member.name,
                    &member.resolved,
                    matched,
                    self.expected.as_ref(),
                    false,
                    self.caps,
                )?;
                self.items.push(candidate);
            }
            return Ok(None);
        }
        let Some(qualifier) = qualifier else {
            return Ok(None);
        };
        if type_switch_guard {
            return Ok(None);
        }
        self.declared
            .extend(self.bindings.iter().map(|binding| binding.name.clone()));
        self.declared.extend(
            resolver
                .package_declarations(package, package)?
                .into_iter()
                .map(|(name, _)| name),
        );
        let names = Names {
            types_only,
            address_of,
            literal: !types_only,
            ..Names::default()
        };
        Ok(Some(self.search(
            Selection::Qualifier {
                name: qualifier.to_owned(),
                start: receiver.start_byte(),
            },
            names,
        )))
    }

    fn fields<'w>(
        &mut self,
        resolver: &mut Resolver<'w, '_>,
        package: &'w Package,
        body: Node<'_>,
        mode: LiteralMode,
    ) -> Result<bool> {
        let Some(ty) = literal_type(resolver, package, self.source, body)? else {
            return Ok(false);
        };
        if resolver.container(ty.clone())?.is_some() || is_interface(resolver, &ty)? {
            return Ok(false);
        }
        if mode == LiteralMode::ValuesOnly {
            return Ok(true);
        }
        let fields = resolver.struct_fields(package, &ty)?;
        let used: HashSet<_> = body
            .named_children(&mut body.walk())
            .filter_map(|node| node.child_by_field_name("key"))
            .filter(|key| {
                self.anchor.start_byte() < key.start_byte()
                    || key.end_byte() < self.anchor.end_byte()
            })
            .map(|key| node_text(key, &self.source.text))
            .collect();
        let has_colon = self.source.text[self.anchor.end_byte()..]
            .trim_start_matches([' ', '\t'])
            .starts_with(':');
        let mut types = HashMap::new();
        for field in fields.iter().filter(|field| field.visible) {
            let path = &field.member.embedded_path;
            if let Some(ty) = &field.ty
                && let Some(ty) =
                    nested_literal_name(resolver, package, self.document.indexed(), ty)?
            {
                let mut path = path.clone();
                path.push(field.member.name.clone());
                types.insert(path, ty);
            }
        }
        for field in fields {
            let member = field.member;
            if !field.visible
                || if member.embedded_path.is_empty() {
                    used.contains(member.name.as_str())
                } else {
                    has_colon || used.contains(member.embedded_path[0].as_str())
                }
            {
                continue;
            }
            let Some(matched) = match_name(self.prefix, &member.name) else {
                continue;
            };
            let mut path = Vec::new();
            let nesting = member
                .embedded_path
                .iter()
                .map(|name| {
                    path.push(name.clone());
                    Some((name.clone(), types.get(&path)?.clone()))
                })
                .collect::<Option<Vec<_>>>();
            let Some(nesting) = nesting else { continue };
            let mut candidate = resolved_candidate(
                resolver,
                package,
                self.document.indexed(),
                &member.name,
                &member.resolved,
                matched,
                None,
                false,
                self.caps,
            )?;
            candidate.weight.depth = Some(nesting.len());
            candidate.insert = Insert::Field { nesting };
            self.items.push(candidate);
        }
        Ok(true)
    }

    fn builtins(&mut self, names: &Names) {
        if names.receiver {
            return;
        }
        let constants = if names.const_spec { &["iota"][..] } else { &[] };
        let values = if names.types_only {
            &[]
        } else {
            BUILTIN_VALUES
        };
        for name in BUILTIN_TYPES.iter().chain(values).chain(constants) {
            if self.declared.contains(*name)
                || self.imports.iter().any(|import| import.qualifier == *name)
            {
                continue;
            }
            let Some(matched) = match_name(self.prefix, name) else {
                continue;
            };
            let function = builtin_function(name);
            let kind = if is_predeclared_type(name) {
                Kind::CLASS
            } else if function.is_some() {
                Kind::FUNCTION
            } else {
                Kind::CONSTANT
            };
            let mut candidate = Candidate::new(*name, kind, matched, SYMBOL);
            if let Some(signature) = function {
                candidate.detail = Some(signature.into());
                candidate.insert = Insert::Call {
                    has_parameters: *name != "recover",
                };
            }
            // Untyped constants and nil receive their contextual type.
            if let Some(expected) = &self.expected {
                candidate.weight.priority += match *name {
                    "nil" if expected.accepts_nil() => 4,
                    "true" | "false" => {
                        i32::from(expected.keys_weight("bool", Some("bool")) > 0) * 5
                    }
                    "iota" => i32::from(expected.keys_weight("int", Some("int")) > 0) * 5,
                    "cap" | "copy" | "len" => expected.keys_weight("int", Some("int")),
                    _ => 0,
                };
            }
            self.items.push(candidate);
        }
        if names.types_only {
            self.items.extend(keywords(TYPE_KEYWORDS, self.prefix));
        }
    }

    fn expression_keywords(&mut self, expression: &Expression) {
        if expression.is_stmt_start {
            self.items.extend(keywords(STMT_KEYWORDS, self.prefix));
        }
        if expression.is_for_range_pos {
            self.items.extend(keywords(&["range"], self.prefix));
        }
    }

    fn search_names(&mut self, names: Names, skip_auto_import: bool) -> Option<SymbolSearch> {
        let skip_auto_import = skip_auto_import || self.followed_by_selector;
        let unresolved_dot = self
            .imports
            .iter()
            .any(|import| import.qualifier == "." && !import.in_graph);
        if (skip_auto_import || self.prefix.is_empty()) && !unresolved_dot {
            return None;
        }
        Some(self.search(
            Selection::Name {
                auto_import: !skip_auto_import && !self.prefix.is_empty(),
            },
            names,
        ))
    }

    fn search(&mut self, selection: Selection, names: Names) -> SymbolSearch {
        SymbolSearch {
            selection,
            types_only: names.types_only,
            address_of: names.address_of,
            literal: names.literal,
            declared: std::mem::take(&mut self.declared),
            skipped_package_names: std::mem::take(&mut self.skipped_package_names),
            imports: std::mem::take(&mut self.imports),
            expected: self.expected.take(),
        }
    }
}

fn expression_names(expression: &Expression) -> Names {
    Names {
        literal: true,
        address_of: expression.is_address_of,
        const_spec: expression.is_const_spec,
        ..Names::default()
    }
}

fn is_type_binding(binding: &VisibleBinding<'_>) -> bool {
    matches!(
        binding.kind,
        VisibleBindingKind::Type | VisibleBindingKind::TypeParameter
    )
}

fn local_candidate(
    source: &Source,
    binding: &VisibleBinding<'_>,
    matched: Match,
    caps: &CompletionClientCapabilities,
) -> Candidate {
    let kind = match binding.kind {
        VisibleBindingKind::Type => Kind::CLASS,
        VisibleBindingKind::TypeParameter => Kind::TYPE_PARAMETER,
        VisibleBindingKind::Const => Kind::CONSTANT,
        _ => Kind::VARIABLE,
    };
    let range = source.range(binding.binding.name);
    let description = describe(source, range, &binding.name, kind, caps.deprecated_support);
    let mut candidate = Candidate::new(&binding.name, kind, matched, SYMBOL);
    candidate.detail = description.detail;
    candidate.deprecated = description.deprecated;
    candidate.target = Some(Target {
        file: source.path.clone(),
        range,
    });
    candidate.weight.same_package = true;
    candidate.weight.depth = Some(binding.scope_depth);
    candidate
}

fn symbol_kind(symbol: &Symbol, member: bool) -> Kind {
    match symbol.kind {
        SymbolKind::Type => Kind::CLASS,
        SymbolKind::Function(_) if member => Kind::METHOD,
        SymbolKind::Function(_) => Kind::FUNCTION,
        _ if member => Kind::FIELD,
        _ => Kind::VARIABLE,
    }
}

fn indexed_candidate(
    source: &Source,
    name: &str,
    symbol: &Symbol,
    matched: Match,
    caps: &CompletionClientCapabilities,
) -> Candidate {
    let description = describe(
        source,
        symbol.range,
        name,
        symbol_kind(symbol, false),
        caps.deprecated_support,
    );
    let mut candidate = Candidate::new(name, description.kind, matched, SYMBOL);
    candidate.detail = description.detail;
    candidate.label_suffix = description.label_suffix;
    candidate.deprecated = description.deprecated;
    candidate.target = Some(Target {
        file: source.path.clone(),
        range: symbol.range,
    });
    if let SymbolKind::Function(signature) = &symbol.kind {
        candidate.insert = Insert::Call {
            has_parameters: description.has_parameters || !signature.parameters.is_empty(),
        };
    }
    candidate.weight.same_package = true;
    candidate
}

pub(super) fn resolved_candidate<'w>(
    resolver: &mut Resolver<'w, '_>,
    from: &'w Package,
    source: &IndexedSource,
    name: &str,
    resolved: &ResolvedSymbol<'w>,
    matched: Match,
    expected: Option<&Expected>,
    literal: bool,
    caps: &CompletionClientCapabilities,
) -> Result<Candidate> {
    let symbol = &resolved.symbol;
    let member = resolved.receiver.is_some();
    let kind = symbol_kind(symbol, member);
    let mut candidate = Candidate::new(
        name,
        kind,
        matched,
        if kind == Kind::FIELD { FIELD } else { SYMBOL },
    );
    candidate.weight.same_package = from.import_path() == resolved.package.import_path();
    candidate.target = Some(Target {
        file: resolved.file.clone(),
        range: symbol.range,
    });
    let same_file = path_key(&source.path) == path_key(&resolved.file);
    let mut definition = None;
    let description = if same_file {
        Some(describe(
            source,
            symbol.range,
            name,
            kind,
            caps.deprecated_support,
        ))
    } else {
        definition = resolver
            .indexes
            .declaration_source(&resolved.file)
            .ok()
            .flatten();
        definition.as_ref().map(|source| {
            describe(
                source.as_ref(),
                symbol.range,
                name,
                kind,
                caps.deprecated_support,
            )
        })
    };
    let mut has_parameters = false;
    if let Some(description) = description {
        candidate.kind = description.kind;
        candidate.detail = description.detail;
        candidate.label_suffix = description.label_suffix;
        candidate.deprecated = description.deprecated;
        has_parameters = description.has_parameters;
    }
    if matches!(symbol.kind, SymbolKind::Type) {
        let _phase = resolver
            .indexes
            .trace()
            .map(|trace| trace.phase("type_classification"));
        let (kind, braces) = type_presentation(resolver, resolved.package, name)?;
        if let Some(kind) = kind {
            candidate.kind = kind;
        }
        if literal && braces {
            candidate.insert = Insert::Braces;
        }
    }
    if let SymbolKind::Function(signature) = &symbol.kind {
        candidate.insert = Insert::Call {
            has_parameters: has_parameters
                || !signature.parameters.is_empty()
                || resolved.method_receiver.is_some(),
        };
    }
    let want_type = kind == Kind::FIELD || kind == Kind::METHOD || expected.is_some();
    if let Some(expected) = expected
        && matches!(symbol.kind, SymbolKind::InferredValue { .. })
    {
        let _phase = resolver
            .indexes
            .trace()
            .map(|trace| trace.phase("inferred_weight"));
        candidate.weight.priority += expected.inferred_weight(resolver, source, resolved, name)?;
        return Ok(candidate);
    }
    let ty = if want_type {
        let _phase = resolver
            .indexes
            .trace()
            .map(|trace| trace.phase("symbol_type"));
        let declaration_source = definition
            .as_ref()
            .map_or(&**source, |source| source.as_ref());
        resolver.symbol_type(declaration_source, resolved, name, 0)?
    } else {
        None
    };
    if let Some(ty) = ty {
        if kind == Kind::FIELD
            && let Some(label) = resolver.type_text(from, source, &ty)?
        {
            candidate.detail = Some(format!("field {name} {label}"));
            candidate.label_suffix = Some(format!(" {label}"));
        }
        if let TypeKind::Function {
            substitutions,
            method_receiver,
            ..
        } = &ty.kind
            && (!substitutions.is_empty() || method_receiver.is_some())
            && let Some(declaration_source) = if same_file {
                Some(&**source)
            } else {
                if definition.is_none() {
                    definition = resolver
                        .indexes
                        .declaration_source(&resolved.file)
                        .ok()
                        .flatten();
                }
                definition.as_ref().map(|source| &**source.as_ref())
            }
            && let Some((detail, suffix)) = specialized_signature(
                resolver,
                from,
                source,
                declaration_source,
                symbol,
                name,
                &ty,
            )?
        {
            candidate.detail = Some(detail);
            candidate.label_suffix = Some(suffix);
        }
        if let Some(expected) = expected {
            let _phase = resolver
                .indexes
                .trace()
                .map(|trace| trace.phase("expected_weight"));
            let weight = expected.weight(resolver, &ty)?;
            if let TypeKind::Function {
                signature,
                file,
                substitutions,
                ..
            } = &ty.kind
                && matches!(symbol.kind, SymbolKind::Function(_))
            {
                if weight == 5 {
                    candidate.insert = Insert::Name;
                } else if signature.results.len() == 1
                    && let Some(result) = argument_or_result(
                        resolver,
                        ty.package,
                        file,
                        substitutions,
                        signature.results[0].as_ref(),
                    )?
                {
                    candidate.weight.priority += expected.weight(resolver, &result)?;
                }
            }
            candidate.weight.priority += weight;
        }
    }
    Ok(candidate)
}

fn type_presentation<'w>(
    resolver: &mut Resolver<'w, '_>,
    package: &'w Package,
    name: &str,
) -> Result<(Option<Kind>, bool)> {
    let key = resolver
        .indexes
        .completion
        .as_ref()
        .map(|_| (package.id.clone(), name.to_owned()));
    if let Some(cached) = key
        .as_ref()
        .and_then(|key| resolver.indexes.completion.as_ref()?.type_kinds.get(key))
    {
        return Ok(*cached);
    }
    let ty = TypeIdentity::named(package, name);
    let mut presentation = (None, false);
    if let Some(underlying) = resolver.underlying_type(ty)? {
        if is_interface(resolver, &underlying)? {
            presentation.0 = Some(Kind::INTERFACE);
        } else if composite_type(resolver, &underlying)? {
            if !matches!(underlying.kind, TypeKind::Container(..)) {
                presentation.0 = Some(Kind::STRUCT);
            }
            presentation.1 = !resolver
                .type_info(package, name)?
                .is_some_and(|(_, details)| !details.parameters.is_empty());
        }
    }
    if let Some(key) = key
        && let Some(read) = &mut resolver.indexes.completion
    {
        read.type_kinds.insert(key, presentation);
    }
    Ok(presentation)
}

fn specialized_signature<'w>(
    resolver: &mut Resolver<'w, '_>,
    from: &'w Package,
    source: &IndexedSource,
    declaration_source: &Source,
    symbol: &Symbol,
    name: &str,
    ty: &TypeIdentity<'w>,
) -> Result<Option<(String, String)>> {
    let TypeKind::Function {
        substitutions,
        method_receiver,
        ..
    } = &ty.kind
    else {
        return Ok(None);
    };
    let Some(function) =
        node_at_range(declaration_source, symbol.range).and_then(|token| token.parent())
    else {
        return Ok(None);
    };
    let Some(parameters) = function.child_by_field_name("parameters") else {
        return Ok(None);
    };
    let mut types = Vec::new();
    for (name, value) in substitutions {
        if let Some(value) = value
            && let Some(label) = resolver.type_text(from, source, value)?
        {
            types.push((name.clone(), label));
        }
    }
    let text = &declaration_source.text;
    let render = |node| syntax_text_with(node, text, &types);
    let mut labels = parameter_labels_with(parameters, text, render);
    if let Some(receiver) = method_receiver
        && let Some(receiver) = resolver.type_text(from, source, receiver)?
    {
        labels.insert(0, receiver);
    }
    let result = result_label_with(function, text, render)
        .map_or_else(String::new, |result| format!(" {result}"));
    let suffix = format!("({}){result}", labels.join(", "));
    Ok(Some((format!("func {name}{suffix}"), suffix)))
}

pub(super) fn composite_type<'w>(
    resolver: &mut Resolver<'w, '_>,
    ty: &TypeIdentity<'w>,
) -> Result<bool> {
    let Some(ty) = resolver.underlying_type(ty.clone())? else {
        return Ok(false);
    };
    if ty.pointers != 0 {
        return Ok(false);
    }
    Ok(match &ty.kind {
        TypeKind::Container(container, _) => {
            !matches!(container.as_ref(), Container::Channel { .. })
        }
        TypeKind::Inline { shape, .. } => !shape.interface,
        TypeKind::Named { .. } => resolver.type_context(&ty)?.is_some_and(|(_, details, _)| {
            !details.interface && details.underlying.is_none() && details.alias.is_none()
        }),
        _ => false,
    })
}

fn nested_literal_name<'w>(
    resolver: &mut Resolver<'w, '_>,
    from: &'w Package,
    source: &IndexedSource,
    ty: &TypeIdentity<'w>,
) -> Result<Option<String>> {
    if ty.pointers != 0 || !matches!(ty.kind, TypeKind::Named { .. }) {
        return Ok(None);
    }
    if !composite_type(resolver, ty)? {
        return Ok(None);
    }
    let mut label = resolver.type_text(from, source, ty)?;
    if from.import_path() != ty.package.import_path() {
        let Some(import) = source.imports.iter().find(|import| {
            resolver
                .workspace
                .resolve_import(from, &import.path)
                .is_some_and(|target| target.import_path() == ty.package.import_path())
        }) else {
            return Ok(None);
        };
        if import.alias.as_deref() == Some(".")
            && let Some(label) = &mut label
        {
            if let Some(unqualified) = label.strip_prefix(&format!("{}.", ty.package.name)) {
                *label = unqualified.to_owned();
            }
        }
    }
    Ok(label)
}

fn keywords<'a>(words: &'a [&'a str], prefix: &'a str) -> impl Iterator<Item = Candidate> + 'a {
    words.iter().filter_map(move |word| {
        let matched = match_start(prefix, word)?;
        Some(Candidate::new(*word, Kind::KEYWORD, matched, KEYWORD))
    })
}

fn labels(source: &Source, anchor: Node<'_>, kind: LabelKind, prefix: &str) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    let mut add = |statement: Node<'_>| {
        if let Some(label) = statement.child_by_field_name("label") {
            let name = node_text(label, &source.text);
            if name != "_"
                && let Some(matched) = match_name(prefix, name)
            {
                candidates.push(Candidate::new(name, Kind::REFERENCE, matched, FIELD));
            }
        }
    };
    for scope in ancestors(anchor) {
        if is_function(scope) {
            break;
        }
        if kind == LabelKind::Goto
            && matches!(
                scope.kind(),
                "block" | "expression_case" | "type_case" | "default_case" | "communication_case"
            )
        {
            for list in scope
                .named_children(&mut scope.walk())
                .filter(|node| node.kind() == "statement_list")
            {
                for mut label in list
                    .named_children(&mut list.walk())
                    .filter(|node| node.kind() == "labeled_statement")
                {
                    loop {
                        add(label);
                        if let Some(next) =
                            labeled_body(label).filter(|node| node.kind() == "labeled_statement")
                        {
                            label = next;
                        } else {
                            break;
                        }
                    }
                }
            }
        } else if kind != LabelKind::Goto
            && scope.kind() == "labeled_statement"
            && let Some(statement) = labeled_body(scope)
            && (statement.kind() == "for_statement"
                || kind == LabelKind::Break
                    && matches!(
                        statement.kind(),
                        "expression_switch_statement"
                            | "type_switch_statement"
                            | "select_statement"
                    ))
        {
            add(scope);
        }
    }
    candidates
}

fn labeled_body(statement: Node<'_>) -> Option<Node<'_>> {
    statement
        .named_children(&mut statement.walk())
        .find(|node| !node.is_extra() && node.kind() != "label_name")
}

fn package_names(
    path: &Path,
    docs: &HashMap<PathBuf, IndexedSource>,
    prefix: &str,
) -> Vec<Candidate> {
    let Some(directory) = path.parent() else {
        return Vec::new();
    };
    let directory_key = path_key(directory);
    let current = path_key(path);
    let test = is_test_source(path);
    let mut names = HashSet::new();
    let acceptable = |sibling: &Path| {
        sibling != current && is_symbol_source(sibling) && (test || !is_test_source(sibling))
    };
    for (sibling, source) in docs {
        if sibling.parent() == Some(&directory_key)
            && acceptable(sibling)
            && let Some(name) = package_name(source)
        {
            names.insert(name.to_owned());
        }
    }
    if let Ok(entries) = fs::read_dir(directory) {
        for entry in entries.flatten() {
            let sibling = entry.path();
            let key = path_key(&sibling);
            if acceptable(&key)
                && !docs.contains_key(&key)
                && let Ok(text) = fs::read_to_string(sibling)
                && let Some(name) = declared_package(&text)
            {
                names.insert(name.to_owned());
            }
        }
    }
    if let Some(name) = directory.file_name().and_then(|name| name.to_str()) {
        let mut name: String = name
            .chars()
            .map(|ch| if identifier_char(ch) { ch } else { '_' })
            .collect();
        if is_keyword(&name) || name.chars().next().is_some_and(|ch| !identifier_start(ch)) {
            name.insert(0, '_');
        }
        if !name.is_empty() && name != "_" {
            names.insert(name);
        }
    }
    if test {
        let tests = names
            .iter()
            .filter(|name| !name.ends_with("_test"))
            .map(|name| format!("{name}_test"))
            .collect::<Vec<_>>();
        names.extend(tests);
    }
    names.insert("main".into());
    names
        .into_iter()
        .filter_map(|name| {
            let matched = match_name(prefix, &name)?;
            let priority = if name == "main" {
                37
            } else if test && name.ends_with("_test") {
                39
            } else {
                38
            };
            Some(Candidate::new(name, Kind::MODULE, matched, priority))
        })
        .collect()
}

fn declared_package(mut text: &str) -> Option<&str> {
    text = text.trim_start_matches('\u{feff}');
    loop {
        text = text.trim_start();
        if let Some(line) = text.strip_prefix("//") {
            text = line.split_once('\n')?.1;
        } else if let Some(block) = text.strip_prefix("/*") {
            text = block.split_once("*/")?.1;
        } else {
            break;
        }
    }
    let rest = text.strip_prefix("package")?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = rest.trim_start();
    let name = &rest[..rest.find(|ch| !identifier_char(ch)).unwrap_or(rest.len())];
    name.chars()
        .next()
        .filter(|ch| identifier_start(*ch))
        .map(|_| name)
}
