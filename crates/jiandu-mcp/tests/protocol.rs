use jiandu_core::{
    ClientId, CreationActor, ErrorEnvelope, Grant, IdempotencyKey, ListSort, MemoryGetRequest,
    MemoryListRequest, MemoryListResult, MemoryRecord, MemoryScope, MemorySearchRequest,
    MemorySearchResult, MemoryType, PageLimit, PrincipalId, ProjectId, ProvenanceInput,
    RememberMemoryCommand, ResultEnvelope, ScopeSelector, Tag, Timestamp, TrustedRequestContext,
};
use jiandu_index::{CursorMacKey, LexicalIndex};
use jiandu_mcp::{
    CanonicalReadBackend, IndexReadHealth, JianduReadServer, McpReadBackend, ReadServiceHealth,
    StoreReadHealth,
};
use jiandu_store::{
    AuthorizedRead, AuthorizedScopes, CanonicalStore, LockOwner, MutationOperation,
};
use rmcp::{
    ClientHandler, ServiceError, ServiceExt,
    model::{
        CallToolRequestParams, ClientInfo, ErrorData, JsonObject, ProtocolVersion,
        ReadResourceRequestParams, ResourceContents,
    },
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};
use tempfile::TempDir;

#[derive(Debug, Clone)]
struct V2025Client;

impl ClientHandler for V2025Client {
    fn get_info(&self) -> ClientInfo {
        let mut info = ClientInfo::default();
        info.protocol_version = ProtocolVersion::V_2025_11_25;
        info
    }
}

struct Fixture {
    _root: TempDir,
    store: Arc<RwLock<CanonicalStore>>,
    backend: Arc<CanonicalReadBackend>,
    authorization: AuthorizedRead,
    principal: PrincipalId,
}

#[tokio::test]
async fn duplex_tools_match_direct_store_and_index_results_with_safe_pagination() {
    let fixture = fixture();
    let get_request = MemoryGetRequest {
        memory_id: jiandu_core::MemoryId::new("mem_mcp_alpha_a").expect("memory ID"),
    };
    let direct_get = fixture
        .backend
        .get(&fixture.authorization, &get_request)
        .expect("direct get");

    let mut list_page_request = list_request(vec![ScopeSelector::Principal {}], 1);
    let direct_list = fixture
        .backend
        .list(&fixture.authorization, &list_page_request)
        .expect("direct list");
    let mut search_page_request = search_request("alpha", vec![ScopeSelector::Principal {}], 1);
    let direct_search = fixture
        .backend
        .search(&fixture.authorization, &search_page_request)
        .expect("direct search");

    let server =
        JianduReadServer::from_authorized(fixture.backend.clone(), fixture.authorization.clone());
    let (server_transport, client_transport) = tokio::io::duplex(256 * 1024);
    let server_task = tokio::spawn(async move {
        server
            .serve(server_transport)
            .await
            .expect("server starts")
            .waiting()
            .await
            .expect("server stops cleanly");
    });
    let client = V2025Client
        .serve(client_transport)
        .await
        .expect("client connects");

    let info = client.peer_info().expect("initialize response");
    assert_eq!(info.protocol_version, ProtocolVersion::V_2025_11_25);
    assert_eq!(
        info.server_info
            .as_ref()
            .expect("server implementation")
            .name,
        "jiandu"
    );
    assert!(info.instructions.is_none());

    let tools = client.list_all_tools().await.expect("list tools");
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.name.as_ref())
            .collect::<Vec<_>>(),
        ["memory_get", "memory_list", "memory_search"]
    );

    let get_result = client
        .call_tool(CallToolRequestParams::new("memory_get").with_arguments(arguments(&get_request)))
        .await
        .expect("get tool");
    let get_envelope: ResultEnvelope<MemoryRecord> = success_envelope(&get_result);
    assert_eq!(get_envelope.store_revision, direct_get.store_revision);
    assert_eq!(get_envelope.result, direct_get.result);
    assert_concise_text(&get_result, &["mem_mcp_alpha_a", "alpha body a"]);
    assert_legacy_result_shape(&get_result);

    let list_result = client
        .call_tool(
            CallToolRequestParams::new("memory_list").with_arguments(arguments(&list_page_request)),
        )
        .await
        .expect("list tool");
    let list_envelope: ResultEnvelope<MemoryListResult> = success_envelope(&list_result);
    assert_eq!(list_envelope.store_revision, direct_list.store_revision);
    assert_eq!(list_envelope.result, direct_list.result);
    assert!(list_envelope.result.has_more);
    list_page_request.cursor = list_envelope.result.next_cursor.clone();
    let second_list = client
        .call_tool(
            CallToolRequestParams::new("memory_list").with_arguments(arguments(&list_page_request)),
        )
        .await
        .expect("second list page");
    let second_list: ResultEnvelope<MemoryListResult> = success_envelope(&second_list);
    assert_ne!(
        list_envelope.result.memories[0].id,
        second_list.result.memories[0].id
    );

    let search_result = client
        .call_tool(
            CallToolRequestParams::new("memory_search")
                .with_arguments(arguments(&search_page_request)),
        )
        .await
        .expect("search tool");
    let search_envelope: ResultEnvelope<MemorySearchResult> = success_envelope(&search_result);
    assert_eq!(search_envelope.store_revision, direct_search.0);
    assert_eq!(search_envelope.result, direct_search.1);
    assert!(search_envelope.result.has_more);
    assert_concise_text(&search_result, &["alpha", "mem_mcp"]);
    search_page_request.cursor = search_envelope.result.next_cursor.clone();
    let second_search = client
        .call_tool(
            CallToolRequestParams::new("memory_search")
                .with_arguments(arguments(&search_page_request)),
        )
        .await
        .expect("second search page");
    let second_search: ResultEnvelope<MemorySearchResult> = success_envelope(&second_search);
    assert_ne!(
        search_envelope.result.memories[0].id,
        second_search.result.memories[0].id
    );

    let isolated_search = client
        .call_tool(
            CallToolRequestParams::new("memory_search").with_arguments(arguments(&search_request(
                "foreign-only-sentinel",
                vec![ScopeSelector::Principal {}],
                10,
            ))),
        )
        .await
        .expect("scope-isolated search");
    let isolated_envelope: ResultEnvelope<MemorySearchResult> = success_envelope(&isolated_search);
    assert!(isolated_envelope.result.memories.is_empty());
    let isolated_wire = serde_json::to_string(&isolated_search).expect("isolated search JSON");
    assert!(!isolated_wire.contains("mem_mcp_foreign"));
    assert!(!isolated_wire.contains("foreign-only-sentinel"));

    let mixed_scope = list_request(
        vec![
            ScopeSelector::Principal {},
            ScopeSelector::Project {
                project_id: ProjectId::new("prj_mcp_foreign").expect("foreign project ID"),
            },
        ],
        10,
    );
    let forbidden = client
        .call_tool(
            CallToolRequestParams::new("memory_list").with_arguments(arguments(&mixed_scope)),
        )
        .await
        .expect("tool-level forbidden response");
    let forbidden_envelope = error_envelope(&forbidden);
    assert_eq!(
        forbidden_envelope.error.code,
        jiandu_core::DomainErrorCode::Forbidden
    );
    let forbidden_wire = serde_json::to_string(&forbidden).expect("forbidden JSON");
    assert!(!forbidden_wire.contains("mem_mcp_alpha"));

    let malformed = client
        .call_tool(
            CallToolRequestParams::new("memory_get").with_arguments(
                json!({"memoryId": "mem_mcp_alpha_a", "principalId": fixture.principal})
                    .as_object()
                    .expect("object")
                    .clone(),
            ),
        )
        .await
        .expect("structured invalid response");
    assert_eq!(
        error_envelope(&malformed).error.code,
        jiandu_core::DomainErrorCode::InvalidArgument
    );

    let absent_get = client
        .call_tool(
            CallToolRequestParams::new("memory_get").with_arguments(arguments(&MemoryGetRequest {
                memory_id: jiandu_core::MemoryId::new("mem_mcp_absent").expect("absent ID"),
            })),
        )
        .await
        .expect("absent tool envelope");
    let inaccessible_get = client
        .call_tool(
            CallToolRequestParams::new("memory_get").with_arguments(arguments(&MemoryGetRequest {
                memory_id: jiandu_core::MemoryId::new("mem_mcp_foreign").expect("foreign ID"),
            })),
        )
        .await
        .expect("inaccessible tool envelope");
    let absent_envelope = error_envelope(&absent_get);
    let inaccessible_envelope = error_envelope(&inaccessible_get);
    assert_eq!(
        absent_envelope.api_version,
        inaccessible_envelope.api_version
    );
    assert_eq!(
        absent_envelope.store_revision,
        inaccessible_envelope.store_revision
    );
    assert_eq!(absent_envelope.error.code, inaccessible_envelope.error.code);
    assert_eq!(
        absent_envelope.error.message,
        inaccessible_envelope.error.message
    );
    assert_eq!(
        absent_envelope.error.retryable,
        inaccessible_envelope.error.retryable
    );
    assert_eq!(
        absent_envelope.error.details,
        inaccessible_envelope.error.details
    );
    assert_eq!(summary_text(&absent_get), summary_text(&inaccessible_get));

    let narrowed_scopes = AuthorizedScopes::new(fixture.principal.clone());
    let narrowed_authorization = narrowed_scopes
        .authorize_read(&TrustedRequestContext {
            principal_id: fixture.principal.clone(),
            client_id: ClientId::new("cli_mcp_narrowed").expect("client ID"),
            grants: BTreeSet::from([Grant::new("memory:read").expect("read grant")]),
        })
        .expect("narrowed read authority");
    let narrowed_server =
        JianduReadServer::from_authorized(fixture.backend.clone(), narrowed_authorization);
    let (narrow_server_transport, narrow_client_transport) = tokio::io::duplex(128 * 1024);
    let narrow_server_task = tokio::spawn(async move {
        narrowed_server
            .serve(narrow_server_transport)
            .await
            .expect("narrowed server starts")
            .waiting()
            .await
            .expect("narrowed server stops");
    });
    let narrow_client = V2025Client
        .serve(narrow_client_transport)
        .await
        .expect("narrowed client connects");
    for (tool, request) in [
        ("memory_list", arguments(&list_page_request)),
        ("memory_search", arguments(&search_page_request)),
    ] {
        let replay = narrow_client
            .call_tool(CallToolRequestParams::new(tool).with_arguments(request))
            .await
            .expect("stale authority cursor envelope");
        assert_eq!(
            error_envelope(&replay).error.code,
            jiandu_core::DomainErrorCode::InvalidArgument
        );
        let wire = serde_json::to_string(&replay).expect("cursor error JSON");
        assert!(!wire.contains("mem_mcp_"));
    }
    narrow_client
        .cancel()
        .await
        .expect("cancel narrowed client");
    narrow_server_task.await.expect("join narrowed server");

    client.cancel().await.expect("cancel client");
    server_task.await.expect("join server");
}

#[tokio::test]
async fn resources_are_query_free_authorized_and_hide_absence_from_inaccessibility() {
    let fixture = fixture();
    let server =
        JianduReadServer::from_authorized(fixture.backend.clone(), fixture.authorization.clone());
    let (server_transport, client_transport) = tokio::io::duplex(256 * 1024);
    let server_task = tokio::spawn(async move {
        server
            .serve(server_transport)
            .await
            .expect("server starts")
            .waiting()
            .await
            .expect("server stops cleanly");
    });
    let client = V2025Client
        .serve(client_transport)
        .await
        .expect("client connects");

    let resources = client.list_all_resources().await.expect("list resources");
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].uri, "jiandu://scope/principal/memories");
    let templates = client
        .list_all_resource_templates()
        .await
        .expect("list resource templates");
    assert_eq!(templates.len(), 4);
    assert!(templates.iter().all(|template| {
        !template.uri_template.contains('?') && !template.uri_template.contains("query")
    }));

    let exact = client
        .read_resource(ReadResourceRequestParams::new(
            "jiandu://memory/mem_mcp_alpha_a",
        ))
        .await
        .expect("exact resource");
    assert_legacy_resource_shape(&exact);
    let exact_envelope: ResultEnvelope<MemoryRecord> = resource_envelope(&exact);
    assert_eq!(exact_envelope.result.id.as_str(), "mem_mcp_alpha_a");

    let principal_list = client
        .read_resource(ReadResourceRequestParams::new(
            "jiandu://scope/principal/memories",
        ))
        .await
        .expect("principal list resource");
    let principal_envelope: ResultEnvelope<MemoryListResult> = resource_envelope(&principal_list);
    assert_eq!(principal_envelope.result.memories.len(), 2);
    assert!(
        principal_envelope
            .result
            .memories
            .iter()
            .all(|memory| memory.scope
                == MemoryScope::Principal {
                    principal_id: fixture.principal.clone()
                })
    );

    let absent = resource_error(
        client
            .read_resource(ReadResourceRequestParams::new(
                "jiandu://memory/mem_mcp_absent",
            ))
            .await
            .expect_err("absent resource"),
    );
    let inaccessible = resource_error(
        client
            .read_resource(ReadResourceRequestParams::new(
                "jiandu://memory/mem_mcp_foreign",
            ))
            .await
            .expect_err("inaccessible resource"),
    );
    let unauthorized_scope = resource_error(
        client
            .read_resource(ReadResourceRequestParams::new(
                "jiandu://scope/project/prj_mcp_foreign/memories",
            ))
            .await
            .expect_err("unauthorized scope resource"),
    );
    let query_uri = resource_error(
        client
            .read_resource(ReadResourceRequestParams::new(
                "jiandu://scope/principal/memories?query=secret",
            ))
            .await
            .expect_err("query-bearing resource URI"),
    );
    for error in [&inaccessible, &unauthorized_scope, &query_uri] {
        assert_eq!(error.code, absent.code);
        assert_eq!(error.message, absent.message);
        assert_eq!(error.data, absent.data);
    }

    client.cancel().await.expect("cancel client");
    server_task.await.expect("join server");
}

#[tokio::test]
async fn missing_index_is_explicitly_degraded_while_get_and_list_remain_available() {
    let fixture = fixture();
    let missing_backend = Arc::new(CanonicalReadBackend::new(
        fixture.store.clone(),
        LexicalIndex::new(fixture._root.path().join("missing-index")),
        CursorMacKey::new([0x91; 32]),
        ReadServiceHealth::new(StoreReadHealth::Ready, IndexReadHealth::Missing),
    ));
    let server = JianduReadServer::from_authorized(missing_backend, fixture.authorization.clone());
    let (server_transport, client_transport) = tokio::io::duplex(256 * 1024);
    let server_task = tokio::spawn(async move {
        server
            .serve(server_transport)
            .await
            .expect("server starts")
            .waiting()
            .await
            .expect("server stops cleanly");
    });
    let client = V2025Client
        .serve(client_transport)
        .await
        .expect("client connects");

    let info = client.peer_info().expect("server info");
    let health = &info.capabilities.experimental.as_ref().expect("health")["jiandu"]["health"];
    assert_eq!(health["index"], "missing");
    assert_eq!(health["exactRead"], true);
    assert_eq!(health["list"], true);
    assert_eq!(health["search"], false);

    let search =
        client
            .call_tool(
                CallToolRequestParams::new("memory_search").with_arguments(arguments(
                    &search_request("alpha", vec![ScopeSelector::Principal {}], 10),
                )),
            )
            .await
            .expect("degraded search envelope");
    assert_eq!(
        error_envelope(&search).error.code,
        jiandu_core::DomainErrorCode::IndexDegraded
    );

    let get = client
        .call_tool(
            CallToolRequestParams::new("memory_get").with_arguments(arguments(&MemoryGetRequest {
                memory_id: jiandu_core::MemoryId::new("mem_mcp_alpha_a").expect("memory ID"),
            })),
        )
        .await
        .expect("get remains available");
    let _: ResultEnvelope<MemoryRecord> = success_envelope(&get);
    let list = client
        .call_tool(
            CallToolRequestParams::new("memory_list").with_arguments(arguments(&list_request(
                vec![ScopeSelector::Principal {}],
                10,
            ))),
        )
        .await
        .expect("list remains available");
    let _: ResultEnvelope<MemoryListResult> = success_envelope(&list);

    client.cancel().await.expect("cancel client");
    server_task.await.expect("join server");
}

fn fixture() -> Fixture {
    let root = TempDir::new().expect("temporary store");
    let principal = PrincipalId::new("prn_mcp_a").expect("principal ID");
    let foreign = PrincipalId::new("prn_mcp_b").expect("foreign principal ID");
    let project = ProjectId::new("prj_mcp_a").expect("project ID");
    let authority = AuthorizedScopes::new(principal.clone()).with_project(project.clone());
    let foreign_authority = AuthorizedScopes::new(foreign.clone());
    let mut store = CanonicalStore::initialize(
        root.path(),
        LockOwner::for_current_process().expect("lock owner"),
    )
    .expect("initialize store");
    create_record(
        &mut store,
        &authority,
        &principal,
        MemoryScope::Principal {
            principal_id: principal.clone(),
        },
        "mem_mcp_alpha_a",
        "Alpha title a",
        "alpha body a",
    );
    create_record(
        &mut store,
        &authority,
        &principal,
        MemoryScope::Principal {
            principal_id: principal.clone(),
        },
        "mem_mcp_alpha_b",
        "Alpha title b",
        "alpha body b",
    );
    create_record(
        &mut store,
        &authority,
        &principal,
        MemoryScope::Project {
            project_id: project.clone(),
        },
        "mem_mcp_project",
        "Project alpha",
        "project alpha body",
    );
    create_record(
        &mut store,
        &foreign_authority,
        &foreign,
        MemoryScope::Principal {
            principal_id: foreign.clone(),
        },
        "mem_mcp_foreign",
        "Foreign alpha",
        "foreign-only-sentinel",
    );

    let index_directory = root.path().join("index");
    assert!(index_directory.is_dir());
    let index = LexicalIndex::new(&index_directory);
    let index_admin = authority
        .authorize_index_rebuild(&TrustedRequestContext {
            principal_id: principal.clone(),
            client_id: ClientId::new("cli_mcp_index_admin").expect("client ID"),
            grants: BTreeSet::from(
                [Grant::new("memory:admin:rebuild_index").expect("admin grant")],
            ),
        })
        .expect("index admin");
    index.rebuild(&store, &index_admin).expect("rebuild index");
    let authorization = authority
        .authorize_read(&TrustedRequestContext {
            principal_id: principal.clone(),
            client_id: ClientId::new("cli_mcp_connection").expect("client ID"),
            grants: BTreeSet::from([Grant::new("memory:read").expect("read grant")]),
        })
        .expect("read authorization");
    let store = Arc::new(RwLock::new(store));
    let backend = Arc::new(CanonicalReadBackend::new(
        store.clone(),
        LexicalIndex::new(&index_directory),
        CursorMacKey::new([0x77; 32]),
        ReadServiceHealth::new(StoreReadHealth::Ready, IndexReadHealth::Ready),
    ));
    Fixture {
        _root: root,
        store,
        backend,
        authorization,
        principal,
    }
}

fn create_record(
    store: &mut CanonicalStore,
    authority: &AuthorizedScopes,
    principal: &PrincipalId,
    scope: MemoryScope,
    id: &str,
    title: &str,
    body: &str,
) {
    let grant = match &scope {
        MemoryScope::Principal { .. } => "memory:write:principal",
        MemoryScope::Project { .. } => "memory:write:project",
        MemoryScope::Session { .. } => "memory:write:session",
        MemoryScope::InstanceGlobal {} => "memory:write:instance_global",
    };
    let authorization = authority
        .authorize_mutation(
            &TrustedRequestContext {
                principal_id: principal.clone(),
                client_id: ClientId::new("cli_mcp_fixture_writer").expect("client ID"),
                grants: BTreeSet::from([Grant::new(grant).expect("write grant")]),
            },
            &scope,
            MutationOperation::Create,
        )
        .expect("create authorization");
    let selector = match &scope {
        MemoryScope::Principal { .. } => ScopeSelector::Principal {},
        MemoryScope::Project { project_id } => ScopeSelector::Project {
            project_id: project_id.clone(),
        },
        MemoryScope::Session { session_id } => ScopeSelector::Session {
            session_id: session_id.clone(),
        },
        MemoryScope::InstanceGlobal {} => ScopeSelector::InstanceGlobal {},
    };
    store
        .create(
            &authorization,
            &RememberMemoryCommand {
                scope: selector,
                memory_type: MemoryType::Decision,
                title: title.to_owned(),
                summary: Some(format!("summary for {id}")),
                body: body.to_owned(),
                tags: vec![Tag::new("mcp-fixture").expect("tag")],
                provenance: ProvenanceInput::default(),
                relations: Vec::new(),
                idempotency_key: IdempotencyKey::new(format!("create-{id}"))
                    .expect("idempotency key"),
            },
            jiandu_core::MemoryId::new(id).expect("memory ID"),
            CreationActor::Host,
            Timestamp::new("2026-08-25T00:00:00Z").expect("timestamp"),
        )
        .expect("create record");
}

fn list_request(scopes: Vec<ScopeSelector>, limit: u16) -> MemoryListRequest {
    MemoryListRequest {
        scopes,
        types: Vec::new(),
        statuses: Vec::new(),
        tags: Vec::new(),
        updated_after: None,
        sort: ListSort::IdAsc,
        limit: PageLimit::new(limit).expect("limit"),
        cursor: None,
    }
}

fn search_request(query: &str, scopes: Vec<ScopeSelector>, limit: u16) -> MemorySearchRequest {
    MemorySearchRequest {
        query: query.to_owned(),
        scopes,
        types: Vec::new(),
        statuses: Vec::new(),
        tags: Vec::new(),
        updated_after: None,
        limit: PageLimit::new(limit).expect("limit"),
        cursor: None,
    }
}

fn arguments(value: &impl Serialize) -> JsonObject {
    serde_json::to_value(value)
        .expect("request JSON")
        .as_object()
        .expect("request object")
        .clone()
}

fn success_envelope<T: DeserializeOwned>(
    result: &rmcp::model::CallToolResult,
) -> ResultEnvelope<T> {
    assert_eq!(result.is_error, Some(false));
    serde_json::from_value(
        result
            .structured_content
            .clone()
            .expect("authoritative structured content"),
    )
    .expect("success envelope")
}

fn error_envelope(result: &rmcp::model::CallToolResult) -> ErrorEnvelope {
    assert_eq!(result.is_error, Some(true));
    serde_json::from_value(
        result
            .structured_content
            .clone()
            .expect("authoritative structured error"),
    )
    .expect("error envelope")
}

fn assert_concise_text(result: &rmcp::model::CallToolResult, forbidden: &[&str]) {
    assert_eq!(result.content.len(), 1);
    let text = result.content[0].as_text().expect("text summary");
    assert!(text.text.len() < 100);
    for sentinel in forbidden {
        assert!(!text.text.contains(sentinel));
    }
}

fn summary_text(result: &rmcp::model::CallToolResult) -> &str {
    result.content[0]
        .as_text()
        .expect("text summary")
        .text
        .as_str()
}

fn assert_legacy_result_shape(result: &rmcp::model::CallToolResult) {
    let wire = serde_json::to_value(result).expect("tool result JSON");
    assert!(wire.get("resultType").is_none());
}

fn assert_legacy_resource_shape(result: &rmcp::model::ReadResourceResult) {
    let wire = serde_json::to_value(result).expect("resource result JSON");
    assert!(wire.get("resultType").is_none());
    assert!(wire.get("cacheScope").is_none());
    assert!(wire.get("ttlMs").is_none());
}

fn resource_envelope<T: DeserializeOwned>(
    result: &rmcp::model::ReadResourceResult,
) -> ResultEnvelope<T> {
    assert_eq!(result.contents.len(), 1);
    let ResourceContents::TextResourceContents { text, .. } = &result.contents[0] else {
        panic!("expected text resource")
    };
    serde_json::from_str(text).expect("resource result envelope")
}

fn resource_error(error: ServiceError) -> ErrorData {
    match error {
        ServiceError::McpError(error) => error,
        other => panic!("expected MCP resource error, got {other:?}"),
    }
}
