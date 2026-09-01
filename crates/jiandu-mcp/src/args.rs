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
    /// Recall durable memory. A non-empty short keyword/entity query uses the
    /// derived lexical index and returns compact ranked hits; omit or leave it
    /// empty only for explicit management/filter listing.
    Query {
        #[schemars(with = "DurableScopeSchema")]
        scope: String,
        #[serde(default)]
        #[schemars(
            length(max = 512),
            description = "Short discriminative keywords, aliases, identifiers, or entities. Non-empty values use index-backed BM25/CJK recall; omitted/blank values preserve management listing semantics."
        )]
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
    /// Fetch the selected memory after `query` returned its id. This is the
    /// explicit full-body path and includes bounded retrieval keywords/entities/
    /// tags plus body and retrieval-metadata truncation signals.
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
    /// Write one confirmed, durable, non-secret atomic fact. Query first, then
    /// provide concise model-selected retrieval hints; Jiandu deterministically
    /// normalizes, bounds, deduplicates, and expands omitted metadata.
    Write {
        #[schemars(with = "DurableScopeSchema")]
        scope: String,
        #[serde(rename = "type")]
        #[schemars(with = "MemoryTypeSchema")]
        r#type: String,
        #[schemars(description = "Concise searchable title for exactly one durable fact.")]
        title: String,
        #[schemars(
            description = "Complete atomic fact body; this is returned only by get, not compact query hits."
        )]
        content: String,
        #[serde(default)]
        #[schemars(
            length(max = 32),
            inner(length(min = 1, max = 64)),
            description = "Optional CJK-safe categorical labels; at most 32, normalized and deduplicated."
        )]
        tags: Vec<String>,
        #[serde(default)]
        #[schemars(
            length(max = 32),
            inner(length(min = 1, max = 96)),
            description = "Up to 32 discriminative model-provided keywords or aliases, prioritized before deterministic fallback expansion."
        )]
        keywords: Vec<String>,
        #[serde(default)]
        #[schemars(
            length(max = 16),
            inner(length(min = 1, max = 96)),
            description = "Up to 16 named identifiers or entities, including CJK and mixed-language values."
        )]
        entities: Vec<String>,
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
        #[schemars(length(max = 32), inner(length(min = 1, max = 64)))]
        tags: Vec<String>,
        #[serde(default)]
        #[schemars(length(max = 32), inner(length(min = 1, max = 96)))]
        keywords: Vec<String>,
        #[serde(default)]
        #[schemars(length(max = 16), inner(length(min = 1, max = 96)))]
        entities: Vec<String>,
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
        #[schemars(length(max = 32), inner(length(min = 1, max = 64)))]
        tags: Vec<String>,
        #[serde(default)]
        #[schemars(length(max = 32), inner(length(min = 1, max = 96)))]
        keywords: Vec<String>,
        #[serde(default)]
        #[schemars(length(max = 16), inner(length(min = 1, max = 96)))]
        entities: Vec<String>,
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
        #[schemars(length(max = 32), inner(length(min = 1, max = 64)))]
        tags: Vec<String>,
        #[serde(default)]
        #[schemars(length(max = 32), inner(length(min = 1, max = 96)))]
        keywords: Vec<String>,
        #[serde(default)]
        #[schemars(length(max = 16), inner(length(min = 1, max = 96)))]
        entities: Vec<String>,
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
    #[schemars(
        range(min = 1, max = 20),
        description = "Maximum returned items. Query defaults to compact top 3 and never exceeds 20."
    )]
    pub limit: Option<usize>,
    #[serde(default)]
    #[schemars(
        range(min = 1, max = 6000),
        description = "Character budget for returned text fields."
    )]
    pub max_chars: Option<usize>,
    #[serde(default)]
    #[schemars(description = "Opaque cursor returned by a preceding query page.")]
    pub cursor: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "When true, query hydrates only the returned K records to include relation ids; false keeps recall index-only."
    )]
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
    #[schemars(length(max = 32), inner(length(min = 1, max = 64)))]
    pub tags: Vec<String>,
    #[serde(default)]
    #[schemars(length(max = 32), inner(length(min = 1, max = 96)))]
    pub keywords: Vec<String>,
    #[serde(default)]
    #[schemars(length(max = 16), inner(length(min = 1, max = 96)))]
    pub entities: Vec<String>,
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
