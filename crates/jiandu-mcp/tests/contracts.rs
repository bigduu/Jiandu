use jiandu_core::{
    ClientId, Grant, MemoryGetRequest, MemoryListRequest, MemoryListResult, MemoryRecord,
    MemorySearchRequest, MemorySearchResult, PrincipalId, ProjectId, ScopeSelector, StoreRevision,
    TrustedRequestContext,
};
use jiandu_mcp::{
    IndexReadHealth, JIANDU_MCP_PROTOCOL_REVISION, JianduReadServer, McpReadBackend,
    OptionalCapability, ReadBackendError, ReadServiceHealth, StoreReadHealth,
};
use jiandu_store::{AuthorizedRead, AuthorizedScopes, StoreErrorCode, StoreRead};
use rmcp::{ServerHandler, model::ProtocolVersion};
use serde_json::Value;
use std::collections::BTreeSet;
use std::sync::Arc;

struct UnavailableBackend {
    health: ReadServiceHealth,
}

impl McpReadBackend for UnavailableBackend {
    fn get(
        &self,
        _authorization: &AuthorizedRead,
        _request: &MemoryGetRequest,
    ) -> Result<StoreRead<MemoryRecord>, ReadBackendError> {
        Err(ReadBackendError::HostUnavailable)
    }

    fn list(
        &self,
        _authorization: &AuthorizedRead,
        _request: &MemoryListRequest,
    ) -> Result<StoreRead<MemoryListResult>, ReadBackendError> {
        Err(ReadBackendError::HostUnavailable)
    }

    fn search(
        &self,
        _authorization: &AuthorizedRead,
        _request: &MemorySearchRequest,
    ) -> Result<(StoreRevision, MemorySearchResult), ReadBackendError> {
        Err(ReadBackendError::HostUnavailable)
    }

    fn store_revision(&self) -> Result<StoreRevision, ReadBackendError> {
        Err(ReadBackendError::HostUnavailable)
    }

    fn health(&self) -> ReadServiceHealth {
        self.health.clone()
    }
}

#[test]
fn trusted_read_capability_requires_context_principal_and_read_grant() {
    let principal = PrincipalId::new("prn_mcp_contract").expect("principal ID");
    let scopes = AuthorizedScopes::new(principal.clone())
        .with_project(ProjectId::new("prj_mcp_contract").expect("project ID"));

    let read = scopes
        .authorize_read(&context(&principal, &["memory:read"]))
        .expect("read grant");
    let debug = format!("{read:?}");
    assert!(!debug.contains(principal.as_str()));
    assert!(!debug.contains("cli_mcp_contract"));

    let backend: Arc<dyn McpReadBackend> = Arc::new(UnavailableBackend {
        health: ReadServiceHealth::new(StoreReadHealth::Ready, IndexReadHealth::Missing),
    });
    assert!(
        JianduReadServer::new(
            backend.clone(),
            &scopes,
            &context(&principal, &["memory:read"])
        )
        .is_ok()
    );
    let constructor_error = match JianduReadServer::new(
        backend,
        &scopes,
        &context(&principal, &["memory:write:principal"]),
    ) {
        Ok(_) => panic!("write-only connection must be rejected"),
        Err(error) => error,
    };
    assert_eq!(constructor_error.code(), StoreErrorCode::Forbidden);

    assert_eq!(
        scopes
            .authorize_read(&context(&principal, &["memory:write:principal"]))
            .expect_err("write grant is not read authority")
            .code(),
        StoreErrorCode::Forbidden
    );
    let other = PrincipalId::new("prn_mcp_other").expect("other principal ID");
    assert_eq!(
        scopes
            .authorize_read(&context(&other, &["memory:read"]))
            .expect_err("principal mismatch")
            .code(),
        StoreErrorCode::Forbidden
    );

    let unauthorized_search = MemorySearchRequest {
        query: "alpha".to_owned(),
        scopes: vec![ScopeSelector::Project {
            project_id: ProjectId::new("prj_mcp_foreign").expect("foreign project ID"),
        }],
        types: Vec::new(),
        statuses: Vec::new(),
        tags: Vec::new(),
        updated_after: None,
        limit: jiandu_core::PageLimit::new(10).expect("limit"),
        cursor: None,
    };
    assert_eq!(
        read.authorize_index_query(&unauthorized_search)
            .expect_err("foreign selector is rejected")
            .code(),
        StoreErrorCode::Forbidden
    );
}

#[test]
fn tools_publish_the_checked_core_request_schemas_without_identity_or_path_fields() {
    let principal = PrincipalId::new("prn_mcp_schema").expect("principal ID");
    let authorization = AuthorizedScopes::new(principal.clone())
        .authorize_read(&context(&principal, &["memory:read"]))
        .expect("read authorization");
    let server = JianduReadServer::from_authorized(
        Arc::new(UnavailableBackend {
            health: ReadServiceHealth::new(StoreReadHealth::Ready, IndexReadHealth::Missing),
        }),
        authorization,
    );
    let checked = jiandu_core::generated_contract_schemas();
    for (tool_name, schema_name, read_only, destructive) in [
        (
            "memory_search",
            "memory-search-request.schema.json",
            true,
            false,
        ),
        ("memory_get", "memory-get-request.schema.json", true, false),
        (
            "memory_list",
            "memory-list-request.schema.json",
            true,
            false,
        ),
        (
            "memory_remember",
            "remember-memory-command.schema.json",
            false,
            false,
        ),
        (
            "memory_update",
            "update-memory-command.schema.json",
            false,
            false,
        ),
        (
            "memory_forget",
            "forget-memory-command.schema.json",
            false,
            true,
        ),
    ] {
        let tool = server.get_tool(tool_name).expect("fixed memory tool");
        let actual = Value::Object(tool.input_schema.as_ref().clone());
        assert_eq!(actual, checked[schema_name], "{tool_name} schema drift");
        let bytes = serde_json::to_string(&actual).expect("schema JSON");
        for forbidden in [
            "principalId",
            "clientId",
            "filesystemPath",
            "workspacePath",
            "promptPlacement",
        ] {
            assert!(
                !bytes.contains(forbidden),
                "{tool_name} exposed {forbidden}"
            );
        }
        let annotations = tool.annotations.expect("tool annotations");
        assert_eq!(annotations.read_only_hint, Some(read_only));
        assert_eq!(annotations.destructive_hint, Some(destructive));
        assert_eq!(annotations.idempotent_hint, Some(true));
        assert_eq!(annotations.open_world_hint, Some(false));
        assert!(tool.output_schema.is_some());
    }
}

#[test]
fn initialize_separates_protocol_and_api_versions_and_exposes_only_closed_health() {
    let principal = PrincipalId::new("prn_mcp_info").expect("principal ID");
    let authorization = AuthorizedScopes::new(principal.clone())
        .authorize_read(&context(&principal, &["memory:read"]))
        .expect("read authorization");
    let server = JianduReadServer::from_authorized(
        Arc::new(UnavailableBackend {
            health: ReadServiceHealth::new(StoreReadHealth::Ready, IndexReadHealth::Degraded),
        }),
        authorization,
    );
    let info = server.get_info();
    assert_eq!(JIANDU_MCP_PROTOCOL_REVISION, "2025-11-25");
    assert_eq!(info.protocol_version, ProtocolVersion::V_2025_11_25);
    assert_ne!(JIANDU_MCP_PROTOCOL_REVISION, jiandu_core::API_VERSION);
    assert!(info.capabilities.tools.is_some());
    assert!(info.capabilities.resources.is_some());
    assert!(info.capabilities.prompts.is_none());

    let experimental = info.capabilities.experimental.expect("experimental health");
    let jiandu = experimental.get("jiandu").expect("jiandu metadata");
    assert_eq!(jiandu["apiVersion"], jiandu_core::API_VERSION);
    assert_eq!(jiandu["health"]["store"], "ready");
    assert_eq!(jiandu["health"]["index"], "degraded");
    assert_eq!(jiandu["health"]["exactRead"], true);
    assert_eq!(jiandu["health"]["list"], true);
    assert_eq!(jiandu["health"]["search"], false);
    assert_eq!(
        jiandu["optionalCapabilities"],
        serde_json::to_value([OptionalCapability::Resources]).expect("capability JSON")
    );
    let wire = serde_json::to_string(&jiandu).expect("health JSON");
    for forbidden in ["count", "watermark", "revision", "path", "reason"] {
        assert!(!wire.to_ascii_lowercase().contains(forbidden));
    }
}

fn context(principal_id: &PrincipalId, grants: &[&str]) -> TrustedRequestContext {
    TrustedRequestContext {
        principal_id: principal_id.clone(),
        client_id: ClientId::new("cli_mcp_contract").expect("client ID"),
        grants: grants
            .iter()
            .map(|grant| Grant::new(*grant).expect("grant"))
            .collect::<BTreeSet<_>>(),
    }
}
