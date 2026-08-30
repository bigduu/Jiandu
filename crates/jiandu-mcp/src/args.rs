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
        #[schemars(with = "DurableScopeSchema")]
        scope: String,
        #[serde(default)]
        query: Option<String>,
        #[serde(default)]
        filters: Option<QueryFilters>,
        #[serde(default)]
        #[schemars(
            length(max = 64),
            regex(pattern = "^[A-Za-z0-9_-]+$"),
            description = "Must match the host execution context Project id; it cannot grant Project access."
        )]
        project_key: Option<String>,
        #[serde(default)]
        options: Option<MemoryActionOptions>,
    },
    Get {
        #[schemars(length(min = 1, max = 128), regex(pattern = "^[A-Za-z0-9_-]+$"))]
        id: String,
        #[serde(default)]
        #[schemars(
            length(max = 64),
            regex(pattern = "^[A-Za-z0-9_-]+$"),
            description = "Must match the host execution context Project id; it cannot grant Project access."
        )]
        project_key: Option<String>,
        #[serde(default)]
        options: Option<MemoryActionOptions>,
    },
    Write {
        #[schemars(with = "DurableScopeSchema")]
        scope: String,
        #[serde(rename = "type")]
        #[schemars(with = "MemoryTypeSchema")]
        r#type: String,
        title: String,
        content: String,
        #[serde(default)]
        tags: Vec<String>,
        #[serde(default)]
        #[schemars(
            length(max = 64),
            regex(pattern = "^[A-Za-z0-9_-]+$"),
            description = "Must match the host execution context Project id; it cannot grant Project access."
        )]
        project_key: Option<String>,
        #[serde(default)]
        #[schemars(with = "Option<GranularitySchema>")]
        granularity: Option<String>,
        #[serde(default)]
        options: Option<WriteOptions>,
    },
    Merge {
        #[schemars(length(min = 1, max = 128), regex(pattern = "^[A-Za-z0-9_-]+$"))]
        id: String,
        content: String,
        #[serde(default)]
        tags: Vec<String>,
        #[serde(default)]
        #[schemars(
            length(max = 64),
            regex(pattern = "^[A-Za-z0-9_-]+$"),
            description = "Must match the host execution context Project id; it cannot grant Project access."
        )]
        project_key: Option<String>,
        #[serde(default)]
        #[schemars(inner(length(min = 1, max = 128), regex(pattern = "^[A-Za-z0-9_-]+$")))]
        source_memory_ids: Vec<String>,
        #[serde(default)]
        #[schemars(with = "Option<MergeModeSchema>")]
        mode: Option<String>,
        #[serde(default)]
        reason: Option<String>,
    },
    Split {
        #[schemars(length(min = 1, max = 128), regex(pattern = "^[A-Za-z0-9_-]+$"))]
        id: String,
        #[serde(default)]
        #[schemars(
            length(max = 64),
            regex(pattern = "^[A-Za-z0-9_-]+$"),
            description = "Must match the host execution context Project id; it cannot grant Project access."
        )]
        project_key: Option<String>,
        #[schemars(length(min = 1))]
        pieces: Vec<SplitPiece>,
    },
    FindDuplicates {
        #[schemars(with = "DurableScopeSchema")]
        scope: String,
        title: String,
        #[serde(default)]
        content: Option<String>,
        #[serde(rename = "type", default)]
        #[schemars(with = "Option<MemoryTypeSchema>")]
        r#type: Option<String>,
        #[serde(default)]
        tags: Vec<String>,
        #[serde(default)]
        #[schemars(
            length(max = 64),
            regex(pattern = "^[A-Za-z0-9_-]+$"),
            description = "Must match the host execution context Project id; it cannot grant Project access."
        )]
        project_key: Option<String>,
        #[serde(default)]
        options: Option<MemoryActionOptions>,
    },
    ScanBlobs {
        #[schemars(with = "DurableScopeSchema")]
        scope: String,
        #[serde(default)]
        #[schemars(
            length(max = 64),
            regex(pattern = "^[A-Za-z0-9_-]+$"),
            description = "Must match the host execution context Project id; it cannot grant Project access."
        )]
        project_key: Option<String>,
        #[serde(default)]
        min_sections: Option<usize>,
        #[serde(default)]
        options: Option<MemoryActionOptions>,
    },
    ScanDuplicates {
        #[schemars(with = "DurableScopeSchema")]
        scope: String,
        #[serde(default)]
        #[schemars(
            length(max = 64),
            regex(pattern = "^[A-Za-z0-9_-]+$"),
            description = "Must match the host execution context Project id; it cannot grant Project access."
        )]
        project_key: Option<String>,
        #[serde(default)]
        min_score: Option<f64>,
        #[serde(default)]
        options: Option<MemoryActionOptions>,
    },
    Consolidate {
        #[schemars(
            length(min = 2),
            inner(length(min = 1, max = 128), regex(pattern = "^[A-Za-z0-9_-]+$"))
        )]
        ids: Vec<String>,
        title: String,
        content: String,
        #[serde(rename = "type", default)]
        #[schemars(with = "Option<MemoryTypeSchema>")]
        r#type: Option<String>,
        #[serde(default)]
        tags: Vec<String>,
        #[serde(default)]
        #[schemars(
            length(max = 64),
            regex(pattern = "^[A-Za-z0-9_-]+$"),
            description = "Must match the host execution context Project id; it cannot grant Project access."
        )]
        project_key: Option<String>,
    },
    Purge {
        #[serde(default)]
        #[schemars(length(min = 1, max = 128), regex(pattern = "^[A-Za-z0-9_-]+$"))]
        id: Option<String>,
        #[serde(default)]
        #[schemars(with = "Option<DurableScopeSchema>")]
        scope: Option<String>,
        #[serde(default)]
        reason: Option<String>,
        #[serde(default)]
        #[schemars(
            length(max = 64),
            regex(pattern = "^[A-Za-z0-9_-]+$"),
            description = "Must match the host execution context Project id; it cannot grant Project access."
        )]
        project_key: Option<String>,
        #[serde(default)]
        filters: Option<QueryFilters>,
        #[serde(default)]
        #[schemars(with = "Option<StatusSchema>")]
        mode: Option<String>,
    },
    Inspect {
        #[schemars(with = "DurableScopeSchema")]
        scope: String,
        #[serde(default)]
        #[schemars(
            length(max = 64),
            regex(pattern = "^[A-Za-z0-9_-]+$"),
            description = "Must match the host execution context Project id; it cannot grant Project access."
        )]
        project_key: Option<String>,
    },
    Rebuild {
        #[schemars(with = "DurableScopeSchema")]
        scope: String,
        #[serde(default)]
        #[schemars(
            length(max = 64),
            regex(pattern = "^[A-Za-z0-9_-]+$"),
            description = "Must match the host execution context Project id; it cannot grant Project access."
        )]
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
    #[schemars(with = "Vec<MemoryTypeSchema>")]
    pub r#type: Vec<String>,
    #[serde(default)]
    #[schemars(with = "Vec<StatusSchema>")]
    pub status: Vec<String>,
    #[serde(default)]
    #[schemars(with = "Vec<GranularitySchema>")]
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
    #[schemars(with = "Option<MemoryTypeSchema>")]
    pub r#type: Option<String>,
    pub content: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[allow(dead_code)]
#[derive(JsonSchema)]
#[schemars(inline, rename_all = "snake_case")]
enum DurableScopeSchema {
    Project,
    Global,
}

#[allow(dead_code)]
#[derive(JsonSchema)]
#[schemars(inline, rename_all = "snake_case")]
enum MemoryTypeSchema {
    User,
    Feedback,
    Project,
    Reference,
}

#[allow(dead_code)]
#[derive(JsonSchema)]
#[schemars(inline, rename_all = "snake_case")]
enum GranularitySchema {
    Day,
    Week,
    Month,
    Quarter,
    Year,
}

#[allow(dead_code)]
#[derive(JsonSchema)]
#[schemars(inline, rename_all = "snake_case")]
enum StatusSchema {
    Active,
    Stale,
    Superseded,
    Contradicted,
    Archived,
}

#[allow(dead_code)]
#[derive(JsonSchema)]
#[schemars(inline, rename_all = "snake_case")]
enum MergeModeSchema {
    Merge,
    SemanticMerge,
    Contradict,
}
