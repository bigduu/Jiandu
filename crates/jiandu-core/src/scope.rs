//! Public scope selectors and resolved record scopes.

use crate::ids::{PrincipalId, ProjectId, SessionId};
use crate::validation::{Validate, ValidationErrors};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Closed v1alpha1 scope kinds.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    Principal,
    Project,
    Session,
    InstanceGlobal,
}

/// Model-visible scope selector.
///
/// Principal identity is intentionally absent: the authenticated principal is
/// always taken from [`crate::TrustedRequestContext`]. Project and Session IDs
/// are opaque host-resolved identities, never paths.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ScopeSelector {
    Principal {},
    Project {
        #[serde(rename = "projectId")]
        project_id: ProjectId,
    },
    Session {
        #[serde(rename = "sessionId")]
        session_id: SessionId,
    },
    InstanceGlobal {},
}

impl ScopeSelector {
    #[must_use]
    pub const fn kind(&self) -> ScopeKind {
        match self {
            Self::Principal {} => ScopeKind::Principal,
            Self::Project { .. } => ScopeKind::Project,
            Self::Session { .. } => ScopeKind::Session,
            Self::InstanceGlobal {} => ScopeKind::InstanceGlobal,
        }
    }
}

impl Validate for ScopeSelector {
    fn validate(&self) -> Result<(), ValidationErrors> {
        Ok(())
    }
}

/// Authoritative scope stamped onto a resolved memory record.
///
/// Unlike a request selector, a Principal record carries the trusted owner ID
/// selected by policy. Project and Session membership remain authorization
/// data outside this value.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MemoryScope {
    Principal {
        #[serde(rename = "principalId")]
        principal_id: PrincipalId,
    },
    Project {
        #[serde(rename = "projectId")]
        project_id: ProjectId,
    },
    Session {
        #[serde(rename = "sessionId")]
        session_id: SessionId,
    },
    InstanceGlobal {},
}

impl MemoryScope {
    #[must_use]
    pub const fn kind(&self) -> ScopeKind {
        match self {
            Self::Principal { .. } => ScopeKind::Principal,
            Self::Project { .. } => ScopeKind::Project,
            Self::Session { .. } => ScopeKind::Session,
            Self::InstanceGlobal {} => ScopeKind::InstanceGlobal,
        }
    }
}

impl Validate for MemoryScope {
    fn validate(&self) -> Result<(), ValidationErrors> {
        Ok(())
    }
}
