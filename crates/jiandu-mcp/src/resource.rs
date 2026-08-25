//! Strict path-free MCP resource URI contracts.

use jiandu_core::{
    ListSort, MemoryGetRequest, MemoryId, MemoryListRequest, PageLimit, ProjectId, ScopeSelector,
    SessionId,
};
use rmcp::model::{Resource, ResourceTemplate};

pub const MEMORY_RESOURCE_TEMPLATE: &str = "jiandu://memory/{memoryId}";
pub const PRINCIPAL_LIST_RESOURCE_URI: &str = "jiandu://scope/principal/memories";
pub const PROJECT_LIST_RESOURCE_TEMPLATE: &str = "jiandu://scope/project/{projectId}/memories";
pub const SESSION_LIST_RESOURCE_TEMPLATE: &str = "jiandu://scope/session/{sessionId}/memories";
pub const INSTANCE_GLOBAL_LIST_RESOURCE_URI: &str = "jiandu://scope/instance_global/memories";

pub(crate) enum ResourceRequest {
    Get(MemoryGetRequest),
    List(MemoryListRequest),
}

pub(crate) fn parse(uri: &str) -> Result<ResourceRequest, ()> {
    if uri.contains(['?', '#']) {
        return Err(());
    }
    if let Some(memory_id) = uri.strip_prefix("jiandu://memory/") {
        if memory_id.is_empty() || memory_id.contains('/') {
            return Err(());
        }
        return MemoryId::new(memory_id)
            .map(|memory_id| ResourceRequest::Get(MemoryGetRequest { memory_id }))
            .map_err(|_| ());
    }

    let selector = match uri {
        PRINCIPAL_LIST_RESOURCE_URI => ScopeSelector::Principal {},
        INSTANCE_GLOBAL_LIST_RESOURCE_URI => ScopeSelector::InstanceGlobal {},
        _ => {
            if let Some(project_id) = uri
                .strip_prefix("jiandu://scope/project/")
                .and_then(|rest| rest.strip_suffix("/memories"))
            {
                if project_id.is_empty() || project_id.contains('/') {
                    return Err(());
                }
                ScopeSelector::Project {
                    project_id: ProjectId::new(project_id).map_err(|_| ())?,
                }
            } else if let Some(session_id) = uri
                .strip_prefix("jiandu://scope/session/")
                .and_then(|rest| rest.strip_suffix("/memories"))
            {
                if session_id.is_empty() || session_id.contains('/') {
                    return Err(());
                }
                ScopeSelector::Session {
                    session_id: SessionId::new(session_id).map_err(|_| ())?,
                }
            } else {
                return Err(());
            }
        }
    };

    Ok(ResourceRequest::List(MemoryListRequest {
        scopes: vec![selector],
        types: Vec::new(),
        statuses: Vec::new(),
        tags: Vec::new(),
        updated_after: None,
        sort: ListSort::IdAsc,
        limit: PageLimit::new(100).expect("the resource page limit is within the core bound"),
        cursor: None,
    }))
}

pub(crate) fn concrete_resources() -> Vec<Resource> {
    vec![
        Resource::new(PRINCIPAL_LIST_RESOURCE_URI, "principal-memories")
            .with_title("Principal memories")
            .with_description("First deterministic page in the authenticated principal scope")
            .with_mime_type("application/json"),
    ]
}

pub(crate) fn templates() -> Vec<ResourceTemplate> {
    vec![
        ResourceTemplate::new(MEMORY_RESOURCE_TEMPLATE, "memory")
            .with_title("Memory by opaque ID")
            .with_description("One authorized memory; absent and inaccessible IDs are identical")
            .with_mime_type("application/json"),
        ResourceTemplate::new(PROJECT_LIST_RESOURCE_TEMPLATE, "project-memories")
            .with_title("Project memories")
            .with_description("First deterministic page for one authorized project selector")
            .with_mime_type("application/json"),
        ResourceTemplate::new(SESSION_LIST_RESOURCE_TEMPLATE, "session-memories")
            .with_title("Session memories")
            .with_description("First deterministic page for one authorized session selector")
            .with_mime_type("application/json"),
        ResourceTemplate::new(
            INSTANCE_GLOBAL_LIST_RESOURCE_URI,
            "instance-global-memories",
        )
        .with_title("Instance-global memories")
        .with_description("First deterministic page when instance-global scope is authorized")
        .with_mime_type("application/json"),
    ]
}
