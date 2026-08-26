//! Loopback-only authenticated Streamable HTTP daemon lifecycle.

use crate::auth::authenticate;
use crate::config::{BearerDigest, ValidatedClient};
use crate::lifecycle::{LifecycleBackend, LifecycleGate, LifecycleMutationPolicy};
use crate::{MCP_ROUTE, ServeConfig};
use axum::Router;
use axum::body::{Body, Bytes, HttpBody};
use axum::extract::{Request, State};
use axum::http::{
    HeaderValue, Method, Response, StatusCode,
    header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, WWW_AUTHENTICATE},
};
use axum::response::IntoResponse;
use axum::routing::{any, get};
use axum::serve::Listener;
use jiandu_index::{IndexReadiness, LexicalIndex};
use jiandu_mcp::{
    CanonicalReadBackend, IndexReadHealth, JianduReadServer, MutationPolicy, ReadHealthSnapshot,
    ReadServiceHealth, StoreReadHealth,
};
use jiandu_store::{
    CanonicalStore, LockOwnerDiagnostics, StoreError, StoreErrorCode, StoreOptions,
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService,
    session::{SessionManager, local::LocalSessionManager},
};
use serde::Serialize;
use std::fmt;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
#[cfg(test)]
use std::sync::{Condvar, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

type McpHttpService = StreamableHttpService<JianduReadServer, LocalSessionManager>;

/// Axum owns its accepted Hyper connection tasks, so aborting the top-level
/// accept loop cannot force a peer that is still uploading a request body to
/// finish. Wrapping every accepted socket makes the service cancellation token
/// an actual connection-level cutoff on every platform.
struct ShutdownListener {
    inner: TcpListener,
    cancellation: CancellationToken,
}

impl ShutdownListener {
    fn new(inner: TcpListener, cancellation: CancellationToken) -> Self {
        Self {
            inner,
            cancellation,
        }
    }
}

impl Listener for ShutdownListener {
    type Io = ShutdownIo;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.inner.accept().await {
                Ok((stream, address)) => {
                    return (ShutdownIo::new(stream, self.cancellation.clone()), address);
                }
                Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

struct ShutdownIo {
    inner: TcpStream,
    cancelled: Pin<Box<dyn Future<Output = ()> + Send>>,
}

impl ShutdownIo {
    fn new(inner: TcpStream, cancellation: CancellationToken) -> Self {
        Self {
            inner,
            cancelled: Box::pin(cancellation.cancelled_owned()),
        }
    }

    fn poll_cancelled(&mut self, context: &mut Context<'_>) -> io::Result<()> {
        if self.cancelled.as_mut().poll(context).is_ready() {
            Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "service transport cancelled",
            ))
        } else {
            Ok(())
        }
    }
}

impl AsyncRead for ShutdownIo {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Err(error) = this.poll_cancelled(context) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut this.inner).poll_read(context, buffer)
    }
}

impl AsyncWrite for ShutdownIo {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if let Err(error) = this.poll_cancelled(context) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut this.inner).poll_write(context, buffer)
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Err(error) = this.poll_cancelled(context) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut this.inner).poll_flush(context)
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Err(error) = this.poll_cancelled(context) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut this.inner).poll_shutdown(context)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffers: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if let Err(error) = this.poll_cancelled(context) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut this.inner).poll_write_vectored(context, buffers)
    }
}

struct ClientRuntime {
    service: McpHttpService,
}

struct DaemonState {
    credential_digests: Vec<BearerDigest>,
    clients: Vec<ClientRuntime>,
    health: ReadHealthSnapshot,
    lifecycle: LifecycleGate,
    response_cancellation: CancellationToken,
    #[cfg(test)]
    final_frame_pause: FinalFramePauseProbe,
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

#[cfg(test)]
#[derive(Clone, Default)]
struct FinalFramePauseProbe(Arc<Mutex<Option<Arc<ResponseFramePause>>>>);

#[cfg(test)]
impl FinalFramePauseProbe {
    fn install(&self, pause: Arc<ResponseFramePause>) {
        *self.0.lock().unwrap_or_else(|error| error.into_inner()) = Some(pause);
    }

    fn current(&self) -> Option<Arc<ResponseFramePause>> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct ResponseFramePause {
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}

#[cfg(test)]
impl ResponseFramePause {
    fn pause(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.0 = true;
        self.changed.notify_all();
        while !state.1 {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
    }

    pub(crate) fn wait_reached(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        while !state.0 && !state.1 {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
        state.0
    }

    pub(crate) fn release(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.1 = true;
        self.changed.notify_all();
    }
}

/// One running daemon. The canonical lock is retained by the supervised
/// backend and any entered worker until [`RunningDaemon::shutdown`] completes.
pub struct RunningDaemon {
    local_addr: SocketAddr,
    listener_cancellation: CancellationToken,
    session_cancellation: CancellationToken,
    connection_cancellation: CancellationToken,
    lifecycle: LifecycleGate,
    drain_timeout: Duration,
    task: Option<JoinHandle<Result<(), io::Error>>>,
    backend_owner: Option<Arc<CanonicalReadBackend>>,
    session_managers: Option<Vec<Arc<LocalSessionManager>>>,
    #[cfg(test)]
    graceful_cleanup_delay: Option<Duration>,
    #[cfg(test)]
    handler_constructions: HandlerConstructionProbe,
    #[cfg(test)]
    final_frame_pause: FinalFramePauseProbe,
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
        Self::start_with_options(config, StoreOptions::default()).await
    }

    #[cfg(test)]
    pub(crate) async fn start_with_store_options_for_test(
        config: ServeConfig,
        options: StoreOptions,
    ) -> Result<Self, DaemonError> {
        Self::start_with_options(config, options).await
    }

    async fn start_with_options(
        config: ServeConfig,
        store_options: StoreOptions,
    ) -> Result<Self, DaemonError> {
        if !config.bind.ip().is_loopback() {
            return Err(DaemonError::InvalidConfiguration);
        }

        let owner = jiandu_store::LockOwner::for_current_process().map_err(map_store_error)?;
        let data_dir = config.data_dir.clone();
        let opened = tokio::task::spawn_blocking(move || {
            let store = CanonicalStore::open_with_options(&data_dir, owner, store_options)?;
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

        let lifecycle = LifecycleGate::new();
        let service_backend = Arc::new(LifecycleBackend::new(&backend, lifecycle.clone()));
        let listener_cancellation = CancellationToken::new();
        let session_cancellation = CancellationToken::new();
        let connection_cancellation = CancellationToken::new();
        let handler_constructions = HandlerConstructionProbe::default();
        #[cfg(test)]
        let final_frame_pause = FinalFramePauseProbe::default();
        let credential_digests = config
            .clients
            .iter()
            .map(|client| client.bearer_digest)
            .collect();
        let mut clients = Vec::with_capacity(config.clients.len());
        let mut session_managers = Vec::with_capacity(config.clients.len());
        for client in config.clients {
            let (service, session_manager) = client_service(
                client,
                service_backend.clone(),
                lifecycle.clone(),
                session_cancellation.clone(),
                handler_constructions.clone(),
            );
            clients.push(ClientRuntime { service });
            session_managers.push(session_manager);
        }
        let state = Arc::new(DaemonState {
            credential_digests,
            clients,
            // Readiness retains only the separately allocated closed health
            // value. Router and connection clones cannot own the store.
            health: backend.health_snapshot(),
            lifecycle: lifecycle.clone(),
            response_cancellation: session_cancellation.clone(),
            #[cfg(test)]
            final_frame_pause: final_frame_pause.clone(),
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
        let shutdown = listener_cancellation.clone();
        let listener = ShutdownListener::new(listener, connection_cancellation.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
        });

        Ok(Self {
            local_addr,
            listener_cancellation,
            session_cancellation,
            connection_cancellation,
            lifecycle,
            drain_timeout: config.drain_timeout,
            task: Some(task),
            backend_owner: Some(backend),
            session_managers: Some(session_managers),
            #[cfg(test)]
            graceful_cleanup_delay: None,
            #[cfg(test)]
            handler_constructions,
            #[cfg(test)]
            final_frame_pause,
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

    #[cfg(test)]
    pub(crate) fn install_http_admission_pause(
        &self,
        pause: Arc<crate::lifecycle::HttpAdmissionPause>,
    ) {
        self.lifecycle.install_http_pause(pause);
    }

    #[cfg(test)]
    pub(crate) fn install_commit_admission_pause(
        &self,
        pause: Arc<crate::lifecycle::HttpAdmissionPause>,
    ) {
        self.lifecycle.install_commit_pause(pause);
    }

    #[cfg(test)]
    pub(crate) fn lifecycle_for_test(&self) -> LifecycleGate {
        self.lifecycle.clone()
    }

    #[cfg(test)]
    pub(crate) fn health_snapshot_for_test(&self) -> ReadHealthSnapshot {
        self.backend_owner
            .as_ref()
            .expect("a running test daemon owns its backend")
            .health_snapshot()
    }

    #[cfg(test)]
    pub(crate) fn session_managers_for_test(&self) -> Vec<Arc<LocalSessionManager>> {
        self.session_managers.clone().unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn install_graceful_cleanup_delay_for_test(&mut self, delay: Duration) {
        self.graceful_cleanup_delay = Some(delay);
    }

    #[cfg(test)]
    pub(crate) fn install_final_frame_pause(&self, pause: Arc<ResponseFramePause>) {
        self.final_frame_pause.install(pause);
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
        self.lifecycle.begin_drain();
        self.lifecycle.force();
        self.session_cancellation.cancel();
        self.connection_cancellation.cancel();
        self.listener_cancellation.cancel();
        if let Some(session_managers) = self.session_managers.take() {
            close_all_sessions(&session_managers).await;
            self.lifecycle.wait_backend_idle().await;
            self.lifecycle.wait_idle().await;
            close_all_sessions(&session_managers).await;
        } else {
            self.lifecycle.wait_backend_idle().await;
            self.lifecycle.wait_idle().await;
        }
        self.backend_owner = None;
        let _ = result;
        Err(DaemonError::RuntimeUnavailable)
    }

    #[cfg(test)]
    pub(crate) fn abort_transport_for_test(&self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }

    /// Close admission immediately, allow already-entered operations one
    /// configured response grace, and supervise cleanup even if the returned
    /// future is dropped. A forced timeout cancels transport acknowledgements
    /// but still waits for an already-started canonical worker to quiesce
    /// before releasing the exclusive store owner.
    pub fn shutdown(mut self) -> impl Future<Output = Result<ShutdownOutcome, DaemonError>> + Send {
        let supervisor = self.spawn_shutdown_supervisor();
        async move {
            supervisor
                .ok_or(DaemonError::RuntimeUnavailable)?
                .await
                .map_err(|_| DaemonError::RuntimeUnavailable)?
        }
    }

    fn spawn_shutdown_supervisor(
        &mut self,
    ) -> Option<JoinHandle<Result<ShutdownOutcome, DaemonError>>> {
        let runtime = tokio::runtime::Handle::try_current().ok()?;
        let task = self.task.take()?;
        // Capture the absolute response-grace deadline synchronously with the
        // admission transition. Runtime scheduling delay must consume, not
        // extend, the configured grace.
        let drain_started_at = tokio::time::Instant::now();
        self.lifecycle.begin_drain();
        let deadline = drain_started_at + self.drain_timeout;
        let lifecycle = self.lifecycle.clone();
        let listener_cancellation = self.listener_cancellation.clone();
        let session_cancellation = self.session_cancellation.clone();
        let connection_cancellation = self.connection_cancellation.clone();
        let backend_owner = self.backend_owner.take();
        let session_managers = self.session_managers.take().unwrap_or_default();
        #[cfg(test)]
        let graceful_cleanup_delay = self.graceful_cleanup_delay.take();
        #[cfg(not(test))]
        let graceful_cleanup_delay = None;
        Some(runtime.spawn(async move {
            supervise_shutdown(
                ShutdownResources {
                    task,
                    backend_owner,
                    lifecycle,
                    listener_cancellation,
                    session_cancellation,
                    connection_cancellation,
                    session_managers,
                    graceful_cleanup_delay,
                },
                deadline,
            )
            .await
        }))
    }
}

impl Drop for RunningDaemon {
    fn drop(&mut self) {
        if self.task.is_none() && self.backend_owner.is_none() && self.session_managers.is_none() {
            return;
        }
        if tokio::runtime::Handle::try_current().is_ok() {
            let _ = self.spawn_shutdown_supervisor();
        } else {
            self.lifecycle.begin_drain();
            self.lifecycle.force();
            self.session_cancellation.cancel();
            self.connection_cancellation.cancel();
            self.listener_cancellation.cancel();
            if let Some(task) = self.task.take() {
                task.abort();
            }
            self.backend_owner = None;
            self.session_managers = None;
        }
    }
}

/// Closed host-visible shutdown result. Forced transport cancellation is not a
/// claim that a blocking canonical filesystem operation was killed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownOutcome {
    /// Every admitted finite response, backend operation, session close, and
    /// listener join completed within the configured absolute grace.
    Drained,
    /// Transport grace expired and response/session work was forced closed.
    /// The supervisor nevertheless waited for canonical worker quiescence and
    /// released the singleton store owner before returning this outcome.
    ForcedAfterTimeout,
}

struct ShutdownResources {
    task: JoinHandle<Result<(), io::Error>>,
    backend_owner: Option<Arc<CanonicalReadBackend>>,
    lifecycle: LifecycleGate,
    listener_cancellation: CancellationToken,
    session_cancellation: CancellationToken,
    connection_cancellation: CancellationToken,
    session_managers: Vec<Arc<LocalSessionManager>>,
    graceful_cleanup_delay: Option<Duration>,
}

async fn supervise_shutdown(
    resources: ShutdownResources,
    deadline: tokio::time::Instant,
) -> Result<ShutdownOutcome, DaemonError> {
    let ShutdownResources {
        mut task,
        backend_owner,
        lifecycle,
        listener_cancellation,
        session_cancellation,
        connection_cancellation,
        session_managers,
        graceful_cleanup_delay,
    } = resources;
    let graceful = tokio::time::timeout_at(deadline, async {
        lifecycle.wait_idle().await;
        if let Some(delay) = graceful_cleanup_delay {
            // Deterministic test seam for proving the one absolute deadline
            // also covers post-idle session/listener cleanup.
            tokio::time::sleep(delay).await;
        }
        // Every admitted HTTP/backend permit is gone, so no session create or
        // insert critical section can race this snapshot. Closing rmcp first
        // lets idle session state terminate before Hyper graceful wait.
        session_cancellation.cancel();
        close_all_sessions(&session_managers).await;
        listener_cancellation.cancel();
        (&mut task)
            .await
            .map_err(|_| DaemonError::RuntimeUnavailable)?
            .map_err(|_| DaemonError::RuntimeUnavailable)
    })
    .await;

    let mut transport_failed = false;
    let outcome = match graceful {
        Ok(Ok(())) => ShutdownOutcome::Drained,
        Ok(Err(_)) => {
            // Cleanup above has already closed sessions and joined transport.
            // Preserve the error, but never return it before canonical worker
            // leases are quiescent and the owner has been released below.
            transport_failed = true;
            ShutdownOutcome::Drained
        }
        Err(_) => {
            lifecycle.force();
            // An admitted request may still be in rmcp. End its response path
            // and every accepted connection before snapshotting the manager.
            session_cancellation.cancel();
            connection_cancellation.cancel();
            listener_cancellation.cancel();
            task.abort();
            match (&mut task).await {
                Ok(Ok(())) => {}
                Err(error) if error.is_cancelled() => {}
                Ok(Err(_)) | Err(_) => {
                    transport_failed = true;
                }
            }
            close_all_sessions(&session_managers).await;
            ShutdownOutcome::ForcedAfterTimeout
        }
    };

    // spawn_blocking cannot be killed safely. Backend permits are owned inside
    // those workers, so this wait survives request-future and transport
    // cancellation. Once canonical work is safe, wait for every force-cancelled
    // socket/response permit as well; ShutdownIo makes that independent of the
    // peer completing an upload or consuming a response.
    lifecycle.wait_backend_idle().await;
    lifecycle.wait_idle().await;
    // A forced Axum abort cannot by itself prove that every spawned Hyper
    // connection stopped before the first manager snapshot. With admission
    // now quiescent, a second exact snapshot closes any late insertion before
    // the weak facade loses its canonical owner.
    close_all_sessions(&session_managers).await;
    drop(backend_owner);
    if transport_failed {
        Err(DaemonError::RuntimeUnavailable)
    } else {
        Ok(outcome)
    }
}

async fn close_all_sessions(session_managers: &[Arc<LocalSessionManager>]) {
    for manager in session_managers {
        let session_ids = manager
            .sessions
            .read()
            .await
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for session_id in session_ids {
            let _ = manager.close_session(&session_id).await;
        }
    }
}

fn client_service(
    client: ValidatedClient,
    backend: Arc<LifecycleBackend>,
    lifecycle: LifecycleGate,
    cancellation: CancellationToken,
    handler_constructions: HandlerConstructionProbe,
) -> (McpHttpService, Arc<LocalSessionManager>) {
    let scopes = client.scopes;
    let context = client.context;
    let creation_actor = client.creation_actor;
    let policy = client
        .mutation_policy
        .map(|policy| -> Arc<dyn MutationPolicy> {
            Arc::new(LifecycleMutationPolicy::new(policy, lifecycle))
        });
    let factory = move || {
        handler_constructions.record();
        match &policy {
            Some(policy) => JianduReadServer::new_with_mutations(
                backend.clone(),
                &scopes,
                &context,
                policy.clone(),
                creation_actor,
            ),
            None => JianduReadServer::new(backend.clone(), &scopes, &context),
        }
        .map_err(|_| io::Error::other("authenticated MCP handler unavailable"))
    };
    let transport_config = StreamableHttpServerConfig::default()
        .with_json_response(true)
        .with_cancellation_token(cancellation);
    let session_manager = Arc::new(LocalSessionManager::default());
    let service = StreamableHttpService::new(factory, session_manager.clone(), transport_config);
    (service, session_manager)
}

async fn mcp(State(state): State<Arc<DaemonState>>, mut request: Request) -> Response<Body> {
    let Some(client_index) = authenticate(request.headers(), &state.credential_digests) else {
        return unauthorized();
    };
    request.headers_mut().remove(AUTHORIZATION);
    let Ok(http_permit) = state.lifecycle.try_enter_http() else {
        return unavailable();
    };
    // A GET is rmcp's long-lived SSE receive stream. It is protected while
    // the handler resolves the session, then explicitly terminated by the
    // bounded session-close phase. Holding its permit to EOF would make every
    // otherwise-idle client force the grace deadline. Finite request/response
    // methods retain their permit through body delivery so a committed tool
    // result is either delivered within grace or transport is forced closed.
    let hold_permit_through_body = request.method() != Method::GET;
    let response = state.clients[client_index].service.handle(request).await;
    let (parts, body) = response.into_parts();
    if !hold_permit_through_body {
        drop(http_permit);
        return Response::from_parts(parts, Body::new(body));
    }
    Response::from_parts(
        parts,
        Body::new(PermitBody {
            inner: Body::new(body),
            cancelled: Box::pin(state.response_cancellation.clone().cancelled_owned()),
            permit: Some(http_permit),
            #[cfg(test)]
            final_frame_pause: state.final_frame_pause.current(),
        }),
    )
}

struct PermitBody {
    inner: Body,
    cancelled: Pin<Box<dyn Future<Output = ()> + Send>>,
    permit: Option<crate::lifecycle::OperationPermit>,
    #[cfg(test)]
    final_frame_pause: Option<Arc<ResponseFramePause>>,
}

impl HttpBody for PermitBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let this = self.as_mut().get_mut();
        if this.cancelled.as_mut().poll(context).is_ready() {
            this.permit.take();
            return Poll::Ready(None);
        }
        let frame = Pin::new(&mut this.inner).poll_frame(context);
        let final_data_frame =
            matches!(frame, Poll::Ready(Some(Ok(_)))) && this.inner.is_end_stream();
        let completed = matches!(frame, Poll::Ready(None)) || final_data_frame;
        if completed || matches!(frame, Poll::Ready(Some(Err(_)))) {
            #[cfg(test)]
            let released = this.permit.take().is_some();
            #[cfg(not(test))]
            this.permit.take();
            #[cfg(test)]
            if released
                && completed
                && let Some(pause) = &this.final_frame_pause
            {
                pause.pause();
            }
        }
        frame
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

fn unavailable() -> Response<Body> {
    let mut response = (
        StatusCode::SERVICE_UNAVAILABLE,
        [(CONTENT_TYPE, "application/json")],
        Body::from(r#"{"error":"service_unavailable"}"#),
    )
        .into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
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
    if !state.lifecycle.is_accepting() {
        let health = state.health.current();
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(ReadinessResponse {
                status: "not_ready",
                health,
            }),
        )
            .into_response();
    }
    let health = state.health.current();
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
