use super::super::*;

type MethodLocation = (PathBuf, Range);

/// The declaration selected by a receiver's method set and, for an interface,
/// the full requirements against which a potential implementer must be tested.
pub(in crate::semantic) struct RenameMethod {
    pub(in crate::semantic) location: MethodLocation,
    pub(in crate::semantic) is_predeclared_error: bool,
    key: MethodKey,
    required: Option<InterfaceMethods>,
}

impl RenameMethod {
    pub(in crate::semantic) fn is_interface(&self) -> bool {
        self.required.is_some()
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(super) struct MethodKey {
    name: String,
    package: Option<String>,
}

impl MethodKey {
    pub(super) fn new(name: &str, package: &Package) -> Self {
        Self {
            name: name.to_owned(),
            package: (!is_exported(name)).then(|| package.private_name_owner().to_owned()),
        }
    }

    pub(super) fn is_exported(&self) -> bool {
        self.package.is_none()
    }

    pub(super) fn name(&self) -> &str {
        &self.name
    }

    fn accessible_from(&self, package: &Package) -> bool {
        self.package
            .as_deref()
            .is_none_or(|owner| owner == package.private_name_owner())
    }
}

struct Method {
    signature: Signature,
    location: MethodLocation,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(super) struct MethodShape {
    pub(super) name: String,
    pub(super) parameters: usize,
    pub(super) results: usize,
    pub(super) variadic: bool,
}

impl MethodShape {
    fn from_symbol(name: &str, member: &Symbol) -> Option<Self> {
        let SymbolKind::Function(signature) = &member.kind else {
            return None;
        };
        Some(Self {
            name: name.to_owned(),
            parameters: signature.parameters.len(),
            results: signature.results.len(),
            variadic: signature.variadic,
        })
    }
}

#[derive(Default)]
pub(super) struct InterfaceMethods {
    methods: BTreeMap<MethodKey, Method>,
    parameter_bounds: Vec<Vec<BoundTerm>>,
    has_type_terms: bool,
    complete: bool,
}

impl InterfaceMethods {
    fn insert(&mut self, key: MethodKey, method: Method) {
        match self.methods.entry(key) {
            std::collections::btree_map::Entry::Occupied(previous) => {
                self.complete &= previous.get().signature == method.signature;
            }
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(method);
            }
        }
    }

    pub(super) fn method_names(&self) -> impl Iterator<Item = &str> {
        self.methods.keys().map(|key| key.name.as_str())
    }

    pub(super) fn method_shapes(&self) -> impl Iterator<Item = MethodShape> + '_ {
        self.methods.iter().map(|(key, method)| MethodShape {
            name: key.name.clone(),
            parameters: method.signature.parameters.len(),
            results: method.signature.results.len(),
            variadic: method.signature.variadic,
        })
    }

    pub(super) fn location(&self, key: &MethodKey) -> Option<MethodLocation> {
        self.methods.get(key).map(|method| method.location.clone())
    }

    pub(super) fn has_predeclared_error_method(&self) -> bool {
        self.complete
            && self
                .methods
                .iter()
                .any(|(key, method)| key.name == "Error" && method.signature.is_predeclared_error())
    }
}

pub(super) struct MatchingMethods {
    pub(super) location: Option<MethodLocation>,
}

enum InterfaceContext<'w> {
    Declared(DeclaredInterface<'w>),
    PredeclaredError(&'w Package),
}

struct DeclaredInterface<'w> {
    ty: TypeIdentity<'w>,
    file: PathBuf,
    details: TypeDetails,
    substitutions: Substitutions<'w>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MethodQuery {
    Navigation,
    Hierarchy,
    Rename,
}

// Call after type lookup has excluded a package declaration with this name.
fn unresolved_error<'w>(ty: &TypeIdentity<'w>, query: MethodQuery) -> Option<&'w Package> {
    (query == MethodQuery::Hierarchy
        && matches!(&ty.kind, TypeKind::Named { name, arguments } if name == "error" && arguments.is_empty()))
    .then_some(ty.package)
}

impl<'w> Resolver<'w, '_> {
    pub(in crate::semantic) fn rename_method(
        &mut self,
        receiver: &TypeIdentity<'w>,
        name: &str,
        declaring_package: &Package,
    ) -> Result<Option<RenameMethod>> {
        let key = MethodKey::new(name, declaring_package);
        let receiver = self.with_receiver_parameters(receiver.clone())?;
        let required = self.interface_methods(&receiver)?;
        let concrete;
        let method = if let Some(interface) = &required {
            let method = interface.methods.get(&key);
            if method.is_some() || self.direct_member(&receiver, name, false)?.is_some() {
                anyhow::ensure!(
                    interface.complete && !interface.has_type_terms,
                    "incomplete interface method set for {name}"
                );
            }
            method
        } else {
            concrete = self.concrete_method(&receiver, &key, true, MethodQuery::Rename)?;
            concrete.as_ref()
        };
        let Some(method) = method else {
            return Ok(None);
        };
        let is_predeclared_error = name == "Error" && method.signature.is_predeclared_error();
        Ok(Some(RenameMethod {
            location: method.location.clone(),
            is_predeclared_error,
            key,
            required,
        }))
    }

    pub(in crate::semantic) fn rename_satisfies(
        &mut self,
        interface: &RenameMethod,
        candidate: &TypeIdentity<'w>,
        pointer: bool,
    ) -> Result<bool> {
        let Some(required) = interface.required.as_ref() else {
            return Ok(false);
        };
        let mut candidate = candidate.clone();
        candidate.pointers = u8::from(pointer);
        Ok(self
            .match_method_set(
                required,
                &candidate,
                Some(&interface.key),
                pointer,
                MethodQuery::Rename,
            )?
            .is_some())
    }

    pub(in crate::semantic) fn rename_value_has_method(
        &mut self,
        ty: &TypeIdentity<'w>,
        method: &RenameMethod,
    ) -> Result<bool> {
        if method.is_interface() {
            return Ok(true);
        }
        let ty = self.with_receiver_parameters(ty.clone())?;
        Ok(self
            .concrete_method(&ty, &method.key, false, MethodQuery::Rename)?
            .is_some())
    }

    pub(super) fn concrete_method_shapes(
        &mut self,
        ty: &TypeIdentity<'w>,
    ) -> Result<Option<HashSet<MethodShape>>> {
        let ty = self.with_receiver_parameters(ty.clone())?;
        let mut pending = vec![(ty, Vec::new())];
        let mut shapes = HashSet::new();
        let mut visited_owners = 0;
        while let Some((owner, mut ancestors)) = pending.pop() {
            let Some(key) = owner.key() else {
                continue;
            };
            visited_owners += 1;
            if visited_owners > MAX_TYPE_DEPTH * MAX_TYPE_DEPTH
                || ancestors.len() >= MAX_TYPE_DEPTH
                || ancestors.contains(&key)
            {
                return Ok(None);
            }
            ancestors.push(key);

            // A prefilter only needs a superset. Keep both receiver kinds and
            // defer shadowing, ambiguity and private method identity to matching.
            match &owner.kind {
                TypeKind::Named { name, .. } => {
                    for path in self.indexes.files(owner.package).iter() {
                        let index = self.indexes.file_index(path)?;
                        shapes.extend(
                            index.symbols.members_of(name).filter_map(|(name, member)| {
                                MethodShape::from_symbol(name, member)
                            }),
                        );
                    }
                }
                TypeKind::Inline { shape, .. } => {
                    shapes.extend(
                        shape
                            .members
                            .iter()
                            .filter_map(|(name, member)| MethodShape::from_symbol(name, member)),
                    );
                }
                _ => {}
            }
            let Some((file, details, substitutions)) = self.type_context(&owner)? else {
                continue;
            };
            let bases = details
                .alias
                .or(details.underlying)
                .into_iter()
                .chain(details.embedded);
            for base in bases {
                let Some(base) =
                    self.implementation_type(owner.package, &file, base, &substitutions)?
                else {
                    return Ok(None);
                };
                pending.push((base, ancestors.clone()));
            }
        }
        Ok(Some(shapes))
    }

    pub(super) fn interface_methods(
        &mut self,
        ty: &TypeIdentity<'w>,
    ) -> Result<Option<InterfaceMethods>> {
        self.interface_methods_for(ty, MethodQuery::Navigation)
    }

    pub(super) fn hierarchy_interface_methods(
        &mut self,
        ty: &TypeIdentity<'w>,
    ) -> Result<Option<InterfaceMethods>> {
        self.interface_methods_for(ty, MethodQuery::Hierarchy)
    }

    fn interface_methods_for(
        &mut self,
        ty: &TypeIdentity<'w>,
        query: MethodQuery,
    ) -> Result<Option<InterfaceMethods>> {
        let ty = self.with_receiver_parameters(ty.clone())?;
        let Some(mut result) = self.interface_methods_at(ty.clone(), 0, query)? else {
            return Ok(None);
        };
        result.parameter_bounds = self.parameter_bounds(&ty)?;
        Ok(Some(result))
    }

    fn interface_methods_at(
        &mut self,
        ty: TypeIdentity<'w>,
        depth: usize,
        query: MethodQuery,
    ) -> Result<Option<InterfaceMethods>> {
        let Some(context) = self.interface_context(ty, query)? else {
            return Ok(None);
        };
        let mut result = InterfaceMethods {
            complete: true,
            ..InterfaceMethods::default()
        };
        self.collect_interface_methods(context, &mut result, &mut Vec::new(), depth, query)?;
        Ok(Some(result))
    }

    fn with_receiver_parameters(&mut self, mut ty: TypeIdentity<'w>) -> Result<TypeIdentity<'w>> {
        if let TypeKind::Named { name, arguments } = &mut ty.kind
            && arguments.is_empty()
            && let Some((_, details)) = self.type_info(ty.package, name)?
        {
            arguments.extend(details.parameters.iter().enumerate().map(|(index, _)| {
                Some(TypeIdentity::new(ty.package, TypeKind::Parameter(index), 0))
            }));
        }
        Ok(ty)
    }

    fn interface_context(
        &mut self,
        mut ty: TypeIdentity<'w>,
        query: MethodQuery,
    ) -> Result<Option<InterfaceContext<'w>>> {
        for _ in 0..MAX_TYPE_DEPTH {
            if ty.pointers != 0 {
                return Ok(None);
            }
            let Some((file, details, substitutions)) = self.type_context(&ty)? else {
                return Ok(unresolved_error(&ty, query).map(InterfaceContext::PredeclaredError));
            };
            if details.interface {
                return Ok(Some(InterfaceContext::Declared(DeclaredInterface {
                    ty,
                    file,
                    details,
                    substitutions,
                })));
            }
            let Some(base) = details.alias.or(details.underlying) else {
                return Ok(None);
            };
            let Some(base) = self.implementation_type(ty.package, &file, base, &substitutions)?
            else {
                return Ok(None);
            };
            ty = base;
        }
        Ok(None)
    }

    fn collect_interface_methods(
        &mut self,
        context: InterfaceContext<'w>,
        result: &mut InterfaceMethods,
        visiting: &mut Vec<TypeKey>,
        depth: usize,
        query: MethodQuery,
    ) -> Result<()> {
        if depth >= MAX_TYPE_DEPTH {
            result.complete = false;
            return Ok(());
        }
        let context = match context {
            InterfaceContext::Declared(context) => context,
            InterfaceContext::PredeclaredError(package) => {
                result.insert(
                    MethodKey::new("Error", package),
                    self.builtin_error_method(package)?,
                );
                return Ok(());
            }
        };
        let Some(key) = context.ty.key().filter(|key| !visiting.contains(key)) else {
            result.complete = false;
            return Ok(());
        };
        visiting.push(key);
        result.has_type_terms |= context.details.has_type_terms;
        if let TypeKind::Inline { shape, .. } = &context.ty.kind {
            result.has_type_terms |= shape.has_type_terms;
        }
        for name in context.details.method_names {
            let method = self.direct_member(&context.ty, &name, false)?;
            let Some(method) = self.normalized_method(method, &context.ty, depth + 1)? else {
                result.complete = false;
                continue;
            };
            result.insert(MethodKey::new(&name, context.ty.package), method);
        }
        for embedded in context.details.embedded {
            let Some(embedded) = self.implementation_type(
                context.ty.package,
                &context.file,
                embedded,
                &context.substitutions,
            )?
            else {
                result.complete = false;
                continue;
            };
            if let Some(context) = self.interface_context(embedded.clone(), query)? {
                self.collect_interface_methods(context, result, visiting, depth + 1, query)?;
            } else if !self.is_predeclared_named(&embedded, "any")? {
                result.has_type_terms = true;
            }
        }
        visiting.pop();
        Ok(())
    }

    fn builtin_error_method(&mut self, package: &Package) -> Result<Method> {
        let file = self
            .workspace
            .builtin_source(package.directory_and_name().0)?;
        let member = self
            .indexes
            .file_index(file)?
            .symbols
            .member("error", "Error")
            .context("expected Error method in Go builtin source")?;
        Ok(Method {
            signature: Signature {
                parameters: Vec::new(),
                results: vec![TypeShape::Builtin("string".to_owned())],
                variadic: false,
            },
            location: (file.to_owned(), member.range),
        })
    }

    pub(super) fn satisfies_interface(
        &mut self,
        required: &InterfaceMethods,
        candidate: &TypeIdentity<'w>,
        selected: Option<&MethodKey>,
        allow_pointer: bool,
    ) -> Result<Option<MatchingMethods>> {
        self.match_method_set(
            required,
            candidate,
            selected,
            allow_pointer,
            MethodQuery::Navigation,
        )
    }

    pub(super) fn satisfies_hierarchy_interface(
        &mut self,
        required: &InterfaceMethods,
        candidate: &TypeIdentity<'w>,
        allow_pointer: bool,
    ) -> Result<bool> {
        Ok(self
            .match_method_set(
                required,
                candidate,
                None,
                allow_pointer,
                MethodQuery::Hierarchy,
            )?
            .is_some())
    }

    fn match_method_set(
        &mut self,
        required: &InterfaceMethods,
        candidate: &TypeIdentity<'w>,
        selected: Option<&MethodKey>,
        allow_pointer: bool,
        query: MethodQuery,
    ) -> Result<Option<MatchingMethods>> {
        if !required.complete
            || required.methods.is_empty()
            || selected.is_some_and(|key| !required.methods.contains_key(key))
        {
            return Ok(None);
        }
        let candidate = self.with_receiver_parameters(candidate.clone())?;
        let candidate_interface = self.interface_methods_at(candidate.clone(), 0, query)?;
        if candidate_interface
            .as_ref()
            .is_some_and(|interface| !interface.complete)
        {
            return Ok(None);
        }
        let mut matcher = SignatureMatcher::default();
        let mut location = None;
        for (key, required) in &required.methods {
            let owned;
            let method = match &candidate_interface {
                Some(interface) => interface.methods.get(key),
                None => {
                    owned = self.concrete_method(&candidate, key, allow_pointer, query)?;
                    owned.as_ref()
                }
            };
            let Some(method) = method else {
                return Ok(None);
            };
            if !matcher.signature(&required.signature, false, &method.signature, true, 0) {
                return Ok(None);
            }
            if selected == Some(key) {
                location = Some(method.location.clone());
            }
        }
        if !matcher.bindings.is_empty() {
            let candidate_bounds = self.parameter_bounds(&candidate)?;
            if !self.valid_bindings(&matcher, &required.parameter_bounds, &candidate_bounds)? {
                return Ok(None);
            }
        }
        Ok(Some(MatchingMethods { location }))
    }

    fn parameter_bounds(&mut self, ty: &TypeIdentity<'w>) -> Result<Vec<Vec<BoundTerm>>> {
        let Some((file, details, substitutions)) = self.type_context(ty)? else {
            return Ok(Vec::new());
        };
        let mut parameters = Vec::with_capacity(details.parameters.len());
        for parameter in details.parameters {
            let mut bounds = Vec::new();
            for term in parameter.constraint.terms {
                let Some(term_ty) =
                    self.implementation_type(ty.package, &file, term.reference, &substitutions)?
                else {
                    bounds.clear();
                    break;
                };
                // A named interface here supplies constraints of its own.
                // Simple terms describe the permitted substituted types.
                if self
                    .interface_context(term_ty.clone(), MethodQuery::Navigation)?
                    .is_some()
                    || self.is_predeclared_named(&term_ty, "any")?
                    || self.is_predeclared_named(&term_ty, "comparable")?
                    || self.is_predeclared_named(&term_ty, "error")?
                {
                    bounds.clear();
                    break;
                }
                let Some(shape) = self.canonical_type(term_ty, ty.package, &file, 0)? else {
                    bounds.clear();
                    break;
                };
                bounds.push(BoundTerm {
                    shape,
                    approximate: term.approximate,
                });
            }
            parameters.push(bounds);
        }
        Ok(parameters)
    }

    fn valid_bindings(
        &mut self,
        matcher: &SignatureMatcher,
        required: &[Vec<BoundTerm>],
        candidate: &[Vec<BoundTerm>],
    ) -> Result<bool> {
        let parameters = [required, candidate];
        let mut unresolved: HashMap<(bool, usize), Vec<&[BoundTerm]>> = HashMap::new();
        for (side, parameters) in parameters.into_iter().enumerate() {
            for (index, bounds) in parameters
                .iter()
                .enumerate()
                .filter(|(_, bounds)| !bounds.is_empty())
            {
                match matcher.parameter_binding(side != 0, index) {
                    Some((side, TypeShape::Parameter(index))) => {
                        unresolved.entry((side, *index)).or_default().push(bounds);
                    }
                    Some((_, shape)) if !self.bounds_accept(bounds, shape)? => return Ok(false),
                    Some(_) => {}
                    None => {
                        unresolved
                            .entry((side != 0, index))
                            .or_default()
                            .push(bounds);
                    }
                }
            }
        }
        for lists in unresolved.values().filter(|lists| lists.len() > 1) {
            let mut compatible = false;
            for term in lists.iter().flat_map(|terms| terms.iter()) {
                let mut accepted = true;
                for terms in lists {
                    if !self.bounds_accept(terms, &term.shape)? {
                        accepted = false;
                        break;
                    }
                }
                if accepted {
                    compatible = true;
                    break;
                }
            }
            if !compatible {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn bounds_accept(&mut self, bounds: &[BoundTerm], shape: &TypeShape) -> Result<bool> {
        if bounds.iter().any(|term| term.shape == *shape) {
            return Ok(true);
        }
        let TypeShape::Named(file, _, name, args) = shape else {
            return Ok(false);
        };
        if !args.is_empty() || !bounds.iter().any(|term| term.approximate) {
            return Ok(false);
        }
        let Some(package) = self.workspace.package_of_file(file) else {
            return Ok(false);
        };
        let Some(base) = self.underlying_type(TypeIdentity::named(package, name))? else {
            return Ok(false);
        };
        let Some(base) = self.canonical_type(base, package, file, 0)? else {
            return Ok(false);
        };
        Ok(bounds
            .iter()
            .any(|term| term.approximate && term.shape == base))
    }

    fn concrete_method(
        &mut self,
        candidate: &TypeIdentity<'w>,
        key: &MethodKey,
        allow_pointer: bool,
        query: MethodQuery,
    ) -> Result<Option<Method>> {
        if candidate.pointers > 1 {
            return Ok(None);
        }
        let mut root = candidate.clone();
        root.pointers = 0;
        let mut frontier = vec![(root, allow_pointer || candidate.pointers == 1, Vec::new())];
        for depth in 0..MAX_TYPE_DEPTH {
            let mut next = Vec::new();
            let mut found = None;
            for (mut owner, mut indirect, mut visited) in frontier {
                let mut inherited = false;
                while let Some(owner_key) = owner.key() {
                    if visited.contains(&owner_key) || visited.len() >= MAX_TYPE_DEPTH {
                        anyhow::ensure!(
                            query != MethodQuery::Rename || visited.len() < MAX_TYPE_DEPTH,
                            "method {} exceeds the supported embedding depth",
                            key.name
                        );
                        break;
                    }
                    visited.push(owner_key);
                    let Some((file, details, substitutions)) = self.type_context(&owner)? else {
                        if key.name == "Error"
                            && let Some(package) = unresolved_error(&owner, query)
                        {
                            if found.is_some() || depth == 0 && candidate.pointers != 0 {
                                return Ok(None);
                            }
                            found = Some(self.builtin_error_method(package)?);
                        }
                        if query == MethodQuery::Rename
                            && let TypeKind::Named { name, .. } = &owner.kind
                            && !is_predeclared_type(name)
                        {
                            anyhow::bail!(
                                "cannot verify embedded type {name} when resolving method {}",
                                key.name
                            );
                        }
                        break;
                    };

                    if details.interface {
                        // A pointer to an interface has no method set. A value
                        // embedding an interface promotes its full method set.
                        if depth == 0 && candidate.pointers != 0 {
                            return Ok(None);
                        }
                        let context = InterfaceContext::Declared(DeclaredInterface {
                            ty: owner,
                            file,
                            details,
                            substitutions,
                        });
                        let mut methods = InterfaceMethods {
                            complete: true,
                            ..InterfaceMethods::default()
                        };
                        self.collect_interface_methods(
                            context,
                            &mut methods,
                            &mut Vec::new(),
                            0,
                            query,
                        )?;
                        anyhow::ensure!(
                            query != MethodQuery::Rename || methods.complete,
                            "incomplete embedded interface when resolving method {}",
                            key.name
                        );
                        if methods.complete
                            && let Some(method) = methods.methods.remove(key)
                        {
                            if found.is_some() {
                                return Ok(None);
                            }
                            found = Some(method);
                        }
                        break;
                    }

                    if query == MethodQuery::Rename
                        && !inherited
                        && key.accessible_from(owner.package)
                        && let TypeKind::Named { name, .. } = &owner.kind
                    {
                        self.check_alias_receivers(&owner, name, key)?;
                    }
                    if key.accessible_from(owner.package)
                        && let Some(symbol) = self.direct_member(&owner, &key.name, inherited)?
                    {
                        anyhow::ensure!(
                            query != MethodQuery::Rename || details.alias.is_none(),
                            "cannot verify method {} declared on a receiver alias",
                            key.name
                        );
                        if found.is_some() || (symbol.symbol.pointer_receiver && !indirect) {
                            return Ok(None);
                        }
                        // Fields block promoted methods at this depth as well.
                        let is_method = matches!(symbol.symbol.kind, SymbolKind::Function(_));
                        let Some(method) = self.normalized_method(Some(symbol), &owner, 0)? else {
                            anyhow::ensure!(
                                query != MethodQuery::Rename || !is_method,
                                "unable to verify the signature of method {}",
                                key.name
                            );
                            return Ok(None);
                        };
                        found = Some(method);
                        break;
                    }

                    if let Some(base) = details.alias.or_else(|| {
                        inherited = true;
                        details.underlying
                    }) {
                        let Some(mut base) =
                            self.implementation_type(owner.package, &file, base, &substitutions)?
                        else {
                            anyhow::ensure!(
                                query != MethodQuery::Rename,
                                "cannot resolve underlying type when checking method {}",
                                key.name
                            );
                            break;
                        };
                        if base.pointers > 1 || (inherited && base.pointers != 0) {
                            break;
                        }
                        indirect |= base.pointers == 1;
                        base.pointers = 0;
                        owner = base;
                        continue;
                    }
                    for embedded in details.embedded {
                        let Some(mut ty) = self.implementation_type(
                            owner.package,
                            &file,
                            embedded,
                            &substitutions,
                        )?
                        else {
                            anyhow::ensure!(
                                query != MethodQuery::Rename,
                                "cannot resolve embedded type when checking method {}",
                                key.name
                            );
                            continue;
                        };
                        let implicit_pointer = indirect || ty.pointers == 1;
                        if ty.pointers <= 1 {
                            ty.pointers = 0;
                            next.push((ty, implicit_pointer, visited.clone()));
                        }
                    }
                    break;
                }
            }
            if found.is_some() || next.is_empty() {
                return Ok(found);
            }
            frontier = next;
        }
        anyhow::ensure!(
            query != MethodQuery::Rename,
            "method {} exceeds the supported embedding depth",
            key.name
        );
        Ok(None)
    }

    // Go permits methods to be attached through a local non-generic alias.
    // The declaration index groups them under the written receiver name.
    // A lookup on the underlying type must not treat such a method as absent.
    fn check_alias_receivers(
        &mut self,
        receiver: &TypeIdentity<'w>,
        name: &str,
        key: &MethodKey,
    ) -> Result<()> {
        let mut owners = HashSet::new();
        for file in self.indexes.files(receiver.package).iter() {
            let symbols = &self.indexes.file_index(file)?.symbols;
            owners.extend(
                symbols
                    .member_owners(&key.name)
                    .filter(|other| {
                        *other != name
                            && symbols
                                .member(other, &key.name)
                                .is_some_and(|s| matches!(s.kind, SymbolKind::Function(_)))
                    })
                    .map(str::to_owned),
            );
        }
        for alias in owners {
            if self
                .type_info(receiver.package, &alias)?
                .is_some_and(|(_, details)| details.alias.is_some())
            {
                let base = self.resolve_alias(TypeIdentity::named(receiver.package, &alias))?;
                anyhow::ensure!(
                    base.as_ref()
                        .is_some_and(|base| base.key() != receiver.key()),
                    "cannot verify method {} declared through receiver alias {alias}",
                    key.name
                );
            }
        }
        Ok(())
    }

    fn normalized_method(
        &mut self,
        symbol: Option<ResolvedSymbol<'w>>,
        receiver: &TypeIdentity<'w>,
        depth: usize,
    ) -> Result<Option<Method>> {
        let Some(mut symbol) = symbol else {
            return Ok(None);
        };
        let SymbolKind::Function(signature) = &symbol.symbol.kind else {
            return Ok(None);
        };
        let reference = TypeRef {
            pointers: 0,
            kind: TypeRefKind::Function(signature.clone()),
        };
        symbol.receiver = Some(receiver.clone());
        let Some(TypeIdentity {
            package,
            kind:
                TypeKind::Function {
                    file,
                    signature,
                    substitutions,
                    ..
                },
            ..
        }) = self.type_from_symbol(&symbol, reference, false)?
        else {
            return Ok(None);
        };
        Ok(self
            .canonical_signature(package, &file, &signature, &substitutions, depth)?
            .map(|signature| Method {
                signature,
                location: symbol.location(),
            }))
    }

    fn implementation_type(
        &mut self,
        package: &'w Package,
        file: &Path,
        reference: TypeRef,
        substitutions: &Substitutions<'w>,
    ) -> Result<Option<TypeIdentity<'w>>> {
        let index = self.indexes.file_index(file)?;
        let dot_imports = matches!(&reference.kind, TypeRefKind::Named(named) if
            named.qualifier.is_none() && is_exported(&named.name)
            && !substitutions.iter().any(|(name, _)| name == &named.name))
            && index
                .imports
                .iter()
                .any(|import| import.alias.as_deref() == Some("."));
        let mut ty = materialize_type(
            self.workspace,
            package,
            file,
            &index.imports,
            reference,
            substitutions,
        );
        if dot_imports
            && let Some(ty) = &mut ty
            && let TypeKind::Named { name, .. } = &ty.kind
        {
            let imports = index.imports.clone();
            if self.type_info(package, name)?.is_none()
                && let Some((declaration, _)) =
                    self.indexes
                        .find_dot_import(self.workspace, package, &imports, name)?
                && let Some(owner) = imports
                    .iter()
                    .filter(|import| import.alias.as_deref() == Some("."))
                    .filter_map(|import| self.workspace.resolve_import(package, &import.path))
                    .find(|owner| self.indexes.files(owner).contains(&declaration))
            {
                ty.package = owner;
            }
        }
        Ok(ty)
    }

    fn canonical_signature(
        &mut self,
        package: &'w Package,
        file: &Path,
        signature: &FunctionSignature,
        substitutions: &Substitutions<'w>,
        depth: usize,
    ) -> Result<Option<Signature>> {
        if depth >= MAX_TYPE_DEPTH {
            return Ok(None);
        }
        let mut normalize = |references: &[Option<TypeRef>]| -> Result<Option<Vec<TypeShape>>> {
            let mut result = Vec::with_capacity(references.len());
            for reference in references {
                let Some(reference) = reference.clone() else {
                    return Ok(None);
                };
                let Some(ty) = self.implementation_type(package, file, reference, substitutions)?
                else {
                    return Ok(None);
                };
                let Some(ty) = self.canonical_type(ty, package, file, depth + 1)? else {
                    return Ok(None);
                };
                result.push(ty);
            }
            Ok(Some(result))
        };
        let (Some(parameters), Some(results)) = (
            normalize(&signature.parameters)?,
            normalize(&signature.results)?,
        ) else {
            return Ok(None);
        };
        Ok(Some(Signature {
            parameters,
            results,
            variadic: signature.variadic,
        }))
    }

    fn canonical_type(
        &mut self,
        ty: TypeIdentity<'w>,
        source_package: &'w Package,
        source_file: &Path,
        depth: usize,
    ) -> Result<Option<TypeShape>> {
        if depth >= MAX_TYPE_DEPTH {
            return Ok(None);
        }
        let Some(ty) = self.resolve_alias(ty)? else {
            return Ok(None);
        };
        let mut nested = |ty| self.canonical_type(ty, source_package, source_file, depth + 1);
        let shape = match ty.kind {
            TypeKind::Parameter(index) => TypeShape::Parameter(index),
            TypeKind::Named { name, arguments } => {
                let owner = self.find_symbol(ty.package, &name, None)?;
                let owner = match owner {
                    Some(owner) => Some(owner.location()),
                    None if std::ptr::eq(ty.package, source_package) && is_exported(&name) => {
                        let imports = self.indexes.file_index(source_file)?.imports.clone();
                        self.indexes
                            .find_dot_import(self.workspace, ty.package, &imports, &name)?
                    }
                    None => None,
                };
                if let Some((file, range)) = owner {
                    let mut parameters = Vec::with_capacity(arguments.len());
                    for arg in arguments {
                        let Some(arg) = arg else { return Ok(None) };
                        let Some(arg) =
                            self.canonical_type(arg, source_package, source_file, depth + 1)?
                        else {
                            return Ok(None);
                        };
                        parameters.push(arg);
                    }
                    TypeShape::Named(path_key(&file), range, name, parameters)
                } else if is_predeclared_type(&name) && arguments.is_empty() {
                    match name.as_str() {
                        "any" => TypeShape::Interface(Vec::new()),
                        "byte" => TypeShape::Builtin("uint8".to_owned()),
                        "rune" => TypeShape::Builtin("int32".to_owned()),
                        _ => TypeShape::Builtin(name),
                    }
                } else {
                    return Ok(None);
                }
            }
            TypeKind::Container(container, origin) => match *container {
                Container::Slice(element) => {
                    let Some(element) = nested(element)? else {
                        return Ok(None);
                    };
                    TypeShape::Slice(Box::new(element))
                }
                Container::Array { element, length } => {
                    let Some(element) = nested(element)? else {
                        return Ok(None);
                    };
                    let Some(length) = self.array_length(ty.package, origin, length.as_deref())?
                    else {
                        return Ok(None);
                    };
                    TypeShape::Array(Box::new(element), length)
                }
                Container::Map { key, value } => {
                    let (Some(key), Some(value)) = (nested(key)?, nested(value)?) else {
                        return Ok(None);
                    };
                    TypeShape::Map(Box::new(key), Box::new(value))
                }
                Container::Channel {
                    element,
                    receive,
                    send,
                } => {
                    let Some(element) = nested(element)? else {
                        return Ok(None);
                    };
                    TypeShape::Channel(Box::new(element), receive, send)
                }
            },
            TypeKind::Function {
                file,
                signature,
                substitutions,
                ..
            } => {
                let Some(signature) = self.canonical_signature(
                    ty.package,
                    &file,
                    &signature,
                    &substitutions,
                    depth + 1,
                )?
                else {
                    return Ok(None);
                };
                TypeShape::Function(Box::new(signature))
            }
            TypeKind::Inline { ref shape, .. } if shape.interface => {
                let mut base = ty.clone();
                base.pointers = 0;
                let Some(methods) =
                    self.interface_methods_at(base, depth + 1, MethodQuery::Navigation)?
                else {
                    return Ok(None);
                };
                if !methods.complete || methods.has_type_terms {
                    return Ok(None);
                }
                TypeShape::Interface(
                    methods
                        .methods
                        .into_iter()
                        .map(|(key, method)| (key, method.signature))
                        .collect(),
                )
            }
            TypeKind::Inline {
                file,
                shape,
                substitutions,
            } => {
                let mut fields = Vec::with_capacity(shape.fields.len());
                for field in &shape.fields {
                    let Some(reference) = field.ty.clone() else {
                        return Ok(None);
                    };
                    let Some(field_type) =
                        self.implementation_type(ty.package, &file, reference, &substitutions)?
                    else {
                        return Ok(None);
                    };
                    let Some(field_type) =
                        self.canonical_type(field_type, ty.package, &file, depth + 1)?
                    else {
                        return Ok(None);
                    };
                    fields.push(Field {
                        key: MethodKey::new(&field.name, ty.package),
                        embedded: field.embedded,
                        tag: field.tag.clone().filter(|tag| tag != "``" && tag != "\"\""),
                        ty: field_type,
                    });
                }
                TypeShape::Struct(fields)
            }
        };
        Ok(Some((0..ty.pointers).fold(shape, |inner, _| {
            TypeShape::Pointer(Box::new(inner))
        })))
    }

    fn is_predeclared_named(&mut self, ty: &TypeIdentity<'w>, spelling: &str) -> Result<bool> {
        Ok(ty.pointers == 0
            && matches!(&ty.kind, TypeKind::Named { name, arguments } if name == spelling && arguments.is_empty())
            && self.type_info(ty.package, spelling)?.is_none())
    }
}

#[derive(Clone, Eq, PartialEq)]
struct Signature {
    parameters: Vec<TypeShape>,
    results: Vec<TypeShape>,
    variadic: bool,
}

impl Signature {
    fn is_predeclared_error(&self) -> bool {
        self.parameters.is_empty()
            && matches!(self.results.as_slice(), [TypeShape::Builtin(name)] if name == "string")
    }
}

#[derive(Clone, Eq, PartialEq)]
enum TypeShape {
    Builtin(String),
    Named(PathBuf, Range, String, Vec<TypeShape>),
    Parameter(usize),
    Pointer(Box<TypeShape>),
    Slice(Box<TypeShape>),
    Array(Box<TypeShape>, u64),
    Map(Box<TypeShape>, Box<TypeShape>),
    Channel(Box<TypeShape>, bool, bool),
    Function(Box<Signature>),
    Interface(Vec<(MethodKey, Signature)>),
    Struct(Vec<Field>),
}

#[derive(Clone, Eq, PartialEq)]
struct Field {
    key: MethodKey,
    embedded: bool,
    tag: Option<String>,
    ty: TypeShape,
}

struct BoundTerm {
    shape: TypeShape,
    approximate: bool,
}

#[derive(Default)]
struct SignatureMatcher {
    // Each side has its own type parameter indices. Keep bindings for all
    // methods of the interface so a repeated parameter requires one type.
    bindings: HashMap<(bool, usize), (bool, TypeShape)>,
}

impl SignatureMatcher {
    fn parameter_binding(&self, mut side: bool, mut index: usize) -> Option<(bool, &TypeShape)> {
        let mut last = None;
        for _ in 0..MAX_TYPE_DEPTH {
            let Some((bound_side, bound)) = self.bindings.get(&(side, index)) else {
                return last;
            };
            last = Some((*bound_side, bound));
            match bound {
                TypeShape::Parameter(var) => (side, index) = (*bound_side, *var),
                _ => return last,
            }
        }
        None
    }

    fn signature(
        &mut self,
        left: &Signature,
        a: bool,
        right: &Signature,
        b: bool,
        depth: usize,
    ) -> bool {
        left.variadic == right.variadic
            && self.list(&left.parameters, a, &right.parameters, b, depth)
            && self.list(&left.results, a, &right.results, b, depth)
    }

    fn list(
        &mut self,
        left: &[TypeShape],
        a: bool,
        right: &[TypeShape],
        b: bool,
        depth: usize,
    ) -> bool {
        left.len() == right.len()
            && left
                .iter()
                .zip(right)
                .all(|(left, right)| self.ty(left, a, right, b, depth + 1))
    }

    fn ty(&mut self, left: &TypeShape, a: bool, right: &TypeShape, b: bool, depth: usize) -> bool {
        if depth >= MAX_TYPE_DEPTH {
            return false;
        }
        if let TypeShape::Parameter(index) = left {
            if a == b && left == right {
                return true;
            }
            if let Some((side, bound)) = self.bindings.get(&(a, *index)).cloned() {
                return self.ty(&bound, side, right, b, depth + 1);
            }
            if let TypeShape::Parameter(other) = right
                && let Some((side, bound)) = self.bindings.get(&(b, *other)).cloned()
            {
                return self.ty(left, a, &bound, side, depth + 1);
            }
            self.bindings.insert((a, *index), (b, right.clone()));
            return true;
        }
        if matches!(right, TypeShape::Parameter(_)) {
            return self.ty(right, b, left, a, depth + 1);
        }
        match (left, right) {
            (TypeShape::Builtin(left), TypeShape::Builtin(right)) => left == right,
            (TypeShape::Named(lp, lr, _, left), TypeShape::Named(rp, rr, _, right)) => {
                lp == rp && lr == rr && self.list(left, a, right, b, depth)
            }
            (TypeShape::Pointer(left), TypeShape::Pointer(right))
            | (TypeShape::Slice(left), TypeShape::Slice(right)) => {
                self.ty(left, a, right, b, depth + 1)
            }
            (TypeShape::Array(left, n), TypeShape::Array(right, m)) => {
                n == m && self.ty(left, a, right, b, depth + 1)
            }
            (TypeShape::Map(lk, lv), TypeShape::Map(rk, rv)) => {
                self.ty(lk, a, rk, b, depth + 1) && self.ty(lv, a, rv, b, depth + 1)
            }
            (TypeShape::Channel(left, lr, ls), TypeShape::Channel(right, rr, rs)) => {
                lr == rr && ls == rs && self.ty(left, a, right, b, depth + 1)
            }
            (TypeShape::Function(left), TypeShape::Function(right)) => {
                self.signature(left, a, right, b, depth + 1)
            }
            (TypeShape::Interface(left), TypeShape::Interface(right)) => {
                left.len() == right.len()
                    && left.iter().zip(right).all(|((lk, left), (rk, right))| {
                        lk == rk && self.signature(left, a, right, b, depth + 1)
                    })
            }
            (TypeShape::Struct(left), TypeShape::Struct(right)) => {
                left.len() == right.len()
                    && left.iter().zip(right).all(|(left, right)| {
                        left.key == right.key
                            && left.embedded == right.embedded
                            && left.tag == right.tag
                            && self.ty(&left.ty, a, &right.ty, b, depth + 1)
                    })
            }
            _ => false,
        }
    }
}
