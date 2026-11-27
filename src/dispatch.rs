use std::cell::OnceCell;
use std::path::{Path, PathBuf};

use anyhow::Result;
use lsp_server::{Connection, ErrorCode, Request, RequestId};
use lsp_types::{
    CallHierarchyIncomingCallsParams, CallHierarchyOutgoingCallsParams,
    CallHierarchyServerCapability, CodeActionKind, CodeActionOptions, CodeActionParams,
    CodeActionProviderCapability, CompletionItem, CompletionOptions,
    CompletionOptionsCompletionItem, CompletionParams, DiagnosticOptions,
    DiagnosticServerCapabilities, DocumentDiagnosticParams, DocumentFormattingParams,
    DocumentSymbolParams, FoldingRangeParams, FoldingRangeProviderCapability,
    HoverProviderCapability, InitializeParams, InlayHintParams, OneOf, Position, ReferenceParams,
    RenameOptions, RenameParams, SemanticTokensFullOptions, SemanticTokensOptions,
    SemanticTokensParams, ServerCapabilities, SignatureHelpOptions, TextDocumentPositionParams,
    TypeHierarchySubtypesParams, TypeHierarchySupertypesParams, Uri, WorkspaceSymbolParams,
    request::{
        CallHierarchyIncomingCalls, CallHierarchyOutgoingCalls, CallHierarchyPrepare,
        CodeActionRequest, Completion, DocumentDiagnosticRequest, DocumentHighlightRequest,
        DocumentSymbolRequest, FoldingRangeRequest, Formatting, GotoDefinition, GotoImplementation,
        GotoTypeDefinition, HoverRequest, InlayHintRequest, PrepareRenameRequest, References,
        Rename, Request as _, ResolveCompletionItem, SemanticTokensFullRequest,
        SignatureHelpRequest, TypeHierarchyPrepare, TypeHierarchySubtypes, TypeHierarchySupertypes,
        WorkspaceSymbolRequest,
    },
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::protocol::{
    RequestResult, file_path, internal_error, invalid_params, send_response, serialize_location,
    serialize_locations, serialize_query,
};
use crate::scheduler::{DeferredQuery, DiagnosticConsumer, RequestProgress, Scheduler, SearchTask};
use crate::semantic::actions::needs_diagnostics;
use crate::semantic::completion::{
    CompletionClientCapabilities, ResolveCompletionResult, StaleCompletion, Trace,
};
use crate::semantic::rename::{RenameError, RenameResult};
use crate::semantic::{
    DefinitionKind, EditorFoldingOptions, Engine, QueryProgress, SemanticTask,
    semantic_tokens_legend,
};

type Handler = fn(&mut RequestContext<'_>, RequestId, Value) -> Result<()>;

#[derive(Clone, Copy)]
pub(super) struct ClientFeatures {
    hierarchical_document_symbols: bool,
    rename: RenameSupport,
    folding_options: EditorFoldingOptions,
    completion: CompletionClientCapabilities,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RenameSupport {
    Unavailable,
    Direct,
    WithPrepare,
}

impl ClientFeatures {
    pub(super) fn from_initialize(params: &InitializeParams) -> Self {
        let caps = &params.capabilities;
        let document = caps.text_document.as_ref();
        let versioned_edits = caps
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.workspace_edit.as_ref())
            .and_then(|edit| edit.document_changes)
            == Some(true);
        let prepare = document
            .and_then(|text| text.rename.as_ref())
            .and_then(|rename| rename.prepare_support)
            == Some(true);
        Self {
            completion: CompletionClientCapabilities::from_initialize(params),
            folding_options: EditorFoldingOptions {
                line_folding_only: document
                    .and_then(|text| text.folding_range.as_ref())
                    .and_then(|folding| folding.line_folding_only)
                    .unwrap_or_default(),
            },
            hierarchical_document_symbols: document
                .and_then(|text| text.document_symbol.as_ref())
                .and_then(|symbols| symbols.hierarchical_document_symbol_support)
                == Some(true),
            rename: match (versioned_edits, prepare) {
                (false, _) => RenameSupport::Unavailable,
                (true, false) => RenameSupport::Direct,
                (true, true) => RenameSupport::WithPrepare,
            },
        }
    }

    fn supports(self, method: &str) -> bool {
        match method {
            Rename::METHOD => self.rename != RenameSupport::Unavailable,
            PrepareRenameRequest::METHOD => self.rename == RenameSupport::WithPrepare,
            _ => true,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Capabilities {
    #[serde(flatten)]
    standard: ServerCapabilities,
    // lsp-types 0.97 supplies the type hierarchy requests but omits this capability.
    type_hierarchy_provider: bool,
}

struct Feature {
    capabilities: fn(&mut Capabilities, ClientFeatures),
    methods: &'static [(&'static str, Handler)],
}

const FEATURES: &[Feature] = &[
    Feature {
        capabilities: |caps, _| {
            caps.standard.definition_provider = Some(OneOf::Left(true));
            caps.standard.type_definition_provider = Some(true.into());
            caps.standard.implementation_provider = Some(true.into());
            caps.standard.references_provider = Some(OneOf::Left(true));
        },
        methods: &[
            (GotoDefinition::METHOD, definition),
            (GotoTypeDefinition::METHOD, type_definition),
            (GotoImplementation::METHOD, implementation),
            (References::METHOD, references),
        ],
    },
    Feature {
        capabilities: |caps, _| {
            caps.standard.hover_provider = Some(HoverProviderCapability::Simple(true));
            caps.standard.signature_help_provider = Some(SignatureHelpOptions {
                trigger_characters: Some(vec!["(".into(), ",".into()]),
                retrigger_characters: Some(vec![",".into()]),
                work_done_progress_options: Default::default(),
            });
        },
        methods: &[
            (HoverRequest::METHOD, hover),
            (SignatureHelpRequest::METHOD, signature_help),
        ],
    },
    Feature {
        capabilities: |caps, _| {
            caps.standard.document_symbol_provider = Some(OneOf::Left(true));
            caps.standard.workspace_symbol_provider = Some(OneOf::Left(true));
        },
        methods: &[
            (DocumentSymbolRequest::METHOD, document_symbols),
            (WorkspaceSymbolRequest::METHOD, workspace_symbols),
        ],
    },
    Feature {
        capabilities: |caps, _| {
            caps.standard.semantic_tokens_provider = Some(
                SemanticTokensOptions {
                    legend: semantic_tokens_legend(),
                    full: Some(SemanticTokensFullOptions::Bool(true)),
                    ..SemanticTokensOptions::default()
                }
                .into(),
            );
            caps.standard.document_highlight_provider = Some(OneOf::Left(true));
            caps.standard.inlay_hint_provider = Some(OneOf::Left(true));
            caps.standard.folding_range_provider =
                Some(FoldingRangeProviderCapability::Simple(true));
        },
        methods: &[
            (SemanticTokensFullRequest::METHOD, semantic_tokens_full),
            (DocumentHighlightRequest::METHOD, document_highlight),
            (InlayHintRequest::METHOD, inlay_hints),
            (FoldingRangeRequest::METHOD, folding_ranges),
        ],
    },
    Feature {
        capabilities: |caps, _| {
            caps.standard.diagnostic_provider =
                Some(DiagnosticServerCapabilities::Options(DiagnosticOptions {
                    identifier: Some("rs-gopls".into()),
                    inter_file_dependencies: true,
                    workspace_diagnostics: false,
                    ..Default::default()
                }));
        },
        methods: &[(DocumentDiagnosticRequest::METHOD, document_diagnostics)],
    },
    Feature {
        capabilities: |caps, _| {
            caps.standard.call_hierarchy_provider =
                Some(CallHierarchyServerCapability::Simple(true));
            caps.type_hierarchy_provider = true;
        },
        methods: &[
            (CallHierarchyPrepare::METHOD, prepare_call_hierarchy),
            (CallHierarchyIncomingCalls::METHOD, incoming_calls),
            (CallHierarchyOutgoingCalls::METHOD, outgoing_calls),
            (TypeHierarchyPrepare::METHOD, prepare_type_hierarchy),
            (TypeHierarchySupertypes::METHOD, supertypes),
            (TypeHierarchySubtypes::METHOD, subtypes),
        ],
    },
    Feature {
        capabilities: |caps, _| {
            caps.standard.document_formatting_provider = Some(OneOf::Left(true));
            caps.standard.code_action_provider =
                Some(CodeActionProviderCapability::Options(CodeActionOptions {
                    code_action_kinds: Some(vec![
                        CodeActionKind::QUICKFIX,
                        CodeActionKind::SOURCE_ORGANIZE_IMPORTS,
                    ]),
                    ..Default::default()
                }));
        },
        methods: &[
            (Formatting::METHOD, formatting),
            (CodeActionRequest::METHOD, code_actions),
        ],
    },
    Feature {
        capabilities: |caps, _| {
            caps.standard.completion_provider = Some(CompletionOptions {
                resolve_provider: Some(true),
                trigger_characters: Some(vec![".".into()]),
                completion_item: Some(CompletionOptionsCompletionItem {
                    label_details_support: Some(true),
                }),
                ..CompletionOptions::default()
            });
        },
        methods: &[
            (Completion::METHOD, completion),
            (ResolveCompletionItem::METHOD, resolve_completion),
        ],
    },
    Feature {
        capabilities: |caps, client| {
            caps.standard.rename_provider = match client.rename {
                RenameSupport::Unavailable => None,
                RenameSupport::Direct => Some(OneOf::Left(true)),
                RenameSupport::WithPrepare => Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: Default::default(),
                })),
            };
        },
        methods: &[
            (PrepareRenameRequest::METHOD, prepare_rename),
            (Rename::METHOD, rename),
        ],
    },
];

pub(super) fn capabilities(standard: ServerCapabilities, client: ClientFeatures) -> impl Serialize {
    let mut capabilities = Capabilities {
        standard,
        type_hierarchy_provider: false,
    };
    for feature in FEATURES {
        (feature.capabilities)(&mut capabilities, client);
    }
    capabilities
}

pub(super) fn handle(
    connection: &Connection,
    engine: &mut Engine,
    scheduler: &mut Scheduler,
    client: ClientFeatures,
    request: Request,
) -> Result<()> {
    let Request { id, method, params } = request;
    let mut context = RequestContext {
        connection,
        engine,
        scheduler,
        hierarchical_document_symbols: client.hierarchical_document_symbols,
        completion_capabilities: client.completion,
        folding_options: client.folding_options,
    };
    for &(name, handler) in FEATURES.iter().flat_map(|feature| feature.methods) {
        if method == name && client.supports(name) {
            return handler(&mut context, id, params);
        }
    }
    context.respond(
        id,
        Err((
            ErrorCode::MethodNotFound,
            format!("unsupported request: {method}"),
        )),
    )
}

pub(super) fn telemetry_method(method: &str) -> Option<&'static str> {
    FEATURES
        .iter()
        .flat_map(|feature| feature.methods)
        .find_map(|&(name, _)| (method == name).then_some(name))
}

struct RequestContext<'a> {
    connection: &'a Connection,
    engine: &'a mut Engine,
    scheduler: &'a mut Scheduler,
    hierarchical_document_symbols: bool,
    completion_capabilities: CompletionClientCapabilities,
    folding_options: EditorFoldingOptions,
}

impl RequestContext<'_> {
    fn respond(&self, id: RequestId, result: RequestResult) -> Result<()> {
        send_response(self.connection, id, result)
    }

    fn document_query<P: 'static>(
        &mut self,
        id: RequestId,
        params: Result<(PathBuf, P)>,
        run: DocumentHandler<P>,
        fallback: Option<DocumentFallback<P>>,
    ) -> Result<()> {
        match params {
            Ok((path, params)) => self.scheduler.submit(
                self.engine,
                self.connection,
                id,
                DocumentQuery {
                    path,
                    params,
                    run,
                    fallback,
                },
            ),
            Err(error) => self.respond(id, Err(invalid_params(error))),
        }
    }

    fn immediate_document_query<P, T: serde::Serialize>(
        &mut self,
        id: RequestId,
        params: Result<(PathBuf, P)>,
        run: impl FnOnce(&mut Engine, &Path, P) -> Result<T>,
    ) -> Result<()> {
        let result = params
            .map_err(invalid_params)
            .and_then(|(path, params)| run(self.engine, &path, params).map_err(internal_error))
            .and_then(serialize_query);
        self.respond(id, result)
    }
}

type DocumentHandler<P> = fn(&mut Engine, &Path, &P) -> Result<RequestProgress>;
type DocumentFallback<P> = fn(&mut Engine, &Path, &P) -> RequestResult;

struct DocumentQuery<P> {
    path: PathBuf,
    params: P,
    run: DocumentHandler<P>,
    fallback: Option<DocumentFallback<P>>,
}

impl<P> DeferredQuery for DocumentQuery<P> {
    fn run(&self, engine: &mut Engine) -> Result<RequestProgress> {
        (self.run)(engine, &self.path, &self.params)
    }

    fn document_path(&self) -> Option<&Path> {
        Some(&self.path)
    }

    fn load_failed(&self, engine: &mut Engine, message: &str) -> RequestResult {
        match self.fallback {
            Some(fallback) => fallback(engine, &self.path, &self.params),
            None => Err((ErrorCode::RequestFailed, message.to_owned())),
        }
    }
}

struct WorkspaceSymbolsQuery(String);

impl DeferredQuery for WorkspaceSymbolsQuery {
    fn run(&self, engine: &mut Engine) -> Result<RequestProgress> {
        Ok(adapt(engine.workspace_symbols(&self.0), serialize_query))
    }
}

fn adapt<T: 'static, Task: SemanticTask<T> + 'static>(
    progress: QueryProgress<T, Task>,
    encode: fn(T) -> RequestResult,
) -> RequestProgress {
    match progress {
        QueryProgress::Ready(result) => QueryProgress::Ready(encode(result)),
        QueryProgress::Pending(load) => QueryProgress::Pending(load),
        QueryProgress::Searching(task) => QueryProgress::Searching(Box::new(EncodedTask {
            task,
            encode,
            stale: false,
        })),
    }
}

struct EncodedTask<T, Task> {
    task: Task,
    encode: fn(T) -> RequestResult,
    stale: bool,
}

impl<T, Task: SemanticTask<T>> SearchTask for EncodedTask<T, Task> {
    fn ready(&self, engine: &Engine) -> bool {
        self.task.ready(engine)
    }

    fn discard(&mut self, code: ErrorCode) {
        self.task.discard(code);
    }

    fn advance(&mut self, engine: &mut Engine) -> Result<bool> {
        match self.task.advance(engine) {
            Err(error) if error.is::<StaleCompletion>() => {
                self.task.discard(ErrorCode::ContentModified);
                self.stale = true;
                Ok(true)
            }
            progress => progress,
        }
    }

    fn finish(self: Box<Self>) -> RequestResult {
        if self.stale {
            Err((ErrorCode::ContentModified, StaleCompletion.to_string()))
        } else {
            (self.encode)(self.task.finish())
        }
    }
}

fn document_position(params: Value) -> Result<(PathBuf, Position)> {
    let params: TextDocumentPositionParams = serde_json::from_value(params)?;
    Ok((file_path(&params.text_document.uri)?, params.position))
}

fn document_params<P: DeserializeOwned>(
    value: Value,
    uri: impl FnOnce(&P) -> &Uri,
) -> Result<(PathBuf, P)> {
    let params = serde_json::from_value(value)?;
    Ok((file_path(uri(&params))?, params))
}

fn prepare_rename(context: &mut RequestContext<'_>, id: RequestId, params: Value) -> Result<()> {
    context.document_query(
        id,
        document_position(params),
        |engine, path, &position| {
            Ok(adapt(
                engine.prepare_rename(path, position)?,
                serialize_query,
            ))
        },
        None,
    )
}

struct RenameQuery {
    path: PathBuf,
    position: Position,
    new_name: String,
    disk_source: OnceCell<Option<Vec<u8>>>,
}

impl DeferredQuery for RenameQuery {
    fn run(&self, engine: &mut Engine) -> Result<RequestProgress> {
        // A cold query re-resolves this cursor after go list completes. A client
        // without file watches may have replaced the declaration in the meantime.
        if !engine.is_open(&self.path) && self.path.extension().is_some_and(|ext| ext == "go") {
            let current = std::fs::read(&self.path).ok();
            if let Some(original) = self.disk_source.get() {
                if original != &current {
                    return Ok(QueryProgress::Ready(serialize_rename(Err(
                        RenameError::ContentModified,
                    ))));
                }
            } else {
                self.disk_source.get_or_init(|| current);
            }
        }
        Ok(adapt(
            engine.rename(&self.path, self.position, &self.new_name)?,
            serialize_rename,
        ))
    }

    fn requires_stable_workspace(&self) -> bool {
        true
    }
}

fn serialize_rename(result: RenameResult) -> RequestResult {
    match result {
        Ok(edit) => serialize_query(edit),
        Err(RenameError::Rejected(reason)) => Err((ErrorCode::RequestFailed, reason)),
        Err(RenameError::ContentModified) => Err((
            ErrorCode::ContentModified,
            "a rename input changed during validation".into(),
        )),
    }
}

fn rename(context: &mut RequestContext<'_>, id: RequestId, params: Value) -> Result<()> {
    let query = (|| -> Result<RenameQuery> {
        let params: RenameParams = serde_json::from_value(params)?;
        let document = params.text_document_position;
        Ok(RenameQuery {
            path: file_path(&document.text_document.uri)?,
            position: document.position,
            new_name: params.new_name,
            disk_source: OnceCell::new(),
        })
    })();
    match query {
        Ok(query) => context
            .scheduler
            .submit(context.engine, context.connection, id, query),
        Err(error) => context.respond(id, Err(invalid_params(error))),
    }
}

fn definition(context: &mut RequestContext<'_>, id: RequestId, params: Value) -> Result<()> {
    request_definition(context, id, params, DefinitionKind::Declaration)
}

fn type_definition(context: &mut RequestContext<'_>, id: RequestId, params: Value) -> Result<()> {
    request_definition(context, id, params, DefinitionKind::Type)
}

fn request_definition(
    context: &mut RequestContext<'_>,
    id: RequestId,
    params: Value,
    kind: DefinitionKind,
) -> Result<()> {
    context.document_query(
        id,
        document_position(params).map(|(path, position)| (path, (position, kind))),
        |engine, path, &(position, kind)| {
            Ok(adapt(
                engine.definition(path, position, kind)?,
                serialize_location,
            ))
        },
        None,
    )
}

fn implementation(context: &mut RequestContext<'_>, id: RequestId, params: Value) -> Result<()> {
    context.document_query(
        id,
        document_position(params),
        |engine, path, &position| {
            Ok(adapt(
                engine.implementations(path, position)?,
                serialize_locations,
            ))
        },
        None,
    )
}

fn references(context: &mut RequestContext<'_>, id: RequestId, params: Value) -> Result<()> {
    let params = (|| {
        let params: ReferenceParams = serde_json::from_value(params)?;
        let document = params.text_document_position;
        Ok((
            file_path(&document.text_document.uri)?,
            (document.position, params.context.include_declaration),
        ))
    })();
    context.document_query(
        id,
        params,
        |engine, path, &(position, include_declaration)| {
            Ok(adapt(
                engine.references(path, position, include_declaration)?,
                serialize_locations,
            ))
        },
        None,
    )
}

fn hover(context: &mut RequestContext<'_>, id: RequestId, params: Value) -> Result<()> {
    context.document_query(
        id,
        document_position(params),
        |engine, path, &position| Ok(adapt(engine.hover(path, position)?, serialize_query)),
        Some(|engine, path, &position| {
            engine
                .hover_after_load_failure(path, position)
                .map_err(internal_error)
                .and_then(serialize_query)
        }),
    )
}

fn signature_help(context: &mut RequestContext<'_>, id: RequestId, params: Value) -> Result<()> {
    context.document_query(
        id,
        document_position(params),
        |engine, path, &position| {
            Ok(adapt(
                engine.signature_help(path, position)?,
                serialize_query,
            ))
        },
        Some(|engine, path, &position| {
            engine
                .signature_help_after_load_failure(path, position)
                .map_err(internal_error)
                .and_then(serialize_query)
        }),
    )
}

fn document_symbols(context: &mut RequestContext<'_>, id: RequestId, params: Value) -> Result<()> {
    let document = serde_json::from_value::<DocumentSymbolParams>(params)
        .map_err(anyhow::Error::from)
        .and_then(|params| {
            let uri = params.text_document.uri;
            Ok((file_path(&uri)?, uri))
        });
    let result = match document {
        Ok((path, uri)) => context
            .engine
            .document_symbols(&path, &uri, context.hierarchical_document_symbols)
            .map_err(internal_error)
            .and_then(serialize_query),
        Err(error) => Err(invalid_params(error)),
    };
    context.respond(id, result)
}

fn semantic_tokens_full(
    context: &mut RequestContext<'_>,
    id: RequestId,
    params: Value,
) -> Result<()> {
    let params = (|| {
        let params: SemanticTokensParams = serde_json::from_value(params)?;
        Ok((file_path(&params.text_document.uri)?, ()))
    })();
    context.immediate_document_query(id, params, |engine, path, ()| {
        engine.semantic_tokens_full(path)
    })
}

fn document_highlight(
    context: &mut RequestContext<'_>,
    id: RequestId,
    params: Value,
) -> Result<()> {
    match document_position(params) {
        Ok((path, position)) => context.scheduler.submit_highlight(
            context.engine,
            context.connection,
            id,
            path,
            position,
        ),
        Err(error) => context.respond(id, Err(invalid_params(error))),
    }
}

fn inlay_hints(context: &mut RequestContext<'_>, id: RequestId, params: Value) -> Result<()> {
    let params = (|| {
        let params: InlayHintParams = serde_json::from_value(params)?;
        Ok((file_path(&params.text_document.uri)?, params.range))
    })();
    context.immediate_document_query(id, params, |engine, path, range| {
        let _operation = crate::telemetry::loop_start("inlay_hint", 20.0);
        engine.inlay_hints(path, range)
    })
}

fn folding_ranges(context: &mut RequestContext<'_>, id: RequestId, params: Value) -> Result<()> {
    let options = context.folding_options;
    let params = (|| {
        let params: FoldingRangeParams = serde_json::from_value(params)?;
        Ok((file_path(&params.text_document.uri)?, options))
    })();
    context.immediate_document_query(id, params, Engine::folding_ranges)
}

fn workspace_symbols(context: &mut RequestContext<'_>, id: RequestId, params: Value) -> Result<()> {
    match serde_json::from_value::<WorkspaceSymbolParams>(params) {
        Ok(params) => context.scheduler.submit(
            context.engine,
            context.connection,
            id,
            WorkspaceSymbolsQuery(params.query),
        ),
        Err(error) => context.respond(id, Err((ErrorCode::InvalidParams, error.to_string()))),
    }
}

fn document_diagnostics(
    context: &mut RequestContext<'_>,
    id: RequestId,
    params: Value,
) -> Result<()> {
    match document_params(params, |p: &DocumentDiagnosticParams| &p.text_document.uri) {
        Ok((path, params)) => context.scheduler.submit_diagnostics(
            context.engine,
            context.connection,
            path,
            DiagnosticConsumer::Pull {
                id,
                previous: params.previous_result_id,
            },
            "textDocument/diagnostic",
        ),
        Err(error) => context.respond(id, Err(invalid_params(error))),
    }
}

fn formatting(context: &mut RequestContext<'_>, id: RequestId, params: Value) -> Result<()> {
    context.document_query(
        id,
        document_params(params, |p: &DocumentFormattingParams| &p.text_document.uri),
        |engine, path, _| {
            Ok(QueryProgress::Ready(serialize_query(
                engine.format_document(path)?,
            )))
        },
        None,
    )
}

fn code_actions(context: &mut RequestContext<'_>, id: RequestId, params: Value) -> Result<()> {
    match document_params(params, |p: &CodeActionParams| &p.text_document.uri) {
        Ok((path, params))
            if path.extension().is_some_and(|ext| ext == "go")
                && needs_diagnostics(params.context.only.as_deref()) =>
        {
            context.scheduler.submit_diagnostics(
                context.engine,
                context.connection,
                path,
                DiagnosticConsumer::Actions {
                    id,
                    uri: params.text_document.uri,
                    range: params.range,
                    only: params.context.only,
                },
                "textDocument/codeAction",
            )
        }
        Ok(_) => context.respond(id, Ok(Value::Array(Vec::new()))),
        Err(error) => context.respond(id, Err(invalid_params(error))),
    }
}

fn prepare_call_hierarchy(
    context: &mut RequestContext<'_>,
    id: RequestId,
    params: Value,
) -> Result<()> {
    context.document_query(
        id,
        document_position(params),
        |engine, path, &pos| {
            Ok(adapt(
                engine.prepare_call_hierarchy(path, pos)?,
                serialize_query,
            ))
        },
        None,
    )
}

fn incoming_calls(context: &mut RequestContext<'_>, id: RequestId, params: Value) -> Result<()> {
    context.document_query(
        id,
        document_params(params, |p: &CallHierarchyIncomingCallsParams| &p.item.uri),
        |engine, _, p| Ok(adapt(engine.incoming_calls(&p.item)?, serialize_query)),
        None,
    )
}

fn outgoing_calls(context: &mut RequestContext<'_>, id: RequestId, params: Value) -> Result<()> {
    context.document_query(
        id,
        document_params(params, |p: &CallHierarchyOutgoingCallsParams| &p.item.uri),
        |engine, _, p| Ok(adapt(engine.outgoing_calls(&p.item)?, serialize_query)),
        None,
    )
}

fn prepare_type_hierarchy(
    context: &mut RequestContext<'_>,
    id: RequestId,
    params: Value,
) -> Result<()> {
    context.document_query(
        id,
        document_position(params),
        |engine, path, &pos| {
            Ok(adapt(
                engine.prepare_type_hierarchy(path, pos)?,
                serialize_query,
            ))
        },
        None,
    )
}

fn supertypes(context: &mut RequestContext<'_>, id: RequestId, params: Value) -> Result<()> {
    context.document_query(
        id,
        document_params(params, |p: &TypeHierarchySupertypesParams| &p.item.uri),
        |engine, _, p| Ok(adapt(engine.supertypes(&p.item)?, serialize_query)),
        None,
    )
}

fn subtypes(context: &mut RequestContext<'_>, id: RequestId, params: Value) -> Result<()> {
    context.document_query(
        id,
        document_params(params, |p: &TypeHierarchySubtypesParams| &p.item.uri),
        |engine, _, p| Ok(adapt(engine.subtypes(&p.item)?, serialize_query)),
        None,
    )
}

fn completion(context: &mut RequestContext<'_>, id: RequestId, params: Value) -> Result<()> {
    let query = (|| {
        let params: CompletionParams = serde_json::from_value(params)?;
        let document = params.text_document_position;
        let path = file_path(&document.text_document.uri)?;
        Ok(CompletionQuery {
            trace: Trace::start(&path, document.position),
            path,
            position: document.position,
            caps: context.completion_capabilities,
        })
    })();
    match query {
        Ok(query) => context
            .scheduler
            .submit(context.engine, context.connection, id, query),
        Err(error) => context.respond(id, Err(invalid_params(error))),
    }
}

struct CompletionQuery {
    path: PathBuf,
    position: Position,
    caps: CompletionClientCapabilities,
    trace: Option<Trace>,
}

impl DeferredQuery for CompletionQuery {
    fn document_path(&self) -> Option<&Path> {
        Some(&self.path)
    }

    fn run(&self, engine: &mut Engine) -> Result<RequestProgress> {
        let result =
            engine.complete_traced(&self.path, self.position, &self.caps, self.trace.as_ref());
        if result.is_err()
            && let Some(trace) = &self.trace
        {
            trace.outcome("failed");
        }
        Ok(adapt(result?, serialize_query))
    }

    fn load_failed(&self, engine: &mut Engine, _message: &str) -> RequestResult {
        let result = engine.complete_after_load_failure(
            &self.path,
            self.position,
            &self.caps,
            self.trace.as_ref(),
        );
        if let Some(trace) = &self.trace {
            trace.outcome(if result.is_ok() {
                "completed with local syntax"
            } else {
                "failed"
            });
        }
        result.map_err(internal_error).and_then(serialize_query)
    }

    fn discard(&mut self, code: ErrorCode) {
        if let Some(trace) = &self.trace {
            trace.error(code);
        }
    }
}

fn resolve_completion(
    context: &mut RequestContext<'_>,
    id: RequestId,
    params: Value,
) -> Result<()> {
    let item = serde_json::from_value::<CompletionItem>(params)
        .map_err(|error| invalid_params(error.into()));
    let result = item.and_then(|item| {
        match context
            .engine
            .resolve_completion_item(item, &context.completion_capabilities)
            .map_err(internal_error)?
        {
            ResolveCompletionResult::Item(item) => serialize_query(item),
            ResolveCompletionResult::ContentModified => {
                Err((ErrorCode::ContentModified, StaleCompletion.to_string()))
            }
            ResolveCompletionResult::InvalidData => Err((
                ErrorCode::InvalidParams,
                "invalid completion item data".into(),
            )),
        }
    });
    context.respond(id, result)
}
