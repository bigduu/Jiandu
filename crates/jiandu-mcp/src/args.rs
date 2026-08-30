use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The exact action names accepted by Bamboo's unified `memory` tool.
pub const MEMORY_ACTIONS: [&str; 17] = [
    "session_read",
    "session_append",
    "session_replace",
    "session_clear",
    "session_list_topics",
    "query",
    "get",
    "find_duplicates",
    "write",
    "merge",
    "split",
    "consolidate",
    "purge",
    "inspect",
    "rebuild",
    "scan_blobs",
    "scan_duplicates",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryToolClass {
    ReadOnlyParallel,
    MutatingSerial,
}

/// Deserialization contract for the unified `memory` tool.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum MemoryArgs {
    SessionRead {
        #[serde(default)]
        topic: Option<String>,
        #[serde(default)]
        options: Option<MemoryActionOptions>,
    },
    SessionAppend {
        #[serde(default)]
        topic: Option<String>,
        content: String,
    },
    SessionReplace {
        #[serde(default)]
        topic: Option<String>,
        content: String,
    },
    SessionClear {
        #[serde(default)]
        topic: Option<String>,
    },
    SessionListTopics,
    Query {
        scope: String,
        #[serde(default)]
        query: Option<String>,
        #[serde(default)]
        filters: Option<QueryFilters>,
        #[serde(default)]
        project_key: Option<String>,
        #[serde(default)]
        options: Option<MemoryActionOptions>,
    },
    Get {
        id: String,
        #[serde(default)]
        project_key: Option<String>,
        #[serde(default)]
        options: Option<MemoryActionOptions>,
    },
    Write {
        scope: String,
        #[serde(rename = "type")]
        r#type: String,
        title: String,
        content: String,
        #[serde(default)]
        tags: Vec<String>,
        #[serde(default)]
        project_key: Option<String>,
        #[serde(default)]
        granularity: Option<String>,
        #[serde(default)]
        options: Option<WriteOptions>,
    },
    Merge {
        id: String,
        content: String,
        #[serde(default)]
        tags: Vec<String>,
        #[serde(default)]
        project_key: Option<String>,
        #[serde(default)]
        source_memory_ids: Vec<String>,
        #[serde(default)]
        mode: Option<String>,
        #[serde(default)]
        reason: Option<String>,
    },
    Split {
        id: String,
        #[serde(default)]
        project_key: Option<String>,
        pieces: Vec<SplitPiece>,
    },
    FindDuplicates {
        scope: String,
        title: String,
        #[serde(default)]
        content: Option<String>,
        #[serde(rename = "type", default)]
        r#type: Option<String>,
        #[serde(default)]
        tags: Vec<String>,
        #[serde(default)]
        project_key: Option<String>,
        #[serde(default)]
        options: Option<MemoryActionOptions>,
    },
    ScanBlobs {
        scope: String,
        #[serde(default)]
        project_key: Option<String>,
        #[serde(default)]
        min_sections: Option<usize>,
        #[serde(default)]
        options: Option<MemoryActionOptions>,
    },
    ScanDuplicates {
        scope: String,
        #[serde(default)]
        project_key: Option<String>,
        #[serde(default)]
        min_score: Option<f64>,
        #[serde(default)]
        options: Option<MemoryActionOptions>,
    },
    Consolidate {
        ids: Vec<String>,
        title: String,
        content: String,
        #[serde(rename = "type", default)]
        r#type: Option<String>,
        #[serde(default)]
        tags: Vec<String>,
        #[serde(default)]
        project_key: Option<String>,
    },
    Purge {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        reason: Option<String>,
        #[serde(default)]
        project_key: Option<String>,
        #[serde(default)]
        filters: Option<QueryFilters>,
        #[serde(default)]
        mode: Option<String>,
    },
    Inspect {
        scope: String,
        #[serde(default)]
        project_key: Option<String>,
    },
    Rebuild {
        scope: String,
        #[serde(default)]
        project_key: Option<String>,
    },
}

impl MemoryArgs {
    #[must_use]
    pub const fn action_name(&self) -> &'static str {
        match self {
            Self::SessionRead { .. } => "session_read",
            Self::SessionAppend { .. } => "session_append",
            Self::SessionReplace { .. } => "session_replace",
            Self::SessionClear { .. } => "session_clear",
            Self::SessionListTopics => "session_list_topics",
            Self::Query { .. } => "query",
            Self::Get { .. } => "get",
            Self::Write { .. } => "write",
            Self::Merge { .. } => "merge",
            Self::Split { .. } => "split",
            Self::FindDuplicates { .. } => "find_duplicates",
            Self::ScanBlobs { .. } => "scan_blobs",
            Self::ScanDuplicates { .. } => "scan_duplicates",
            Self::Consolidate { .. } => "consolidate",
            Self::Purge { .. } => "purge",
            Self::Inspect { .. } => "inspect",
            Self::Rebuild { .. } => "rebuild",
        }
    }

    #[must_use]
    pub const fn class(&self) -> MemoryToolClass {
        match self {
            Self::SessionRead { .. }
            | Self::SessionListTopics
            | Self::Query { .. }
            | Self::Get { .. }
            | Self::FindDuplicates { .. }
            | Self::ScanBlobs { .. }
            | Self::ScanDuplicates { .. }
            | Self::Inspect { .. } => MemoryToolClass::ReadOnlyParallel,
            _ => MemoryToolClass::MutatingSerial,
        }
    }

    pub(crate) fn project_key(&self) -> Option<&str> {
        match self {
            Self::Query { project_key, .. }
            | Self::Get { project_key, .. }
            | Self::Write { project_key, .. }
            | Self::Merge { project_key, .. }
            | Self::Split { project_key, .. }
            | Self::FindDuplicates { project_key, .. }
            | Self::ScanBlobs { project_key, .. }
            | Self::ScanDuplicates { project_key, .. }
            | Self::Consolidate { project_key, .. }
            | Self::Purge { project_key, .. }
            | Self::Inspect { project_key, .. }
            | Self::Rebuild { project_key, .. } => project_key.as_deref(),
            Self::SessionRead { .. }
            | Self::SessionAppend { .. }
            | Self::SessionReplace { .. }
            | Self::SessionClear { .. }
            | Self::SessionListTopics => None,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct MemoryActionOptions {
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub max_chars: Option<usize>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub include_related: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct QueryFilters {
    #[serde(default)]
    pub r#type: Vec<String>,
    #[serde(default)]
    pub status: Vec<String>,
    #[serde(default)]
    pub granularity: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct WriteOptions {
    #[serde(default)]
    pub allow_merge_if_similar: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct SplitPiece {
    pub title: String,
    #[serde(rename = "type", default)]
    pub r#type: Option<String>,
    pub content: String,
    #[serde(default)]
    pub tags: Vec<String>,
}
