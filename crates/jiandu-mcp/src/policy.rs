//! Agent-neutral, injectable mutation admission policy.

use jiandu_core::{
    ClientId, CorrelationId, ForgetMemoryCommand, MAX_BODY_BYTES, MemoryRecord, MemoryScope,
    MemoryType, PrincipalId, RememberMemoryCommand, UpdateMemoryCommand,
};
use jiandu_store::{AuthorizedMutation, MutationOperation};
use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

/// Closed scope categories used by configured mutation policy.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum MutationScopeKind {
    Principal,
    Project,
    Session,
    InstanceGlobal,
}

impl MutationScopeKind {
    const fn from_scope(scope: &MemoryScope) -> Self {
        match scope {
            MemoryScope::Principal { .. } => Self::Principal,
            MemoryScope::Project { .. } => Self::Project,
            MemoryScope::Session { .. } => Self::Session,
            MemoryScope::InstanceGlobal {} => Self::InstanceGlobal,
        }
    }

    const ALL: [Self; 4] = [
        Self::Principal,
        Self::Project,
        Self::Session,
        Self::InstanceGlobal,
    ];
}

/// Secret-safe admission failure. No policy-produced message or inspected
/// content crosses into public MCP diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutationPolicyError {
    InvalidRequest,
    Forbidden,
}

/// Trusted identity and durable-attempt correlation passed to policy without
/// implementing a wire codec. Debug output deliberately redacts all fields.
#[derive(Clone, Eq, PartialEq)]
pub struct MutationPolicyContext {
    principal_id: PrincipalId,
    client_id: ClientId,
    correlation_id: CorrelationId,
    scope: MemoryScope,
    operation: MutationOperation,
}

impl fmt::Debug for MutationPolicyContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MutationPolicyContext")
            .field("identity", &"[REDACTED]")
            .field("correlation", &"[REDACTED]")
            .field("scope", &"[REDACTED]")
            .field("operation", &self.operation)
            .finish()
    }
}

impl MutationPolicyContext {
    pub(crate) fn new(authorization: &AuthorizedMutation, correlation_id: CorrelationId) -> Self {
        Self {
            principal_id: authorization.principal_id().clone(),
            client_id: authorization.client_id().clone(),
            correlation_id,
            scope: authorization.as_scope().clone(),
            operation: authorization.operation(),
        }
    }

    #[must_use]
    pub fn principal_id(&self) -> &PrincipalId {
        &self.principal_id
    }

    #[must_use]
    pub fn client_id(&self) -> &ClientId {
        &self.client_id
    }

    #[must_use]
    pub fn correlation_id(&self) -> &CorrelationId {
        &self.correlation_id
    }

    #[must_use]
    pub fn scope(&self) -> &MemoryScope {
        &self.scope
    }

    #[must_use]
    pub const fn operation(&self) -> MutationOperation {
        self.operation
    }
}

/// Borrowed canonical command supplied to a trusted policy implementation.
pub enum MutationPolicyRequest<'a> {
    Remember {
        command: &'a RememberMemoryCommand,
        target: &'a MemoryRecord,
    },
    Update {
        command: &'a UpdateMemoryCommand,
        target: &'a MemoryRecord,
    },
    Forget(&'a ForgetMemoryCommand),
}

impl MutationPolicyRequest<'_> {
    const fn operation(&self) -> MutationOperation {
        match self {
            Self::Remember { .. } => MutationOperation::Create,
            Self::Update { .. } => MutationOperation::Update,
            Self::Forget(_) => MutationOperation::Forget,
        }
    }

    fn canonical_bytes(&self) -> Result<Vec<u8>, MutationPolicyError> {
        match self {
            Self::Remember { command, target } => serde_json::to_vec(&(command, target)),
            Self::Update { command, target } => serde_json::to_vec(&(command, target)),
            Self::Forget(command) => serde_json::to_vec(command),
        }
        .map_err(|_| MutationPolicyError::InvalidRequest)
    }
}

/// Narrow host policy seam. Implementations receive trusted identity beside a
/// validated domain command and must return only a closed safe decision.
/// Implementations must be local, synchronous, bounded, non-reentrant,
/// panic-free, and free of network I/O because evaluation occurs while the
/// canonical writer guard is held immediately before WAL persistence.
pub trait MutationPolicy: Send + Sync + 'static {
    fn evaluate(
        &self,
        context: &MutationPolicyContext,
        request: MutationPolicyRequest<'_>,
    ) -> Result<(), MutationPolicyError>;
}

/// Secret detector supplied by the host. Inspected canonical command bytes are
/// never retained or included in the boolean result.
pub trait SecretContentPolicy: Send + Sync + 'static {
    fn contains_secret(&self, context: &MutationPolicyContext, canonical_command: &[u8]) -> bool;
}

/// Explicit policy for deployments whose upstream admission layer has no
/// additional secret detector. Selecting this is an intentional host choice.
#[derive(Clone, Copy, Debug, Default)]
pub struct AllowAllSecretContent;

impl SecretContentPolicy for AllowAllSecretContent {
    fn contains_secret(&self, _context: &MutationPolicyContext, _canonical_command: &[u8]) -> bool {
        false
    }
}

/// Configured size/type/scope admission with an injected secret-content
/// detector. Core command validation remains authoritative for the public
/// v1alpha1 maximums; this policy may only narrow them.
pub struct ConfiguredMutationPolicy {
    max_body_bytes: usize,
    allowed_types: BTreeSet<MemoryType>,
    allowed_scopes: BTreeSet<MutationScopeKind>,
    secret_content: Arc<dyn SecretContentPolicy>,
}

impl fmt::Debug for ConfiguredMutationPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConfiguredMutationPolicy")
            .field("max_body_bytes", &self.max_body_bytes)
            .field("allowed_types", &self.allowed_types)
            .field("allowed_scopes", &self.allowed_scopes)
            .field("secret_content", &"[REDACTED]")
            .finish()
    }
}

impl ConfiguredMutationPolicy {
    /// Construct a policy that can only narrow the committed core bounds.
    pub fn new(
        max_body_bytes: usize,
        allowed_types: BTreeSet<MemoryType>,
        allowed_scopes: BTreeSet<MutationScopeKind>,
        secret_content: Arc<dyn SecretContentPolicy>,
    ) -> Result<Self, MutationPolicyError> {
        if max_body_bytes == 0
            || max_body_bytes > MAX_BODY_BYTES
            || allowed_types.is_empty()
            || allowed_scopes.is_empty()
        {
            return Err(MutationPolicyError::InvalidRequest);
        }
        Ok(Self {
            max_body_bytes,
            allowed_types,
            allowed_scopes,
            secret_content,
        })
    }

    /// Explicit all-domain policy, still using the supplied secret detector.
    pub fn allow_all(
        secret_content: Arc<dyn SecretContentPolicy>,
    ) -> Result<Self, MutationPolicyError> {
        Self::new(
            MAX_BODY_BYTES,
            BTreeSet::from([
                MemoryType::Preference,
                MemoryType::Decision,
                MemoryType::Project,
                MemoryType::Fact,
                MemoryType::Feedback,
                MemoryType::Reference,
            ]),
            BTreeSet::from(MutationScopeKind::ALL),
            secret_content,
        )
    }
}

impl MutationPolicy for ConfiguredMutationPolicy {
    fn evaluate(
        &self,
        context: &MutationPolicyContext,
        request: MutationPolicyRequest<'_>,
    ) -> Result<(), MutationPolicyError> {
        if request.operation() != context.operation()
            || !self
                .allowed_scopes
                .contains(&MutationScopeKind::from_scope(context.scope()))
        {
            return Err(MutationPolicyError::Forbidden);
        }
        match &request {
            MutationPolicyRequest::Remember { target, .. }
            | MutationPolicyRequest::Update { target, .. } => {
                if target.scope != *context.scope()
                    || !self.allowed_types.contains(&target.memory_type)
                {
                    return Err(MutationPolicyError::Forbidden);
                }
                if target.body.len() > self.max_body_bytes {
                    return Err(MutationPolicyError::InvalidRequest);
                }
            }
            MutationPolicyRequest::Forget(_) => {}
        }
        let canonical = request.canonical_bytes()?;
        if self.secret_content.contains_secret(context, &canonical) {
            return Err(MutationPolicyError::Forbidden);
        }
        Ok(())
    }
}
