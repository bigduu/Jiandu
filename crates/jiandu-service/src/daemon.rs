//! Loopback-only authenticated Streamable HTTP daemon lifecycle.

use crate::auth::authenticate;
use crate::config::{BearerDigest, ValidatedClient};
use crate::{MCP_ROUTE, ServeConfig};
use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{
    HeaderValue, Response, StatusCode,
    header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, WWW_AUTHENTICATE},
};
use axum::response::IntoResponse;
use axum::routing::{any, get};
use jiandu_index::{IndexReadiness, LexicalIndex};
use jiandu_mcp::{
    CanonicalReadBackend, IndexReadHealth, JianduReadServer, McpReadBackend, MutationPolicy,
    ReadServiceHealth, StoreReadHealth,
};
use jiandu_store::{CanonicalStore, LockOwnerDiagnostics, StoreError, StoreErrorCode};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use serde::Serialize;
use std::fmt;
use std::io;
use std::net::SocketAddr;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

type McpHttpService = StreamableHttpService<JianduReadServer, LocalSessionManager>;

struct ClientRuntime {
    service: McpHttpService,
}

struct DaemonState {
    credential_digests: Vec<BearerDigest>,
    clients: Vec<ClientRuntime>,
    backend: Arc<CanonicalReadBackend>,
}

#[cfg(test)]
#[derive(Clone, Default)]
struct HandlerConstructionProbe(Arc<AtomicUsize>);

#[cfg(not(test))]
#[derive(Clone, Default)]
struct HandlerConstructionProbe {
    _private: (),
}

impl HandlerConstructionProbe {
    fn record(&self) {
        #[cfg(test)]
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    #[cfg(test)]
    fn count(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }
}

/// One running daemon. The canonical lock is held by the task-owned router
/// until [`RunningDaemon::shutdown`] completes.
pub struct RunningDaemon {
    local_addr: SocketAddr,
    cancellation: CancellationToken,
    task: Option<JoinHandle<Result<(), io::Error>>>,
    #[cfg(test)]
    handler_constructions: HandlerConstructionProbe,
}

impl fmt::Debug for RunningDaemon {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RunningDaemon")
            .field("local_addr", &self.local_addr)
            .field("credential_state", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl RunningDaemon {
    /// Validate the loopback boundary, acquire and recover one existing store,
    /// construct the singleton backend, and only then bind the HTTP listener.
    pub async fn start(config: ServeConfig) -> Result<Self, DaemonError> {
        if !config.bind.ip().is_loopback() {
            return Err(DaemonError::InvalidConfiguration);
        }

        let owner = jiandu_store::LockOwner::for_current_process().map_err(map_store_error)?;
        let data_dir = config.data_dir.clone();
        let opened = tokio::task::spawn_blocking(move || {
            let store = CanonicalStore::open(&data_dir, owner)?;
            let index = LexicalIndex::new(data_dir.join("index"));
            let readiness = index.readiness(&store);
            Ok::<_, StoreError>((store, index, readiness))
        })
        .await
        .map_err(|_| DaemonError::RuntimeUnavailable)?
        .map_err(map_store_error)?;
        let (store, index, index_readiness) = opened;
        let health = ReadServiceHealth::new(
            StoreReadHealth::Ready,
            match index_readiness {
                IndexReadiness::Ready => IndexReadHealth::Ready,
                IndexReadiness::Degraded => IndexReadHealth::Degraded,
                IndexReadiness::Missing => IndexReadHealth::Missing,
            },
        );
        let backend = Arc::new(CanonicalReadBackend::new(
            Arc::new(RwLock::new(store)),
            index,
            config.cursor_mac_key,
            health,
        ));

        let cancellation = CancellationToken::new();
        let handler_constructions = HandlerConstructionProbe::default();
        let credential_digests = config
            .clients
            .iter()
            .map(|client| client.bearer_digest)
            .collect();
        let clients = config
            .clients
            .into_iter()
            .map(|client| ClientRuntime {
                service: client_service(
                    client,
                    backend.clone(),
                    cancellation.clone(),
                    handler_constructions.clone(),
                ),
            })
            .collect();
        let state = Arc::new(DaemonState {
            credential_digests,
            clients,
            backend,
        });

        let listener = TcpListener::bind(config.bind)
            .await
            .map_err(|_| DaemonError::ListenerUnavailable)?;
        let local_addr = listener
            .local_addr()
            .map_err(|_| DaemonError::ListenerUnavailable)?;
        let router = Router::new()
            .route(MCP_ROUTE, any(mcp))
            .route(crate::LIVENESS_ROUTE, get(liveness))
            .route(crate::READINESS_ROUTE, get(readiness))
            .with_state(state);
        let shutdown = cancellation.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
        });

        Ok(Self {
            local_addr,
            cancellation,
            task: Some(task),
            #[cfg(test)]
            handler_constructions,
        })
    }

    #[must_use]
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    #[cfg(test)]
    pub(crate) fn handler_construction_count(&self) -> usize {
        self.handler_constructions.count()
    }

    /// Wait for the HTTP task to terminate unexpectedly. Awaiting the task by
    /// mutable reference is cancellation-safe: if this future is dropped by a
    /// signal race, [`RunningDaemon::shutdown`] still owns and can join it.
    pub async fn wait(&mut self) -> Result<(), DaemonError> {
        let result = self
            .task
            .as_mut()
            .ok_or(DaemonError::RuntimeUnavailable)?
            .await;
        self.task = None;
        let _ = result;
        Err(DaemonError::RuntimeUnavailable)
    }

    #[cfg(test)]
    pub(crate) fn abort_transport_for_test(&self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }

    /// Stop accepting requests, cancel active MCP sessions, and wait until the
    /// router releases the singleton store and its exclusive filesystem lock.
    pub async fn shutdown(mut self) -> Result<(), DaemonError> {
        self.cancellation.cancel();
        let task = self.task.take().ok_or(DaemonError::RuntimeUnavailable)?;
        task.await
            .map_err(|_| DaemonError::RuntimeUnavailable)?
            .map_err(|_| DaemonError::RuntimeUnavailable)
    }
}

impl Drop for RunningDaemon {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

fn client_service(
    client: ValidatedClient,
    backend: Arc<CanonicalReadBackend>,
    cancellation: CancellationToken,
    handler_constructions: HandlerConstructionProbe,
) -> McpHttpService {
    let scopes = client.scopes;
    let context = client.context;
    let creation_actor = client.creation_actor;
    let policy: Arc<dyn MutationPolicy> = client.mutation_policy;
    let factory = move || {
        handler_constructions.record();
        JianduReadServer::new_with_mutations(
            backend.clone(),
            &scopes,
            &context,
            policy.clone(),
            creation_actor,
        )
        .map_err(|_| io::Error::other("authenticated MCP handler unavailable"))
    };
    let transport_config = StreamableHttpServerConfig::default()
        .with_json_response(true)
        .with_cancellation_token(cancellation);
    StreamableHttpService::new(
        factory,
        Arc::new(LocalSessionManager::default()),
        transport_config,
    )
}

async fn mcp(State(state): State<Arc<DaemonState>>, mut request: Request) -> Response<Body> {
    let Some(client_index) = authenticate(request.headers(), &state.credential_digests) else {
        return unauthorized();
    };
    request.headers_mut().remove(AUTHORIZATION);
    let response = state.clients[client_index].service.handle(request).await;
    let (parts, body) = response.into_parts();
    Response::from_parts(parts, Body::new(body))
}

fn unauthorized() -> Response<Body> {
    let mut response = (
        StatusCode::UNAUTHORIZED,
        [(CONTENT_TYPE, "application/json")],
        Body::from(r#"{"error":"unauthorized"}"#),
    )
        .into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    response
}

#[derive(Serialize)]
struct LivenessResponse {
    status: &'static str,
}

async fn liveness() -> impl IntoResponse {
    axum::Json(LivenessResponse { status: "alive" })
}

#[derive(Serialize)]
struct ReadinessResponse {
    status: &'static str,
    health: ReadServiceHealth,
}

async fn readiness(State(state): State<Arc<DaemonState>>) -> Response<Body> {
    let health = state.backend.health();
    let ready = health.store() == StoreReadHealth::Ready;
    let status = if ready { "ready" } else { "not_ready" };
    (
        if ready {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        axum::Json(ReadinessResponse { status, health }),
    )
        .into_response()
}

fn map_store_error(error: StoreError) -> DaemonError {
    match error {
        StoreError::StoreLocked { owner } => DaemonError::StoreLocked { owner },
        other => DaemonError::StoreFailure { code: other.code() },
    }
}

/// Secret- and path-free daemon startup/lifecycle failure.
pub enum DaemonError {
    InvalidConfiguration,
    StoreLocked { owner: Option<LockOwnerDiagnostics> },
    StoreFailure { code: StoreErrorCode },
    ListenerUnavailable,
    RuntimeUnavailable,
}

impl fmt::Debug for DaemonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguration => formatter.write_str("InvalidConfiguration"),
            Self::StoreLocked { owner } => formatter
                .debug_struct("StoreLocked")
                .field("owner", owner)
                .finish(),
            Self::StoreFailure { code } => formatter
                .debug_struct("StoreFailure")
                .field("code", code)
                .finish(),
            Self::ListenerUnavailable => formatter.write_str("ListenerUnavailable"),
            Self::RuntimeUnavailable => formatter.write_str("RuntimeUnavailable"),
        }
    }
}

impl fmt::Display for DaemonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguration => formatter.write_str("daemon configuration is invalid"),
            Self::StoreLocked { owner: Some(owner) } => write!(
                formatter,
                "canonical store is owned by instance {} (pid {}, started {})",
                owner.instance_id, owner.process_id, owner.started_at
            ),
            Self::StoreLocked { owner: None } => {
                formatter.write_str("canonical store is owned by another instance")
            }
            Self::StoreFailure { code } => {
                write!(formatter, "canonical store startup failed: {code:?}")
            }
            Self::ListenerUnavailable => formatter.write_str("loopback listener is unavailable"),
            Self::RuntimeUnavailable => formatter.write_str("daemon runtime is unavailable"),
        }
    }
}

impl std::error::Error for DaemonError {}
