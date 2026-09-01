use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

mod access_log;
pub mod freshness;
mod lexical_bm25;
pub mod paths;
pub mod recall;
pub mod store;
pub mod types;

pub use freshness::{
    FreshnessKind, memory_age_days, memory_age_label, memory_freshness_text,
    render_memory_freshness_note,
};
pub use paths::{MemoryPathResolver, ProjectMemoryPathResolver, SESSIONS_DIR, TOPICS_DIR};
pub use recall::{
    MemoryRecallCandidate, MemoryRecallOptions, MemoryRecallSelection, MemoryRecallStrategy,
    recall_candidates_from_lexical_index, select_relevant_memories, shortlist_relevant_memories,
};
pub use store::MemoryStore;
pub use types::{
    BlobScanItem, BlobScanReport, CreatedBy, DreamReadResult, DreamSnapshot, DuplicateCluster,
    DuplicateClusterMember, DuplicateScanReport, DurableContentLocation, DurableMemoryDocument,
    DurableMemoryFrontmatter, DurableMemoryRef, DurableMemoryRelations, DurableMemoryRetrieval,
    DurableMemorySource, DurableMemoryStatus, DurableMemoryType, MemoryConsolidateResult,
    MemoryContradictionResult, MemoryDuplicateCandidate, MemoryInspectResult, MemoryMergeResult,
    MemoryPurgeResult, MemoryQueryCursor, MemoryQueryItem, MemoryQueryOptions, MemoryQueryResult,
    MemoryRetrievalInput, MemoryScope, MemorySplitPiece, MemorySplitResult, SessionState,
    TemporalGranularity,
};

pub const MEMORY_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_SESSION_TOPIC: &str = "default";
pub const MAX_SESSION_TOPIC_LEN: usize = 50;
pub const MAX_MEMORY_ID_LEN: usize = 128;
pub const MAX_MEMORY_TITLE_LEN: usize = 160;
pub const MAX_MEMORY_TAGS: usize = 32;
/// Caller-supplied retrieval hints stay intentionally small; deterministic
/// fallback expansion retains the wider legacy coverage below.
pub const MAX_EXPLICIT_MEMORY_KEYWORDS: usize = 32;
pub const MAX_EXPLICIT_MEMORY_ENTITIES: usize = 16;
pub const MAX_MEMORY_KEYWORDS: usize = 128;
pub const MAX_MEMORY_ENTITIES: usize = 64;
pub const MAX_RETRIEVAL_TERM_CHARS: usize = 96;
pub const MAX_MEMORY_TAG_CHARS: usize = 64;
pub const MAX_MEMORY_QUERY_CHARS: usize = 512;
pub const DEFAULT_QUERY_LIMIT: usize = 3;
pub const MAX_QUERY_LIMIT: usize = 20;
pub const DEFAULT_MAX_CHARS: usize = 3_000;
pub const MAX_MAX_CHARS: usize = 6_000;
pub const WRITE_AUDIT_LOG: &str = "write_audit.jsonl";
pub const MERGE_AUDIT_LOG: &str = "merge_audit.jsonl";
pub const PURGE_AUDIT_LOG: &str = "purge_audit.jsonl";
pub const CONTRADICTION_AUDIT_LOG: &str = "contradiction_audit.jsonl";
pub const MEMORY_VIEW_FILE: &str = "MEMORY.md";
pub const RECENT_VIEW_FILE: &str = "RECENT.md";
pub const STALE_VIEW_FILE: &str = "STALE.md";
pub const DREAM_VIEW_FILE: &str = "DREAM.md";
pub const SCOPE_GENERATION_FILE: &str = "source_generation.json";
pub const MAX_DREAM_CONTENT_CHARS: usize = 12_000;
pub const LEXICAL_INDEX_FILE: &str = "lexical.json";
pub const GRAPH_INDEX_FILE: &str = "graph.json";
pub const RECENT_INDEX_FILE: &str = "recent.json";
pub const STALE_CANDIDATES_INDEX_FILE: &str = "stale_candidates.json";
pub const TAXONOMY_INDEX_FILE: &str = "taxonomy.json";

/// Validate an opaque durable-memory identifier before it is used for lookup or
/// as part of a filesystem path. Leading and trailing whitespace is accepted at
/// API boundaries and the returned slice is the canonical, trimmed identifier.
pub fn validate_memory_id(memory_id: &str) -> io::Result<&str> {
    let trimmed = memory_id.trim();
    if trimmed.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "memory id cannot be empty",
        ));
    }
    if trimmed.len() > MAX_MEMORY_ID_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "memory id too long (max {} bytes, got {})",
                MAX_MEMORY_ID_LEN,
                trimmed.len()
            ),
        ));
    }
    if !trimmed
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "memory id must contain only ASCII alphanumeric, dash, or underscore characters",
        ));
    }
    Ok(trimmed)
}

pub fn validate_session_id(session_id: &str) -> io::Result<&str> {
    let trimmed = session_id.trim();
    if trimmed.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session_id cannot be empty",
        ));
    }
    if matches!(trimmed, "." | "..")
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains("..")
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session_id contains invalid path characters",
        ));
    }
    if !trimmed
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session_id contains unsupported characters",
        ));
    }
    Ok(trimmed)
}

pub fn validate_session_topic(topic: &str) -> io::Result<&str> {
    let trimmed = topic.trim();
    if trimmed.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "topic cannot be empty",
        ));
    }
    if trimmed.len() > MAX_SESSION_TOPIC_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "topic name too long (max {} chars, got {})",
                MAX_SESSION_TOPIC_LEN,
                trimmed.len()
            ),
        ));
    }
    if trimmed.contains('/') || trimmed.contains('\\') || trimmed.contains("..") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "topic contains invalid path characters",
        ));
    }
    if !trimmed
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "topic must contain only alphanumeric, dash, or underscore characters",
        ));
    }
    Ok(trimmed)
}

pub fn validate_memory_title(title: &str) -> io::Result<&str> {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "title cannot be empty",
        ));
    }
    if trimmed.chars().count() > MAX_MEMORY_TITLE_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("title too long (max {} chars)", MAX_MEMORY_TITLE_LEN),
        ));
    }
    Ok(trimmed)
}

pub fn normalize_tag(tag: &str) -> Option<String> {
    let normalized = tag.nfkc().collect::<String>();
    let trimmed = normalized.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(trimmed.len());
    let mut prev_dash = false;
    for ch in trimmed.chars().flat_map(char::to_lowercase) {
        if ch.is_alphanumeric() {
            prev_dash = false;
            out.push(ch);
        } else if (matches!(ch, '-' | '_' | '.' | '/') || ch.is_whitespace()) && !prev_dash {
            prev_dash = true;
            out.push('-');
        }
    }
    let normalized = out
        .trim_matches('-')
        .chars()
        .take(MAX_MEMORY_TAG_CHARS)
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    (!normalized.is_empty()).then_some(normalized)
}

pub fn normalize_tags<I, S>(tags: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut seen = BTreeSet::new();
    for tag in tags {
        if let Some(tag) = normalize_tag(tag.as_ref()) {
            seen.insert(tag);
            if seen.len() >= MAX_MEMORY_TAGS {
                break;
            }
        }
    }
    seen.into_iter().collect()
}

fn normalize_retrieval_term(value: &str) -> Option<String> {
    let normalized = value.nfkc().collect::<String>();
    let collapsed = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    let bounded = collapsed
        .chars()
        .filter(|ch| !ch.is_control())
        .take(MAX_RETRIEVAL_TERM_CHARS)
        .collect::<String>();
    let bounded = bounded.trim();
    (!bounded.is_empty()).then(|| bounded.to_string())
}

/// Normalize, deduplicate, and bound stored or returned lexical retrieval terms.
///
/// This does not derive or expand terms; callers choose the appropriate count
/// limit for model input, canonical storage, or a response surface.
pub fn normalize_retrieval_terms<I, S>(values: I, limit: usize) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut seen = HashSet::new();
    let mut normalized = Vec::new();
    for value in values {
        let Some(value) = normalize_retrieval_term(value.as_ref()) else {
            continue;
        };
        let key = value.to_lowercase();
        if seen.insert(key) {
            normalized.push(value);
            if normalized.len() >= limit {
                break;
            }
        }
    }
    normalized
}

fn merge_retrieval_terms(
    explicit: &[String],
    fallback: &[String],
    explicit_limit: usize,
    total_limit: usize,
) -> Vec<String> {
    let explicit = normalize_retrieval_terms(explicit, explicit_limit);
    normalize_retrieval_terms(
        explicit.iter().chain(fallback.iter()).map(String::as_str),
        total_limit,
    )
}

pub fn retrieval_keywords(
    title: &str,
    content: &str,
    tags: &[String],
    explicit: &[String],
) -> Vec<String> {
    let fallback = extract_keywords(title, content, tags);
    merge_retrieval_terms(
        explicit,
        &fallback,
        MAX_EXPLICIT_MEMORY_KEYWORDS,
        MAX_MEMORY_KEYWORDS,
    )
}

fn retrieval_keywords_preserving(
    title: &str,
    content: &str,
    tags: &[String],
    explicit: &[String],
    preserved: &[String],
) -> Vec<String> {
    let explicit = normalize_retrieval_terms(explicit, MAX_EXPLICIT_MEMORY_KEYWORDS);
    let fallback = extract_keywords(title, content, tags);
    normalize_retrieval_terms(
        explicit
            .iter()
            .chain(preserved.iter())
            .chain(fallback.iter())
            .map(String::as_str),
        MAX_MEMORY_KEYWORDS,
    )
}

pub fn retrieval_entities(title: &str, content: &str, explicit: &[String]) -> Vec<String> {
    let fallback = detect_entities(title, content);
    merge_retrieval_terms(
        explicit,
        &fallback,
        MAX_EXPLICIT_MEMORY_ENTITIES,
        MAX_MEMORY_ENTITIES,
    )
}

fn retrieval_entities_preserving(
    title: &str,
    content: &str,
    explicit: &[String],
    preserved: &[String],
) -> Vec<String> {
    let explicit = normalize_retrieval_terms(explicit, MAX_EXPLICIT_MEMORY_ENTITIES);
    let fallback = detect_entities(title, content);
    normalize_retrieval_terms(
        explicit
            .iter()
            .chain(preserved.iter())
            .chain(fallback.iter())
            .map(String::as_str),
        MAX_MEMORY_ENTITIES,
    )
}

pub fn truncate_chars(value: &str, max_chars: usize) -> (String, bool) {
    let mut out = String::new();
    for (count, ch) in value.chars().enumerate() {
        if count >= max_chars {
            return (out, true);
        }
        out.push(ch);
    }
    (out, false)
}

pub fn count_chars(value: &str) -> usize {
    value.chars().count()
}

pub fn now_rfc3339() -> String {
    Utc::now().to_rfc3339()
}

pub fn derive_summary(content: &str, max_chars: usize) -> String {
    let collapsed = content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let (summary, truncated) = truncate_chars(&collapsed, max_chars);
    if truncated {
        format!("{}...", summary.trim_end())
    } else {
        summary
    }
}

pub fn extract_keywords(title: &str, content: &str, tags: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    for tag in tags {
        if let Some(tag) = normalize_tag(tag) {
            seen.insert(tag);
        }
    }

    // CJK-aware tokenization (shared with recall's `lexical_bm25::tokenize`) so a
    // bilingual (中文 + English) library stores Chinese keywords too. The old pass
    // keyed on `is_ascii_alphanumeric`, dropping ALL CJK — Chinese docs got no
    // keyword-boost and (since keywords feed the recall index) full-body Chinese
    // content was never indexed beyond the 240-char summary. English tokens keep
    // their prior lowercased form; each Chinese run contributes char bigrams. (#242)
    let combined = format!("{}\n{}", title, content);
    for token in lexical_bm25::tokenize(&combined) {
        seen.insert(token);
    }

    seen.into_iter().take(MAX_MEMORY_KEYWORDS).collect()
}

pub fn detect_entities(title: &str, content: &str) -> Vec<String> {
    let mut entities = BTreeSet::new();
    for token in format!("{}\n{}", title, content)
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '/'))
    {
        let trimmed = token.trim();
        if trimmed.len() < 3 {
            continue;
        }
        let has_upper = trimmed.chars().any(|ch| ch.is_ascii_uppercase());
        let has_separator = trimmed.contains('-') || trimmed.contains('_') || trimmed.contains('/');
        if has_upper || has_separator {
            entities.insert(trimmed.to_string());
        }
    }
    entities.into_iter().take(MAX_MEMORY_ENTITIES).collect()
}

pub fn sanitize_component(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return "unknown".to_string();
    }

    let mut out = String::with_capacity(trimmed.len());
    let mut prev_dash = false;
    for ch in trimmed.chars() {
        let normalized = match ch {
            'A'..='Z' => ch.to_ascii_lowercase(),
            'a'..='z' | '0'..='9' => ch,
            _ => '-',
        };
        if normalized == '-' {
            if prev_dash {
                continue;
            }
            prev_dash = true;
            out.push('-');
        } else {
            prev_dash = false;
            out.push(normalized);
        }
    }

    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "unknown".to_string()
    } else {
        out
    }
}

pub fn short_stable_hash(input: &str) -> Option<String> {
    use std::hash::{Hash, Hasher};

    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    trimmed.hash(&mut hasher);
    Some(format!("{:08x}", (hasher.finish() & 0xffff_ffff) as u32))
}

pub fn build_yaml_frontmatter(frontmatter: &DurableMemoryFrontmatter) -> io::Result<String> {
    serde_yaml::to_string(frontmatter).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("failed to serialize memory frontmatter: {error}"),
        )
    })
}

pub fn parse_markdown_document(content: &str) -> io::Result<(DurableMemoryFrontmatter, String)> {
    let trimmed = content.trim_start_matches('\u{feff}');
    let Some(rest) = trimmed.strip_prefix("---\n") else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "missing frontmatter start marker",
        ));
    };
    let Some(end_idx) = rest.find("\n---\n") else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "missing frontmatter end marker",
        ));
    };
    let yaml = &rest[..end_idx];
    let body = &rest[end_idx + "\n---\n".len()..];
    let mut frontmatter: DurableMemoryFrontmatter =
        serde_yaml::from_str(yaml).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("failed to parse memory frontmatter: {error}"),
            )
        })?;
    let memory_id = validate_memory_id(&frontmatter.id).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid memory frontmatter id: {error}"),
        )
    })?;
    frontmatter.id = memory_id.to_string();
    Ok((frontmatter, body.trim().to_string()))
}

pub fn render_markdown_document(
    frontmatter: &DurableMemoryFrontmatter,
    body: &str,
) -> io::Result<String> {
    let mut frontmatter = frontmatter.clone();
    frontmatter.id = validate_memory_id(&frontmatter.id)?.to_string();
    let yaml = build_yaml_frontmatter(&frontmatter)?;
    Ok(format!("---\n{}---\n\n{}\n", yaml, body.trim()))
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LexicalIndex {
    pub generated_at: String,
    pub items: Vec<LexicalIndexItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LexicalIndexItem {
    pub id: String,
    pub title: String,
    pub scope: MemoryScope,
    pub project_key: Option<String>,
    pub r#type: DurableMemoryType,
    pub status: DurableMemoryStatus,
    pub tags: Vec<String>,
    pub keywords: Vec<String>,
    pub entities: Vec<String>,
    pub updated_at: String,
    pub created_at: String,
    pub summary: String,
    /// Optional temporal granularity carried from the source frontmatter so recall
    /// can rank by cache-stability without re-reading every document. Back-compat:
    /// older `lexical.json` index files predate this field and deserialize to `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granularity: Option<TemporalGranularity>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RecentIndex {
    pub generated_at: String,
    pub items: Vec<RecentIndexItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentIndexItem {
    pub id: String,
    pub title: String,
    pub updated_at: String,
    pub last_accessed_at: Option<String>,
    pub status: DurableMemoryStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GraphIndex {
    pub generated_at: String,
    pub items: Vec<GraphIndexItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphIndexItem {
    pub id: String,
    pub related: Vec<String>,
    pub supersedes: Vec<String>,
    pub contradicted_by: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StaleCandidatesIndex {
    pub generated_at: String,
    pub items: Vec<StaleCandidateItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaleCandidateItem {
    pub id: String,
    pub title: String,
    pub status: DurableMemoryStatus,
    pub updated_at: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TaxonomyIndex {
    pub generated_at: String,
    pub by_type: BTreeMap<String, usize>,
    pub by_status: BTreeMap<String, usize>,
    pub by_scope: BTreeMap<String, usize>,
    pub total: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditLogEntry {
    pub timestamp: String,
    pub action: String,
    pub scope: MemoryScope,
    pub memory_id: Option<String>,
    pub session_id: Option<String>,
    pub topic: Option<String>,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

pub fn parse_rfc3339(value: &str) -> Option<DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

pub fn sort_memories_desc(memories: &mut [DurableMemoryDocument]) {
    memories.sort_by(|left, right| {
        let left_dt =
            parse_rfc3339(&left.frontmatter.updated_at).unwrap_or(DateTime::<Utc>::MIN_UTC);
        let right_dt =
            parse_rfc3339(&right.frontmatter.updated_at).unwrap_or(DateTime::<Utc>::MIN_UTC);
        right_dt
            .cmp(&left_dt)
            .then_with(|| left.frontmatter.id.cmp(&right.frontmatter.id))
    });
}

pub fn match_memory_query(
    doc: &DurableMemoryDocument,
    query: Option<&str>,
    filter_types: Option<&HashSet<DurableMemoryType>>,
    filter_statuses: Option<&HashSet<DurableMemoryStatus>>,
    filter_granularity: Option<&HashSet<TemporalGranularity>>,
) -> Option<f64> {
    if let Some(types) = filter_types
        && !types.contains(&doc.frontmatter.r#type)
    {
        return None;
    }
    if let Some(statuses) = filter_statuses
        && !statuses.contains(&doc.frontmatter.status)
    {
        return None;
    }
    // Absent filter (`None`) = no filtering, matching every memory regardless of
    // whether it carries a granularity. An active filter ("只看本周的 memory") only
    // matches memories that
    // carry one of the requested granularities; an untagged memory (`None`) never
    // matches an active granularity filter, since the caller asked for a specific
    // time horizon and an untagged memory doesn't claim one.
    if let Some(granularities) = filter_granularity {
        match doc.frontmatter.granularity {
            Some(granularity) if granularities.contains(&granularity) => {}
            _ => return None,
        }
    }

    let Some(query) = query.map(str::trim).filter(|value| !value.is_empty()) else {
        return Some(1.0);
    };

    // CJK-aware tokenization (shared with recall) so a Chinese `search`/`query_scope`
    // query matches (bigrams substring-match the Chinese-preserving title/body).
    // The old ASCII-only pass yielded zero tokens for Chinese → matched everything
    // unranked. (#242)
    let query_tokens = lexical_bm25::tokenize(query);
    if query_tokens.is_empty() {
        return Some(1.0);
    }

    let title = doc.frontmatter.title.to_ascii_lowercase();
    let body = doc.body.to_ascii_lowercase();
    let keywords: HashSet<String> = doc
        .frontmatter
        .retrieval
        .keywords
        .iter()
        .map(|value| value.to_ascii_lowercase())
        .collect();
    let tags: HashSet<String> = doc
        .frontmatter
        .tags
        .iter()
        .map(|value| value.to_ascii_lowercase())
        .collect();
    let entities: HashSet<String> = doc
        .frontmatter
        .retrieval
        .entities
        .iter()
        .map(|value| value.to_ascii_lowercase())
        .collect();

    let mut score = 0.0;
    let mut matched_any = false;
    for token in &query_tokens {
        let mut token_score = 0.0;
        if title.contains(token) {
            token_score += 3.0;
        }
        if keywords.contains(token) {
            token_score += 2.5;
        }
        if tags.contains(token) {
            token_score += 2.0;
        }
        if entities.contains(token) {
            token_score += 1.5;
        }
        if body.contains(token) {
            token_score += 1.0;
        }
        if token_score > 0.0 {
            matched_any = true;
            score += token_score;
        }
    }

    matched_any.then_some(score / query_tokens.len() as f64)
}

pub fn build_memory_markdown_view(
    scope: MemoryScope,
    project_key: Option<&str>,
    docs: &[DurableMemoryDocument],
) -> String {
    let title = match scope {
        MemoryScope::Global => "# Jiandu Memory Index (Global)".to_string(),
        MemoryScope::Project => format!(
            "# Jiandu Memory Index (Project: {})",
            project_key.unwrap_or("unknown")
        ),
        MemoryScope::Session => "# Jiandu Memory Index (Session)".to_string(),
    };
    let mut out = String::new();
    out.push_str(&title);
    out.push_str("\n\n");
    if docs.is_empty() {
        out.push_str("_(empty)_\n");
        return out;
    }

    for doc in docs {
        out.push_str(&format!(
            "- `{}` {} [{} / {}] updated {}\n",
            doc.frontmatter.id,
            doc.frontmatter.title,
            doc.frontmatter.r#type.as_str(),
            doc.frontmatter.status.as_str(),
            doc.frontmatter.updated_at,
        ));
        let summary = derive_summary(&doc.body, 160);
        if !summary.is_empty() {
            out.push_str(&format!("  - {}\n", summary));
        }
    }
    out
}

pub fn build_recent_markdown_view(docs: &[DurableMemoryDocument]) -> String {
    let mut out = String::from("# Recent Memory Updates\n\n");
    if docs.is_empty() {
        out.push_str("_(empty)_\n");
        return out;
    }
    for doc in docs.iter().take(20) {
        out.push_str(&format!(
            "- `{}` {} — {}\n",
            doc.frontmatter.id, doc.frontmatter.title, doc.frontmatter.updated_at
        ));
    }
    out
}

pub fn build_stale_markdown_view(docs: &[DurableMemoryDocument]) -> String {
    let mut out = String::from("# Stale Memory Candidates\n\n");
    let stale: Vec<_> = docs
        .iter()
        .filter(|doc| doc.frontmatter.status != DurableMemoryStatus::Active)
        .collect();
    if stale.is_empty() {
        out.push_str("_(no stale items)_\n");
        return out;
    }
    for doc in stale {
        out.push_str(&format!(
            "- `{}` {} [{}]\n",
            doc.frontmatter.id,
            doc.frontmatter.title,
            doc.frontmatter.status.as_str()
        ));
    }
    out
}

pub fn parse_query_cursor(cursor: Option<&str>) -> usize {
    cursor
        .and_then(|raw| raw.rsplit(':').next())
        .and_then(|raw| raw.parse::<usize>().ok())
        .unwrap_or(0)
}

pub fn make_query_cursor(scope: MemoryScope, offset: usize) -> String {
    format!("{}:{}", scope.as_str(), offset)
}

pub fn summary_json(items: usize, total: usize) -> String {
    if total == 0 {
        "No matching memories found.".to_string()
    } else {
        format!("Returned top {} of {} matching memories.", items, total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_tags_dedupes_and_sanitizes() {
        let tags = normalize_tags(["User Preference", "user-preference", "release/freeze"]);
        assert_eq!(tags, vec!["release-freeze", "user-preference"]);
    }

    #[test]
    fn retrieval_metadata_is_cjk_safe_bounded_and_keeps_legacy_expansion() {
        let tags = normalize_tags([" Ａuth 认证 ", "auth-认证", "发布/流程"]);
        assert!(tags.contains(&"auth-认证".to_string()));
        assert!(tags.contains(&"发布-流程".to_string()));

        let legacy_body = (0..160)
            .map(|index| format!("legacyterm{index:03}"))
            .collect::<Vec<_>>()
            .join(" ");
        let fallback = extract_keywords("legacy", &legacy_body, &[]);
        assert_eq!(fallback.len(), MAX_MEMORY_KEYWORDS);

        let explicit = (0..40)
            .map(|index| format!("model-alias-{index:02}"))
            .chain(["专用别名".to_string(), "model-alias-00".to_string()])
            .collect::<Vec<_>>();
        let keywords = retrieval_keywords("legacy", &legacy_body, &[], &explicit);
        assert_eq!(keywords.len(), MAX_MEMORY_KEYWORDS);
        assert_eq!(keywords[0], "model-alias-00");
        assert!(keywords.contains(&"model-alias-31".to_string()));
        assert!(!keywords.contains(&"model-alias-32".to_string()));
        assert!(
            keywords.iter().any(|value| value.starts_with("legacyterm")),
            "bounded explicit hints must not displace all legacy fallback coverage"
        );
    }

    #[test]
    fn legacy_embedding_fields_are_read_but_never_rendered_again() {
        let document = "---\n\
id: legacy_memory\n\
title: Legacy metadata\n\
type: project\n\
scope: global\n\
status: active\n\
created_at: 2026-01-01T00:00:00Z\n\
updated_at: 2026-01-01T00:00:00Z\n\
created_by:\n  kind: session\n\
updated_by:\n  kind: memory_write\n\
retrieval:\n  keywords: [legacy]\n  embedding_ready: true\n\
---\n\
Legacy body.\n";
        let (frontmatter, body) = parse_markdown_document(document).unwrap();
        assert_eq!(frontmatter.retrieval.keywords, vec!["legacy"]);
        let rendered = render_markdown_document(&frontmatter, &body).unwrap();
        assert!(!rendered.contains("embedding_ready"));

        let legacy_index_item = serde_json::json!({
            "id": "legacy_memory",
            "title": "Legacy metadata",
            "scope": "global",
            "project_key": null,
            "type": "project",
            "status": "active",
            "tags": [],
            "keywords": ["legacy"],
            "entities": [],
            "updated_at": "2026-01-01T00:00:00Z",
            "created_at": "2026-01-01T00:00:00Z",
            "summary": "Legacy body.",
            "embedding": [0.1, 0.2]
        });
        let parsed: LexicalIndexItem = serde_json::from_value(legacy_index_item).unwrap();
        let serialized = serde_json::to_string(&parsed).unwrap();
        assert!(!serialized.contains("embedding"));
    }

    #[test]
    fn parse_markdown_document_requires_frontmatter() {
        let result = parse_markdown_document("plain body");
        assert!(result.is_err());
    }

    #[test]
    fn memory_and_session_ids_reject_path_syntax_and_reserved_components() {
        assert_eq!(
            validate_memory_id("Memory_01-safe").unwrap(),
            "Memory_01-safe"
        );
        assert_eq!(validate_memory_id(" mem_1 ").unwrap(), "mem_1");
        assert!(validate_memory_id(&"a".repeat(MAX_MEMORY_ID_LEN)).is_ok());
        for invalid in ["", "/tmp/escape", "../escape", "nested/id", "nested\\id"] {
            assert!(validate_memory_id(invalid).is_err(), "accepted {invalid:?}");
        }
        assert!(validate_memory_id(&"a".repeat(MAX_MEMORY_ID_LEN + 1)).is_err());

        for invalid in [".", "..", " . ", " .. "] {
            assert!(
                validate_session_id(invalid).is_err(),
                "accepted {invalid:?}"
            );
        }
    }

    #[test]
    fn markdown_frontmatter_ids_are_normalized_before_use_or_persistence() {
        let document = "---\n\
id: ' mem_1 '\n\
title: Canonical id\n\
type: project\n\
scope: project\n\
project_key: proj-1\n\
status: active\n\
created_at: 2026-01-01T00:00:00Z\n\
updated_at: 2026-01-01T00:00:00Z\n\
created_by:\n  kind: session\n\
updated_by:\n  kind: memory_write\n\
---\n\
Body.\n";
        let (mut frontmatter, body) = parse_markdown_document(document).unwrap();
        assert_eq!(frontmatter.id, "mem_1");

        frontmatter.id = " mem_2 ".to_string();
        let rendered = render_markdown_document(&frontmatter, &body).unwrap();
        assert!(rendered.contains("id: mem_2\n"));
        assert!(!rendered.contains(" mem_2 "));
    }

    /// A granularity set on the frontmatter round-trips through render + parse.
    #[test]
    fn granularity_round_trips_through_render_and_parse() {
        let document = "---\n\
id: mem-granularity\n\
title: Granularity memory\n\
type: project\n\
scope: project\n\
project_key: proj-1\n\
status: active\n\
created_at: 2026-01-01T00:00:00Z\n\
updated_at: 2026-01-01T00:00:00Z\n\
created_by:\n  kind: session\n\
updated_by:\n  kind: memory_write\n\
---\n\
Body.\n";
        let (mut frontmatter, body) = parse_markdown_document(document).unwrap();
        frontmatter.granularity = Some(TemporalGranularity::Quarter);

        let rendered = render_markdown_document(&frontmatter, &body).unwrap();
        assert!(rendered.contains("granularity: quarter"));

        let (reparsed, _) = parse_markdown_document(&rendered).unwrap();
        assert_eq!(reparsed.granularity, Some(TemporalGranularity::Quarter));
    }

    /// `None` granularity must not emit a `granularity:` key (skip_serializing_if),
    /// so existing documents re-rendered after a load keep their on-disk shape.
    #[test]
    fn none_granularity_is_omitted_on_render() {
        let document = "---\n\
id: mem-default\n\
title: Memory with defaults\n\
type: project\n\
scope: project\n\
project_key: proj-1\n\
status: active\n\
created_at: 2026-01-01T00:00:00Z\n\
updated_at: 2026-01-01T00:00:00Z\n\
created_by:\n  kind: session\n\
updated_by:\n  kind: memory_write\n\
---\n\
Body.\n";
        let (frontmatter, body) = parse_markdown_document(document).unwrap();
        let rendered = render_markdown_document(&frontmatter, &body).unwrap();
        assert!(!rendered.contains("granularity"));
    }

    #[test]
    fn temporal_granularity_parse_is_case_insensitive_and_rejects_unknown() {
        assert_eq!(
            TemporalGranularity::parse("Week"),
            Some(TemporalGranularity::Week)
        );
        assert_eq!(
            TemporalGranularity::parse("  YEAR "),
            Some(TemporalGranularity::Year)
        );
        assert_eq!(TemporalGranularity::parse("decade"), None);
    }

    #[test]
    fn cache_stability_rank_orders_coarse_before_fine_and_none_first() {
        use TemporalGranularity::*;
        let rank = TemporalGranularity::cache_stability_rank;
        // None is treated as most stable, then coarsest → finest.
        assert!(rank(None) < rank(Some(Year)));
        assert!(rank(Some(Year)) < rank(Some(Quarter)));
        assert!(rank(Some(Quarter)) < rank(Some(Month)));
        assert!(rank(Some(Month)) < rank(Some(Week)));
        assert!(rank(Some(Week)) < rank(Some(Day)));
    }
}
