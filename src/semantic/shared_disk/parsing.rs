use super::*;

#[cfg(test)]
mod tests;

#[derive(Debug)]
pub(in crate::semantic) struct NeedsFullDeclarations;

impl std::fmt::Display for NeedsFullDeclarations {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("source requires full Go declaration recovery")
    }
}

impl std::error::Error for NeedsFullDeclarations {}

impl SharedDiskCache {
    pub(in crate::semantic) fn declarations_snapshot(
        &self,
        parser: &mut Parser,
        path: &Path,
        text: NavigationText,
        cancelled: Option<&AtomicBool>,
        probe: Option<&NavigationProbe>,
    ) -> Result<PreparedDeclarations> {
        self.projected_snapshot(
            parser,
            path,
            text,
            cancelled,
            probe,
            DeclarationProducer::Stub,
        )
    }

    pub(super) fn projected_snapshot(
        &self,
        parser: &mut Parser,
        path: &Path,
        text: NavigationText,
        cancelled: Option<&AtomicBool>,
        probe: Option<&NavigationProbe>,
        producer: DeclarationProducer,
    ) -> Result<PreparedDeclarations> {
        let key = ContentKey::new(&text);
        let declarations =
            self.project_declarations(path, &key, cancelled, probe, producer, |slot, trace| {
                let produce = self.declaration_job(path.to_owned(), key.clone(), producer, trace);
                Self::dispatch_parse(parser, slot, trace, produce)
            })?;
        Ok(PreparedDeclarations {
            declarations,
            text,
            fingerprint: key.fingerprint,
            path: path.to_owned(),
        })
    }

    pub(in crate::semantic) fn source_declarations(
        &self,
        source: &IndexedSource,
        cancelled: Option<&AtomicBool>,
    ) -> Result<Arc<SharedDeclarations>> {
        let key = ContentKey {
            fingerprint: source.fingerprint(),
            text: source.text.clone(),
            overlay_syntax: None,
        };
        self.project_declarations(
            &source.path,
            &key,
            cancelled,
            None,
            DeclarationProducer::Stub,
            |slot, trace| {
                let abort = self.abort_parse(&key, DeclarationProducer::Stub);
                let produce = move |source: &IndexedSource, cache: &Self, key: &ContentKey| {
                    let mut abort = abort;
                    let result = trace::measure(trace, "index", None, || {
                        cache.collect_declarations(&source.path, source)
                    });
                    cache.publish_declarations(key, DeclarationProducer::Stub, result);
                    abort.0.take();
                };
                // The full snapshot already verifies the syntax and byte positions.
                // Refill the bounded declaration cache without parsing it again.
                if let Some(slot) = slot {
                    let revision = source.revision();
                    let cache = self.clone();
                    let key = key.clone();
                    slot.submit_traced(
                        trace,
                        Box::new(move |_| produce(&revision.into_indexed(), &cache, &key)),
                    )
                } else {
                    produce(source, self, &key);
                    Ok(())
                }
            },
        )
    }

    fn project_declarations(
        &self,
        path: &Path,
        key: &ContentKey,
        cancelled: Option<&AtomicBool>,
        probe: Option<&NavigationProbe>,
        producer: DeclarationProducer,
        produce: impl FnOnce(Option<LoadSlot>, Option<FlightTrace>) -> Result<()>,
    ) -> Result<Arc<SharedDeclarations>> {
        loop {
            check_cancelled(cancelled.unwrap_or(&SHARED_LOAD))?;
            let mut slot = None;
            let request = {
                let mut parsed = self.parsed.lock().unwrap();
                parsed.request_declarations(key, &self.wake.0, producer, || {
                    self.reserve_for(cancelled, &mut slot)
                })
            };
            request.trace(Artifact::Declarations);
            match request {
                SourceRequest::Cached(declarations) => return Ok(declarations),
                SourceRequest::Join(flight) => {
                    diagnostic_cold_parse(path);
                    #[cfg(test)]
                    self.test_event(&path_key(path), CacheEvent::WaitDeclarations);
                    match flight.wait(cancelled) {
                        Err(error)
                            if error.is::<NeedsFullDeclarations>()
                                && producer == DeclarationProducer::Stub =>
                        {
                            continue;
                        }
                        result => return result,
                    }
                }
                SourceRequest::Produce(flight) => {
                    let _parse = sample(probe, |counts| &mut counts.parse);
                    produce(slot, flight.trace)?;
                    diagnostic_cold_parse(path);
                    return flight.wait(cancelled);
                }
                SourceRequest::Busy(pending) => pending.wait_for_slot(cancelled)?,
            }
        }
    }

    pub(in crate::semantic) fn parse_snapshot_with(
        &self,
        parser: &mut Parser,
        path: &Path,
        text: NavigationText,
        cancelled: Option<&AtomicBool>,
        probe: Option<&NavigationProbe>,
    ) -> Result<PreparedReferenceSource> {
        let key = ContentKey::new(&text);
        let revision = loop {
            check_cancelled(cancelled.unwrap_or(&SHARED_LOAD))?;
            let mut slot = None;
            let request = {
                let mut parsed = self.parsed.lock().unwrap();
                parsed.request_full(&key, &self.wake.0, || {
                    self.reserve_for(cancelled, &mut slot)
                })
            };
            request.trace(Artifact::FullAst);
            match request {
                SourceRequest::Cached(revision) => break revision,
                SourceRequest::Join(flight) => {
                    diagnostic_cold_parse(path);
                    #[cfg(test)]
                    self.test_event(&path_key(path), CacheEvent::WaitParse);
                    break flight.wait(cancelled)?;
                }
                SourceRequest::Produce(flight) => {
                    let _parse = sample(probe, |counts| &mut counts.parse);
                    let produce = self.full_job(path.to_owned(), key.clone(), flight.trace, "full");
                    Self::dispatch_parse(parser, slot, flight.trace, produce)?;
                    record(probe, |counts| counts.ast_parses += 1);
                    diagnostic_cold_parse(path);
                    break flight.wait(cancelled)?;
                }
                SourceRequest::Busy(pending) => pending.wait_for_slot(cancelled)?,
            }
        };
        Ok(PreparedReferenceSource::new(
            revision,
            path,
            text,
            key.fingerprint,
        ))
    }

    fn dispatch_parse(
        parser: &mut Parser,
        slot: Option<LoadSlot>,
        trace: Option<FlightTrace>,
        produce: impl FnOnce(&mut Parser) + Send + 'static,
    ) -> Result<()> {
        if let Some(slot) = slot {
            slot.submit_traced(trace, Box::new(produce))
        } else {
            produce(parser);
            Ok(())
        }
    }

    pub(super) fn declaration_job(
        &self,
        path: PathBuf,
        key: ContentKey,
        producer: DeclarationProducer,
        trace: Option<FlightTrace>,
    ) -> impl FnOnce(&mut Parser) + Send + 'static {
        let cache = self.clone();
        let abort = self.abort_parse(&key, producer);
        move |parser| {
            let mut abort = abort;
            cache.produce_declarations(parser, &path, &key, producer, trace);
            abort.0.take();
        }
    }

    pub(super) fn full_job(
        &self,
        path: PathBuf,
        key: ContentKey,
        trace: Option<FlightTrace>,
        parse_mode: &'static str,
    ) -> impl FnOnce(&mut Parser) + Send + 'static {
        let cache = self.clone();
        let abort = self.abort_parse(&key, DeclarationProducer::Full);
        move |parser| {
            let mut abort = abort;
            cache.produce_full(parser, &path, &key, trace, parse_mode);
            abort.0.take();
        }
    }

    pub(super) fn abort_parse(
        &self,
        key: &ContentKey,
        producer: DeclarationProducer,
    ) -> AbortOnDrop<impl FnOnce() + use<>> {
        let parsed = self.parsed.clone();
        let key = key.clone();
        AbortOnDrop(Some(move || {
            let mut parsed = parsed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(loads) = parsed.loading.get_mut(&key) else {
                return;
            };
            if producer == DeclarationProducer::Full
                && let Some(full) = loads.full.take()
            {
                full.finish(Err(anyhow::anyhow!("shared Go source parse aborted")));
            }
            if loads
                .declarations
                .as_ref()
                .is_some_and(|(owner, _)| *owner == producer)
                && let Some((_, declarations)) = loads.declarations.take()
            {
                declarations.finish(Err(anyhow::anyhow!("shared Go declarations aborted")));
            }
            parsed.retire_flights(&key);
        }))
    }

    fn produce_declarations(
        &self,
        parser: &mut Parser,
        path: &Path,
        key: &ContentKey,
        producer: DeclarationProducer,
        trace: Option<FlightTrace>,
    ) {
        let full = self.parsed.lock().unwrap().cached.get(key).cloned();
        if let Some(revision) = full {
            let source = revision.as_ref().clone().into_indexed();
            let result = trace::measure(trace, "index", None, || {
                self.collect_declarations(path, &source)
            });
            self.publish_declarations(key, producer, result);
            return;
        }

        #[cfg(test)]
        self.test_event(&path_key(path), CacheEvent::Declarations);
        let projected = scan_function_bodies(&key.text)
            .map(|bodies| {
                let mode = if bodies.is_empty() {
                    "full"
                } else {
                    "body_elided"
                };
                trace::measure(trace, "parse", Some(mode), || {
                    Source::try_parse_dependency_declarations(
                        parser,
                        path,
                        key.text.clone(),
                        &bodies,
                        &SHARED_LOAD,
                    )
                })
            })
            .transpose()
            .map(Option::flatten)
            .and_then(|source| match source {
                Some(source)
                    if has_reliable_package_scope(source.tree.root_node(), Some(&SHARED_LOAD))? =>
                {
                    trace::measure(trace, "index", None, || {
                        self.collect_declarations(path, &IndexedSource::new(source, None))
                    })
                    .map(Some)
                }
                _ => Ok(None),
            });
        match projected {
            Ok(None) if producer == DeclarationProducer::Stub => {
                self.fallback_to_full(parser, path, key, trace)
            }
            Ok(None) => self.publish_declarations(key, producer, Err(NeedsFullDeclarations.into())),
            result => self.publish_declarations(
                key,
                producer,
                result.map(|declarations| declarations.expect("successful projection")),
            ),
        }
    }

    fn fallback_to_full(
        &self,
        parser: &mut Parser,
        path: &Path,
        key: &ContentKey,
        trace: Option<FlightTrace>,
    ) {
        let (full, fallback_trace) = {
            let mut parsed = self.parsed.lock().unwrap();
            let cached = parsed.cached.get(key).cloned();
            let Some(loads) = parsed.loading.get_mut(key) else {
                return;
            };
            let full_active = loads.active_full().is_some();
            let Some((owner @ DeclarationProducer::Stub, _)) = &mut loads.declarations else {
                return;
            };
            if full_active {
                // That producer will publish to the existing declaration
                // flight. A loader must never wait for another loader.
                *owner = DeclarationProducer::Full;
                return;
            }
            let full = cached.or_else(|| loads.full.as_ref()?.completed());
            let fallback_trace = if full.is_none() {
                *owner = DeclarationProducer::Full;
                let flight = Flight::traced(&self.wake.0, Artifact::FullAst);
                let trace = flight.trace;
                loads.full = Some(Arc::new(flight));
                trace
            } else {
                None
            };
            (full, fallback_trace)
        };
        if let Some(revision) = full {
            let source = revision.as_ref().clone().into_indexed();
            let result = trace::measure(trace, "index", None, || {
                self.collect_declarations(path, &source)
            });
            self.publish_declarations(key, DeclarationProducer::Stub, result);
        } else {
            self.full_job(
                path.to_owned(),
                key.clone(),
                fallback_trace,
                "fallback_full",
            )(parser);
        }
    }

    pub(super) fn collect_declarations(
        &self,
        path: &Path,
        source: &IndexedSource,
    ) -> Result<Arc<SharedDeclarations>> {
        #[cfg(test)]
        self.test_event(&path_key(path), CacheEvent::DeclarationBuild);
        #[cfg(not(test))]
        let _ = path;
        SharedDeclarations::collect(source, &SHARED_LOAD).map(Arc::new)
    }

    fn produce_full(
        &self,
        parser: &mut Parser,
        path: &Path,
        key: &ContentKey,
        trace: Option<FlightTrace>,
        parse_mode: &'static str,
    ) {
        #[cfg(test)]
        self.test_event(&path_key(path), CacheEvent::Parse);
        let result = trace::measure(trace, "parse", Some(parse_mode), || {
            Source::parse_cancellable(parser, path, key.text.clone(), &SHARED_LOAD)
        })
        .and_then(|source| {
            trace::measure(trace, "index", None, || {
                IndexedSourceRevision::from_disk_source(source, &SHARED_LOAD)
            })
            .map(|(revision, nodes)| (Arc::new(revision), nodes))
        });
        let source = result
            .as_ref()
            .ok()
            .map(|(revision, _)| revision.as_ref().clone().into_indexed());
        let recovery = source.as_ref().is_some_and(|source| {
            !has_reliable_package_scope(source.tree.root_node(), None).unwrap_or(false)
        });
        let mut parsed = self.parsed.lock().unwrap();
        let needs_recovery = recovery
            && !matches!(
                parsed.declarations.peek(key),
                Some(DeclarationProjection::FullRecovery(_))
            );
        let superseded = needs_recovery
            .then(|| parsed.declarations.remove(key))
            .flatten();
        let loads = parsed.loading.get_mut(key).expect("registered full parse");
        if needs_recovery {
            // Prefer the full recovery projection when it disproves the
            // declaration boundaries used by a concurrent elided parse.
            match &mut loads.declarations {
                Some((owner, _)) => *owner = DeclarationProducer::Full,
                None => {
                    loads.declarations = Some((
                        DeclarationProducer::Full,
                        Arc::new(Flight::traced(&self.wake.0, Artifact::Declarations)),
                    ))
                }
            }
        }
        let declarations = if let Some((DeclarationProducer::Full, flight)) = &loads.declarations {
            let declarations_trace = flight.trace;
            drop(parsed);
            if let Some((declarations, full)) = declarations_trace.zip(trace) {
                declarations.handoff(full);
            }
            let declarations = match source {
                Some(source) => trace::measure(declarations_trace, "index", None, || {
                    self.collect_declarations(path, &source)
                }),
                None => Err(anyhow::anyhow!(
                    "full Go parse failed: {:#}",
                    result.as_ref().err().unwrap()
                )),
            };
            let declarations = Self::weigh_declarations(key, declarations);
            parsed = self.parsed.lock().unwrap();
            Some(declarations)
        } else {
            None
        };
        let mut retired_full = Retired::empty();
        let result = result.map(|(revision, nodes)| {
            // Tree-sitter does not expose an allocation size. Use a per-node
            // allowance plus the input bytes.
            let weight = nodes.saturating_mul(64).saturating_add(key.text.capacity());
            retired_full = parsed.cached.insert(key.clone(), revision.clone(), weight);
            revision
        });
        let retired_declarations = declarations
            .map(|result| parsed.publish_declarations(key, DeclarationProducer::Full, result));
        let full = parsed
            .loading
            .get(key)
            .and_then(|loads| loads.full.as_ref())
            .unwrap();
        full.finish(result);
        parsed.retire_flights(key);
        drop(parsed);
        retired_full.report("full_ast");
        if let Some(retired) = retired_declarations {
            retired.report("declarations");
        }
        Removal::Recovery.report("declarations", usize::from(superseded.is_some()));
    }

    pub(super) fn publish_declarations(
        &self,
        key: &ContentKey,
        producer: DeclarationProducer,
        result: Result<Arc<SharedDeclarations>>,
    ) {
        let result = Self::weigh_declarations(key, result);
        let retired = self
            .parsed
            .lock()
            .unwrap()
            .publish_declarations(key, producer, result);
        retired.report("declarations");
    }

    fn weigh_declarations(
        key: &ContentKey,
        result: Result<Arc<SharedDeclarations>>,
    ) -> Result<(Arc<SharedDeclarations>, usize)> {
        result.map(|declarations| {
            let weight = declarations
                .cache_weight()
                .saturating_add(key.text.capacity());
            (declarations, weight)
        })
    }
}

impl Parsed {
    pub(super) fn request_declarations(
        &mut self,
        key: &ContentKey,
        wake: &Sender<()>,
        producer: DeclarationProducer,
        admit: impl FnOnce() -> Result<(), SharedLoadPending>,
    ) -> SourceRequest<Arc<SharedDeclarations>> {
        if let Some(declarations) = self.declarations.get(key) {
            return SourceRequest::Cached(declarations.declarations().clone());
        }
        if let Some((_, flight)) = self
            .loading
            .get(key)
            .and_then(|loads| loads.declarations.as_ref())
        {
            return SourceRequest::Join(flight.clone());
        }
        if let Some(pending) = self.busy().or_else(|| admit().err()) {
            return SourceRequest::Busy(pending);
        }
        let flight = Arc::new(Flight::traced(wake, Artifact::Declarations));
        self.loading.entry(key.clone()).or_default().declarations =
            Some((producer, flight.clone()));
        SourceRequest::Produce(flight)
    }

    pub(super) fn request_full(
        &mut self,
        key: &ContentKey,
        wake: &Sender<()>,
        admit: impl FnOnce() -> Result<(), SharedLoadPending>,
    ) -> SourceRequest<Arc<IndexedSourceRevision>> {
        if let Some(revision) = self.cached.get(key) {
            return SourceRequest::Cached(revision.clone());
        }
        if let Some(flight) = self.loading.get(key).and_then(|loads| loads.full.as_ref()) {
            return SourceRequest::Join(flight.clone());
        }
        if let Some(pending) = self.busy().or_else(|| admit().err()) {
            return SourceRequest::Busy(pending);
        }
        let provide_declarations = self.declarations.peek(key).is_none();
        let full = Arc::new(Flight::traced(wake, Artifact::FullAst));
        let loads = self.loading.entry(key.clone()).or_default();
        loads.full = Some(full.clone());
        if provide_declarations && loads.declarations.is_none() {
            loads.declarations = Some((
                DeclarationProducer::Full,
                Arc::new(Flight::traced(wake, Artifact::Declarations)),
            ));
        }
        SourceRequest::Produce(full)
    }

    fn publish_declarations(
        &mut self,
        key: &ContentKey,
        producer: DeclarationProducer,
        result: Result<(Arc<SharedDeclarations>, usize)>,
    ) -> Retired<DeclarationProjection> {
        let Some(loads) = self.loading.get_mut(key) else {
            return Retired::empty();
        };
        if loads
            .declarations
            .as_ref()
            .is_none_or(|(owner, _)| *owner != producer)
        {
            return Retired::empty();
        }
        let (_, flight) = loads.declarations.take().unwrap();
        let mut retired = Retired::empty();
        let result = result.map(|(declarations, weight)| {
            if weight <= DECLARATION_ITEM_BYTES {
                let projection = if declarations.reliable_package_scope {
                    DeclarationProjection::Reliable(declarations.clone())
                } else {
                    DeclarationProjection::FullRecovery(declarations.clone())
                };
                retired = self.declarations.insert(key.clone(), projection, weight);
            } else {
                retired.values.extend(self.declarations.remove(key));
                if telemetry::enabled() {
                    retired.record(Removal::OversizedReject, 1);
                    retired.record(Removal::Replacement, retired.values.len());
                }
            }
            declarations
        });
        flight.finish(result);
        self.retire_flights(key);
        retired
    }
}
