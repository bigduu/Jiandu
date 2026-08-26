use super::{
    ConformanceManifest, INVALID_TOKEN, NO_FORGET_TOKEN, OFFICIAL_TOKEN, PublicMcpDriver,
    RAW_TOKEN, SuiteContext, assert_fixed_unauthorized,
};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

pub(crate) async fn run<D: PublicMcpDriver>(
    endpoint: &str,
    token: &str,
    context: &SuiteContext,
    store_path: &Path,
    manifest: &ConformanceManifest,
) {
    assert_fixed_unauthorized(endpoint, manifest).await;
    assert!(
        D::rejects_invalid_credential(endpoint).await,
        "{} accepted an invalid credential",
        D::NAME
    );
    let mut client = D::connect(endpoint, token)
        .await
        .unwrap_or_else(|error| panic!("{} initialize failed: {error}", D::NAME));
    assert_discovery(&mut client, manifest).await;
    assert_scope_isolation(&mut client, context, store_path, manifest).await;
    assert_reads(&mut client, context, manifest).await;
    assert_mutations_and_errors(&mut client, context, manifest).await;
    client
        .close()
        .await
        .unwrap_or_else(|error| panic!("{} close failed: {error}", D::NAME));
}

async fn assert_scope_isolation<D: PublicMcpDriver>(
    client: &mut D,
    context: &SuiteContext,
    store_path: &Path,
    manifest: &ConformanceManifest,
) {
    let authorized_scopes = [
        (
            json!({ "kind": "principal" }),
            context.own.principal_memory,
            context.own.principal_query,
            context.foreign.principal_memory,
            context.foreign.principal_query,
            "jiandu://scope/principal/memories".to_owned(),
        ),
        (
            json!({ "kind": "project", "projectId": context.own.project_id }),
            context.own.project_memory,
            context.own.project_query,
            context.foreign.project_memory,
            context.foreign.project_query,
            format!("jiandu://scope/project/{}/memories", context.own.project_id),
        ),
        (
            json!({ "kind": "session", "sessionId": context.own.session_id }),
            context.own.session_memory,
            context.own.session_query,
            context.foreign.session_memory,
            context.foreign.session_query,
            format!("jiandu://scope/session/{}/memories", context.own.session_id),
        ),
    ];
    for (selector, own_id, own_query, foreign_id, foreign_query, resource_uri) in authorized_scopes
    {
        let listed = success(
            client
                .call_tool(
                    "memory_list",
                    json!({ "scopes": [selector.clone()], "sort": "id_asc", "limit": 100 }),
                )
                .await
                .expect("authorized isolated list"),
            manifest,
        );
        assert_eq!(memory_ids(&listed), vec![own_id]);
        assert!(!listed.to_string().contains(foreign_id));
        assert_redacted(&listed, context, store_path);

        let own_search = success(
            client
                .call_tool(
                    "memory_search",
                    json!({ "query": own_query, "scopes": [selector.clone()], "limit": 100 }),
                )
                .await
                .expect("authorized own-sentinel search"),
            manifest,
        );
        assert_eq!(memory_ids(&own_search), vec![own_id]);
        let foreign_search = success(
            client
                .call_tool(
                    "memory_search",
                    json!({ "query": foreign_query, "scopes": [selector], "limit": 100 }),
                )
                .await
                .expect("authorized foreign-sentinel search"),
            manifest,
        );
        assert!(memory_ids(&foreign_search).is_empty());
        assert_redacted(&foreign_search, context, store_path);

        let resource = resource_envelope(
            client
                .read_resource(&resource_uri)
                .await
                .expect("authorized isolated scope resource"),
            manifest,
        );
        assert_eq!(memory_ids(&resource), vec![own_id]);
        assert_redacted(&resource, context, store_path);
    }

    assert_exact_reads_are_hidden(client, context, store_path, manifest).await;
    assert_scope_selectors_are_hidden(client, context, store_path, manifest).await;
    assert_resource_reads_are_hidden(client, context, store_path).await;
}

async fn assert_exact_reads_are_hidden<D: PublicMcpDriver>(
    client: &mut D,
    context: &SuiteContext,
    store_path: &Path,
    manifest: &ConformanceManifest,
) {
    let absent = client
        .call_tool(
            "memory_get",
            json!({ "memoryId": "mem_conformance_absent" }),
        )
        .await
        .expect("absent exact read");
    for memory_id in [
        context.foreign.principal_memory,
        context.foreign.project_memory,
        context.foreign.session_memory,
    ] {
        let hidden = client
            .call_tool("memory_get", json!({ "memoryId": memory_id }))
            .await
            .expect("hidden exact read");
        assert_error(&hidden, "NOT_FOUND", manifest);
        assert_eq!(tool_error_signature(&hidden), tool_error_signature(&absent));
        assert_redacted(&hidden, context, store_path);
    }
}

async fn assert_scope_selectors_are_hidden<D: PublicMcpDriver>(
    client: &mut D,
    context: &SuiteContext,
    store_path: &Path,
    manifest: &ConformanceManifest,
) {
    let foreign_project = json!({ "kind": "project", "projectId": context.foreign.project_id });
    let absent_project = json!({ "kind": "project", "projectId": "prj_conformance_absent" });
    let foreign_session = json!({ "kind": "session", "sessionId": context.foreign.session_id });
    let absent_session = json!({ "kind": "session", "sessionId": "ses_conformance_absent" });
    let selector_pairs = [
        (vec![foreign_project.clone()], vec![absent_project.clone()]),
        (vec![foreign_session.clone()], vec![absent_session.clone()]),
        (
            vec![
                json!({ "kind": "project", "projectId": context.shared_project }),
                foreign_project,
            ],
            vec![
                json!({ "kind": "project", "projectId": context.shared_project }),
                absent_project,
            ],
        ),
        (
            vec![json!({ "kind": "principal" }), foreign_session],
            vec![json!({ "kind": "principal" }), absent_session],
        ),
    ];
    for (foreign, absent) in selector_pairs {
        for tool in ["memory_list", "memory_search"] {
            let arguments = |scopes: Vec<Value>| {
                if tool == "memory_list" {
                    json!({ "scopes": scopes, "sort": "id_asc", "limit": 100 })
                } else {
                    json!({ "query": context.foreign.principal_query, "scopes": scopes, "limit": 100 })
                }
            };
            let hidden = client
                .call_tool(tool, arguments(foreign.clone()))
                .await
                .expect("hidden scope response");
            let unknown = client
                .call_tool(tool, arguments(absent.clone()))
                .await
                .expect("unknown scope response");
            for result in [&hidden, &unknown] {
                assert_error(result, "FORBIDDEN", manifest);
                assert!(result["structuredContent"].get("result").is_none());
                assert_redacted(result, context, store_path);
            }
            assert_eq!(
                tool_error_signature(&hidden),
                tool_error_signature(&unknown)
            );
        }
    }
}

async fn assert_resource_reads_are_hidden<D: PublicMcpDriver>(
    client: &mut D,
    context: &SuiteContext,
    store_path: &Path,
) {
    let absent_exact = client
        .read_resource_error("jiandu://memory/mem_conformance_absent")
        .await
        .expect("absent exact resource error");
    assert!(absent_exact["code"].is_number());
    assert!(absent_exact["message"].is_string());
    for memory_id in [
        context.foreign.principal_memory,
        context.foreign.project_memory,
        context.foreign.session_memory,
    ] {
        let hidden = client
            .read_resource_error(&format!("jiandu://memory/{memory_id}"))
            .await
            .expect("hidden exact resource error");
        assert_eq!(hidden, absent_exact);
        assert_redacted(&hidden, context, store_path);
    }
    for (hidden_uri, absent_uri) in [
        (
            format!(
                "jiandu://scope/project/{}/memories",
                context.foreign.project_id
            ),
            "jiandu://scope/project/prj_conformance_absent/memories".to_owned(),
        ),
        (
            format!(
                "jiandu://scope/session/{}/memories",
                context.foreign.session_id
            ),
            "jiandu://scope/session/ses_conformance_absent/memories".to_owned(),
        ),
    ] {
        let hidden = client
            .read_resource_error(&hidden_uri)
            .await
            .expect("hidden scope resource error");
        let absent = client
            .read_resource_error(&absent_uri)
            .await
            .expect("absent scope resource error");
        assert_eq!(hidden, absent);
        assert_redacted(&hidden, context, store_path);
    }
}

fn resource_envelope(result: Value, manifest: &ConformanceManifest) -> Value {
    let text = result["contents"][0]["text"]
        .as_str()
        .expect("resource text");
    let envelope: Value = serde_json::from_str(text).expect("resource JSON");
    assert_eq!(envelope["apiVersion"], manifest.api_version);
    envelope
}

fn memory_ids(envelope: &Value) -> Vec<&str> {
    envelope["result"]["memories"]
        .as_array()
        .expect("memory result array")
        .iter()
        .map(|memory| memory["id"].as_str().expect("memory ID"))
        .collect()
}

fn tool_error_signature(result: &Value) -> Value {
    json!({
        "apiVersion": result["structuredContent"]["apiVersion"],
        "storeRevision": result["structuredContent"]["storeRevision"],
        "error": result["structuredContent"]["error"],
        "text": result["content"]
    })
}

fn assert_redacted(result: &Value, context: &SuiteContext, store_path: &Path) {
    let wire = result.to_string();
    for sentinel in [
        context.foreign.principal_id,
        context.foreign.project_id,
        context.foreign.session_id,
        context.foreign.principal_memory,
        context.foreign.project_memory,
        context.foreign.session_memory,
        context.foreign.principal_query,
        context.foreign.project_query,
        context.foreign.session_query,
        OFFICIAL_TOKEN,
        RAW_TOKEN,
        NO_FORGET_TOKEN,
        INVALID_TOKEN,
    ] {
        assert!(
            !wire.contains(sentinel),
            "public response leaked {sentinel}"
        );
    }
    assert!(!wire.contains(store_path.to_string_lossy().as_ref()));
}

async fn assert_discovery<D: PublicMcpDriver>(client: &mut D, manifest: &ConformanceManifest) {
    let initialize = client.initialize_result();
    assert_eq!(initialize["protocolVersion"], manifest.protocol_version);
    assert_eq!(
        initialize["capabilities"]["experimental"]["jiandu"]["apiVersion"],
        manifest.api_version
    );
    assert_eq!(
        initialize["serverInfo"]["version"],
        manifest.harness_version
    );

    let tools = client.list_tools().await.expect("tools/list");
    let tools = tools["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), manifest.tools.len());
    for expected in &manifest.tools {
        let tool = tools
            .iter()
            .find(|tool| tool["name"] == expected.name)
            .expect("manifest tool");
        let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let schema: Value = serde_json::from_slice(
            &fs::read(repository.join(&expected.input_schema)).expect("checked schema"),
        )
        .expect("checked schema JSON");
        assert_eq!(
            tool["inputSchema"], schema,
            "{} schema drift",
            expected.name
        );
    }

    let resources = client.list_resources().await.expect("resources/list");
    let resource_uris = resources["resources"]
        .as_array()
        .expect("resources array")
        .iter()
        .map(|resource| resource["uri"].as_str().expect("resource URI"))
        .collect::<Vec<_>>();
    assert_eq!(resource_uris, manifest.resources);
    let templates = client
        .list_resource_templates()
        .await
        .expect("resources/templates/list");
    let template_uris = templates["resourceTemplates"]
        .as_array()
        .expect("template array")
        .iter()
        .map(|template| template["uriTemplate"].as_str().expect("template URI"))
        .collect::<Vec<_>>();
    assert_eq!(template_uris, manifest.resource_templates);
}

async fn assert_reads<D: PublicMcpDriver>(
    client: &mut D,
    context: &SuiteContext,
    manifest: &ConformanceManifest,
) {
    let get = success(
        client
            .call_tool("memory_get", json!({ "memoryId": context.seed_memory }))
            .await
            .expect("memory_get"),
        manifest,
    );
    assert_eq!(get["result"]["id"], context.seed_memory);

    let resource = client
        .read_resource(&format!("jiandu://memory/{}", context.seed_memory))
        .await
        .expect("resources/read");
    let resource_text = resource["contents"][0]["text"]
        .as_str()
        .expect("resource text");
    let resource_envelope: Value = serde_json::from_str(resource_text).expect("resource JSON");
    assert_eq!(resource_envelope["result"], get["result"]);

    let selector = json!({ "kind": "project", "projectId": context.shared_project });
    let list = success(
        client
            .call_tool(
                "memory_list",
                json!({ "scopes": [selector.clone()], "sort": "id_asc", "limit": 1 }),
            )
            .await
            .expect("first list page"),
        manifest,
    );
    assert_eq!(list["result"]["hasMore"], true);
    let list_cursor = list["result"]["nextCursor"].clone();
    let next_list = success(
        client
            .call_tool(
                "memory_list",
                json!({ "scopes": [selector.clone()], "sort": "id_asc", "limit": 1, "cursor": list_cursor }),
            )
            .await
            .expect("second list page"),
        manifest,
    );
    assert_eq!(next_list["result"]["hasMore"], true);
    let final_list = success(
        client
            .call_tool(
                "memory_list",
                json!({ "scopes": [selector.clone()], "sort": "id_asc", "limit": 1,
                    "cursor": next_list["result"]["nextCursor"].clone() }),
            )
            .await
            .expect("final list page"),
        manifest,
    );
    assert_eq!(final_list["result"]["hasMore"], false);
    assert_eq!(final_list["result"]["nextCursor"], Value::Null);
    assert_exhausted_shared_pages([&list, &next_list, &final_list], context.shared_project);

    let search = success(
        client
            .call_tool(
                "memory_search",
                json!({ "query": "ordinary conformance", "scopes": [selector.clone()], "limit": 1 }),
            )
            .await
            .expect("first search page"),
        manifest,
    );
    assert_eq!(search["result"]["hasMore"], true);
    let search_cursor = search["result"]["nextCursor"].clone();
    let next_search = success(
        client
            .call_tool(
                "memory_search",
                json!({ "query": "ordinary conformance", "scopes": [selector.clone()], "limit": 1, "cursor": search_cursor }),
            )
            .await
            .expect("second search page"),
        manifest,
    );
    assert_eq!(next_search["result"]["hasMore"], true);
    let final_search = success(
        client
            .call_tool(
                "memory_search",
                json!({ "query": "ordinary conformance", "scopes": [selector], "limit": 1,
                    "cursor": next_search["result"]["nextCursor"].clone() }),
            )
            .await
            .expect("final search page"),
        manifest,
    );
    assert_eq!(final_search["result"]["hasMore"], false);
    assert_eq!(final_search["result"]["nextCursor"], Value::Null);
    assert_exhausted_shared_pages(
        [&search, &next_search, &final_search],
        context.shared_project,
    );
}

fn assert_exhausted_shared_pages(pages: [&Value; 3], shared_project: &str) {
    let mut ids = BTreeSet::new();
    for page in pages {
        let memories = page["result"]["memories"]
            .as_array()
            .expect("page memories");
        assert_eq!(memories.len(), 1);
        assert_eq!(memories[0]["scope"]["kind"], "project");
        assert_eq!(memories[0]["scope"]["projectId"], shared_project);
        ids.insert(memories[0]["id"].as_str().expect("page memory ID"));
    }
    assert_eq!(ids.len(), 3);
}

async fn assert_mutations_and_errors<D: PublicMcpDriver>(
    client: &mut D,
    context: &SuiteContext,
    manifest: &ConformanceManifest,
) {
    let key = format!("{}-remember", context.key_prefix);
    let remember = json!({
        "scope": { "kind": "project", "projectId": context.shared_project },
        "type": "decision", "title": format!("{} shared record", D::NAME),
        "body": format!("{} ordinary mutation body", D::NAME),
        "tags": ["conformance"], "provenance": {}, "idempotencyKey": key
    });
    let created = success(
        client
            .call_tool("memory_remember", remember.clone())
            .await
            .expect("remember"),
        manifest,
    );
    let memory_id = created["result"]["record"]["id"]
        .as_str()
        .expect("created ID")
        .to_owned();
    assert_eq!(created["result"]["record"]["revision"], 1);

    let invalid = client
        .call_tool(
            "memory_get",
            json!({ "memoryId": memory_id, "principalId": "prn_injected" }),
        )
        .await
        .expect("invalid argument envelope");
    assert_error(&invalid, "INVALID_ARGUMENT", manifest);
    let absent = client
        .call_tool(
            "memory_get",
            json!({ "memoryId": "mem_conformance_absent" }),
        )
        .await
        .expect("not found envelope");
    assert_error(&absent, "NOT_FOUND", manifest);

    let mut conflicting = remember;
    conflicting["title"] = Value::String("conflicting replay".to_owned());
    let conflict = client
        .call_tool("memory_remember", conflicting)
        .await
        .expect("idempotency conflict envelope");
    assert_error(&conflict, "IDEMPOTENCY_CONFLICT", manifest);

    let updated = success(
        client
            .call_tool(
                "memory_update",
                json!({
                    "memoryId": memory_id, "expectedRevision": 1,
                    "patch": { "title": format!("{} updated", D::NAME) },
                    "reason": "ordinary conformance update",
                    "idempotencyKey": format!("{}-update", context.key_prefix)
                }),
            )
            .await
            .expect("update"),
        manifest,
    );
    assert_eq!(updated["result"]["record"]["revision"], 2);
    let stale = client
        .call_tool(
            "memory_update",
            json!({
                "memoryId": memory_id, "expectedRevision": 1,
                "patch": { "title": "must not overwrite" }, "reason": "stale",
                "idempotencyKey": format!("{}-stale", context.key_prefix)
            }),
        )
        .await
        .expect("revision conflict envelope");
    assert_error(&stale, "REVISION_CONFLICT", manifest);

    let forbidden = client
        .call_tool(
            "memory_remember",
            json!({
                "scope": { "kind": "session", "sessionId": context.own.session_id },
                "type": "fact", "title": "forbidden", "body": "must not persist",
                "provenance": {}, "idempotencyKey": format!("{}-forbidden", context.key_prefix)
            }),
        )
        .await
        .expect("forbidden envelope");
    assert_error(&forbidden, "FORBIDDEN", manifest);

    let forgotten = success(
        client
            .call_tool(
                "memory_forget",
                json!({
                    "memoryId": memory_id, "expectedRevision": 2,
                    "reason": "ordinary conformance forget",
                    "idempotencyKey": format!("{}-forget", context.key_prefix)
                }),
            )
            .await
            .expect("forget"),
        manifest,
    );
    assert_eq!(forgotten["result"]["memoryId"], memory_id);
    let after = client
        .call_tool("memory_get", json!({ "memoryId": memory_id }))
        .await
        .expect("get after forget");
    assert_error(&after, "NOT_FOUND", manifest);
}

fn success(result: Value, manifest: &ConformanceManifest) -> Value {
    assert_eq!(result["isError"], false);
    assert_eq!(
        result["structuredContent"]["apiVersion"],
        manifest.api_version
    );
    result["structuredContent"].clone()
}

pub(crate) fn assert_error(result: &Value, code: &str, manifest: &ConformanceManifest) {
    assert_eq!(result["isError"], true);
    assert_eq!(
        result["structuredContent"]["apiVersion"],
        manifest.api_version
    );
    assert_eq!(result["structuredContent"]["error"]["code"], code);
    assert!(manifest.domain_errors.contains(&code.to_owned()));
    assert!(result["structuredContent"]["error"]["message"].is_string());
}

pub(crate) fn expected_error_set(manifest: &ConformanceManifest) -> BTreeSet<&str> {
    manifest.domain_errors.iter().map(String::as_str).collect()
}
