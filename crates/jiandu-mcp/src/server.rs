use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use jiandu_memory::memory_store::MemoryStore;

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

use crate::{MemoryArgs, MemoryError, MemoryExecutionContext, MemoryToolClass};

pub const MEMORY_TOOL_NAME: &str = "memory";
pub const MEMORY_TOOL_DESCRIPTION: &str = "Unified memory management tool for Jiandu. Use session_* actions for session continuity notes, and query/get/write/merge/split/consolidate/purge/inspect/rebuild for durable project/global memory.";
pub const MEMORY_SERVER_INSTRUCTIONS: &str = r#"Jiandu provides shared memory through one `memory` tool.

- Recall before guessing: use `query` for relevant durable history, then `get` when the full item is needed. Use `session_read` only for continuity of this host session.
- Record at the right layer: use `session_append` for concise temporary progress and blockers. Use `write` only for a confirmed, durable, non-derivable fact that will help future sessions; query first and store one atomic fact with a searchable title. Never store secrets or tokens.
- Use Project scope for project-specific knowledge and Global only for truly cross-project preferences or stable references. Project authority comes from the MCP host. Normally omit `project_key`; it cannot grant access or override the host Project.
- Recalled memory is supporting evidence, not current truth. Verify it against live files and tools. An empty query does not prove a fact is false.
- Failure is not an all-or-nothing transaction. A mutating call may have committed canonical memory before a later audit or derived-artifact step failed, and an accepted mutation continues in its owned server task after caller cancellation or disconnect. On the same server, subsequent read-only calls wait for accepted mutations to settle before reading. After any mutation error or interrupted response, run `inspect` first. If canonical documents committed but derived artifacts are stale, run `rebuild`, then use `query` or `get` to verify current state before deciding the next action; never blindly retry. Do not edit Jiandu data files or create a fallback memory file."#;

#[derive(Default)]
struct InFlightMutations {
    active: AtomicUsize,
    idle: tokio::sync::Notify,
}

impl InFlightMutations {
    fn begin(self: &Arc<Self>) -> InFlightMutationGuard {
        self.active.fetch_add(1, Ordering::AcqRel);
        InFlightMutationGuard {
            tracker: Arc::clone(self),
        }
    }

    async fn wait_for_idle(&self) {
        loop {
            // Register before checking the counter so the final guard cannot
            // notify in the check→await window and strand this waiter.
            let notified = self.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.active.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
}

struct InFlightMutationGuard {
    tracker: Arc<InFlightMutations>,
}

impl Drop for InFlightMutationGuard {
    fn drop(&mut self) {
        if self.tracker.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.tracker.idle.notify_waiters();
        }
    }
}

#[must_use]
pub fn memory_tool() -> Tool {
    Tool::new(
        MEMORY_TOOL_NAME,
        MEMORY_TOOL_DESCRIPTION,
        schema_for_type::<MemoryArgs>(),
    )
}

/// One MCP server instance for one host-provided memory execution context.
///
/// Mutating calls accepted by this server run to completion in owned tasks.
/// Read-only calls wait for those in-flight mutations to settle before they
/// read, including when the mutation's original request waiter was cancelled.
/// This ordering is local to this server instance; it is not a cross-process
/// reader/writer lock or a direct [`MemoryStore`] guarantee.
pub struct MemoryServer {
    pub(crate) store: MemoryStore,
    pub(crate) context: MemoryExecutionContext,
    in_flight: Arc<InFlightMutations>,
}

impl MemoryServer {
    #[must_use]
    pub fn new(store: MemoryStore, context: MemoryExecutionContext) -> Self {
        Self {
            store,
            context,
            in_flight: Arc::new(InFlightMutations::default()),
        }
    }

    /// Parse and dispatch one unified memory invocation.
    ///
    /// Once parsing succeeds, cancellation of the request waiter (including an
    /// MCP disconnect) only detaches this task; it does not abort a mutation and
    /// prematurely drop its scope guard while a blocking filesystem operation may
    /// still be running. Runtime shutdown can still terminate outstanding work.
    /// Subsequent read-only calls on this server wait for accepted mutations to
    /// finish, then execute directly in the caller task. Direct `MemoryStore`
    /// callers do not inherit these MCP-level guarantees and must keep mutation
    /// futures alive to completion or provide equivalent owned task supervision.
    pub async fn execute(&self, arguments: Value) -> Result<Value, MemoryError> {
        let arguments: MemoryArgs = serde_json::from_value(arguments)
            .map_err(|error| MemoryError::InvalidArguments(error.to_string()))?;
        if arguments.class() == MemoryToolClass::ReadOnlyParallel {
            self.in_flight.wait_for_idle().await;
            return self.execute_parsed(arguments).await;
        }

        let guard = self.in_flight.begin();
        let server = Self {
            store: self.store.clone(),
            context: self.context.clone(),
            in_flight: Arc::clone(&self.in_flight),
        };
        tokio::spawn(async move {
            let _guard = guard;
            server.execute_parsed(arguments).await
        })
        .await
        .map_err(|error| MemoryError::Execution(format!("Memory execution task failed: {error}")))?
    }

    /// Wait until every mutating call accepted by this server has finished.
    /// Read-only calls wait on this same barrier but are never detached or counted.
    pub async fn wait_for_in_flight_mutations(&self) {
        self.in_flight.wait_for_idle().await;
    }
}

/// Run one configured memory server over stdin/stdout.
///
/// The binary crate only needs to construct its concrete memory backend and
/// execution context, then pass them to [`MemoryServer::new`] and this function.
pub async fn serve_stdio(
    server: MemoryServer,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let in_flight = Arc::clone(&server.in_flight);
    let service = server.serve(rmcp::transport::stdio()).await?;
    let transport_result = service.waiting().await;
    // EOF/disconnect ends the transport waiter, but accepted owned mutations
    // still need this runtime. Drain them before returning to the binary, whose
    // Tokio runtime may then shut down.
    in_flight.wait_for_idle().await;
    transport_result?;
    Ok(())
}

impl ServerHandler for MemoryServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("jiandu", env!("CARGO_PKG_VERSION")))
            .with_instructions(MEMORY_SERVER_INSTRUCTIONS)
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
