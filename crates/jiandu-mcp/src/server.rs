//! MCP tool handler. Transport and daemon lifecycle policy stay outside this
//! adapter crate.

use crate::{
    McpMutationBackend, McpReadBackend, MutationBackendError, MutationPolicy, MutationPolicyError,
    OptionalCapability, ReadBackendError, ReadServiceHealth,
    resource::{self, ResourceRequest},
};
use jiandu_core::{
    API_VERSION, ApiVersion, CorrelationId, CreationActor, DomainError, DomainErrorCode,
    ErrorEnvelope, ForgetMemoryCommand, ForgetMemoryResult, MemoryGetRequest, MemoryListRequest,
    MemoryListResult, MemoryRecord, MemorySearchRequest, MemorySearchResult, MutationInvocation,
    RememberMemoryCommand, RememberMemoryResult, ResultEnvelope, StoreRevision,
    TrustedRequestContext, UpdateMemoryCommand, UpdateMemoryResult, Validate,
};
use jiandu_index::IndexErrorCode;
use jiandu_store::{
    AuthorizedRead, AuthorizedScopes, MutationOperation, StoreError, StoreErrorCode,
};
use rmcp::{
    RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, ContentBlock, ErrorData, ExperimentalCapabilities, Implementation,
        InitializeResult, JsonObject, ListResourceTemplatesResult, ListResourcesResult,
        PaginatedRequestParams, ProtocolVersion, ReadResourceRequestParams, ReadResourceResponse,
        ReadResourceResult, ResourceContents, ServerCapabilities,
    },
    service::RequestContext,
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::borrow::Cow;
use std::sync::Arc;
use uuid::Uuid;

/// MCP protocol revision supported by this adapter. It evolves independently
/// from [`API_VERSION`].
pub const JIANDU_MCP_PROTOCOL_REVISION: &str = "2025-11-25";

#[derive(JsonSchema, Serialize)]
#[serde(untagged)]
enum ToolEnvelope<T> {
    Success(ResultEnvelope<T>),
    Error(ErrorEnvelope),
}

struct MutationConnection {
    backend: Arc<dyn McpMutationBackend>,
    scopes: AuthorizedScopes,
    context: TrustedRequestContext,
    policy: Arc<dyn MutationPolicy>,
    creation_actor: CreationActor,
}

/// One authenticated MCP handler per connection. Read-only connections retain
/// the same constructor and tools; mutation tools return `FORBIDDEN` unless a
/// trusted host explicitly installs a mutation backend and policy.
pub struct JianduReadServer {
    backend: Arc<dyn McpReadBackend>,
    authorization: AuthorizedRead,
    mutation: Option<MutationConnection>,
    tool_router: ToolRouter<Self>,
}

#[tool_router(router = tool_router)]
impl JianduReadServer {
    /// Construct one handler at the trusted connection boundary. Authentication,
    /// principal equality, and the `memory:read` grant are checked exactly once
    /// before any MCP request can be served.
    pub fn new(
        backend: Arc<dyn McpReadBackend>,
        scopes: &AuthorizedScopes,
        context: &TrustedRequestContext,
    ) -> Result<Self, StoreError> {
        Ok(Self::from_authorized(
            backend,
            scopes.authorize_read(context)?,
        ))
    }

    /// Compose a handler from an already minted private-field read capability.
    /// This remains safe for hosts that authenticate outside the adapter.
    #[must_use]
    pub fn from_authorized(
        backend: Arc<dyn McpReadBackend>,
        authorization: AuthorizedRead,
    ) -> Self {
        Self {
            backend,
            authorization,
            mutation: None,
            tool_router: Self::tool_router(),
        }
    }

    /// Construct one read-and-mutation handler from the same production
    /// backend. `memory:read` remains required for the existing read surface;
    /// write and destructive grants are checked independently per tool call.
    pub fn new_with_mutations<B>(
        backend: Arc<B>,
        scopes: &AuthorizedScopes,
        context: &TrustedRequestContext,
        policy: Arc<dyn MutationPolicy>,
        creation_actor: CreationActor,
    ) -> Result<Self, StoreError>
    where
        B: McpReadBackend + McpMutationBackend,
    {
        let authorization = scopes.authorize_read(context)?;
        let read_backend: Arc<dyn McpReadBackend> = backend.clone();
        let mutation_backend: Arc<dyn McpMutationBackend> = backend;
        Ok(Self {
            backend: read_backend,
            authorization,
            mutation: Some(MutationConnection {
                backend: mutation_backend,
                scopes: scopes.clone(),
                context: context.clone(),
                policy,
                creation_actor,
            }),
            tool_router: Self::tool_router(),
        })
    }

    #[must_use]
    pub fn health(&self) -> ReadServiceHealth {
        self.backend.health()
    }

    /// Search authorized memories through the disposable lexical index.
    #[tool(
        name = "memory_search",
        input_schema = core_request_schema("memory-search-request.schema.json"),
        output_schema = rmcp::handler::server::tool::schema_for_output::<ToolEnvelope<MemorySearchResult>>(),
        annotations(
            title = "Search memories",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_search(&self, parameters: Parameters<JsonObject>) -> CallToolResult {
        let correlation_id = next_correlation_id();
        let request = match decode_validated_request::<MemorySearchRequest>(parameters.0) {
            Ok(request) => request,
            Err(()) => return self.invalid_request(correlation_id),
        };
        match self.backend.search(&self.authorization, &request) {
            Ok((store_revision, result)) => success_result(
                correlation_id,
                store_revision,
                result,
                "Returned authorized memory search results.",
            ),
            Err(error) => self.backend_error(correlation_id, error),
        }
    }

    /// Read one authorized memory by opaque ID.
    #[tool(
        name = "memory_get",
        input_schema = core_request_schema("memory-get-request.schema.json"),
        output_schema = rmcp::handler::server::tool::schema_for_output::<ToolEnvelope<MemoryRecord>>(),
        annotations(
            title = "Get memory",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_get(&self, parameters: Parameters<JsonObject>) -> CallToolResult {
        let correlation_id = next_correlation_id();
        let request = match decode_request::<MemoryGetRequest>(parameters.0) {
            Ok(request) => request,
            Err(()) => return self.invalid_request(correlation_id),
        };
        match self.backend.get(&self.authorization, &request) {
            Ok(read) => success_result(
                correlation_id,
                read.store_revision,
                read.result,
                "Returned one authorized memory.",
            ),
            Err(error) => self.backend_error(correlation_id, error),
        }
    }

    /// List authorized memories with deterministic filters and pagination.
    #[tool(
        name = "memory_list",
        input_schema = core_request_schema("memory-list-request.schema.json"),
        output_schema = rmcp::handler::server::tool::schema_for_output::<ToolEnvelope<MemoryListResult>>(),
        annotations(
            title = "List memories",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_list(&self, parameters: Parameters<JsonObject>) -> CallToolResult {
        let correlation_id = next_correlation_id();
        let request = match decode_validated_request::<MemoryListRequest>(parameters.0) {
            Ok(request) => request,
            Err(()) => return self.invalid_request(correlation_id),
        };
        match self.backend.list(&self.authorization, &request) {
            Ok(read) => success_result(
                correlation_id,
                read.store_revision,
                read.result,
                "Returned authorized memory list results.",
            ),
            Err(error) => self.backend_error(correlation_id, error),
        }
    }

    /// Remember one memory in an independently write-authorized exact scope.
    #[tool(
        name = "memory_remember",
        input_schema = core_request_schema("remember-memory-command.schema.json"),
        output_schema = rmcp::handler::server::tool::schema_for_output::<ToolEnvelope<RememberMemoryResult>>(),
        annotations(
            title = "Remember memory",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_remember(&self, parameters: Parameters<JsonObject>) -> CallToolResult {
        let invocation = next_mutation_invocation();
        let request = match decode_validated_request::<RememberMemoryCommand>(parameters.0) {
            Ok(request) => request,
            Err(()) => return self.invalid_mutation_request(&invocation),
        };
        let Some(connection) = &self.mutation else {
            return self.pre_backend_mutation_error(
                invocation.correlation_id().clone(),
                ReadBackendError::Store(StoreError::Forbidden),
            );
        };
        let authorization = match connection
            .scopes
            .authorize_mutation_set(&connection.context, MutationOperation::Create)
        {
            Ok(authorization) => authorization,
            Err(error) => {
                return self.pre_backend_mutation_error(
                    invocation.correlation_id().clone(),
                    ReadBackendError::Store(error),
                );
            }
        };
        if let Err(error) = authorization.authorize_selector(&request.scope) {
            return self.pre_backend_mutation_error(
                invocation.correlation_id().clone(),
                ReadBackendError::Store(error),
            );
        }
        let attempt_correlation = invocation.correlation_id().clone();
        let backend = Arc::clone(&connection.backend);
        let policy = Arc::clone(&connection.policy);
        let creation_actor = connection.creation_actor;
        let result = tokio::task::spawn_blocking(move || {
            backend.remember(
                &authorization,
                &invocation,
                policy.as_ref(),
                creation_actor,
                &request,
            )
        })
        .await;
        match result {
            Ok(Ok(commit)) => success_result(
                commit.correlation_id,
                commit.store_revision,
                commit.result,
                "Remembered one memory.",
            ),
            Ok(Err(error)) => self.mutation_error(attempt_correlation, error),
            Err(_) => self
                .pre_backend_mutation_error(attempt_correlation, ReadBackendError::HostUnavailable),
        }
    }

    /// Update one memory using optimistic revision CAS and exact replay.
    #[tool(
        name = "memory_update",
        input_schema = core_request_schema("update-memory-command.schema.json"),
        output_schema = rmcp::handler::server::tool::schema_for_output::<ToolEnvelope<UpdateMemoryResult>>(),
        annotations(
            title = "Update memory",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_update(&self, parameters: Parameters<JsonObject>) -> CallToolResult {
        let invocation = next_mutation_invocation();
        let request = match decode_validated_request::<UpdateMemoryCommand>(parameters.0) {
            Ok(request) => request,
            Err(()) => return self.invalid_mutation_request(&invocation),
        };
        let Some(connection) = &self.mutation else {
            return self.pre_backend_mutation_error(
                invocation.correlation_id().clone(),
                ReadBackendError::Store(StoreError::Forbidden),
            );
        };
        let authorization = match connection
            .scopes
            .authorize_mutation_set(&connection.context, MutationOperation::Update)
        {
            Ok(authorization) => authorization,
            Err(error) => {
                return self.pre_backend_mutation_error(
                    invocation.correlation_id().clone(),
                    ReadBackendError::Store(error),
                );
            }
        };
        let attempt_correlation = invocation.correlation_id().clone();
        let backend = Arc::clone(&connection.backend);
        let policy = Arc::clone(&connection.policy);
        let result = tokio::task::spawn_blocking(move || {
            backend.update(&authorization, &invocation, policy.as_ref(), &request)
        })
        .await;
        match result {
            Ok(Ok(commit)) => success_result(
                commit.correlation_id,
                commit.store_revision,
                commit.result,
                "Updated one memory.",
            ),
            Ok(Err(error)) => self.mutation_error(attempt_correlation, error),
            Err(_) => self
                .pre_backend_mutation_error(attempt_correlation, ReadBackendError::HostUnavailable),
        }
    }

    /// Forget exactly one memory using the independent destructive grant.
    #[tool(
        name = "memory_forget",
        input_schema = core_request_schema("forget-memory-command.schema.json"),
        output_schema = rmcp::handler::server::tool::schema_for_output::<ToolEnvelope<ForgetMemoryResult>>(),
        annotations(
            title = "Forget memory",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_forget(&self, parameters: Parameters<JsonObject>) -> CallToolResult {
        let invocation = next_mutation_invocation();
        let request = match decode_validated_request::<ForgetMemoryCommand>(parameters.0) {
            Ok(request) => request,
            Err(()) => return self.invalid_mutation_request(&invocation),
        };
        let Some(connection) = &self.mutation else {
            return self.pre_backend_mutation_error(
                invocation.correlation_id().clone(),
                ReadBackendError::Store(StoreError::Forbidden),
            );
        };
        let authorization = match connection
            .scopes
            .authorize_mutation_set(&connection.context, MutationOperation::Forget)
        {
            Ok(authorization) => authorization,
            Err(error) => {
                return self.pre_backend_mutation_error(
                    invocation.correlation_id().clone(),
                    ReadBackendError::Store(error),
                );
            }
        };
        let attempt_correlation = invocation.correlation_id().clone();
        let backend = Arc::clone(&connection.backend);
        let policy = Arc::clone(&connection.policy);
        let result = tokio::task::spawn_blocking(move || {
            backend.forget(&authorization, &invocation, policy.as_ref(), &request)
        })
        .await;
        match result {
            Ok(Ok(commit)) => success_result(
                commit.correlation_id,
                commit.store_revision,
                commit.result,
                "Forgot one memory.",
            ),
            Ok(Err(error)) => self.mutation_error(attempt_correlation, error),
            Err(_) => self
                .pre_backend_mutation_error(attempt_correlation, ReadBackendError::HostUnavailable),
        }
    }

    fn invalid_request(&self, correlation_id: CorrelationId) -> CallToolResult {
        let revision = self.backend.store_revision().unwrap_or(StoreRevision(0));
        error_result(
            correlation_id,
            revision,
            DomainError::new(
                DomainErrorCode::InvalidArgument,
                "The memory request is invalid.",
            ),
        )
    }

    fn invalid_mutation_request(&self, invocation: &MutationInvocation) -> CallToolResult {
        error_result(
            invocation.correlation_id().clone(),
            StoreRevision(0),
            DomainError::new(
                DomainErrorCode::InvalidArgument,
                "The memory request is invalid.",
            ),
        )
    }

    fn backend_error(
        &self,
        correlation_id: CorrelationId,
        error: ReadBackendError,
    ) -> CallToolResult {
        let revision = self.backend.store_revision().unwrap_or(StoreRevision(0));
        error_result(correlation_id, revision, public_error(&error))
    }

    fn mutation_error(
        &self,
        correlation_id: CorrelationId,
        error: MutationBackendError,
    ) -> CallToolResult {
        let (error, store_revision) = error.into_parts();
        error_result(correlation_id, store_revision, public_error(&error))
    }

    fn pre_backend_mutation_error(
        &self,
        correlation_id: CorrelationId,
        error: ReadBackendError,
    ) -> CallToolResult {
        error_result(correlation_id, StoreRevision(0), public_error(&error))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for JianduReadServer {
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(&[ProtocolVersion::V_2025_11_25])
    }

    fn get_info(&self) -> InitializeResult {
        let health = self.backend.health();
        let health_value = serde_json::to_value(&health).unwrap_or_else(|_| {
            json!({
                "store": "degraded",
                "index": "degraded",
                "exactRead": false,
                "list": false,
                "search": false
            })
        });
        let mut jiandu = JsonObject::new();
        jiandu.insert(
            "apiVersion".to_owned(),
            Value::String(API_VERSION.to_owned()),
        );
        jiandu.insert("health".to_owned(), health_value);
        jiandu.insert(
            "optionalCapabilities".to_owned(),
            serde_json::to_value([OptionalCapability::Resources])
                .expect("closed optional capabilities are JSON serializable"),
        );
        let mut experimental = ExperimentalCapabilities::new();
        experimental.insert("jiandu".to_owned(), jiandu);
        let capabilities = ServerCapabilities::builder()
            .enable_experimental_with(experimental)
            .enable_resources()
            .enable_tools()
            .build();
        InitializeResult::new(capabilities)
            .with_protocol_version(ProtocolVersion::V_2025_11_25)
            .with_server_info(Implementation::new("jiandu", env!("CARGO_PKG_VERSION")))
    }

    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        reject_protocol_cursor(request)?;
        Ok(ListResourcesResult::with_all_items(
            resource::concrete_resources(),
        ))
    }

    async fn list_resource_templates(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        reject_protocol_cursor(request)?;
        Ok(ListResourceTemplatesResult::with_all_items(
            resource::templates(),
        ))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let uri = request.uri;
        let correlation_id = next_correlation_id();
        let parsed = resource::parse(&uri).map_err(|()| resource_not_found())?;
        match parsed {
            ResourceRequest::Get(request) => {
                let read = self
                    .backend
                    .get(&self.authorization, &request)
                    .map_err(resource_backend_error)?;
                resource_result(uri, correlation_id, read.store_revision, read.result)
            }
            ResourceRequest::List(request) => {
                let read = self
                    .backend
                    .list(&self.authorization, &request)
                    .map_err(resource_backend_error)?;
                resource_result(uri, correlation_id, read.store_revision, read.result)
            }
        }
    }
}

fn decode_request<T: DeserializeOwned>(arguments: JsonObject) -> Result<T, ()> {
    serde_json::from_value(Value::Object(arguments)).map_err(|_| ())
}

fn core_request_schema(name: &'static str) -> Arc<JsonObject> {
    let schema = jiandu_core::generated_contract_schemas()
        .remove(name)
        .unwrap_or_else(|| panic!("missing checked core schema {name}"));
    let Value::Object(schema) = schema else {
        panic!("checked core request schema {name} must be an object")
    };
    Arc::new(schema)
}

fn decode_validated_request<T>(arguments: JsonObject) -> Result<T, ()>
where
    T: DeserializeOwned + Validate,
{
    let request: T = decode_request(arguments)?;
    request.validate().map_err(|_| ())?;
    Ok(request)
}

fn next_correlation_id() -> CorrelationId {
    CorrelationId::new(format!("req_mcp_{}", Uuid::new_v4().simple()))
        .expect("UUID-derived correlation IDs satisfy the core grammar")
}

fn next_mutation_invocation() -> MutationInvocation {
    let correlation_id = CorrelationId::new(format!("req_txn_{}", Uuid::new_v4().simple()))
        .expect("UUID-derived mutation correlations satisfy the core ID grammar");
    MutationInvocation::new(correlation_id)
        .expect("UUIDv4-derived correlations satisfy the mutation invocation contract")
}

fn success_result<T: Serialize>(
    correlation_id: CorrelationId,
    store_revision: StoreRevision,
    result: T,
    summary: &'static str,
) -> CallToolResult {
    let envelope = ResultEnvelope {
        api_version: ApiVersion::default(),
        correlation_id,
        store_revision,
        result,
    };
    let structured = serde_json::to_value(ToolEnvelope::Success(envelope))
        .expect("public result envelopes are JSON serializable");
    let mut output = CallToolResult::structured(structured);
    output.content = vec![ContentBlock::text(summary)];
    output
}

fn error_result(
    correlation_id: CorrelationId,
    store_revision: StoreRevision,
    error: DomainError,
) -> CallToolResult {
    let code = serde_json::to_value(error.code)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "INTERNAL".to_owned());
    let envelope = ErrorEnvelope {
        api_version: ApiVersion::default(),
        correlation_id,
        store_revision,
        error,
    };
    let structured = serde_json::to_value(ToolEnvelope::<Value>::Error(envelope))
        .expect("public error envelopes are JSON serializable");
    let mut output = CallToolResult::structured_error(structured);
    output.content = vec![ContentBlock::text(format!(
        "Memory operation failed: {code}."
    ))];
    output
}

fn public_error(error: &ReadBackendError) -> DomainError {
    match error {
        ReadBackendError::Store(error) => match error.code() {
            StoreErrorCode::Unauthenticated => DomainError::new(
                DomainErrorCode::Unauthenticated,
                "The connection is not authenticated.",
            ),
            StoreErrorCode::Forbidden => DomainError::new(
                DomainErrorCode::Forbidden,
                "The memory operation is not authorized.",
            ),
            StoreErrorCode::InvalidRequest
            | StoreErrorCode::InvalidCursor
            | StoreErrorCode::StaleCursor => DomainError::new(
                DomainErrorCode::InvalidArgument,
                "The memory request is invalid.",
            ),
            StoreErrorCode::NotFound => {
                DomainError::new(DomainErrorCode::NotFound, "The memory is not available.")
            }
            StoreErrorCode::RevisionConflict => {
                let domain = DomainError::new(
                    DomainErrorCode::RevisionConflict,
                    "The memory revision changed.",
                );
                if let StoreError::RevisionConflict { current_revision } = error {
                    domain.with_detail("currentRevision", current_revision.get())
                } else {
                    domain
                }
            }
            StoreErrorCode::IdempotencyConflict => DomainError::new(
                DomainErrorCode::IdempotencyConflict,
                "The idempotency key was already used for different input.",
            ),
            _ => DomainError::new(
                DomainErrorCode::StoreUnavailable,
                "Canonical memory storage is unavailable.",
            ),
        },
        ReadBackendError::Index(error) => match error.code() {
            IndexErrorCode::InvalidRequest
            | IndexErrorCode::InvalidCursor
            | IndexErrorCode::StaleCursor => DomainError::new(
                DomainErrorCode::InvalidArgument,
                "The memory request is invalid.",
            ),
            IndexErrorCode::Unauthenticated => DomainError::new(
                DomainErrorCode::Unauthenticated,
                "The connection is not authenticated.",
            ),
            IndexErrorCode::Forbidden => DomainError::new(
                DomainErrorCode::Forbidden,
                "The memory operation is not authorized.",
            ),
            _ => DomainError::new(
                DomainErrorCode::IndexDegraded,
                "Lexical memory search is temporarily unavailable.",
            ),
        },
        ReadBackendError::Policy(MutationPolicyError::InvalidRequest) => DomainError::new(
            DomainErrorCode::InvalidArgument,
            "The memory request violates configured policy.",
        ),
        ReadBackendError::Policy(MutationPolicyError::Forbidden) => DomainError::new(
            DomainErrorCode::Forbidden,
            "The memory operation is not authorized.",
        ),
        ReadBackendError::UnstableSearchSnapshot => DomainError::new(
            DomainErrorCode::IndexDegraded,
            "Lexical memory search is temporarily unavailable.",
        ),
        ReadBackendError::HostUnavailable => DomainError::new(
            DomainErrorCode::StoreUnavailable,
            "Canonical memory storage is unavailable.",
        ),
    }
}

fn reject_protocol_cursor(request: Option<PaginatedRequestParams>) -> Result<(), ErrorData> {
    if request.is_some_and(|request| request.cursor.is_some()) {
        return Err(ErrorData::invalid_params(
            "Resource pagination is not supported.",
            None,
        ));
    }
    Ok(())
}

fn resource_result<T: Serialize>(
    uri: String,
    correlation_id: CorrelationId,
    store_revision: StoreRevision,
    result: T,
) -> Result<ReadResourceResponse, ErrorData> {
    let envelope = ResultEnvelope {
        api_version: ApiVersion::default(),
        correlation_id,
        store_revision,
        result,
    };
    let text = serde_json::to_string(&envelope)
        .map_err(|_| ErrorData::internal_error("Memory resource is unavailable.", None))?;
    Ok(ReadResourceResult::new(vec![
        ResourceContents::text(text, uri).with_mime_type("application/json"),
    ])
    .into())
}

fn resource_not_found() -> ErrorData {
    ErrorData::resource_not_found("Memory resource not found.", None)
}

fn resource_backend_error(error: ReadBackendError) -> ErrorData {
    match &error {
        ReadBackendError::Store(store_error)
            if matches!(
                store_error.code(),
                StoreErrorCode::NotFound
                    | StoreErrorCode::Forbidden
                    | StoreErrorCode::InvalidRequest
                    | StoreErrorCode::InvalidCursor
                    | StoreErrorCode::StaleCursor
            ) =>
        {
            resource_not_found()
        }
        _ => ErrorData::internal_error("Memory resource is unavailable.", None),
    }
}
