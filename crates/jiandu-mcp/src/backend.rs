use std::fmt;

use async_trait::async_trait;
use serde_json::Value;

use crate::MemoryArgs;

/// Opaque, path-safe, stable Project identity.
///
/// This mirrors Bamboo's `ProjectId` boundary without importing a Project store
/// or accepting a path-derived compatibility identity.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProjectId(String);

impl ProjectId {
    pub const MAX_LEN: usize = 64;

    pub fn parse(value: impl Into<String>) -> Result<Self, MemoryError> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= Self::MAX_LEN
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
        if valid {
            Ok(Self(value))
        } else {
            Err(MemoryError::InvalidArguments(
                "project_id must be a 1-64 character opaque identifier containing only ASCII alphanumeric, '-' or '_'"
                    .to_string(),
            ))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Host-owned execution context. It stays outside [`MemoryArgs`], so MCP
/// requests retain Bamboo's exact 17-action shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryExecutionContext {
    session_id: String,
    project_id: Option<ProjectId>,
}

impl MemoryExecutionContext {
    pub fn new(session_id: impl Into<String>) -> Result<Self, MemoryError> {
        let session_id = session_id.into();
        let normalized = session_id.trim();
        let valid = !normalized.is_empty()
            && !normalized.contains("..")
            && normalized
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.');
        if !valid {
            return Err(MemoryError::InvalidArguments(
                "session_id must be a non-empty opaque identifier containing only ASCII alphanumeric, '-', '_' or '.'"
                    .to_string(),
            ));
        }
        Ok(Self {
            session_id: normalized.to_string(),
            project_id: None,
        })
    }

    pub fn with_project_id(mut self, project_id: impl Into<String>) -> Result<Self, MemoryError> {
        self.project_id = Some(ProjectId::parse(project_id)?);
        Ok(self)
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    #[must_use]
    pub fn project_id(&self) -> Option<&ProjectId> {
        self.project_id.as_ref()
    }

    fn resolve_project_id(&self, arguments: &MemoryArgs) -> Result<Option<ProjectId>, MemoryError> {
        let requested = arguments
            .project_key()
            .map(|value| ProjectId::parse(value.to_string()))
            .transpose()?;
        match (&self.project_id, requested) {
            (Some(context), Some(requested)) if context != &requested => {
                Err(MemoryError::InvalidArguments(
                    "project_key cannot override the MCP execution context's project_id"
                        .to_string(),
                ))
            }
            (Some(context), _) => Ok(Some(context.clone())),
            (None, requested) => Ok(requested),
        }
    }
}

/// Fully parsed invocation delivered to the memory implementation.
#[derive(Clone, Debug, PartialEq)]
pub struct MemoryInvocation {
    pub session_id: String,
    pub project_id: Option<ProjectId>,
    pub arguments: MemoryArgs,
}

/// The only backend seam owned by the MCP crate.
#[async_trait]
pub trait MemoryBackend: Send + Sync + 'static {
    async fn execute(&self, invocation: MemoryInvocation) -> Result<Value, MemoryError>;
}

/// Caller-visible tool failure. Storage and protocol error taxonomies remain
/// outside this transport adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemoryError {
    InvalidArguments(String),
    Execution(String),
}

impl fmt::Display for MemoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidArguments(message) => write!(formatter, "Invalid memory args: {message}"),
            Self::Execution(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for MemoryError {}

impl MemoryInvocation {
    pub(crate) fn from_context(
        context: &MemoryExecutionContext,
        arguments: MemoryArgs,
    ) -> Result<Self, MemoryError> {
        let project_id = context.resolve_project_id(&arguments)?;
        Ok(Self {
            session_id: context.session_id.clone(),
            project_id,
            arguments,
        })
    }
}
