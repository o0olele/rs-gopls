use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::mem::size_of;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use tree_sitter::Node;

use super::resolver::{MAX_TYPE_DEPTH, Substitutions, TypeIdentity, TypeKind};
use super::source::{Source, check_cancelled};
use crate::symbols::{Container, FunctionSignature};
use crate::workspace::Package;

const ENTRY_LIMIT: usize = 65_536;
const BYTE_LIMIT: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct QuerySourceId {
    // The revision is preserved by syntax snapshots and changes if a shared tree
    // is rebound to another logical path.
    syntax_revision: u64,
    package_view: usize,
}

impl QuerySourceId {
    fn new(source: &Source, package: &Package) -> Self {
        Self {
            syntax_revision: source.syntax_revision,
            package_view: std::ptr::from_ref(package).addr(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum InferenceMode {
    Diagnostics,
    Resolver,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct SubstId(u32);

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum InferenceKey {
    Expr(ExprKey),
    Local(LocalKey),
    InstantiatedResult(SubstId, usize),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct ExprKey {
    source: QuerySourceId,
    node_id: usize,
    mode: InferenceMode,
    result_index: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct LocalKey {
    source: QuerySourceId,
    scope_node: usize,
    declaration_node: usize,
    mode: InferenceMode,
    result_index: usize,
}

impl InferenceKey {
    pub(super) fn expr(
        source: &Source,
        package: &Package,
        mode: InferenceMode,
        node: Node<'_>,
        result_index: usize,
    ) -> Self {
        Self::Expr(ExprKey {
            source: QuerySourceId::new(source, package),
            node_id: node.id(),
            mode,
            result_index,
        })
    }

    pub(super) fn local(
        source: &Source,
        package: &Package,
        mode: InferenceMode,
        scope_node: usize,
        name: Node<'_>,
        result_index: usize,
    ) -> Self {
        Self::Local(LocalKey {
            source: QuerySourceId::new(source, package),
            scope_node,
            declaration_node: name.id(),
            mode,
            result_index,
        })
    }
}

struct Ready<'w> {
    value: Option<Box<TypeIdentity<'w>>>,
    evaluated_at_depth: usize,
}

pub(super) enum Lookup<'w> {
    Ready(Option<TypeIdentity<'w>>),
    Evaluate(Evaluation),
}

pub(super) struct Evaluation {
    key: InferenceKey,
    depth: usize,
    transient_epoch: u64,
    stable_input: bool,
}

pub(super) struct InferenceMemo<'w> {
    ready: HashMap<InferenceKey, Ready<'w>>,
    active: Vec<InferenceKey>,
    substitutions: HashMap<u64, Vec<(OwnedEnvironment<'w>, SubstId)>>,
    next_subst: u32,
    retained_bytes: usize,
    transient_epoch: u64,
    enabled: bool,
}

impl<'w> InferenceMemo<'w> {
    pub(super) fn new() -> Self {
        Self {
            ready: HashMap::new(),
            active: Vec::new(),
            substitutions: HashMap::new(),
            next_subst: 1,
            retained_bytes: 0,
            transient_epoch: 0,
            #[cfg(test)]
            enabled: MEMO_ENABLED.with(std::cell::Cell::get),
            #[cfg(not(test))]
            enabled: true,
        }
    }

    pub(super) fn transient(&mut self) {
        #[cfg(test)]
        if !self.enabled {
            return;
        }
        self.transient_epoch = self.transient_epoch.wrapping_add(1);
        #[cfg(test)]
        record_stats(|stats| stats.transient_count += 1);
    }

    #[cfg(test)]
    pub(super) fn enabled(&self) -> bool {
        self.enabled
    }

    pub(super) fn clear(&mut self) {
        debug_assert!(self.active.is_empty());
        self.ready = HashMap::new();
        self.substitutions = HashMap::new();
        self.next_subst = 1;
        self.retained_bytes = 0;
        self.transient();
    }

    pub(super) fn start(
        &mut self,
        key: InferenceKey,
        depth: usize,
        stable_input: bool,
        cancelled: Option<&AtomicBool>,
    ) -> Result<Lookup<'w>> {
        if let Some(cancelled) = cancelled
            && let Err(error) = check_cancelled(cancelled)
        {
            self.transient();
            return Err(error);
        }
        if depth >= MAX_TYPE_DEPTH || self.active.contains(&key) {
            self.transient();
            return Ok(Lookup::Ready(None));
        }
        if !stable_input {
            self.transient();
        }
        if stable_input
            && let Some(entry) = self.ready.get(&key)
            && depth <= entry.evaluated_at_depth
        {
            #[cfg(test)]
            record_stats(|stats| match key {
                InferenceKey::Expr(_) => stats.expr_hits += 1,
                InferenceKey::Local(_) => stats.local_hits += 1,
                InferenceKey::InstantiatedResult(..) => stats.instantiated_hits += 1,
            });
            return Ok(Lookup::Ready(entry.value.as_deref().cloned()));
        }
        self.active.push(key);
        Ok(Lookup::Evaluate(Evaluation {
            key,
            depth,
            transient_epoch: self.transient_epoch,
            stable_input,
        }))
    }

    pub(super) fn finish(
        &mut self,
        evaluation: Evaluation,
        result: Result<Option<TypeIdentity<'w>>>,
        cancelled: Option<&AtomicBool>,
    ) -> Result<Option<TypeIdentity<'w>>> {
        let current = self.active.pop();
        debug_assert!(current == Some(evaluation.key));
        if let Some(cancelled) = cancelled
            && let Err(error) = check_cancelled(cancelled)
        {
            self.transient();
            return Err(error);
        }
        let value = match &result {
            Ok(value) => value,
            Err(_) => {
                self.transient();
                return result;
            }
        };
        if evaluation.stable_input && evaluation.transient_epoch == self.transient_epoch {
            self.remember(evaluation, value.as_ref());
        }
        result
    }

    fn remember(&mut self, evaluation: Evaluation, value: Option<&TypeIdentity<'w>>) {
        if !self.enabled || self.at_entry_limit() {
            return;
        }
        // Charge both the occupied and spare table buckets. HashMap grows before
        // its current logical capacity is exhausted by the bucket count.
        let bytes = 3 * (size_of::<(InferenceKey, Ready<'_>)>() + 1)
            + value.map_or(0, |ty| size_of::<TypeIdentity<'_>>() + type_heap_weight(ty));
        if self.retained_bytes.saturating_add(bytes) > BYTE_LIMIT {
            return;
        }
        self.retained_bytes += bytes;
        self.ready.insert(
            evaluation.key,
            Ready {
                evaluated_at_depth: evaluation.depth,
                value: value.cloned().map(Box::new),
            },
        );
        #[cfg(test)]
        record_stats(|stats| stats.ready_writes += 1);
    }

    pub(super) fn substitution_id(
        &mut self,
        package: &'w Package,
        file: &Path,
        signature: &Arc<FunctionSignature>,
        substitutions: &Substitutions<'w>,
    ) -> Option<SubstId> {
        if !self.enabled || substitutions.is_empty() {
            return None;
        }
        let environment = SubstitutionEnvironment {
            package,
            file,
            signature,
            substitutions,
        };
        let mut fingerprint = std::hash::DefaultHasher::new();
        environment.hash(&mut fingerprint);
        let fingerprint = fingerprint.finish();
        if let Some(found) = self.substitutions.get(&fingerprint).and_then(|bucket| {
            bucket
                .iter()
                .find_map(|(existing, id)| (existing.as_ref() == environment).then_some(*id))
        }) {
            return Some(found);
        }
        if self.at_entry_limit() {
            return None;
        }
        let bytes = 3 * (size_of::<(u64, Vec<(OwnedEnvironment<'_>, SubstId)>)>() + 1)
            + 4 * size_of::<(OwnedEnvironment<'_>, SubstId)>()
            + file.as_os_str().len()
            + FunctionSignature::shared_heap_weight(signature)
            + substitutions_heap_weight(substitutions);
        if self.retained_bytes.saturating_add(bytes) > BYTE_LIMIT {
            return None;
        }
        let id = SubstId(self.next_subst);
        self.next_subst += 1;
        self.retained_bytes += bytes;
        self.substitutions
            .entry(fingerprint)
            .or_default()
            .push((environment.to_owned(), id));
        Some(id)
    }

    fn at_entry_limit(&self) -> bool {
        self.ready.len() + (self.next_subst - 1) as usize >= ENTRY_LIMIT
    }
}

struct OwnedEnvironment<'w> {
    package: &'w Package,
    file: std::path::PathBuf,
    signature: Arc<FunctionSignature>,
    substitutions: Substitutions<'w>,
}

impl<'w> OwnedEnvironment<'w> {
    fn as_ref(&self) -> SubstitutionEnvironment<'_, 'w> {
        SubstitutionEnvironment {
            package: self.package,
            file: &self.file,
            signature: &self.signature,
            substitutions: &self.substitutions,
        }
    }
}

struct SubstitutionEnvironment<'a, 'w> {
    package: &'w Package,
    file: &'a Path,
    signature: &'a Arc<FunctionSignature>,
    substitutions: &'a Substitutions<'w>,
}

impl<'w> SubstitutionEnvironment<'_, 'w> {
    fn to_owned(&self) -> OwnedEnvironment<'w> {
        OwnedEnvironment {
            package: self.package,
            file: self.file.to_owned(),
            signature: self.signature.clone(),
            substitutions: self.substitutions.clone(),
        }
    }
}

impl PartialEq for SubstitutionEnvironment<'_, '_> {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.package, other.package)
            && self.file == other.file
            && Arc::ptr_eq(self.signature, other.signature)
            && substitution_types(self.substitutions).eq(substitution_types(other.substitutions))
    }
}

impl Hash for SubstitutionEnvironment<'_, '_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::ptr::from_ref(self.package).hash(state);
        self.file.hash(state);
        Arc::as_ptr(self.signature).hash(state);
        hash_types(substitution_types(self.substitutions), state);
    }
}

fn substitution_types<'a, 'w>(
    values: &'a Substitutions<'w>,
) -> impl ExactSizeIterator<Item = (&'a str, Option<ExactType<'a, 'w>>)> {
    values
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_ref().map(ExactType)))
}

fn hash_types<T: Hash>(values: impl ExactSizeIterator<Item = T>, state: &mut impl Hasher) {
    values.len().hash(state);
    for value in values {
        value.hash(state);
    }
}

// Inline shapes and signatures remain alive in the owned environment. Their
// allocation identities conservatively separate separately parsed definitions.
#[derive(Clone, Copy)]
struct ExactType<'a, 'w>(&'a TypeIdentity<'w>);

impl PartialEq for ExactType<'_, '_> {
    fn eq(&self, other: &Self) -> bool {
        let (a, b) = (self.0, other.0);
        if !std::ptr::eq(a.package, b.package)
            || a.pointers != b.pointers
            || a.addressable != b.addressable
            || a.type_expression != b.type_expression
        {
            return false;
        }
        match (&a.kind, &b.kind) {
            (
                TypeKind::Named {
                    name: an,
                    arguments: aa,
                },
                TypeKind::Named {
                    name: bn,
                    arguments: ba,
                },
            ) => an == bn && aa.iter().map(exact_type).eq(ba.iter().map(exact_type)),
            (TypeKind::Container(a, al), TypeKind::Container(b, bl)) => {
                al == bl && exact_container(a) == exact_container(b)
            }
            (
                TypeKind::Function {
                    file: af,
                    signature: ak,
                    substitutions: as_,
                    method_receiver: ar,
                },
                TypeKind::Function {
                    file: bf,
                    signature: bk,
                    substitutions: bs,
                    method_receiver: br,
                },
            ) => {
                af == bf
                    && Arc::ptr_eq(ak, bk)
                    && ar.as_deref().map(ExactType) == br.as_deref().map(ExactType)
                    && substitution_types(as_).eq(substitution_types(bs))
            }
            (
                TypeKind::Inline {
                    file: af,
                    shape: ak,
                    substitutions: as_,
                },
                TypeKind::Inline {
                    file: bf,
                    shape: bk,
                    substitutions: bs,
                },
            ) => {
                af == bf
                    && Arc::ptr_eq(ak, bk)
                    && substitution_types(as_).eq(substitution_types(bs))
            }
            (TypeKind::Parameter(a), TypeKind::Parameter(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for ExactType<'_, '_> {}

impl Hash for ExactType<'_, '_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let ty = self.0;
        std::ptr::from_ref(ty.package).hash(state);
        ty.pointers.hash(state);
        ty.addressable.hash(state);
        ty.type_expression.hash(state);
        std::mem::discriminant(&ty.kind).hash(state);
        match &ty.kind {
            TypeKind::Named { name, arguments } => {
                name.hash(state);
                hash_types(arguments.iter().map(exact_type), state);
            }
            TypeKind::Container(container, location) => {
                location.hash(state);
                exact_container(container).hash(state);
            }
            TypeKind::Function {
                file,
                signature,
                substitutions,
                method_receiver,
            } => {
                file.hash(state);
                Arc::as_ptr(signature).hash(state);
                hash_types(substitution_types(substitutions), state);
                method_receiver.as_deref().map(ExactType).hash(state);
            }
            TypeKind::Inline {
                file,
                shape,
                substitutions,
            } => {
                file.hash(state);
                Arc::as_ptr(shape).hash(state);
                hash_types(substitution_types(substitutions), state);
            }
            TypeKind::Parameter(index) => index.hash(state),
        }
    }
}

fn exact_type<'a, 'w>(ty: &'a Option<TypeIdentity<'w>>) -> Option<ExactType<'a, 'w>> {
    ty.as_ref().map(ExactType)
}

// Borrow array length values along with the type hierarchy when comparing or
// hashing environments.
#[derive(Eq, PartialEq, Hash)]
enum ExactContainer<'a, 'w> {
    Slice(ExactType<'a, 'w>),
    Array(ExactType<'a, 'w>, Option<&'a str>),
    Map(ExactType<'a, 'w>, ExactType<'a, 'w>),
    Channel(ExactType<'a, 'w>, bool, bool),
}

fn exact_container<'a, 'w>(container: &'a Container<TypeIdentity<'w>>) -> ExactContainer<'a, 'w> {
    match container {
        Container::Slice(element) => ExactContainer::Slice(ExactType(element)),
        Container::Array { element, length } => {
            ExactContainer::Array(ExactType(element), length.as_deref())
        }
        Container::Map { key, value } => ExactContainer::Map(ExactType(key), ExactType(value)),
        Container::Channel {
            element,
            receive,
            send,
        } => ExactContainer::Channel(ExactType(element), *receive, *send),
    }
}

fn type_heap_weight(value: &TypeIdentity<'_>) -> usize {
    match &value.kind {
        TypeKind::Named { name, arguments } => {
            name.len()
                + arguments.len() * size_of::<Option<TypeIdentity<'_>>>()
                + arguments
                    .iter()
                    .flatten()
                    .map(type_heap_weight)
                    .sum::<usize>()
        }
        TypeKind::Container(container, location) => {
            size_of::<Container<TypeIdentity<'_>>>()
                + location
                    .as_ref()
                    .map_or(0, |(path, _)| path.as_os_str().len())
                + match container.as_ref() {
                    Container::Slice(element) | Container::Channel { element, .. } => {
                        type_heap_weight(element)
                    }
                    Container::Array { element, length } => {
                        type_heap_weight(element) + length.as_ref().map_or(0, String::len)
                    }
                    Container::Map { key, value } => {
                        type_heap_weight(key) + type_heap_weight(value)
                    }
                }
        }
        TypeKind::Function {
            file,
            signature,
            substitutions,
            method_receiver,
        } => {
            file.as_os_str().len()
                + FunctionSignature::shared_heap_weight(signature)
                + substitutions_heap_weight(substitutions)
                + method_receiver.as_ref().map_or(0, |receiver| {
                    size_of::<TypeIdentity<'_>>() + type_heap_weight(receiver)
                })
        }
        TypeKind::Inline {
            file,
            shape,
            substitutions,
        } => {
            file.as_os_str().len()
                + crate::symbols::InlineType::shared_heap_weight(shape)
                + substitutions_heap_weight(substitutions)
        }
        TypeKind::Parameter(_) => 0,
    }
}

fn substitutions_heap_weight(substitutions: &Substitutions<'_>) -> usize {
    substitutions.len() * size_of::<(String, Option<TypeIdentity<'_>>)>()
        + substitutions
            .iter()
            .map(|(name, ty)| name.len() + ty.as_ref().map_or(0, type_heap_weight))
            .sum::<usize>()
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct MemoStats {
    pub(crate) expr_hits: usize,
    pub(crate) local_hits: usize,
    pub(crate) instantiated_hits: usize,
    pub(crate) transient_count: usize,
    pub(crate) ready_writes: usize,
}

#[cfg(test)]
thread_local! {
    static MEMO_ENABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
    static STATS: std::cell::Cell<Option<MemoStats>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) fn without_memo<T>(run: impl FnOnce() -> T) -> T {
    MEMO_ENABLED.with(|flag| {
        let _restore = RestoreCell(flag, flag.replace(false));
        run()
    })
}

#[cfg(test)]
pub(crate) fn with_memo_stats<T>(run: impl FnOnce() -> T) -> (T, MemoStats) {
    STATS.with(|stats| {
        let _restore = RestoreCell(stats, stats.replace(Some(MemoStats::default())));
        let result = run();
        (result, stats.get().unwrap())
    })
}

#[cfg(test)]
fn record_stats(update: impl FnOnce(&mut MemoStats)) {
    STATS.with(|cell| {
        if let Some(mut stats) = cell.get() {
            update(&mut stats);
            cell.set(Some(stats));
        }
    });
}

#[cfg(test)]
struct RestoreCell<'a, T: Copy>(&'a std::cell::Cell<T>, T);

#[cfg(test)]
impl<T: Copy> Drop for RestoreCell<'_, T> {
    fn drop(&mut self) {
        self.0.set(self.1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbols::FunctionOrigin;
    use crate::test_support::TestProject;
    use std::sync::atomic::Ordering::Relaxed;

    fn key(node_id: usize) -> InferenceKey {
        InferenceKey::Expr(ExprKey {
            source: QuerySourceId {
                syntax_revision: 1,
                package_view: 1,
            },
            node_id,
            mode: InferenceMode::Resolver,
            result_index: 0,
        })
    }

    fn evaluate(memo: &mut InferenceMemo<'_>, key: InferenceKey, depth: usize) -> Evaluation {
        let Lookup::Evaluate(evaluation) = memo.start(key, depth, true, None).unwrap() else {
            panic!("expected an uncached evaluation");
        };
        evaluation
    }

    #[test]
    fn respects_depth_and_discards_fallback_after_reentry() {
        let project = TestProject::new();
        let file = project.write("sample.go", "package sample\n");
        let workspace = project.workspace(&["sample.go"], false);
        let package = workspace.package_of_file(&file).unwrap();
        let mut memo = InferenceMemo::new();
        let key = key(1);
        let eval = evaluate(&mut memo, key, 3);
        memo.finish(eval, Ok(Some(TypeIdentity::named(package, "int"))), None)
            .unwrap();
        assert!(matches!(
            memo.start(key, 1, true, None).unwrap(),
            Lookup::Ready(Some(_))
        ));

        let eval = evaluate(&mut memo, key, 4);
        assert!(matches!(
            memo.start(key, 1, true, None).unwrap(),
            Lookup::Ready(None)
        ));
        assert!(
            memo.finish(eval, Ok(Some(TypeIdentity::named(package, "string"))), None)
                .unwrap()
                .is_some()
        );
        assert_eq!(memo.ready[&key].evaluated_at_depth, 3);
        assert!(memo.active.is_empty());
        assert!(matches!(
            memo.start(key, MAX_TYPE_DEPTH, true, None).unwrap(),
            Lookup::Ready(None)
        ));
    }

    #[test]
    fn errors_recovery_and_cancellation_release_active_and_taint_parents() {
        let mut memo = InferenceMemo::new();
        let parent = evaluate(&mut memo, key(1), 0);
        let child = evaluate(&mut memo, key(2), 1);
        assert!(
            memo.finish(child, Err(anyhow::anyhow!("pending dependency")), None)
                .is_err()
        );
        memo.finish(parent, Ok(None), None).unwrap();
        assert!(memo.ready.is_empty());
        assert!(memo.active.is_empty());

        let parent = evaluate(&mut memo, key(1), 0);
        let Lookup::Evaluate(child) = memo.start(key(2), 1, false, None).unwrap() else {
            panic!("expected recovery to follow the original evaluation path");
        };
        memo.finish(child, Ok(None), None).unwrap();
        memo.finish(parent, Ok(None), None).unwrap();
        assert!(memo.ready.is_empty());

        let cancelled = AtomicBool::new(false);
        let eval = evaluate(&mut memo, key(1), 0);
        cancelled.store(true, Relaxed);
        assert!(memo.finish(eval, Ok(None), Some(&cancelled)).is_err());
        assert!(memo.ready.is_empty());
        assert!(memo.active.is_empty());

        let eval = evaluate(&mut memo, key(1), 0);
        memo.finish(eval, Ok(None), None).unwrap();
        assert!(memo.start(key(1), 0, true, Some(&cancelled)).is_err());
        memo.clear();
        assert!(memo.ready.is_empty());
    }

    #[test]
    fn substitution_identity_preserves_owner_order_absence_and_type_properties() {
        let project = TestProject::new();
        let file = project.write("sample.go", "package sample\n");
        let workspace = project.workspace(&["sample.go"], false);
        let package = workspace.package_of_file(&file).unwrap();
        let signature = Arc::new(FunctionSignature {
            origin: FunctionOrigin::Name(Default::default()),
            result_tuple: None,
            results: Arc::default(),
            parameters: Vec::new(),
            parameters_known: true,
            variadic: false,
            type_parameters: Vec::new(),
        });
        let mut memo = InferenceMemo::new();
        let mut id = |file: &Path, signature: &Arc<FunctionSignature>, values| {
            memo.substitution_id(package, file, signature, values)
                .unwrap()
                .0
        };
        let int = TypeIdentity::named(package, "int");
        let only_t = vec![("T".to_owned(), Some(int.clone()))];
        let only_t_cloned = only_t.clone();
        let unbound_u = vec![only_t[0].clone(), ("U".to_owned(), None)];
        let reordered = unbound_u.iter().rev().cloned().collect();
        let pointer_t = vec![("T".to_owned(), Some(TypeIdentity { pointers: 1, ..int }))];
        let absent_u = id(&file, &signature, &only_t);
        assert_eq!(id(&file, &signature, &only_t_cloned), absent_u);
        let unbound = id(&file, &signature, &unbound_u);
        assert_ne!(unbound, absent_u);
        assert_ne!(unbound, id(&file, &signature, &reordered));
        assert_ne!(
            absent_u,
            id(&file, &Arc::new(signature.as_ref().clone()), &only_t)
        );
        assert_ne!(
            absent_u,
            id(&file.with_file_name("other.go"), &signature, &only_t)
        );
        assert_ne!(absent_u, id(&file, &signature, &pointer_t));
    }

    #[test]
    fn the_ready_budget_counts_negative_entries() {
        let mut memo = InferenceMemo::new();
        for i in 0..ENTRY_LIMIT + 1 {
            let eval = evaluate(&mut memo, key(i), 0);
            memo.finish(eval, Ok(None), None).unwrap();
        }
        assert!(memo.ready.len() <= ENTRY_LIMIT);
        assert!(memo.retained_bytes <= BYTE_LIMIT);
        assert!(!memo.ready.contains_key(&key(ENTRY_LIMIT)));
        assert!(memo.active.is_empty());
    }
}
