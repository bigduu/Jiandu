//! MCP tool handler. Transport and daemon lifecycle policy stay outside this
//! adapter crate.

use crate::{
    McpReadBackend, OptionalCapability, ReadBackendError, ReadServiceHealth,
    resource::{self, ResourceRequest},
};
use jiandu_core::{
    API_VERSION, ApiVersion, CorrelationId, DomainError, DomainErrorCode, ErrorEnvelope,
    MemoryGetRequest, MemoryListRequest, MemoryListResult, MemoryRecord, MemorySearchRequest,
    MemorySearchResult, ResultEnvelope, StoreRevision, TrustedRequestContext, Validate,
};
use jiandu_index::IndexErrorCode;
use jiandu_store::{AuthorizedRead, AuthorizedScopes, StoreError, StoreErrorCode};
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

/// One read-only MCP handler per authenticated connection.
pub struct JianduReadServer {
    backend: Arc<dyn McpReadBackend>,
    authorization: AuthorizedRead,
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
            tool_router: Self::tool_router(),
        }
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

    fn invalid_request(&self, correlation_id: CorrelationId) -> CallToolResult {
        let revision = self.backend.store_revision().unwrap_or(StoreRevision(0));
        error_result(
            correlation_id,
            revision,
            DomainErrorCode::InvalidArgument,
            "The memory request is invalid.",
        )
    }

    fn backend_error(
        &self,
        correlation_id: CorrelationId,
        error: ReadBackendError,
    ) -> CallToolResult {
        let revision = self.backend.store_revision().unwrap_or(StoreRevision(0));
        let (code, message) = public_error(&error);
        error_result(correlation_id, revision, code, message)
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
    code: DomainErrorCode,
    message: &'static str,
) -> CallToolResult {
    let envelope = ErrorEnvelope {
        api_version: ApiVersion::default(),
        correlation_id,
        store_revision,
        error: DomainError::new(code, message),
    };
    let structured = serde_json::to_value(ToolEnvelope::<Value>::Error(envelope))
        .expect("public error envelopes are JSON serializable");
    let mut output = CallToolResult::structured_error(structured);
    output.content = vec![ContentBlock::text(format!("Memory read failed: {code:?}."))];
    output
}

fn public_error(error: &ReadBackendError) -> (DomainErrorCode, &'static str) {
    match error {
        ReadBackendError::Store(error) => match error.code() {
            StoreErrorCode::Unauthenticated => (
                DomainErrorCode::Unauthenticated,
                "The connection is not authenticated.",
            ),
            StoreErrorCode::Forbidden => (
                DomainErrorCode::Forbidden,
                "The memory operation is not authorized.",
            ),
            StoreErrorCode::InvalidRequest
            | StoreErrorCode::InvalidCursor
            | StoreErrorCode::StaleCursor => (
                DomainErrorCode::InvalidArgument,
                "The memory request is invalid.",
            ),
            StoreErrorCode::NotFound => (DomainErrorCode::NotFound, "The memory is not available."),
            _ => (
                DomainErrorCode::StoreUnavailable,
                "Canonical memory storage is unavailable.",
            ),
        },
        ReadBackendError::Index(error) => match error.code() {
            IndexErrorCode::InvalidRequest
            | IndexErrorCode::InvalidCursor
            | IndexErrorCode::StaleCursor => (
                DomainErrorCode::InvalidArgument,
                "The memory request is invalid.",
            ),
            IndexErrorCode::Unauthenticated => (
                DomainErrorCode::Unauthenticated,
                "The connection is not authenticated.",
            ),
            IndexErrorCode::Forbidden => (
                DomainErrorCode::Forbidden,
                "The memory operation is not authorized.",
            ),
            _ => (
                DomainErrorCode::IndexDegraded,
                "Lexical memory search is temporarily unavailable.",
            ),
        },
        ReadBackendError::UnstableSearchSnapshot => (
            DomainErrorCode::IndexDegraded,
            "Lexical memory search is temporarily unavailable.",
        ),
        ReadBackendError::HostUnavailable => (
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
