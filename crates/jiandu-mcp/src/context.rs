use std::fmt;

use jiandu_memory::{ProjectId, memory_store::validate_session_id};

/// Host-owned identity for one MCP server process.
///
/// Identity stays outside the unified tool arguments: all five `session_*`
/// actions use `session_id`, while durable project actions use the validated
/// opaque `ProjectId` when one is present.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryExecutionContext {
    session_id: String,
    project_id: Option<ProjectId>,
}

impl MemoryExecutionContext {
    pub fn new(session_id: impl Into<String>) -> Result<Self, MemoryError> {
        let session_id = session_id.into();
        let session_id = validate_session_id(&session_id)
            .map_err(|error| MemoryError::InvalidArguments(error.to_string()))?;
        Ok(Self {
            session_id: session_id.to_string(),
            project_id: None,
        })
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
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    #[must_use]
    pub fn project_id(&self) -> Option<&ProjectId> {
        self.project_id.as_ref()
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
                    "project_key cannot override the MCP execution context's project_id"
                        .to_string(),
                ))
            }
            (Some(context), _) => Ok(Some(context.clone())),
            (None, Some(_)) => Err(MemoryError::InvalidArguments(
                "project_key cannot grant Project access without a project_id in the MCP execution context"
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
