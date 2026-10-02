use std::fmt;

use jiandu_memory::{ProjectId, memory_store::validate_session_id};
use rmcp::model::RequestMetaObject;
use serde::Deserialize;

/// Host-supplied per-call identity, outside model-generated tool arguments.
pub const MEMORY_CONTEXT_META_KEY: &str = "io.github.bigduu.jiandu/context";

/// Host-owned identity for one memory invocation or optional server defaults.
///
/// Identity stays outside the unified tool arguments: all five `session_*`
/// actions require `session_id`, while durable project actions require a
/// validated opaque `ProjectId`. Global actions need neither identity.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemoryExecutionContext {
    session_id: Option<String>,
    project_id: Option<ProjectId>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostCallContext {
    session_id: Option<String>,
    project_id: Option<String>,
}

impl MemoryExecutionContext {
    /// Construct a fixed Session context for existing dedicated-process hosts.
    pub fn new(session_id: impl Into<String>) -> Result<Self, MemoryError> {
        Self::default().with_session_id(session_id)
    }

    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Result<Self, MemoryError> {
        let session_id = session_id.into();
        let session_id = validate_session_id(&session_id)
            .map_err(|error| MemoryError::InvalidArguments(error.to_string()))?;
        self.session_id = Some(session_id.to_string());
        Ok(self)
    }

    pub fn with_project_id(mut self, project_id: impl Into<String>) -> Result<Self, MemoryError> {
        self.project_id = Some(ProjectId::parse(project_id).map_err(|error| {
            MemoryError::InvalidArguments(format!(
                "project_id must be a 1-64 character opaque identifier containing only ASCII alphanumeric, '-' or '_': {error}"
            ))
        })?);
        Ok(self)
    }

    #[must_use]
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    #[must_use]
    pub fn project_id(&self) -> Option<&ProjectId> {
        self.project_id.as_ref()
    }

    pub(crate) fn require_session_id(&self) -> Result<&str, MemoryError> {
        self.session_id().ok_or_else(|| {
            MemoryError::InvalidArguments(
                "session_* actions require a session_id in the host execution context; supply per-call Jiandu metadata or the optional --session-id default".to_string(),
            )
        })
    }

    /// An explicit metadata context replaces all defaults for this call.
    /// Never retain a request's authority or merge omitted fields from defaults.
    pub(crate) fn resolve_request(
        &self,
        metadata: &RequestMetaObject,
    ) -> Result<Self, MemoryError> {
        let Some(value) = metadata.get(MEMORY_CONTEXT_META_KEY) else {
            return Ok(self.clone());
        };
        if !value.is_object() {
            return Err(MemoryError::InvalidArguments(format!(
                "host metadata {MEMORY_CONTEXT_META_KEY} must be an object"
            )));
        }
        let context: HostCallContext = serde_json::from_value(value.clone()).map_err(|error| {
            MemoryError::InvalidArguments(format!(
                "invalid host metadata {MEMORY_CONTEXT_META_KEY}: {error}"
            ))
        })?;
        let mut resolved = Self::default();
        if let Some(session_id) = context.session_id {
            resolved = resolved.with_session_id(session_id)?;
        }
        if let Some(project_id) = context.project_id {
            resolved = resolved.with_project_id(project_id)?;
        }
        Ok(resolved)
    }

    pub(crate) fn resolve_project_id(
        &self,
        requested: Option<&str>,
    ) -> Result<Option<ProjectId>, MemoryError> {
        let requested = requested
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| {
                ProjectId::parse(value.to_string()).map_err(|error| {
                    MemoryError::InvalidArguments(format!(
                        "project_key must be a valid opaque Project id: {error}"
                    ))
                })
            })
            .transpose()?;

        match (&self.project_id, requested) {
            (Some(context), Some(requested)) if context != &requested => {
                Err(MemoryError::InvalidArguments(
                    "project_key cannot override the current host execution context's project_id"
                        .to_string(),
                ))
            }
            (Some(context), _) => Ok(Some(context.clone())),
            (None, Some(_)) => Err(MemoryError::InvalidArguments(
                "project_key cannot grant Project access without a project_id in the host execution context"
                    .to_string(),
            )),
            (None, None) => Ok(None),
        }
    }
}

/// Caller-visible unified tool failure.
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
