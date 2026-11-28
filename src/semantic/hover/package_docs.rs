use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;

use super::super::index::SourceRev;
use super::super::resolver::Resolver;
use crate::{rpc_telemetry, workspace::Package};

const MAX_ENTRIES: usize = 128;
const MAX_BYTES: usize = 2 * 1024 * 1024;

#[derive(Default)]
pub(in crate::semantic) struct HoverDocCache {
    graph_generation: u64,
    entries: VecDeque<(String, Arc<PackageDoc>)>,
    retained_bytes: usize,
    #[cfg(test)]
    pub(super) hits: usize,
}

struct PackageDoc {
    sources: Vec<(PathBuf, SourceRev)>,
    text: Arc<str>,
}

impl PackageDoc {
    fn weight(&self, package_id: &str) -> usize {
        size_of::<Self>()
            + size_of::<(String, Arc<Self>)>()
            + package_id.len()
            + self.text.len()
            + self.sources.capacity() * size_of::<(PathBuf, SourceRev)>()
            + self
                .sources
                .iter()
                .map(|(path, _)| 2 * path.as_os_str().len())
                .sum::<usize>()
    }
}

impl HoverDocCache {
    fn get(&mut self, graph_generation: u64, package_id: &str) -> Option<Arc<PackageDoc>> {
        if self.graph_generation != graph_generation {
            self.entries.clear();
            self.retained_bytes = 0;
            self.graph_generation = graph_generation;
        }
        let index = self.entries.iter().position(|(id, _)| id == package_id)?;
        let entry = self.entries.remove(index).unwrap();
        let document = Arc::clone(&entry.1);
        self.entries.push_front(entry);
        Some(document)
    }

    fn insert(&mut self, package_id: &str, document: PackageDoc) {
        let weight = document.weight(package_id);
        if let Some(index) = self.entries.iter().position(|(id, _)| id == package_id) {
            let (id, old) = self.entries.remove(index).unwrap();
            self.retained_bytes -= old.weight(&id);
        }
        if weight > MAX_BYTES {
            return;
        }
        while self.entries.len() >= MAX_ENTRIES || self.retained_bytes + weight > MAX_BYTES {
            let (id, old) = self.entries.pop_back().unwrap();
            self.retained_bytes -= old.weight(&id);
        }
        self.retained_bytes += weight;
        self.entries
            .push_front((package_id.to_owned(), Arc::new(document)));
    }
}

#[derive(Clone, Copy)]
pub(super) struct PackageDocs<'a> {
    pub(super) cache: &'a RefCell<HoverDocCache>,
    pub(super) graph_generation: u64,
}

impl PackageDocs<'_> {
    pub(super) fn documentation(
        self,
        resolver: &mut Resolver<'_, '_>,
        package: &Package,
    ) -> Result<String> {
        let _package_doc = rpc_telemetry::phase("package_doc");
        let files = resolver.indexes.files(package);
        let path_count = files.len();
        let is_preferred = |path: &Path| path.file_name().is_some_and(|name| name == "doc.go");
        let preferred = files.iter().filter(|path| is_preferred(path));
        let other = files.iter().filter(|path| !is_preferred(path));
        let paths = preferred.chain(other);
        let cached = self
            .cache
            .borrow_mut()
            .get(self.graph_generation, &package.id);
        if let Some(cached) = cached
            && (!cached.text.is_empty() || cached.sources.len() == path_count)
            && cached.sources.len() <= path_count
        {
            let mut valid = true;
            for (path, (old_path, old_revision)) in paths.clone().zip(&cached.sources) {
                if path != old_path || resolver.indexes.source_revision(path)? != *old_revision {
                    valid = false;
                    break;
                }
            }
            if valid {
                #[cfg(test)]
                {
                    self.cache.borrow_mut().hits += 1;
                }
                return Ok(cached.text.to_string());
            }
        }

        let mut sources = Vec::new();
        let mut text = Arc::<str>::default();
        for path in paths {
            let revision = resolver.indexes.source_revision(path)?;
            let documentation = if revision == SourceRev::Missing {
                None
            } else {
                package_documentation(resolver, path, package)?
            };
            sources.push((path.clone(), revision));
            if let Some(documentation) = documentation {
                text = documentation;
                break;
            }
        }
        let result = text.to_string();
        self.cache
            .borrow_mut()
            .insert(&package.id, PackageDoc { sources, text });
        Ok(result)
    }
}

fn package_documentation(
    resolver: &mut Resolver<'_, '_>,
    path: &Path,
    package: &Package,
) -> Result<Option<Arc<str>>> {
    Ok(resolver
        .indexes
        .declaration_source(path)?
        .and_then(|source| {
            let (name, doc) = source.package_doc.as_ref()?;
            (name.as_ref() == package.name.as_str()).then(|| doc.clone())
        }))
}
