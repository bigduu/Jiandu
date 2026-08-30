use std::{
    collections::HashSet,
    io,
    sync::{Arc, OnceLock},
};

use dashmap::DashMap;
use jiandu_memory::{
    ProjectId,
    memory_store::{
        DEFAULT_SESSION_TOPIC, DurableMemoryDocument, DurableMemoryStatus, DurableMemoryType,
        MAX_MAX_CHARS, MAX_QUERY_LIMIT, MemoryQueryOptions, MemoryScope, MemorySplitPiece,
        MemoryStore, TemporalGranularity, count_chars, summary_json, truncate_chars,
    },
};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::{MemoryArgs, MemoryError, MemoryServer, QueryFilters, SplitPiece as ToolSplitPiece};

const MAX_SESSION_NOTE_CHARS: usize = 12_000;
const MAX_MEMORY_ID_LEN: usize = 128;
const ACTOR: &str = "main-model";

fn session_locks() -> &'static DashMap<String, Arc<Mutex<()>>> {
    static SESSION_LOCKS: OnceLock<DashMap<String, Arc<Mutex<()>>>> = OnceLock::new();
    SESSION_LOCKS.get_or_init(DashMap::new)
}

fn session_memory_lock(session_id: &str) -> Arc<Mutex<()>> {
    session_locks()
        .entry(session_id.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

struct ResolvedMemoryAccess {
    store: MemoryStore,
    project_id: Option<ProjectId>,
}

impl ResolvedMemoryAccess {
    fn project_key(&self) -> Option<&str> {
        self.project_id.as_ref().map(ProjectId::as_str)
    }
}

type FilterSets = (
    Option<HashSet<DurableMemoryType>>,
    Option<HashSet<DurableMemoryStatus>>,
    Option<HashSet<TemporalGranularity>>,
);

impl MemoryServer {
    pub(crate) async fn execute_parsed(&self, arguments: MemoryArgs) -> Result<Value, MemoryError> {
        match arguments {
            MemoryArgs::SessionRead { topic, options } => {
                let session_lock = session_memory_lock(self.context.session_id());
                let _guard = session_lock.lock().await;
                let topic = session_topic(topic.as_deref());
                let max_chars = options
                    .and_then(|value| value.max_chars)
                    .unwrap_or(MAX_SESSION_NOTE_CHARS)
                    .clamp(1, MAX_SESSION_NOTE_CHARS);
                let content = self
                    .store
                    .read_session_topic(self.context.session_id(), topic)
                    .await
                    .map_err(|error| execution("Failed to read note", error))?;
                let exists = content.is_some();
                let body = content.unwrap_or_default();
                let length_chars = count_chars(&body);
                let (content, body_truncated) = truncate_chars(&body, max_chars);
                Ok(json!({
                    "action": "session_read",
                    "session_id": self.context.session_id(),
                    "topic": topic,
                    "exists": exists,
                    "content": content,
                    "length_chars": length_chars,
                    "body_truncated": body_truncated,
                    "max_chars": max_chars,
                }))
            }
            MemoryArgs::SessionAppend { topic, content } => {
                let session_lock = session_memory_lock(self.context.session_id());
                let _guard = session_lock.lock().await;
                let topic = session_topic(topic.as_deref());
                let content = required_content(&content, "session_append")?;
                let existing = self
                    .store
                    .read_session_topic(self.context.session_id(), topic)
                    .await
                    .map_err(|error| execution("Failed to read note", error))?;
                let mut next = existing.unwrap_or_default();
                if !next.is_empty() {
                    next.push_str("\n\n");
                }
                next.push_str(content);
                let length_chars = count_chars(&next);
                if length_chars > MAX_SESSION_NOTE_CHARS {
                    return Err(MemoryError::Execution(format!(
                        "session note would exceed the limit ({length_chars}>{MAX_SESSION_NOTE_CHARS} chars); replace it with a shorter note before appending"
                    )));
                }
                let path = self
                    .store
                    .write_session_topic(self.context.session_id(), topic, &next)
                    .await
                    .map_err(|error| execution("Failed to write note", error))?;
                Ok(json!({
                    "action": "session_append",
                    "session_id": self.context.session_id(),
                    "topic": topic,
                    "path": path,
                    "length_chars": length_chars,
                    "max_chars": MAX_SESSION_NOTE_CHARS,
                }))
            }
            MemoryArgs::SessionReplace { topic, content } => {
                let session_lock = session_memory_lock(self.context.session_id());
                let _guard = session_lock.lock().await;
                let topic = session_topic(topic.as_deref());
                let content = required_content(&content, "session_replace")?;
                let length_chars = count_chars(content);
                if length_chars > MAX_SESSION_NOTE_CHARS {
                    return Err(MemoryError::Execution(format!(
                        "session note too long (>{MAX_SESSION_NOTE_CHARS} chars); replace it with a shorter note"
                    )));
                }
                let path = self
                    .store
                    .write_session_topic(self.context.session_id(), topic, content)
                    .await
                    .map_err(|error| execution("Failed to write note", error))?;
                Ok(json!({
                    "action": "session_replace",
                    "session_id": self.context.session_id(),
                    "topic": topic,
                    "path": path,
                    "length_chars": length_chars,
                    "max_chars": MAX_SESSION_NOTE_CHARS,
                }))
            }
            MemoryArgs::SessionClear { topic } => {
                let session_lock = session_memory_lock(self.context.session_id());
                let _guard = session_lock.lock().await;
                let topic = session_topic(topic.as_deref());
                let deleted = self
                    .store
                    .delete_session_topic(self.context.session_id(), topic)
                    .await
                    .map_err(|error| execution("Failed to delete note", error))?;
                Ok(json!({
                    "action": "session_clear",
                    "session_id": self.context.session_id(),
                    "topic": topic,
                    "deleted": deleted,
                }))
            }
            MemoryArgs::SessionListTopics => {
                let session_lock = session_memory_lock(self.context.session_id());
                let _guard = session_lock.lock().await;
                let topics = self
                    .store
                    .list_session_topics(self.context.session_id())
                    .await
                    .map_err(|error| execution("Failed to list topics", error))?;
                Ok(json!({
                    "action": "session_list_topics",
                    "session_id": self.context.session_id(),
                    "count": topics.len(),
                    "topics": topics,
                }))
            }
            MemoryArgs::Query {
                scope,
                query,
                filters,
                project_key,
                options,
            } => {
                let scope = parse_durable_scope(&scope, "query")?;
                let access = self.resolve_access(project_key.as_deref(), scope)?;
                let options = MemoryQueryOptions {
                    limit: options
                        .as_ref()
                        .and_then(|value| value.limit)
                        .map(|value| value.min(MAX_QUERY_LIMIT)),
                    max_chars: options
                        .as_ref()
                        .and_then(|value| value.max_chars)
                        .map(|value| value.min(MAX_MAX_CHARS)),
                    cursor: options.as_ref().and_then(|value| value.cursor.clone()),
                    include_related: options
                        .as_ref()
                        .and_then(|value| value.include_related)
                        .unwrap_or(false),
                };
                let (types, statuses, granularities) = parse_query_filters(filters.as_ref())?;
                let result = access
                    .store
                    .query_scope(
                        scope,
                        access.project_key(),
                        query.as_deref(),
                        types.as_ref(),
                        statuses.as_ref(),
                        granularities.as_ref(),
                        &options,
                    )
                    .await
                    .map_err(|error| execution("Failed to query memory", error))?;
                let summary = summary_json(result.returned_count, result.matched_count);
                Ok(json!({
                    "action": "query",
                    "success": true,
                    "data": result,
                    "summary": summary,
                    "warnings": [],
                }))
            }
            MemoryArgs::Get {
                id,
                project_key,
                options,
            } => {
                let id = validate_memory_id(&id)?;
                let access = self.resolve_id_access(project_key.as_deref(), id).await?;
                let max_chars = options
                    .and_then(|value| value.max_chars)
                    .unwrap_or(MAX_MAX_CHARS)
                    .min(MAX_MAX_CHARS);
                let Some(mut doc) = access
                    .store
                    .get_memory(id, access.project_key())
                    .await
                    .map_err(|error| execution("Failed to get memory", error))?
                else {
                    return Err(memory_not_found(id));
                };
                let (body, body_truncated) = truncate_chars(&doc.body, max_chars);
                doc.body = body;
                Ok(document_result("get", doc, body_truncated))
            }
            MemoryArgs::Write {
                scope,
                r#type,
                title,
                content,
                tags,
                project_key,
                granularity,
                options,
            } => {
                let scope = parse_durable_scope(&scope, "write")?;
                let access = self.resolve_access(project_key.as_deref(), scope)?;
                let doc = access
                    .store
                    .write_memory(
                        scope,
                        access.project_key(),
                        parse_type(&r#type)?,
                        &title,
                        &content,
                        &tags,
                        Some(self.context.session_id()),
                        ACTOR,
                        options
                            .and_then(|value| value.allow_merge_if_similar)
                            .unwrap_or(false),
                        parse_granularity(granularity.as_deref())?,
                    )
                    .await
                    .map_err(|error| execution("Failed to write memory", error))?;
                Ok(json!({
                    "action": "write",
                    "memory": {
                        "id": doc.frontmatter.id,
                        "title": doc.frontmatter.title,
                        "type": doc.frontmatter.r#type,
                        "scope": doc.frontmatter.scope,
                        "status": doc.frontmatter.status,
                        "project_key": doc.frontmatter.project_key,
                        "path": doc.path,
                    }
                }))
            }
            MemoryArgs::Merge {
                id,
                content,
                tags,
                project_key,
                source_memory_ids,
                mode,
                reason,
            } => {
                let id = validate_memory_id(&id)?;
                let source_memory_ids = validate_memory_ids(&source_memory_ids)?;
                let access = self.resolve_id_access(project_key.as_deref(), id).await?;
                self.ensure_related_ids_accessible(&access, &source_memory_ids)
                    .await?;
                let mode = parse_merge_mode(mode.as_deref())?;
                if mode.as_deref() == Some("contradict") {
                    let Some(result) = access
                        .store
                        .mark_memory_contradicted(
                            id,
                            access.project_key(),
                            &source_memory_ids,
                            reason.as_deref().or(Some(content.trim())),
                            Some(self.context.session_id()),
                            ACTOR,
                        )
                        .await
                        .map_err(|error| execution("Failed to contradict memory", error))?
                    else {
                        return Err(memory_not_found(id));
                    };
                    Ok(json!({"action": "merge", "mode": "contradict", "data": result}))
                } else {
                    let Some(result) = access
                        .store
                        .merge_memory(
                            id,
                            access.project_key(),
                            &content,
                            &tags,
                            Some(self.context.session_id()),
                            ACTOR,
                            &source_memory_ids,
                        )
                        .await
                        .map_err(|error| execution("Failed to merge memory", error))?
                    else {
                        return Err(memory_not_found(id));
                    };
                    Ok(json!({
                        "action": "merge",
                        "mode": mode.unwrap_or_else(|| "merge".to_string()),
                        "data": result,
                    }))
                }
            }
            MemoryArgs::Split {
                id,
                project_key,
                pieces,
            } => {
                if pieces.is_empty() {
                    return Err(MemoryError::InvalidArguments(
                        "split requires at least one piece".to_string(),
                    ));
                }
                let id = validate_memory_id(&id)?;
                let access = self.resolve_id_access(project_key.as_deref(), id).await?;
                let pieces = parse_split_pieces(pieces)?;
                let Some(result) = access
                    .store
                    .split_memory(
                        id,
                        access.project_key(),
                        &pieces,
                        Some(self.context.session_id()),
                        ACTOR,
                    )
                    .await
                    .map_err(|error| execution("Failed to split memory", error))?
                else {
                    return Err(memory_not_found(id));
                };
                Ok(json!({"action": "split", "data": result}))
            }
            MemoryArgs::FindDuplicates {
                scope,
                title,
                content,
                r#type,
                tags,
                project_key,
                options,
            } => {
                let scope = parse_durable_scope(&scope, "find_duplicates")?;
                let access = self.resolve_access(project_key.as_deref(), scope)?;
                let r#type = r#type.as_deref().map(parse_type).transpose()?;
                let limit = options
                    .and_then(|value| value.limit)
                    .unwrap_or(5)
                    .clamp(1, MAX_QUERY_LIMIT);
                let candidates = access
                    .store
                    .find_duplicate_candidates(
                        scope,
                        access.project_key(),
                        r#type,
                        &title,
                        content.as_deref().unwrap_or(""),
                        &tags,
                        limit,
                    )
                    .await
                    .map_err(|error| execution("Failed to find duplicates", error))?;
                Ok(json!({"action": "find_duplicates", "candidates": candidates}))
            }
            MemoryArgs::ScanBlobs {
                scope,
                project_key,
                min_sections,
                options,
            } => {
                let scope = parse_durable_scope(&scope, "scan_blobs")?;
                let access = self.resolve_access(project_key.as_deref(), scope)?;
                let report = access
                    .store
                    .scan_blob_candidates(
                        scope,
                        access.project_key(),
                        min_sections.unwrap_or(3),
                        options
                            .and_then(|value| value.limit)
                            .unwrap_or(20)
                            .clamp(1, 200),
                    )
                    .await
                    .map_err(|error| execution("Failed to scan blobs", error))?;
                Ok(json!({"action": "scan_blobs", "report": report}))
            }
            MemoryArgs::ScanDuplicates {
                scope,
                project_key,
                min_score,
                options,
            } => {
                let scope = parse_durable_scope(&scope, "scan_duplicates")?;
                let access = self.resolve_access(project_key.as_deref(), scope)?;
                let report = access
                    .store
                    .scan_duplicate_clusters(
                        scope,
                        access.project_key(),
                        min_score.unwrap_or(0.6),
                        5,
                        options
                            .and_then(|value| value.limit)
                            .unwrap_or(20)
                            .clamp(1, 200),
                    )
                    .await
                    .map_err(|error| execution("Failed to scan duplicates", error))?;
                Ok(json!({"action": "scan_duplicates", "report": report}))
            }
            MemoryArgs::Consolidate {
                ids,
                title,
                content,
                r#type,
                tags,
                project_key,
            } => {
                if ids.len() < 2 {
                    return Err(MemoryError::InvalidArguments(
                        "consolidate requires at least two source memory ids".to_string(),
                    ));
                }
                let ids = validate_memory_ids(&ids)?;
                let access = self
                    .resolve_id_access(project_key.as_deref(), &ids[0])
                    .await?;
                self.ensure_related_ids_accessible(&access, &ids[1..])
                    .await?;
                let merged = MemorySplitPiece {
                    title,
                    r#type: r#type.as_deref().map(parse_type).transpose()?,
                    content,
                    tags,
                };
                let Some(result) = access
                    .store
                    .consolidate_memories(
                        &ids,
                        access.project_key(),
                        &merged,
                        Some(self.context.session_id()),
                        ACTOR,
                    )
                    .await
                    .map_err(|error| execution("Failed to consolidate memories", error))?
                else {
                    return Err(MemoryError::Execution(
                        "one or more source memories not found".to_string(),
                    ));
                };
                Ok(json!({"action": "consolidate", "data": result}))
            }
            MemoryArgs::Purge {
                id,
                scope,
                reason,
                project_key,
                filters,
                mode,
            } => {
                let mode = mode
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(parse_status)
                    .transpose()?
                    .unwrap_or(DurableMemoryStatus::Archived);
                if let Some(id) = id
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                {
                    let id = validate_memory_id(id)?;
                    let access = self.resolve_id_access(project_key.as_deref(), id).await?;
                    let Some(doc) = access
                        .store
                        .archive_memory(id, access.project_key(), mode, reason.as_deref())
                        .await
                        .map_err(|error| execution("Failed to purge memory", error))?
                    else {
                        return Err(memory_not_found(id));
                    };
                    Ok(json!({
                        "action": "purge",
                        "id": doc.frontmatter.id,
                        "status": doc.frontmatter.status,
                    }))
                } else {
                    let scope =
                        parse_durable_scope(scope.as_deref().unwrap_or("session"), "purge")?;
                    let access = self.resolve_access(project_key.as_deref(), scope)?;
                    let (types, statuses, granularities) = parse_query_filters(filters.as_ref())?;
                    let result = access
                        .store
                        .purge_memories(
                            scope,
                            access.project_key(),
                            types.as_ref(),
                            statuses.as_ref(),
                            granularities.as_ref(),
                            mode,
                            reason.as_deref(),
                        )
                        .await
                        .map_err(|error| execution("Failed to purge memory", error))?;
                    Ok(json!({"action": "purge", "data": result}))
                }
            }
            MemoryArgs::Inspect { scope, project_key } => {
                let scope = parse_durable_scope(&scope, "inspect")?;
                let access = self.resolve_access(project_key.as_deref(), scope)?;
                let data = access
                    .store
                    .inspect_scope(scope, access.project_key())
                    .await
                    .map_err(|error| execution("Failed to inspect memory", error))?;
                Ok(json!({"action": "inspect", "data": data}))
            }
            MemoryArgs::Rebuild { scope, project_key } => {
                let scope = parse_durable_scope(&scope, "rebuild")?;
                let access = self.resolve_access(project_key.as_deref(), scope)?;
                access
                    .store
                    .rebuild_scope(scope, access.project_key())
                    .await
                    .map_err(|error| execution("Failed to rebuild memory artifacts", error))?;
                let data = access
                    .store
                    .inspect_scope(scope, access.project_key())
                    .await
                    .map_err(|error| execution("Failed to inspect rebuilt memory", error))?;
                Ok(json!({
                    "action": "rebuild",
                    "scope": scope,
                    "project_key": access.project_key(),
                    "data": data,
                }))
            }
        }
    }

    fn resolve_access(
        &self,
        requested: Option<&str>,
        scope: MemoryScope,
    ) -> Result<ResolvedMemoryAccess, MemoryError> {
        let project_id = self.context.resolve_project_id(requested)?;
        if scope == MemoryScope::Project && project_id.is_none() {
            return Err(MemoryError::InvalidArguments(
                "project scope requires a project_id in the MCP execution context or project_key"
                    .to_string(),
            ));
        }
        let store = project_id
            .as_ref()
            .map_or_else(|| self.store.clone(), |id| self.store.for_project(id));
        Ok(ResolvedMemoryAccess { store, project_id })
    }

    async fn resolve_id_access(
        &self,
        requested: Option<&str>,
        id: &str,
    ) -> Result<ResolvedMemoryAccess, MemoryError> {
        let id = validate_memory_id(id)?;
        let access = self.resolve_access(requested, MemoryScope::Global)?;
        self.ensure_id_accessible(&access, id).await?;
        Ok(access)
    }

    async fn ensure_related_ids_accessible(
        &self,
        access: &ResolvedMemoryAccess,
        ids: &[String],
    ) -> Result<(), MemoryError> {
        for id in ids {
            self.ensure_id_accessible(access, validate_memory_id(id)?)
                .await?;
        }
        Ok(())
    }

    async fn ensure_id_accessible(
        &self,
        access: &ResolvedMemoryAccess,
        id: &str,
    ) -> Result<(), MemoryError> {
        let id = validate_memory_id(id)?;
        if access.project_id.is_some() {
            return Ok(());
        }

        let exists = access
            .store
            .list_memory_documents(MemoryScope::Global, None)
            .await
            .map_err(|error| execution("Failed to resolve memory scope", error))?
            .iter()
            .any(|doc| doc.frontmatter.id == id);
        if exists {
            Ok(())
        } else {
            Err(memory_not_found(id))
        }
    }
}

fn session_topic(topic: Option<&str>) -> &str {
    topic
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_SESSION_TOPIC)
}

fn required_content<'a>(content: &'a str, action: &str) -> Result<&'a str, MemoryError> {
    let content = content.trim();
    if content.is_empty() {
        Err(MemoryError::InvalidArguments(format!(
            "content is required for action={action}"
        )))
    } else {
        Ok(content)
    }
}

fn validate_memory_id(id: &str) -> Result<&str, MemoryError> {
    let id = id.trim();
    let valid = !id.is_empty()
        && id.len() <= MAX_MEMORY_ID_LEN
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    if valid {
        Ok(id)
    } else {
        Err(MemoryError::InvalidArguments(format!(
            "memory id must be a 1-{MAX_MEMORY_ID_LEN} character path-safe identifier containing only ASCII alphanumeric, '-' or '_'"
        )))
    }
}

fn validate_memory_ids(ids: &[String]) -> Result<Vec<String>, MemoryError> {
    ids.iter()
        .map(|id| validate_memory_id(id).map(ToString::to_string))
        .collect()
}

fn parse_scope(scope: &str) -> Result<MemoryScope, MemoryError> {
    match scope.trim().to_ascii_lowercase().as_str() {
        "session" => Ok(MemoryScope::Session),
        "project" => Ok(MemoryScope::Project),
        "global" => Ok(MemoryScope::Global),
        other => Err(MemoryError::InvalidArguments(format!(
            "invalid scope '{other}'; expected one of: session, project, global"
        ))),
    }
}

fn parse_durable_scope(scope: &str, action: &str) -> Result<MemoryScope, MemoryError> {
    let scope = parse_scope(scope)?;
    if scope == MemoryScope::Session {
        Err(MemoryError::InvalidArguments(format!(
            "{action} supports durable scopes only"
        )))
    } else {
        Ok(scope)
    }
}

fn parse_granularity(value: Option<&str>) -> Result<Option<TemporalGranularity>, MemoryError> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    TemporalGranularity::parse(value).map(Some).ok_or_else(|| {
        MemoryError::InvalidArguments(format!(
            "invalid granularity '{value}'; expected one of: day, week, month, quarter, year"
        ))
    })
}

fn parse_type(value: &str) -> Result<DurableMemoryType, MemoryError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "user" => Ok(DurableMemoryType::User),
        "feedback" => Ok(DurableMemoryType::Feedback),
        "project" => Ok(DurableMemoryType::Project),
        "reference" => Ok(DurableMemoryType::Reference),
        other => Err(MemoryError::InvalidArguments(format!(
            "invalid type '{other}'; expected one of: user, feedback, project, reference"
        ))),
    }
}

fn parse_status(value: &str) -> Result<DurableMemoryStatus, MemoryError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "active" => Ok(DurableMemoryStatus::Active),
        "stale" => Ok(DurableMemoryStatus::Stale),
        "superseded" => Ok(DurableMemoryStatus::Superseded),
        "contradicted" => Ok(DurableMemoryStatus::Contradicted),
        "archived" => Ok(DurableMemoryStatus::Archived),
        other => Err(MemoryError::InvalidArguments(format!(
            "invalid status '{other}'; expected one of: active, stale, superseded, contradicted, archived"
        ))),
    }
}

fn parse_query_filters(filters: Option<&QueryFilters>) -> Result<FilterSets, MemoryError> {
    let types = filters
        .filter(|value| !value.r#type.is_empty())
        .map(|value| {
            value
                .r#type
                .iter()
                .map(|item| parse_type(item))
                .collect::<Result<HashSet<_>, _>>()
        })
        .transpose()?;
    let statuses = filters
        .filter(|value| !value.status.is_empty())
        .map(|value| {
            value
                .status
                .iter()
                .map(|item| parse_status(item))
                .collect::<Result<HashSet<_>, _>>()
        })
        .transpose()?;
    let granularities = filters
        .filter(|value| !value.granularity.is_empty())
        .map(|value| {
            value
                .granularity
                .iter()
                .map(|item| {
                    TemporalGranularity::parse(item).ok_or_else(|| {
                        MemoryError::InvalidArguments(format!(
                            "invalid granularity filter '{item}'; expected one of: day, week, month, quarter, year"
                        ))
                    })
                })
                .collect::<Result<HashSet<_>, _>>()
        })
        .transpose()?;
    Ok((types, statuses, granularities))
}

fn parse_merge_mode(value: Option<&str>) -> Result<Option<String>, MemoryError> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let value = value.to_ascii_lowercase();
    match value.as_str() {
        "semantic_merge" | "merge" | "contradict" => Ok(Some(value)),
        other => Err(MemoryError::InvalidArguments(format!(
            "invalid merge mode '{other}'; expected one of: merge, semantic_merge, contradict"
        ))),
    }
}

fn parse_split_pieces(pieces: Vec<ToolSplitPiece>) -> Result<Vec<MemorySplitPiece>, MemoryError> {
    pieces
        .into_iter()
        .map(|piece| {
            Ok(MemorySplitPiece {
                title: piece.title,
                r#type: piece.r#type.as_deref().map(parse_type).transpose()?,
                content: piece.content,
                tags: piece.tags,
            })
        })
        .collect()
}

fn document_result(action: &str, doc: DurableMemoryDocument, body_truncated: bool) -> Value {
    json!({
        "action": action,
        "id": doc.frontmatter.id,
        "memory": {
            "frontmatter": doc.frontmatter,
            "body": doc.body,
            "path": doc.path,
            "body_truncated": body_truncated,
        }
    })
}

fn execution(context: &str, error: io::Error) -> MemoryError {
    MemoryError::Execution(format!("{context}: {error}"))
}

fn memory_not_found(id: &str) -> MemoryError {
    MemoryError::Execution(format!("memory not found: {}", id.trim()))
}
