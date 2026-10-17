//! Native, read-only Project recall and host-pinned content references.
//!
//! A matching version proves content consistency, never truth, confirmation, or
//! execution authority. Hosts own authorization, source adoption, prompt budgets,
//! task state, and recovery. This module adds no persisted format or MCP action.

use std::{collections::HashSet, io};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    DEFAULT_QUERY_LIMIT, DurableMemoryDocument, DurableMemoryStatus, MAX_MAX_CHARS,
    MemoryQueryItem, MemoryQueryOptions, MemoryScope, MemoryStore, validate_memory_id,
};
use crate::ProjectId;

const VERSION_PREFIX: &str = "jiandu-content-v1:sha256:";
const QUERY_MAX_CHARS: usize = 1_600;

/// An opaque reference the host may pin in its existing authoritative context.
///
/// Deserialization validates shape only. Receiving this value never grants
/// Project access. A host must authorize the Project separately before reads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProjectMemoryRef {
    project_id: ProjectId,
    memory_id: String,
    content_version: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireProjectMemoryRef {
    project_id: ProjectId,
    memory_id: String,
    content_version: String,
}

impl<'de> Deserialize<'de> for ProjectMemoryRef {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = WireProjectMemoryRef::deserialize(deserializer)?;
        Self::new(wire.project_id, wire.memory_id, wire.content_version)
            .map_err(serde::de::Error::custom)
    }
}

impl ProjectMemoryRef {
    pub fn new(
        project_id: ProjectId,
        memory_id: impl Into<String>,
        content_version: impl Into<String>,
    ) -> io::Result<Self> {
        let memory_id = memory_id.into();
        if validate_memory_id(&memory_id)? != memory_id {
            return Err(invalid("memory reference id must be canonical"));
        }
        let content_version = content_version.into();
        let valid = content_version
            .strip_prefix(VERSION_PREFIX)
            .is_some_and(|digest| {
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            });
        if !valid {
            return Err(invalid(
                "memory reference requires a v1 lowercase SHA-256 content version",
            ));
        }
        Ok(Self {
            project_id,
            memory_id,
            content_version,
        })
    }

    /// Capture a host-selected Project document for an explicit source-adoption
    /// step. This does not adopt the reference or confirm any claim in its body.
    ///
    /// v1 hashes a domain-separated JSON tuple of the complete typed frontmatter
    /// and body. Filesystem location is excluded; source/status/metadata changes
    /// invalidate the pin. Hosts must not treat a timestamp or Session ID alone
    /// as this version, or reuse this version as an event/ingestion watermark.
    pub fn from_document(document: &DurableMemoryDocument) -> io::Result<Self> {
        if document.frontmatter.scope != MemoryScope::Project {
            return Err(invalid("memory reference requires a Project document"));
        }
        let project_id = document
            .frontmatter
            .project_key
            .as_deref()
            .ok_or_else(|| invalid("Project document is missing its Project identity"))?;
        let project_id = ProjectId::parse(project_id)
            .map_err(|_| invalid("Project document has an invalid Project identity"))?;
        let bytes = serde_json::to_vec(&(&document.frontmatter, &document.body))
            .map_err(io::Error::other)?;
        let mut digest = Sha256::new();
        digest.update(b"jiandu-project-memory-content-v1\0");
        digest.update(bytes);
        Self::new(
            project_id,
            document.frontmatter.id.clone(),
            format!("{VERSION_PREFIX}{:x}", digest.finalize()),
        )
    }

    pub fn project_id(&self) -> &ProjectId {
        &self.project_id
    }

    pub fn memory_id(&self) -> &str {
        &self.memory_id
    }

    pub fn content_version(&self) -> &str {
        &self.content_version
    }

    /// A collision-free source-map key for this Project and memory identity.
    /// The key is a locator, not a path to open or an authorization token.
    pub fn source_key(&self) -> String {
        format!(
            "jiandu:project:{}:memory:{}",
            self.project_id, self.memory_id
        )
    }
}

/// Compact discovery only. No pagination cursor or event watermark is returned.
/// A result's summary is not pinned evidence; selected records need exact get.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectMemoryCandidates {
    pub items: Vec<MemoryQueryItem>,
    pub more_available: bool,
}

/// Resolution at one read, not a lease or permission to execute/accept/replay.
/// Missing, changed, inactive, and over-budget content never returns a body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ProjectMemoryRead {
    VersionMatched {
        reference: ProjectMemoryRef,
        title: String,
        content: String,
        /// Always unknown: a content pin is not a verified claim or a legacy rating.
        confidence: Option<String>,
    },
    VersionMismatch {
        observed: ProjectMemoryRef,
    },
    Missing,
    Inactive {
        status: DurableMemoryStatus,
    },
    OverBudget {
        required_chars: usize,
        max_chars: usize,
    },
}

impl MemoryStore {
    /// Recall at most three Active records from exactly the host's Project.
    /// No Global fallback, Session discovery, hydration, access-log writes, or
    /// maintenance occurs. Empty queries are rejected instead of listing history.
    pub async fn query_project_sources_read_only(
        &self,
        project_id: &ProjectId,
        query: &str,
    ) -> io::Result<ProjectMemoryCandidates> {
        if query.trim().is_empty() {
            return Err(invalid(
                "Project source recall requires a non-empty lexical query",
            ));
        }
        let statuses = HashSet::from([DurableMemoryStatus::Active]);
        let result = self
            .query_scope_read_only(
                MemoryScope::Project,
                Some(project_id.as_str()),
                Some(query),
                None,
                Some(&statuses),
                None,
                &MemoryQueryOptions {
                    limit: Some(DEFAULT_QUERY_LIMIT),
                    max_chars: Some(QUERY_MAX_CHARS),
                    cursor: None,
                    include_related: false,
                },
            )
            .await?;
        if result.items.iter().any(|item| {
            item.scope != MemoryScope::Project
                || item.project_key.as_deref() != Some(project_id.as_str())
        }) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Project lexical index contains inconsistent scope metadata",
            ));
        }
        Ok(ProjectMemoryCandidates {
            items: result.items,
            more_available: result.remaining_count > 0,
        })
    }

    /// Resolve only this exact Project locator against the host-adopted version.
    /// Unlike general get, this cannot fall back to a same-ID Global record.
    /// Hosts must recheck their canonical context/permission at actual execution;
    /// this read cannot prevent a later source update or authorize recovery.
    pub async fn get_project_source_read_only(
        &self,
        expected: &ProjectMemoryRef,
    ) -> io::Result<ProjectMemoryRead> {
        let Some(document) = self
            .get_memory_at_locator(
                MemoryScope::Project,
                Some(expected.project_id.as_str()),
                &expected.memory_id,
            )
            .await?
        else {
            return Ok(ProjectMemoryRead::Missing);
        };
        if document.frontmatter.status != DurableMemoryStatus::Active {
            return Ok(ProjectMemoryRead::Inactive {
                status: document.frontmatter.status,
            });
        }
        let observed = ProjectMemoryRef::from_document(&document)?;
        if &observed != expected {
            return Ok(ProjectMemoryRead::VersionMismatch { observed });
        }
        let required_chars = document.body.chars().count();
        if required_chars > MAX_MAX_CHARS {
            return Ok(ProjectMemoryRead::OverBudget {
                required_chars,
                max_chars: MAX_MAX_CHARS,
            });
        }
        Ok(ProjectMemoryRead::VersionMatched {
            reference: observed,
            title: document.frontmatter.title,
            content: document.body,
            confidence: None,
        })
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
