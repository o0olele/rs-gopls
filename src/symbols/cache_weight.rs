use std::collections::{HashMap, HashSet};
use std::mem::size_of;
use std::sync::Arc;

use super::{
    Container, FileSymbols, FunctionSignature, GenericParameter, InlineType, Symbol, SymbolKind,
    TypeRef, TypeRefKind,
};

impl FileSymbols {
    pub(crate) fn heap_weight(&self) -> usize {
        let mut weight = HeapWeight::default();
        weight.bytes += map_weight(&self.members);
        weight.symbols(&self.declarations);
        for (name, members) in &self.members {
            weight.bytes += name.capacity();
            weight.symbols(members);
        }
        weight.bytes += map_weight(&self.types);
        for (name, details) in &self.types {
            weight.bytes += name.capacity();
            weight.optional_type(&details.alias);
            weight.optional_type(&details.underlying);
            weight.bytes += vector_weight(&details.method_names)
                + details
                    .method_names
                    .iter()
                    .map(String::capacity)
                    .sum::<usize>();
            weight.bytes += vector_weight(&details.embedded);
            for ty in &details.embedded {
                weight.ty(ty);
            }
            weight.parameters(&details.parameters);
        }
        weight.bytes
    }
}

impl FunctionSignature {
    pub(crate) fn shared_heap_weight(signature: &Arc<Self>) -> usize {
        let mut weight = HeapWeight::default();
        weight.signature(signature);
        weight.bytes
    }
}

impl InlineType {
    pub(crate) fn shared_heap_weight(shape: &Arc<Self>) -> usize {
        let mut weight = HeapWeight::default();
        weight.inline_type(shape);
        weight.bytes
    }
}

#[derive(Default)]
struct HeapWeight {
    bytes: usize,
    inline_types: HashSet<*const InlineType>,
    signatures: HashSet<*const FunctionSignature>,
}

impl HeapWeight {
    fn symbols(&mut self, symbols: &HashMap<String, Symbol>) {
        self.bytes += map_weight(symbols);
        for (name, symbol) in symbols {
            self.bytes += name.capacity() + vector_weight(&symbol.receiver_parameters);
            self.bytes += symbol
                .receiver_parameters
                .iter()
                .flatten()
                .map(String::capacity)
                .sum::<usize>();
            match &symbol.kind {
                SymbolKind::Value(ty) => self.optional_type(ty),
                SymbolKind::Function(signature) => self.signature(signature),
                SymbolKind::InferredValue { .. } | SymbolKind::Type => {}
            }
        }
    }

    fn optional_type(&mut self, ty: &Option<TypeRef>) {
        if let Some(ty) = ty {
            self.ty(ty);
        }
    }

    fn ty(&mut self, ty: &TypeRef) {
        match &ty.kind {
            TypeRefKind::Named(named) => {
                self.bytes += named.name.capacity()
                    + named.qualifier.as_ref().map_or(0, String::capacity)
                    + vector_weight(&named.arguments);
                for argument in &named.arguments {
                    self.optional_type(argument);
                }
            }
            TypeRefKind::Container(container, _) => {
                self.bytes += size_of::<Container<TypeRef>>();
                match container.as_ref() {
                    Container::Slice(element) | Container::Channel { element, .. } => {
                        self.ty(element);
                    }
                    Container::Array { element, length } => {
                        self.bytes += length.as_ref().map_or(0, String::capacity);
                        self.ty(element);
                    }
                    Container::Map { key, value } => {
                        self.ty(key);
                        self.ty(value);
                    }
                }
            }
            TypeRefKind::Inline(inline) => self.inline_type(inline),
            TypeRefKind::Function(signature) => self.signature(signature),
        }
    }

    fn inline_type(&mut self, inline: &Arc<InlineType>) {
        if !self.inline_types.insert(Arc::as_ptr(inline)) {
            return;
        }
        self.bytes += ARC_HEADER
            + size_of::<InlineType>()
            + vector_weight(&inline.embedded)
            + vector_weight(&inline.fields);
        self.symbols(&inline.members);
        for ty in &inline.embedded {
            self.ty(ty);
        }
        for field in &inline.fields {
            self.bytes += field.name.capacity() + field.tag.as_ref().map_or(0, String::capacity);
            self.optional_type(&field.ty);
        }
    }

    fn signature(&mut self, signature: &Arc<FunctionSignature>) {
        if !self.signatures.insert(Arc::as_ptr(signature)) {
            return;
        }
        self.bytes += ARC_HEADER * 2
            + size_of::<FunctionSignature>()
            + size_of_val(&*signature.results)
            + vector_weight(&signature.parameters);
        for ty in signature.results.iter().chain(&signature.parameters) {
            self.optional_type(ty);
        }
        self.parameters(&signature.type_parameters);
    }

    fn parameters(&mut self, parameters: &Vec<GenericParameter>) {
        self.bytes += vector_weight(parameters);
        for parameter in parameters {
            self.bytes += parameter.name.capacity() + vector_weight(&parameter.constraint.terms);
            self.optional_type(&parameter.constraint.methods);
            self.optional_type(&parameter.constraint.approximation);
            for term in &parameter.constraint.terms {
                self.ty(&term.reference);
            }
        }
    }
}

const ARC_HEADER: usize = 2 * size_of::<usize>();

fn vector_weight<T>(values: &Vec<T>) -> usize {
    values.capacity() * size_of::<T>()
}

fn map_weight<K, V>(map: &HashMap<K, V>) -> usize {
    // Include spare buckets and their control bytes.
    (map.capacity() * 8).div_ceil(7) * (size_of::<(K, V)>() + 1)
}
