//! Read-only, local-human adapter; never used to grant an MCP caller authority.

use std::{collections::HashSet, io, io::Write, path::PathBuf};

use axum::{
    Json, Router,
    extract::{Query, Request, State},
    http::{Method, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use jiandu_memory::{
    ProjectId,
    memory_store::{
        DurableMemoryStatus, MAX_MEMORY_QUERY_CHARS, MemoryQueryOptions, MemoryScope, MemoryStore,
    },
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpListener;

#[derive(Clone)]
struct Console {
    store: MemoryStore,
    authority: String,
}

/// Serve the canonical data root on IPv4 loopback until Ctrl-C. Port zero lets
/// the OS select a free port. Startup and every endpoint leave the store intact.
pub async fn serve_ui(data_dir: PathBuf, port: u16) -> io::Result<()> {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    let authority = listener.local_addr()?.to_string();
    let data_dir = if data_dir.is_absolute() {
        data_dir
    } else {
        std::env::current_dir()?.join(data_dir)
    };
    let state = Console {
        store: MemoryStore::new(data_dir),
        authority,
    };
    let app = Router::new()
        .route("/", get(|| async { Html(include_str!("ui/index.html")) }))
        .route(
            "/console.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("ui/console.css"),
                )
            }),
        )
        .route(
            "/console.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("ui/console.js"),
                )
            }),
        )
        .route("/api/catalog", get(catalog))
        .route("/api/memories", get(memories))
        .route("/api/memory", get(memory))
        .route("/api/status", get(status))
        .route("/api/session", get(session))
        .route("/api/topic", get(topic))
        .layer(middleware::from_fn_with_state(state.clone(), local_request))
        .with_state(state.clone());
    println!("Jiandu console: http://{}/", state.authority);
    io::stdout().flush()?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
}

async fn local_request(State(state): State<Console>, request: Request, next: Next) -> Response {
    let headers = request.headers();
    let origin = format!("http://{}", state.authority);
    let foreign = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        != Some(state.authority.as_str())
        || headers
            .get(header::ORIGIN)
            .is_some_and(|value| value != origin.as_str())
        || headers
            .get("sec-fetch-site")
            .is_some_and(|value| value != "same-origin" && value != "none");
    let api = request.uri().path().starts_with("/api/");
    let mut response = if foreign
        || (api
            && headers
                .get("x-jiandu-console")
                .is_none_or(|value| value != "1"))
    {
        (
            StatusCode::FORBIDDEN,
            Json(json!({"error":"Use the console at its printed local URL."})),
        )
            .into_response()
    } else if request.method() != Method::GET && request.method() != Method::HEAD {
        (
            StatusCode::METHOD_NOT_ALLOWED,
            Json(json!({"error":"This console is read-only."})),
        )
            .into_response()
    } else {
        next.run(request).await
    };
    for (name, value) in [
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        (
            "content-security-policy",
            "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
        ),
    ] {
        response.headers_mut().insert(
            header::HeaderName::from_static(name),
            header::HeaderValue::from_static(value),
        );
    }
    response
}

struct ConsoleError(io::Error);
impl From<io::Error> for ConsoleError {
    fn from(error: io::Error) -> Self {
        Self(error)
    }
}
impl IntoResponse for ConsoleError {
    fn into_response(self) -> Response {
        let status = match self.0.kind() {
            io::ErrorKind::InvalidInput => StatusCode::BAD_REQUEST,
            io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
            io::ErrorKind::InvalidData => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(json!({"error":self.0.to_string()}))).into_response()
    }
}
type ApiResult = Result<Json<Value>, ConsoleError>;
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
fn absent(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, message)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryParams {
    scope: MemoryScope,
    project_id: Option<String>,
    q: Option<String>,
    status: Option<DurableMemoryStatus>,
    cursor: Option<String>,
    id: Option<String>,
}

async fn selected_store(
    console: &Console,
    params: &MemoryParams,
) -> io::Result<(MemoryStore, Option<ProjectId>)> {
    match params.scope {
        MemoryScope::Global if params.project_id.is_none() => Ok((console.store.clone(), None)),
        MemoryScope::Project => {
            let id = ProjectId::parse(
                params
                    .project_id
                    .as_deref()
                    .ok_or_else(|| invalid("project_id is required"))?,
            )
            .map_err(|_| invalid("Invalid Project id"))?;
            if !console.store.list_project_ids().await?.contains(&id) {
                return Err(absent("Project is not in the first-class inventory"));
            }
            Ok((console.store.for_project(&id), Some(id)))
        }
        _ => Err(invalid(
            "Choose Global without a Project id, or a first-class Project",
        )),
    }
}

async fn catalog(State(console): State<Console>) -> ApiResult {
    let projects = console.store.list_project_ids().await?;
    let sessions = console.store.list_session_ids().await?;
    Ok(Json(json!({"projects":projects,"sessions":sessions,
        "data_dir":console.store.resolver().data_dir(),"read_only":true})))
}

async fn memories(State(console): State<Console>, Query(params): Query<MemoryParams>) -> ApiResult {
    if params
        .q
        .as_ref()
        .is_some_and(|query| query.chars().count() > MAX_MEMORY_QUERY_CHARS)
    {
        return Err(invalid("Search query is too long").into());
    }
    let (store, id) = selected_store(&console, &params).await?;
    let key = id.as_ref().map(ProjectId::as_str);
    let statuses = params.status.map(|status| HashSet::from([status]));
    let options = MemoryQueryOptions {
        limit: Some(20),
        max_chars: Some(6000),
        cursor: params.cursor,
        include_related: false,
    };
    let query = params.q.as_deref();
    // A cold, empty scope has no lexical index. An empty result is sufficient;
    // nonempty scopes still surface the store's missing/invalid-index error.
    let query = if query.is_some_and(|value| !value.trim().is_empty())
        && store.count_scope_memories(params.scope, key).await? == 0
    {
        None
    } else {
        query
    };
    let result = store
        .query_scope_read_only(
            params.scope,
            key,
            query,
            None,
            statuses.as_ref(),
            None,
            &options,
        )
        .await?;
    Ok(Json(json!(result)))
}

async fn memory(State(console): State<Console>, Query(params): Query<MemoryParams>) -> ApiResult {
    let (store, project) = selected_store(&console, &params).await?;
    let id = params
        .id
        .as_deref()
        .ok_or_else(|| invalid("id is required"))?;
    let document = store
        .get_memory(id, project.as_ref().map(ProjectId::as_str))
        .await?
        .filter(|document| document.frontmatter.scope == params.scope)
        .ok_or_else(|| absent("Memory is not in the selected scope"))?;
    Ok(Json(
        json!({"frontmatter":document.frontmatter,"body":document.body}),
    ))
}

async fn status(State(console): State<Console>, Query(params): Query<MemoryParams>) -> ApiResult {
    let (store, project) = selected_store(&console, &params).await?;
    let key = project.as_ref().map(ProjectId::as_str);
    let inspection = store.inspect_scope(params.scope, key).await?;
    let index = match store.read_lexical_index(params.scope, key).await {
        Ok(Some(index)) => json!({"state":"ready","generated_at":index.generated_at}),
        Ok(None) if inspection.total_memories == 0 => json!({"state":"empty"}),
        Ok(None) => json!({"state":"missing"}),
        Err(error) => json!({"state":"invalid","error":error.to_string()}),
    };
    let mut result = json!({"inspection":inspection,"index":index});
    match store.read_dream_snapshot(params.scope, key).await {
        Ok(dream) => result["dream"] = json!(dream),
        Err(error) => result["dream_error"] = json!(error.to_string()),
    }
    Ok(Json(result))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionParams {
    session_id: String,
    q: Option<String>,
    cursor: Option<String>,
    topic: Option<String>,
}

async fn require_session(console: &Console, id: &str) -> io::Result<()> {
    if !console
        .store
        .list_session_ids()
        .await?
        .iter()
        .any(|stored| stored == id)
    {
        return Err(absent("Session is not in the inventory"));
    }
    Ok(())
}

async fn session(State(console): State<Console>, Query(params): Query<SessionParams>) -> ApiResult {
    require_session(&console, &params.session_id).await?;
    let query = params.q.as_deref().unwrap_or("").trim().to_lowercase();
    if query.chars().count() > MAX_MEMORY_QUERY_CHARS {
        return Err(invalid("Search query is too long").into());
    }
    let offset = params
        .cursor
        .as_deref()
        .unwrap_or("0")
        .parse::<usize>()
        .map_err(|_| invalid("Invalid cursor"))?;
    let state = console.store.read_session_state(&params.session_id).await?;
    let mut matches = Vec::new();
    for topic in console
        .store
        .list_session_topics(&params.session_id)
        .await?
    {
        let Some(content) = console
            .store
            .read_session_topic(&params.session_id, &topic)
            .await?
        else {
            continue;
        };
        if query.is_empty()
            || topic.to_lowercase().contains(&query)
            || content.to_lowercase().contains(&query)
        {
            matches.push(json!({"topic":topic,"summary":content.trim().chars().take(220).collect::<String>()}));
        }
    }
    let count = matches.len();
    let topics = matches
        .into_iter()
        .skip(offset)
        .take(20)
        .collect::<Vec<_>>();
    let next = offset.saturating_add(topics.len());
    let mut result = json!({"state":state,"topics":topics,"matched_count":count});
    if next < count {
        result["next_cursor"] = json!(next.to_string());
    }
    Ok(Json(result))
}

async fn topic(State(console): State<Console>, Query(params): Query<SessionParams>) -> ApiResult {
    require_session(&console, &params.session_id).await?;
    let topic = params
        .topic
        .as_deref()
        .ok_or_else(|| invalid("topic is required"))?;
    let content = console
        .store
        .read_session_topic(&params.session_id, topic)
        .await?
        .ok_or_else(|| absent("Topic is not in the selected Session"))?;
    Ok(Json(json!({"topic":topic,"content":content})))
}
