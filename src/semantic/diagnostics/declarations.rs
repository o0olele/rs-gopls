use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::*;
use crate::scope::{initializer_at, receiver_type_parameters};
use crate::semantic::completion::render::package_name;
use crate::semantic::index::draft_files;
use crate::semantic::package_index::PackageDeclFile;
use crate::semantic::source::EditorNames;
use crate::symbols::{DiagnosticOutline, OutlineKind};
use crate::workspace::path_key;

#[derive(Clone, Copy)]
pub(super) struct Local<'t> {
    pub(super) name: Node<'t>,
    pub(super) scope_node: usize,
    pub(super) recovered: bool,
    pub(super) kind: &'static str,
    pub(super) explicit_type: Option<Node<'t>>,
    pub(super) initializer: Option<Node<'t>>,
    pub(super) result_index: usize,
    visible_from: usize,
}

impl<'t> Local<'t> {
    fn new(name: Node<'t>, owner: Node<'t>, visible_from: usize) -> Self {
        Self {
            name,
            scope_node: owner.id(),
            recovered: false,
            kind: owner.kind(),
            explicit_type: owner.child_by_field_name("type"),
            initializer: None,
            result_index: 0,
            visible_from,
        }
    }

    pub(super) fn is_type(self) -> bool {
        matches!(
            self.kind,
            "type_spec" | "type_alias" | "type_parameter_declaration" | "type_elem"
        )
    }
}

#[derive(Default)]
pub(super) struct Locals<'t> {
    bindings: HashMap<(usize, &'t str), Local<'t>>,
    scopes: Vec<(usize, Option<usize>)>,
    scope_changes: Vec<(usize, Option<usize>)>,
    current_scope: Option<usize>,
    pub(super) declarations: HashSet<usize>,
    pub(super) variables: Vec<Node<'t>>,
    pub(super) globals: Arc<HashMap<String, OutlineKind>>,
    pub(super) incomplete_package: bool,
    uncertain_bindings: Vec<Bytes<usize>>,
}

impl<'t> Locals<'t> {
    pub(super) fn has_bindings(&self) -> bool {
        !self.bindings.is_empty()
    }

    pub(super) fn declared_in_same_block(
        &self,
        parents: Parents<'_, '_>,
        name: &str,
        previous: Node<'_>,
    ) -> bool {
        block_scope(parents)
            .and_then(|scope| self.bindings.get(&(scope.id(), name)))
            .is_some_and(|b| b.name == previous)
    }

    pub(super) fn lookup(&self, node: Node<'_>, name: &str) -> Option<Local<'t>> {
        if self.bindings.is_empty() {
            return None;
        }
        let offset = node.start_byte();
        let index = self.scope_changes.partition_point(|(at, _)| *at <= offset);
        let mut scope = self.scope_changes.get(index.checked_sub(1)?)?.1;
        while let Some(index) = scope {
            let (id, parent) = self.scopes[index];
            if let Some(binding) = self.bindings.get(&(id, name))
                && binding.visible_from <= offset
            {
                return Some(*binding);
            }
            scope = parent;
        }
        None
    }

    pub(super) fn may_have_recovered_binding(&self, node: Node<'_>) -> bool {
        let offset = node.start_byte();
        let index = self
            .uncertain_bindings
            .partition_point(|range| range.start <= offset);
        index > 0 && self.uncertain_bindings[index - 1].contains(&offset)
    }

    fn record_uncertain_binding(&mut self, range: Bytes<usize>) {
        if let Some(previous) = self.uncertain_bindings.last_mut()
            && previous.end >= range.start
        {
            previous.end = previous.end.max(range.end);
        } else {
            self.uncertain_bindings.push(range);
        }
    }

    fn push_scope(&mut self, node: Node<'_>) {
        let scope = self.scopes.len();
        self.scopes.push((node.id(), self.current_scope));
        self.current_scope = Some(scope);
        self.scope_changes.push((node.start_byte(), Some(scope)));
    }

    fn pop_scope(&mut self, node: Node<'_>) {
        let (_, parent) = self.scopes[self.current_scope.expect("active lexical scope")];
        self.current_scope = parent;
        self.scope_changes.push((node.end_byte(), parent));
    }
}

pub(super) fn collect<'s>(analysis: &mut Analysis<'s, '_, '_>) -> Result<()> {
    package_declarations(analysis)?;
    collect_locals(analysis, WalkMode::Intact)
}

pub(super) fn collect_locals<'s>(
    analysis: &mut Analysis<'s, '_, '_>,
    mode: WalkMode<'_>,
) -> Result<()> {
    let mut lexical = LexicalAnalysis {
        source: analysis.source,
        locals: &mut analysis.locals,
        items: &mut analysis.items,
        cancelled: analysis.cancelled,
    };
    lexical.collect(analysis.source.tree.root_node(), mode, !mode.recovers())
}

pub(super) struct LexicalAnalysis<'s, 'c, 'a> {
    pub(super) source: &'s IndexedSource,
    pub(super) locals: &'a mut Locals<'s>,
    pub(super) items: &'a mut Vec<PendingItem>,
    pub(super) cancelled: Option<&'c AtomicBool>,
}

impl<'s> LexicalAnalysis<'s, '_, '_> {
    fn text(&self, node: Node<'_>) -> &'s str {
        node_text(node, &self.source.text)
    }

    fn error(
        &mut self,
        code: DiagnosticCode,
        node: Node<'_>,
        message: impl Into<String>,
    ) -> &mut PendingItem {
        self.items.push(PendingItem {
            code,
            range: node.byte_range(),
            message: message.into(),
            fix: Fix::None,
        });
        self.items.last_mut().unwrap()
    }

    pub(super) fn collect(
        &mut self,
        root: Node<'s>,
        mode: WalkMode<'_>,
        detailed: bool,
    ) -> Result<()> {
        collect_lexical(self, root, mode, detailed)
    }
}

fn collect_lexical<'s>(
    analysis: &mut LexicalAnalysis<'s, '_, '_>,
    root: Node<'s>,
    mode: WalkMode<'_>,
    detailed: bool,
) -> Result<()> {
    let mut member_names = HashSet::new();
    walk(
        root,
        analysis.cancelled,
        mode,
        |node, kind, parents, entering| {
            if !entering {
                analysis.locals.pop_scope(node);
                return Ok(WalkControl::Children);
            }
            let scoped = scope_node(kind);
            if scoped {
                analysis.locals.push_scope(node);
            }
            match kind {
                "ERROR" => {
                    if let Some(scope) = block_scope(parents)
                        && damaged_binding(node, analysis.cancelled)?
                    {
                        analysis
                            .locals
                            .record_uncertain_binding(node.start_byte()..scope.end_byte());
                    }
                    return Ok(WalkControl::Skip);
                }
                "package_clause" | "import_declaration" => return Ok(WalkControl::Skip),
                "var_spec" | "const_spec" | "type_spec" | "type_alias" => {
                    if let Some(scope) = block_scope(parents) {
                        let mut cursor = node.walk();
                        for (index, name) in node
                            .children_by_field_name("name", &mut cursor)
                            .filter(|name| name.is_named() && !name.is_extra())
                            .enumerate()
                        {
                            if index & 255 == 0 {
                                checkpoint(analysis.cancelled)?;
                            }
                            let start = if matches!(kind, "type_spec" | "type_alias") {
                                name.start_byte()
                            } else {
                                node.end_byte()
                            };
                            let mut local = Local::new(name, node, start);
                            if detailed {
                                (local.initializer, local.result_index) =
                                    initializer_at(node.child_by_field_name("value"), index);
                            }
                            bind(analysis, scope, local, kind == "var_spec", detailed);
                        }
                    }
                }
                "parameter_declaration"
                | "variadic_parameter_declaration"
                | "type_parameter_declaration" => {
                    if let Some(owner) = parents.at(2) {
                        let start = if kind == "type_parameter_declaration" {
                            owner.start_byte()
                        } else {
                            owner
                                .child_by_field_name("body")
                                .map_or(owner.end_byte(), |n| n.start_byte())
                        };
                        let mut cursor = node.walk();
                        for (i, name) in node
                            .children_by_field_name("name", &mut cursor)
                            .filter(|name| name.is_named() && !name.is_extra())
                            .enumerate()
                        {
                            if i & 255 == 0 {
                                checkpoint(analysis.cancelled)?;
                            }
                            bind(
                                analysis,
                                owner,
                                Local::new(name, node, start),
                                false,
                                detailed,
                            );
                        }
                    }
                }
                "method_declaration" => {
                    if let Some((_, arguments)) = receiver_type_parameters(node) {
                        let mut cursor = arguments.walk();
                        for (i, arg) in arguments
                            .named_children(&mut cursor)
                            .filter(|arg| !arg.is_extra())
                            .enumerate()
                        {
                            if i & 255 == 0 {
                                checkpoint(analysis.cancelled)?;
                            }
                            if let Some(name) =
                                arg.named_child(0).filter(|n| n.kind() == "type_identifier")
                            {
                                bind(
                                    analysis,
                                    node,
                                    Local::new(name, arg, node.start_byte()),
                                    false,
                                    detailed,
                                );
                            }
                        }
                    }
                }
                "short_var_declaration" | "receive_statement" | "range_clause" => {
                    if let (Some(operator), Some(left), Some(scope)) = (
                        token(node, ":="),
                        node.child_by_field_name("left"),
                        block_scope(parents),
                    ) {
                        let mut distinct = HashSet::new();
                        let mut new = false;
                        let mut cursor = left.walk();
                        for (index, name) in left
                            .named_children(&mut cursor)
                            .filter(|name| !name.is_extra())
                            .enumerate()
                        {
                            if index & 255 == 0 {
                                checkpoint(analysis.cancelled)?;
                            }
                            let spelling = analysis.text(name);
                            analysis.locals.declarations.insert(name.id());
                            if spelling == "_" {
                                continue;
                            }
                            if !is_identifier(name) {
                                return Ok(WalkControl::Children);
                            }
                            if !distinct.insert(spelling) && detailed {
                                redeclared(analysis, name);
                            } else if !analysis
                                .locals
                                .bindings
                                .contains_key(&(scope.id(), spelling))
                            {
                                let mut local = Local::new(name, node, node.end_byte());
                                if detailed {
                                    (local.initializer, local.result_index) = match kind {
                                        "range_clause" => (Some(node), index),
                                        "receive_statement" => {
                                            (node.child_by_field_name("right"), index)
                                        }
                                        _ => {
                                            initializer_at(node.child_by_field_name("right"), index)
                                        }
                                    };
                                }
                                bind(analysis, scope, local, true, detailed);
                                new = true;
                            }
                        }
                        if !new && detailed {
                            let item = analysis.error(
                                DiagnosticCode::ShortVarNoNewVars,
                                operator,
                                "No new variables on the left of :=",
                            );
                            item.fix = Fix::Assign;
                        }
                    }
                }
                "type_switch_statement" => {
                    if let Some(alias) = node
                        .child_by_field_name("alias")
                        .and_then(|n| n.named_child(0))
                        && analysis.text(alias) != "_"
                    {
                        analysis.locals.declarations.insert(alias.id());
                        if detailed {
                            analysis.locals.variables.push(alias);
                        }
                    }
                }
                "type_case" | "default_case" => {
                    if let Some(switch) = parents
                        .at(1)
                        .filter(|n| n.kind() == "type_switch_statement")
                        && let Some(alias) = switch
                            .child_by_field_name("alias")
                            .and_then(|n| n.named_child(0))
                        && analysis.text(alias) != "_"
                    {
                        let start = children(node)
                            .into_iter()
                            .find(|n| n.kind() == "statement_list")
                            .map_or(node.end_byte(), |n| n.start_byte());
                        let mut local = Local::new(alias, node, start);
                        let mut cursor = node.walk();
                        let mut types = node
                            .children_by_field_name("type", &mut cursor)
                            .filter(Node::is_named);
                        local.explicit_type = types
                            .next()
                            .filter(|n| analysis.text(*n) != "nil" && types.next().is_none());
                        if local.explicit_type.is_none() {
                            local.initializer = switch.child_by_field_name("value");
                        }
                        bind(analysis, node, local, false, detailed);
                    }
                }
                "field_declaration_list" | "interface_type" if detailed => {
                    if member_declarations(analysis, node, kind, &mut member_names)? {
                        return Ok(WalkControl::Skip);
                    }
                }
                _ => {}
            }
            Ok(if scoped {
                WalkControl::Scoped
            } else {
                WalkControl::Children
            })
        },
    )
}

fn damaged_binding(root: Node<'_>, cancelled: Option<&AtomicBool>) -> Result<bool> {
    let mut cursor = root.walk();
    let mut visited = 0_u32;
    loop {
        if visited & 255 == 0 {
            checkpoint(cancelled)?;
        }
        visited = visited.wrapping_add(1);
        if matches!(cursor.node().kind(), ":=" | "var" | "const" | "type") {
            return Ok(true);
        }
        if !cursor.goto_first_child() {
            while !cursor.goto_next_sibling() {
                if !cursor.goto_parent() {
                    return Ok(false);
                }
            }
        }
    }
}

pub(super) fn stage1_summaries(
    snapshot: &EngineSnapshot<'_>,
    document: &DocumentSnapshot<'_>,
    view: &inputs::DiagnosticView<'_>,
    cancelled: Option<&AtomicBool>,
) -> Result<Option<Vec<Arc<EditorNames>>>> {
    let Some(package) = view.package.filter(|package| !package.load_error) else {
        return Ok(None);
    };
    let current = document.indexed();
    if package_name(current) != Some(&package.name)
        || !syntax::has_reliable_package_scope(document.root_node(), cancelled)?
    {
        return Ok(None);
    }
    let current_key = path_key(document.path());
    let drafts = draft_files(view.graph, snapshot.docs(), snapshot.new_documents());
    let mut summaries = vec![current.editor_names.clone()];
    for path in package.files.iter().chain(
        drafts
            .get(&package.directory_and_name())
            .into_iter()
            .flatten()
            .copied(),
    ) {
        checkpoint(cancelled)?;
        let key = path_key(path);
        if key == current_key {
            continue;
        }
        let names = if let Some(sibling) = snapshot.docs().get(&key) {
            if package_name(sibling) != Some(&package.name)
                || !syntax::has_reliable_package_scope(sibling.tree.root_node(), cancelled)?
            {
                return Ok(None);
            }
            sibling.editor_names.clone()
        } else {
            let Some(names) =
                snapshot.diagnostic_cached_package_names(view.graph, &key, &package.name)
            else {
                return Ok(None);
            };
            names
        };
        summaries.push(names);
    }
    Ok(Some(summaries))
}

pub(super) fn check_assignable(analysis: &mut Analysis<'_, '_, '_>, node: Node<'_>) -> Result<()> {
    let operands = match node.kind() {
        "assignment_statement" | "receive_statement" | "range_clause"
            if token(node, ":=").is_none() =>
        {
            node.child_by_field_name("left")
                .map(children)
                .unwrap_or_default()
        }
        "inc_statement" | "dec_statement" => children(node),
        _ => return Ok(()),
    };
    for operand in operands {
        let operand = unparen(operand);
        let name = analysis.text(operand);
        let constant = if is_identifier(operand) {
            if let Some(local) = analysis.locals.lookup(operand, name) {
                local.kind == "const_spec"
            } else if let Some(kind) = analysis.locals.globals.get(name) {
                *kind == OutlineKind::Constant
            } else {
                analysis
                    .resolver
                    .indexes
                    .find_dot_import(
                        analysis.resolver.workspace,
                        analysis.package,
                        &analysis.source.imports,
                        name,
                    )?
                    .and_then(|(path, _)| analysis.resolver.workspace.package_of_file(&path))
                    .map(|pkg| analysis.resolver.find_symbol(pkg, name, None))
                    .transpose()?
                    .flatten()
                    .is_some_and(|s| {
                        matches!(s.symbol.kind, crate::symbols::SymbolKind::Value(_))
                            && !s.symbol.addressable
                    })
            }
        } else if operand.kind() == "selector_expression"
            && let (Some(receiver), Some(field)) = (
                operand.child_by_field_name("operand"),
                operand.child_by_field_name("field"),
            )
            && let Some(package) = analysis.imports.package(analysis.text(receiver))
            && analysis
                .locals
                .lookup(receiver, analysis.text(receiver))
                .is_none()
        {
            analysis
                .resolver
                .find_symbol(package, analysis.text(field), None)?
                .is_some_and(|s| {
                    matches!(s.symbol.kind, crate::symbols::SymbolKind::Value(_))
                        && !s.symbol.addressable
                })
        } else {
            false
        };
        if constant {
            analysis.error(
                DiagnosticCode::ConstReassign,
                operand,
                format!("Cannot assign to constant {name}"),
            );
        }
    }
    Ok(())
}

pub(super) fn is_identifier(node: Node<'_>) -> bool {
    identifier_kind(node.kind())
}

pub(super) fn identifier_kind(kind: &str) -> bool {
    matches!(
        kind,
        "identifier" | "type_identifier" | "package_identifier" | "nil" | "true" | "false" | "iota"
    )
}

fn block_scope<'t>(parents: Parents<'t, '_>) -> Option<Node<'t>> {
    for (index, scope) in parents.iter().rev().enumerate() {
        match scope.kind() {
            "block" => {
                if let Some(function) = parents.at(index + 2).filter(|n| is_function(n.kind())) {
                    return Some(function);
                }
                return Some(scope);
            }
            "if_statement"
            | "for_statement"
            | "expression_switch_statement"
            | "type_switch_statement"
            | "type_case"
            | "expression_case"
            | "default_case"
            | "communication_case" => return Some(scope),
            _ => {}
        }
    }
    None
}

fn scope_node(kind: &str) -> bool {
    is_function(kind)
        || matches!(
            kind,
            "block"
                | "if_statement"
                | "for_statement"
                | "expression_switch_statement"
                | "type_switch_statement"
                | "type_case"
                | "expression_case"
                | "default_case"
                | "communication_case"
                | "type_spec"
                | "type_alias"
                | "function_type"
                | "method_elem"
        )
}

fn bind<'s>(
    analysis: &mut LexicalAnalysis<'s, '_, '_>,
    scope: Node<'s>,
    mut local: Local<'s>,
    variable: bool,
    detailed: bool,
) {
    let name = local.name;
    let spelling = analysis.text(name);
    analysis.locals.declarations.insert(name.id());
    if spelling == "_" {
        return;
    }
    if let std::collections::hash_map::Entry::Vacant(entry) =
        analysis.locals.bindings.entry((scope.id(), spelling))
    {
        local.scope_node = scope.id();
        local.recovered = !detailed;
        entry.insert(local);
        if variable && detailed {
            analysis.locals.variables.push(name);
        }
    } else if detailed {
        redeclared(analysis, name);
    }
}

fn redeclared(analysis: &mut LexicalAnalysis<'_, '_, '_>, name: Node<'_>) {
    analysis.error(
        DiagnosticCode::NameRedeclared,
        name,
        format!("{} redeclared in this scope", analysis.text(name)),
    );
}

// A flat field list contains no declarations outside the fields themselves.
fn member_declarations<'s>(
    analysis: &mut LexicalAnalysis<'s, '_, '_>,
    list: Node<'_>,
    kind: &str,
    seen: &mut HashSet<&'s str>,
) -> Result<bool> {
    seen.clear();
    let mut flat = kind == "field_declaration_list";
    let mut cursor = list.walk();
    let mut field_cursor = list.walk();
    for (i, field) in list
        .named_children(&mut cursor)
        .filter(|n| !n.is_extra())
        .enumerate()
    {
        if i & 255 == 0 {
            checkpoint(analysis.cancelled)?;
        }
        let ty = field.child_by_field_name("type");
        flat &= ty.is_some_and(|ty| matches!(ty.kind(), "type_identifier" | "qualified_type"));
        let mut names = field
            .children_by_field_name("name", &mut field_cursor)
            .filter(|name| name.is_named() && !name.is_extra())
            .peekable();
        let embedded = if names.peek().is_none() && field.kind() == "field_declaration" {
            ty.map(|ty| {
                let base = if ty.kind() == "generic_type" {
                    ty.child_by_field_name("type").unwrap_or(ty)
                } else {
                    ty
                };
                base.child_by_field_name("name").unwrap_or(base)
            })
        } else {
            None
        };
        for name in names.chain(embedded) {
            let spelling = analysis.text(name);
            if spelling != "_" && !seen.insert(spelling) {
                redeclared(analysis, name);
            }
        }
    }
    Ok(flat)
}

pub(in crate::semantic) struct DiagnosticPackageNames {
    globals: Arc<HashMap<String, OutlineKind>>,
    first_globals: HashMap<String, usize>,
    first_methods: HashMap<String, HashMap<String, usize>>,
    direct_fields: HashMap<String, HashSet<String>>,
    paths: Vec<PathBuf>,
    incomplete: bool,
}

impl DiagnosticPackageNames {
    pub(in crate::semantic) fn collect(
        files: &[PackageDeclFile],
        package_name: &str,
        cancelled: &AtomicBool,
    ) -> Result<Self> {
        let mut files: Vec<_> = files.iter().collect();
        files.sort_by(|left, right| left.key.cmp(&right.key));
        let mut globals = HashMap::new();
        let mut first_globals = HashMap::new();
        let mut first_methods = HashMap::<String, HashMap<String, usize>>::new();
        let mut direct_fields = HashMap::<String, HashSet<String>>::new();
        let mut paths = Vec::with_capacity(files.len());
        let mut incomplete = false;
        for file in files {
            checkpoint(Some(cancelled))?;
            let outline = &file.declarations.outline;
            if !file.declarations.reliable_declarations
                || outline.package.as_deref() != Some(package_name)
            {
                incomplete = true;
                continue;
            }
            let index = paths.len();
            paths.push(file.key.clone());
            for (i, symbol) in outline.declarations.iter().enumerate() {
                if i & 255 == 0 {
                    checkpoint(Some(cancelled))?;
                }
                if let Some(receiver) = &symbol.receiver {
                    first_methods
                        .entry(receiver.clone())
                        .or_default()
                        .entry(symbol.name.clone())
                        .or_insert(index);
                } else {
                    globals.insert(symbol.name.clone(), symbol.kind);
                    first_globals.entry(symbol.name.clone()).or_insert(index);
                }
                if !symbol.direct_fields.is_empty() {
                    let fields = direct_fields.entry(symbol.name.clone()).or_default();
                    for (i, field) in symbol.direct_fields.iter().enumerate() {
                        if i & 255 == 0 {
                            checkpoint(Some(cancelled))?;
                        }
                        fields.insert(field.clone());
                    }
                }
            }
        }
        Ok(Self {
            globals: Arc::new(globals),
            first_globals,
            first_methods,
            direct_fields,
            paths,
            incomplete,
        })
    }

    fn first_declaration_file(&self, receiver: Option<&str>, name: &str) -> Option<&Path> {
        let names = match receiver {
            Some(receiver) => self.first_methods.get(receiver)?,
            None => &self.first_globals,
        };
        Some(&self.paths[*names.get(name)?])
    }

    fn has_direct_field(&self, receiver: &str, name: &str) -> bool {
        self.direct_fields
            .get(receiver)
            .is_some_and(|fields| fields.contains(name))
    }

    pub(in crate::semantic) fn cache_weight(&self) -> usize {
        fn map_weight<V>(map: &HashMap<String, V>) -> usize {
            map.capacity().saturating_mul(2) * (size_of::<(String, V)>() + 1)
                + map.keys().map(String::capacity).sum::<usize>()
        }

        2 * size_of::<usize>()
            + size_of::<HashMap<String, OutlineKind>>()
            + map_weight(&self.globals)
            + map_weight(&self.first_globals)
            + map_weight(&self.first_methods)
            + self.first_methods.values().map(map_weight).sum::<usize>()
            + map_weight(&self.direct_fields)
            + self
                .direct_fields
                .values()
                .map(|fields| {
                    fields.capacity().saturating_mul(2) * (size_of::<String>() + 1)
                        + fields.iter().map(String::capacity).sum::<usize>()
                })
                .sum::<usize>()
            + self.paths.capacity() * size_of::<PathBuf>()
            + self.paths.iter().map(PathBuf::capacity).sum::<usize>()
    }
}

fn package_declarations(analysis: &mut Analysis<'_, '_, '_>) -> Result<()> {
    #[cfg(test)]
    if !crate::semantic::package_index::enabled() {
        let outline = crate::symbols::collect_diagnostic_outline(
            &analysis.source.tree,
            &analysis.source.text,
            analysis.cancelled,
        )?;
        return package_declarations_linear(analysis, &outline);
    }

    let current = analysis
        .resolver
        .indexes
        .indexed_declarations(analysis.source)?;
    let Some(index) = analysis
        .resolver
        .indexes
        .prepare_package_index(analysis.package)?
    else {
        return package_declarations_linear(analysis, &current.outline);
    };

    let key = path_key(&analysis.source.path);
    let names = &index.diagnostics;
    analysis.locals.globals = names.globals.clone();
    analysis.locals.incomplete_package = names.incomplete;
    let mut seen = HashSet::new();
    for (i, symbol) in current.outline.declarations.iter().enumerate() {
        if i & 255 == 0 {
            checkpoint(analysis.cancelled)?;
        }
        let name = symbol.name.as_str();
        if name == "_" || (name == "init" && symbol.kind == OutlineKind::Function) {
            continue;
        }
        let receiver = symbol.receiver.as_deref();
        if receiver.is_some_and(|owner| names.has_direct_field(owner, name))
            || names
                .first_declaration_file(receiver, name)
                .is_some_and(|first| first < key.as_path())
            || !seen.insert((receiver, name))
        {
            analysis.error_bytes(
                DiagnosticCode::NameRedeclared,
                symbol.selection_range.clone(),
                format!("{name} redeclared in this package"),
            );
        }
    }
    Ok(())
}

fn package_declarations_linear(
    analysis: &mut Analysis<'_, '_, '_>,
    outline: &DiagnosticOutline,
) -> Result<()> {
    let key = path_key(&analysis.source.path);
    let mut seen = HashSet::new();
    let mut fields = HashSet::new();
    let mut methods = HashMap::<&str, HashSet<&str>>::new();
    let mut globals = HashMap::new();
    for (i, declaration) in outline.declarations.iter().enumerate() {
        if i & 255 == 0 {
            checkpoint(analysis.cancelled)?;
        }
        if let Some(receiver) = declaration.receiver.as_deref() {
            methods
                .entry(receiver)
                .or_default()
                .insert(&declaration.name);
        }
    }
    let mut files = analysis
        .resolver
        .indexes
        .files(analysis.package)
        .into_owned();
    files.sort_by_cached_key(|path| path_key(path));
    for file in &files {
        checkpoint(analysis.cancelled)?;
        let path = path_key(file);
        let sibling;
        let file_outline = if path == key {
            outline
        } else {
            let Some(declarations) = analysis.resolver.indexes.declaration_source(file)? else {
                analysis.locals.incomplete_package = true;
                continue;
            };
            if !declarations.reliable_declarations {
                analysis.locals.incomplete_package = true;
                continue;
            }
            sibling = declarations;
            &sibling.outline
        };
        if file_outline.package.as_deref() != Some(analysis.package.name.as_str()) {
            analysis.locals.incomplete_package = true;
            continue;
        }
        for (i, symbol) in file_outline.declarations.iter().enumerate() {
            if i & 255 == 0 {
                checkpoint(analysis.cancelled)?;
            }
            let owner = symbol.receiver.clone();
            if owner.is_none() {
                globals.insert(symbol.name.clone(), symbol.kind);
            }
            if path < key {
                seen.insert((owner, symbol.name.clone()));
            }
            if let Some(names) = methods.get(symbol.name.as_str()) {
                for (i, field) in symbol.direct_fields.iter().enumerate() {
                    if i & 255 == 0 {
                        checkpoint(analysis.cancelled)?;
                    }
                    if names.contains(field.as_str()) {
                        fields.insert((Some(symbol.name.clone()), field.clone()));
                    }
                }
            }
        }
    }
    analysis.locals.globals = Arc::new(globals);
    for (i, symbol) in outline.declarations.iter().enumerate() {
        if i & 255 == 0 {
            checkpoint(analysis.cancelled)?;
        }
        if symbol.name == "_" || (symbol.name == "init" && symbol.kind == OutlineKind::Function) {
            continue;
        }
        let member = (symbol.receiver.clone(), symbol.name.clone());
        if fields.contains(&member) || !seen.insert(member) {
            analysis.error_bytes(
                DiagnosticCode::NameRedeclared,
                symbol.selection_range.clone(),
                format!("{} redeclared in this package", symbol.name),
            );
        }
    }
    Ok(())
}
