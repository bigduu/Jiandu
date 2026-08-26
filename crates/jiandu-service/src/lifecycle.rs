//! Cancellation-safe daemon admission and bounded transport draining.

use jiandu_core::{
    CreationActor, ForgetMemoryCommand, ForgetMemoryResult, MemoryGetRequest, MemoryListRequest,
    MemoryListResult, MemoryRecord, MemorySearchRequest, MemorySearchResult, MutationInvocation,
    RememberMemoryCommand, RememberMemoryResult, StoreRevision, UpdateMemoryCommand,
    UpdateMemoryResult,
};
use jiandu_mcp::{
    CanonicalReadBackend, McpMutationBackend, McpReadBackend, MutationBackendCommit,
    MutationBackendError, MutationPolicy, MutationPolicyContext, MutationPolicyError,
    MutationPolicyRequest, ReadBackendError, ReadHealthSnapshot, ReadServiceHealth,
};
use jiandu_store::{AuthorizedMutationSet, AuthorizedRead, StoreRead};
use std::fmt;
#[cfg(test)]
use std::sync::Condvar;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use tokio::sync::Notify;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Accepting,
    Draining,
    Forced,
}

#[derive(Debug)]
struct State {
    phase: Phase,
    active_http_operations: usize,
    active_backend_operations: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OperationKind {
    Http,
    Backend,
}

/// One process-local admission authority shared by HTTP routing, MCP mutation
/// workers, and the shutdown supervisor. The mutex makes closing admission and
/// acquiring a worker permit one linearizable operation.
#[derive(Clone)]
pub(crate) struct LifecycleGate {
    state: Arc<Mutex<State>>,
    changed: Arc<Notify>,
    #[cfg(test)]
    http_hook: Arc<Mutex<Option<Arc<HttpAdmissionPause>>>>,
    #[cfg(test)]
    commit_hook: Arc<Mutex<Option<Arc<HttpAdmissionPause>>>>,
}

impl fmt::Debug for LifecycleGate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LifecycleGate")
            .field("state", &"[REDACTED]")
            .finish()
    }
}

impl LifecycleGate {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                phase: Phase::Accepting,
                active_http_operations: 0,
                active_backend_operations: 0,
            })),
            changed: Arc::new(Notify::new()),
            #[cfg(test)]
            http_hook: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            commit_hook: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) fn is_accepting(&self) -> bool {
        self.lock().phase == Phase::Accepting
    }

    /// Close both HTTP and backend admission. A worker that already owns a
    /// permit remains counted until its synchronous backend call returns.
    pub(crate) fn begin_drain(&self) {
        let mut state = self.lock();
        if state.phase == Phase::Accepting {
            state.phase = Phase::Draining;
        }
        drop(state);
        self.changed.notify_waiters();
    }

    /// End the response grace. Fresh workers that have not crossed their
    /// pre-WAL policy boundary must now fail closed.
    pub(crate) fn force(&self) {
        let mut state = self.lock();
        state.phase = Phase::Forced;
        drop(state);
        self.changed.notify_waiters();
    }

    pub(crate) fn ensure_commit_allowed(&self) -> Result<(), MutationPolicyError> {
        if self.lock().phase == Phase::Forced {
            Err(MutationPolicyError::Unavailable)
        } else {
            Ok(())
        }
    }

    #[cfg(test)]
    fn pause_before_commit_recheck(&self) {
        if let Some(hook) = self
            .commit_hook
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
        {
            hook.pause();
        }
    }

    fn try_enter(&self, kind: OperationKind) -> Result<OperationPermit, ReadBackendError> {
        let mut state = self.lock();
        if state.phase != Phase::Accepting {
            return Err(ReadBackendError::HostUnavailable);
        }
        let active = match kind {
            OperationKind::Http => &mut state.active_http_operations,
            OperationKind::Backend => &mut state.active_backend_operations,
        };
        *active = active
            .checked_add(1)
            .ok_or(ReadBackendError::HostUnavailable)?;
        Ok(OperationPermit {
            gate: self.clone(),
            kind,
        })
    }

    /// Atomically linearize one HTTP request against shutdown admission. The
    /// returned guard must span the complete rmcp service future, including a
    /// possible session initialize.
    pub(crate) fn try_enter_http(&self) -> Result<OperationPermit, ReadBackendError> {
        let permit = self.try_enter(OperationKind::Http)?;
        #[cfg(test)]
        if let Some(hook) = self
            .http_hook
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
        {
            hook.pause();
        }
        Ok(permit)
    }

    pub(crate) async fn wait_idle(&self) {
        self.wait_until(|state| {
            state.active_http_operations == 0 && state.active_backend_operations == 0
        })
        .await;
    }

    /// Wait only for work that can own or touch the canonical backend. A
    /// forced shutdown must not depend on an untrusted peer completing an HTTP
    /// upload, but it must retain the writer owner until every backend worker
    /// is quiescent.
    pub(crate) async fn wait_backend_idle(&self) {
        self.wait_until(|state| state.active_backend_operations == 0)
            .await;
    }

    async fn wait_until(&self, idle: impl Fn(&State) -> bool) {
        loop {
            // Register before checking so a final permit cannot notify between
            // the observation and the await.
            let changed = self.changed.notified();
            if idle(&self.lock()) {
                return;
            }
            changed.await;
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // No caller-controlled code runs while this mutex is held. Recovering
        // the tiny counter state keeps permit Drop from leaking a count if an
        // unrelated panic poisoned the mutex.
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    #[cfg(test)]
    pub(crate) fn active_operations(&self) -> usize {
        let state = self.lock();
        state.active_http_operations + state.active_backend_operations
    }

    #[cfg(test)]
    pub(crate) fn active_backend_operations(&self) -> usize {
        self.lock().active_backend_operations
    }

    #[cfg(test)]
    pub(crate) fn is_forced(&self) -> bool {
        self.lock().phase == Phase::Forced
    }

    #[cfg(test)]
    pub(crate) fn install_http_pause(&self, pause: Arc<HttpAdmissionPause>) {
        *self
            .http_hook
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(pause);
    }

    #[cfg(test)]
    pub(crate) fn install_commit_pause(&self, pause: Arc<HttpAdmissionPause>) {
        *self
            .commit_hook
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(pause);
    }
}

pub(crate) struct OperationPermit {
    gate: LifecycleGate,
    kind: OperationKind,
}

#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct HttpAdmissionPause {
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}

#[cfg(test)]
impl HttpAdmissionPause {
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

impl Drop for OperationPermit {
    fn drop(&mut self) {
        let mut state = self.gate.lock();
        let active = match self.kind {
            OperationKind::Http => &mut state.active_http_operations,
            OperationKind::Backend => &mut state.active_backend_operations,
        };
        *active = active
            .checked_sub(1)
            .expect("a lifecycle permit is released exactly once");
        let changed = *active == 0;
        drop(state);
        if changed {
            self.gate.changed.notify_waiters();
        }
    }
}

/// Service-private backend facade. Session handlers retain only a weak link
/// to the canonical owner, so stale rmcp session tasks cannot prolong the
/// filesystem lock after the supervisor has drained all entered operations.
pub(crate) struct LifecycleBackend {
    inner: Weak<CanonicalReadBackend>,
    health: ReadHealthSnapshot,
    gate: LifecycleGate,
}

impl LifecycleBackend {
    pub(crate) fn new(inner: &Arc<CanonicalReadBackend>, gate: LifecycleGate) -> Self {
        Self {
            inner: Arc::downgrade(inner),
            health: inner.health_snapshot(),
            gate,
        }
    }

    fn entered(&self) -> Result<(OperationPermit, Arc<CanonicalReadBackend>), ReadBackendError> {
        let permit = self.gate.try_enter(OperationKind::Backend)?;
        let inner = self
            .inner
            .upgrade()
            .ok_or(ReadBackendError::HostUnavailable)?;
        Ok((permit, inner))
    }

    fn entered_mutation(
        &self,
    ) -> Result<(OperationPermit, Arc<CanonicalReadBackend>), MutationBackendError> {
        self.entered()
            .map_err(|error| MutationBackendError::new(error, StoreRevision(0)))
    }
}

impl fmt::Debug for LifecycleBackend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LifecycleBackend")
            .field("backend", &"[REDACTED]")
            .field("lifecycle", &"[REDACTED]")
            .finish()
    }
}

impl McpReadBackend for LifecycleBackend {
    fn get(
        &self,
        authorization: &AuthorizedRead,
        request: &MemoryGetRequest,
    ) -> Result<StoreRead<MemoryRecord>, ReadBackendError> {
        let (_permit, inner) = self.entered()?;
        inner.get(authorization, request)
    }

    fn list(
        &self,
        authorization: &AuthorizedRead,
        request: &MemoryListRequest,
    ) -> Result<StoreRead<MemoryListResult>, ReadBackendError> {
        let (_permit, inner) = self.entered()?;
        inner.list(authorization, request)
    }

    fn search(
        &self,
        authorization: &AuthorizedRead,
        request: &MemorySearchRequest,
    ) -> Result<(StoreRevision, MemorySearchResult), ReadBackendError> {
        let (_permit, inner) = self.entered()?;
        inner.search(authorization, request)
    }

    fn store_revision(&self) -> Result<StoreRevision, ReadBackendError> {
        let (_permit, inner) = self.entered()?;
        inner.store_revision()
    }

    fn health(&self) -> ReadServiceHealth {
        self.health.current()
    }
}

impl McpMutationBackend for LifecycleBackend {
    fn remember(
        &self,
        authorization: &AuthorizedMutationSet,
        invocation: &MutationInvocation,
        policy: &dyn MutationPolicy,
        creation_actor: CreationActor,
        command: &RememberMemoryCommand,
    ) -> Result<MutationBackendCommit<RememberMemoryResult>, MutationBackendError> {
        let (_permit, inner) = self.entered_mutation()?;
        inner.remember(authorization, invocation, policy, creation_actor, command)
    }

    fn update(
        &self,
        authorization: &AuthorizedMutationSet,
        invocation: &MutationInvocation,
        policy: &dyn MutationPolicy,
        command: &UpdateMemoryCommand,
    ) -> Result<MutationBackendCommit<UpdateMemoryResult>, MutationBackendError> {
        let (_permit, inner) = self.entered_mutation()?;
        inner.update(authorization, invocation, policy, command)
    }

    fn forget(
        &self,
        authorization: &AuthorizedMutationSet,
        invocation: &MutationInvocation,
        policy: &dyn MutationPolicy,
        command: &ForgetMemoryCommand,
    ) -> Result<MutationBackendCommit<ForgetMemoryResult>, MutationBackendError> {
        let (_permit, inner) = self.entered_mutation()?;
        inner.forget(authorization, invocation, policy, command)
    }
}

/// Checks forced shutdown at the existing canonical admission closure, after
/// replay/conflict/CAS resolution but before the first WAL byte.
pub(crate) struct LifecycleMutationPolicy {
    inner: Arc<dyn MutationPolicy>,
    gate: LifecycleGate,
}

impl LifecycleMutationPolicy {
    pub(crate) fn new(inner: Arc<dyn MutationPolicy>, gate: LifecycleGate) -> Self {
        Self { inner, gate }
    }
}

impl fmt::Debug for LifecycleMutationPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LifecycleMutationPolicy")
            .field("policy", &"[REDACTED]")
            .field("lifecycle", &"[REDACTED]")
            .finish()
    }
}

impl MutationPolicy for LifecycleMutationPolicy {
    fn evaluate(
        &self,
        context: &MutationPolicyContext,
        request: MutationPolicyRequest<'_>,
    ) -> Result<(), MutationPolicyError> {
        self.inner.evaluate(context, request)?;
        // This is the final lifecycle check before the canonical admission
        // closure returns and WAL persistence can begin.
        #[cfg(test)]
        self.gate.pause_before_commit_recheck();
        self.gate.ensure_commit_allowed()
    }
}
