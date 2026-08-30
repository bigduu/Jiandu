use std::sync::Arc;

use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
    handler::server::tool::schema_for_type,
    model::{
        CallToolRequestMethod, CallToolRequestParams, CallToolResponse, CallToolResult,
        ContentBlock, Implementation, ListToolsResult, PaginatedRequestParams, ServerCapabilities,
        ServerInfo, Tool,
    },
    service::RequestContext,
};
use serde_json::Value;

use crate::{MemoryArgs, MemoryBackend, MemoryError, MemoryExecutionContext, MemoryInvocation};

pub const MEMORY_TOOL_NAME: &str = "memory";
pub const MEMORY_TOOL_DESCRIPTION: &str = "Unified memory management tool for Jiandu. Use session_* actions for session continuity notes, and query/get/write/merge/split/consolidate/purge/inspect/rebuild for durable project/global memory.";

#[must_use]
pub fn memory_tool() -> Tool {
    Tool::new(
        MEMORY_TOOL_NAME,
        MEMORY_TOOL_DESCRIPTION,
        schema_for_type::<MemoryArgs>(),
    )
}

/// One MCP server instance for one host-provided memory execution context.
pub struct MemoryServer {
    backend: Arc<dyn MemoryBackend>,
    context: MemoryExecutionContext,
}

impl MemoryServer {
    #[must_use]
    pub fn new(backend: Arc<dyn MemoryBackend>, context: MemoryExecutionContext) -> Self {
        Self { backend, context }
    }

    /// Transport-independent dispatch used by the stdio adapter and tests.
    pub async fn execute(&self, arguments: Value) -> Result<Value, MemoryError> {
        let arguments: MemoryArgs = serde_json::from_value(arguments)
            .map_err(|error| MemoryError::InvalidArguments(error.to_string()))?;
        let invocation = MemoryInvocation::from_context(&self.context, arguments)?;
        self.backend.execute(invocation).await
    }
}

/// Run one configured memory server over stdin/stdout.
///
/// The binary crate only needs to construct its concrete memory backend and
/// execution context, then pass them to [`MemoryServer::new`] and this function.
pub async fn serve_stdio(
    server: MemoryServer,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    server
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}

impl ServerHandler for MemoryServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("jiandu", env!("CARGO_PKG_VERSION")))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(vec![memory_tool()]))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        (name == MEMORY_TOOL_NAME).then(memory_tool)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        if request.name != MEMORY_TOOL_NAME {
            return Err(McpError::method_not_found::<CallToolRequestMethod>());
        }
        let arguments = Value::Object(request.arguments.unwrap_or_default());
        let result = match self.execute(arguments).await {
            Ok(value) => success_result(value),
            Err(error) => CallToolResult::error(vec![ContentBlock::text(error.to_string())]),
        };
        Ok(result.into())
    }
}

fn success_result(value: Value) -> CallToolResult {
    let text = serde_json::to_string(&value).unwrap_or_else(|_| "{\"success\":false}".to_string());
    let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
    result.structured_content = Some(value);
    result
}
