use std::cmp::Ordering;
use std::io;

use super::{
    DurableMemoryStatus, LexicalIndex, MemoryScope, MemoryStore, TemporalGranularity, parse_rfc3339,
};

#[derive(Debug, Clone, PartialEq)]
pub struct MemoryRecallCandidate {
    pub id: String,
    pub title: String,
    pub score: f64,
    pub scope: MemoryScope,
    pub project_key: Option<String>,
    pub status: DurableMemoryStatus,
    pub updated_at: String,
    pub summary: String,
    pub granularity: Option<TemporalGranularity>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryRecallOptions {
    pub shortlist_limit: usize,
    pub include_global_fallback: bool,
    pub max_candidates_per_scope: usize,
}

impl Default for MemoryRecallOptions {
    fn default() -> Self {
        Self {
            shortlist_limit: 3,
            include_global_fallback: true,
            max_candidates_per_scope: 20,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryRecallStrategy {
    Lexical,
}

impl MemoryRecallStrategy {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        "lexical"
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MemoryRecallSelection {
    pub candidates: Vec<MemoryRecallCandidate>,
    pub strategy: MemoryRecallStrategy,
}

pub async fn shortlist_relevant_memories(
    store: &MemoryStore,
    project_key: Option<&str>,
    query: &str,
    options: &MemoryRecallOptions,
) -> io::Result<Vec<MemoryRecallCandidate>> {
    let limit = options.shortlist_limit.max(1);
    let mut candidates =
        lexical_shortlist_relevant_memories(store, project_key, query, options).await?;
    candidates.truncate(limit);
    Ok(candidates)
}

pub async fn select_relevant_memories(
    store: &MemoryStore,
    project_key: Option<&str>,
    query: &str,
    options: &MemoryRecallOptions,
) -> io::Result<MemoryRecallSelection> {
    Ok(MemoryRecallSelection {
        candidates: shortlist_relevant_memories(store, project_key, query, options).await?,
        strategy: MemoryRecallStrategy::Lexical,
    })
}

/// Rank recall candidates from a caller-provided lexical index.
///
/// This function is deterministic and performs no filesystem or network I/O. It
/// applies Jiandu's fixed BM25/CJK scoring, status eligibility, and deterministic
/// ordering, while remaining policy-neutral about cross-index composition: it
/// ranks only the items in `index`. The caller remains responsible for scope
/// precedence, fallback behavior, index unions, and any primary-first ID
/// deduplication. `limit` is an exact upper bound; a zero limit, a blank query,
/// or a query without scorable matches returns an empty vector.
#[must_use]
pub fn recall_candidates_from_lexical_index(
    index: &LexicalIndex,
    query: &str,
    limit: usize,
) -> Vec<MemoryRecallCandidate> {
    recall_candidates_from_lexical_index_with_status_policy(index, query, limit, false)
}

pub(crate) fn recall_candidates_from_lexical_index_with_status_policy(
    index: &LexicalIndex,
    query: &str,
    limit: usize,
    include_all_statuses: bool,
) -> Vec<MemoryRecallCandidate> {
    let query = query.trim();
    if limit == 0 || query.is_empty() {
        return Vec::new();
    }

    let query_tokens = super::lexical_bm25::tokenize(query);
    if query_tokens.is_empty() {
        return Vec::new();
    }

    let corpus = if include_all_statuses {
        super::lexical_bm25::Bm25Corpus::build_including_all_statuses(&index.items)
    } else {
        super::lexical_bm25::Bm25Corpus::build(&index.items)
    };
    let mut candidates = index
        .items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            corpus
                .score(index, &query_tokens)
                .map(|score| (item, score))
        })
        .map(|(item, score)| MemoryRecallCandidate {
            id: item.id.clone(),
            title: item.title.clone(),
            score,
            scope: item.scope,
            project_key: item.project_key.clone(),
            status: item.status,
            updated_at: item.updated_at.clone(),
            summary: item.summary.clone(),
            granularity: item.granularity,
        })
        .collect::<Vec<_>>();

    sort_recall_candidates(&mut candidates);
    candidates.truncate(limit);
    candidates
}

async fn lexical_shortlist_relevant_memories(
    store: &MemoryStore,
    project_key: Option<&str>,
    query: &str,
    options: &MemoryRecallOptions,
) -> io::Result<Vec<MemoryRecallCandidate>> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }

    let limit = options.shortlist_limit.max(1);
    let per_scope_limit = options.max_candidates_per_scope.max(limit);

    if let Some(project_key) = project_key.map(str::trim).filter(|value| !value.is_empty()) {
        let project_hits = shortlist_scope(
            store,
            MemoryScope::Project,
            Some(project_key),
            query,
            per_scope_limit,
        )
        .await?;
        if !project_hits.is_empty() {
            return Ok(project_hits);
        }
    }

    if options.include_global_fallback {
        return shortlist_scope(store, MemoryScope::Global, None, query, per_scope_limit).await;
    }

    Ok(Vec::new())
}

async fn shortlist_scope(
    store: &MemoryStore,
    scope: MemoryScope,
    project_key: Option<&str>,
    query: &str,
    limit: usize,
) -> io::Result<Vec<MemoryRecallCandidate>> {
    let Some(index) = store.read_lexical_index(scope, project_key).await? else {
        return Ok(Vec::new());
    };
    Ok(recall_candidates_from_lexical_index(&index, query, limit))
}

fn sort_recall_candidates(candidates: &mut [MemoryRecallCandidate]) {
    candidates.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| {
                TemporalGranularity::cache_stability_rank(left.granularity).cmp(
                    &TemporalGranularity::cache_stability_rank(right.granularity),
                )
            })
            .then_with(|| {
                let left_dt = parse_rfc3339(&left.updated_at)
                    .unwrap_or(chrono::DateTime::<chrono::Utc>::MIN_UTC);
                let right_dt = parse_rfc3339(&right.updated_at)
                    .unwrap_or(chrono::DateTime::<chrono::Utc>::MIN_UTC);
                right_dt.cmp(&left_dt)
            })
            .then_with(|| left.title.cmp(&right.title))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_store::{DurableMemoryType, LexicalIndexItem};
    use tempfile::tempdir;

    fn candidate(
        id: &str,
        score: f64,
        granularity: Option<TemporalGranularity>,
    ) -> MemoryRecallCandidate {
        MemoryRecallCandidate {
            id: id.to_string(),
            title: id.to_string(),
            score,
            scope: MemoryScope::Project,
            project_key: Some("proj-1".to_string()),
            status: DurableMemoryStatus::Active,
            updated_at: "2026-04-09T00:00:00Z".to_string(),
            summary: "summary".to_string(),
            granularity,
        }
    }

    fn lexical_item(id: &str, status: DurableMemoryStatus, title: &str) -> LexicalIndexItem {
        LexicalIndexItem {
            id: id.to_string(),
            title: title.to_string(),
            scope: MemoryScope::Project,
            project_key: Some("proj-1".to_string()),
            r#type: DurableMemoryType::Project,
            status,
            tags: Vec::new(),
            keywords: Vec::new(),
            entities: Vec::new(),
            updated_at: "2026-04-09T00:00:00Z".to_string(),
            created_at: "2026-04-09T00:00:00Z".to_string(),
            summary: title.to_string(),
            granularity: None,
        }
    }

    #[test]
    fn equal_score_candidates_sort_coarse_granularity_first_for_cache_stability() {
        let mut candidates = vec![
            candidate("day", 5.0, Some(TemporalGranularity::Day)),
            candidate("year", 5.0, Some(TemporalGranularity::Year)),
            candidate("none", 5.0, None),
            candidate("month", 5.0, Some(TemporalGranularity::Month)),
        ];
        sort_recall_candidates(&mut candidates);
        let order: Vec<&str> = candidates
            .iter()
            .map(|candidate| candidate.id.as_str())
            .collect();
        assert_eq!(order, vec!["none", "year", "month", "day"]);
    }

    #[test]
    fn higher_score_still_wins_over_cache_stable_granularity() {
        let mut candidates = vec![
            candidate("year-low", 1.0, Some(TemporalGranularity::Year)),
            candidate("day-high", 9.0, Some(TemporalGranularity::Day)),
        ];
        sort_recall_candidates(&mut candidates);
        assert_eq!(candidates[0].id, "day-high");
    }

    #[test]
    fn lexical_index_recall_is_cjk_status_aware_limited_and_deterministic() {
        let index = LexicalIndex {
            generated_at: "2026-04-09T00:00:00Z".to_string(),
            items: vec![
                lexical_item("stale", DurableMemoryStatus::Stale, "多租户隔离设计"),
                lexical_item("active", DurableMemoryStatus::Active, "多租户隔离设计"),
                lexical_item("archived", DurableMemoryStatus::Archived, "多租户隔离设计"),
                lexical_item("unrelated", DurableMemoryStatus::Active, "缓存前缀稳定性"),
            ],
        };

        let limited = recall_candidates_from_lexical_index(&index, "多租户", 1);
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].id, "active");
        assert_eq!(limited[0].status, DurableMemoryStatus::Active);

        let all = recall_candidates_from_lexical_index(&index, "多租户", 10);
        assert_eq!(
            all.iter()
                .map(|candidate| candidate.id.as_str())
                .collect::<Vec<_>>(),
            vec!["active", "stale"]
        );
        assert_eq!(
            all,
            recall_candidates_from_lexical_index(&index, "多租户", 10)
        );
        assert!(recall_candidates_from_lexical_index(&index, "多租户", 0).is_empty());
    }

    #[tokio::test]
    async fn store_shortlist_matches_pure_lexical_index_recall() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::new(dir.path());
        store
            .write_memory(
                MemoryScope::Project,
                Some("proj-1"),
                DurableMemoryType::Project,
                "Checkpoint recovery decision",
                "Restore the database snapshot before reopening traffic.",
                &[],
                Some("session-1"),
                "main-model",
                false,
                None,
            )
            .await
            .unwrap();

        let options = MemoryRecallOptions {
            shortlist_limit: 2,
            include_global_fallback: false,
            max_candidates_per_scope: 4,
        };
        let from_store =
            shortlist_relevant_memories(&store, Some("proj-1"), "checkpoint", &options)
                .await
                .unwrap();
        let index = store
            .read_lexical_index(MemoryScope::Project, Some("proj-1"))
            .await
            .unwrap()
            .unwrap();
        let from_index =
            recall_candidates_from_lexical_index(&index, "checkpoint", options.shortlist_limit);

        assert_eq!(from_store, from_index);
    }

    #[tokio::test]
    async fn project_scope_shortlist_excludes_global_when_project_hits_exist() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::new(dir.path());
        store
            .write_memory(
                MemoryScope::Project,
                Some("proj-1"),
                DurableMemoryType::Project,
                "Release freeze decision",
                "Project-specific release freeze note.",
                &["release".to_string()],
                Some("session-1"),
                "main-model",
                false,
                None,
            )
            .await
            .unwrap();
        store
            .write_memory(
                MemoryScope::Global,
                None,
                DurableMemoryType::Reference,
                "Global release guidance",
                "Global note that should not be used when project hits exist.",
                &["release".to_string()],
                Some("session-1"),
                "main-model",
                false,
                None,
            )
            .await
            .unwrap();

        let candidates = shortlist_relevant_memories(
            &store,
            Some("proj-1"),
            "release freeze",
            &MemoryRecallOptions::default(),
        )
        .await
        .unwrap();
        assert!(!candidates.is_empty());
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.scope == MemoryScope::Project)
        );
    }

    #[tokio::test]
    async fn global_fallback_triggers_only_when_project_hits_are_absent() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::new(dir.path());
        store
            .write_memory(
                MemoryScope::Global,
                None,
                DurableMemoryType::Reference,
                "Global release guidance",
                "Fallback note for release work.",
                &["release".to_string()],
                Some("session-1"),
                "main-model",
                false,
                None,
            )
            .await
            .unwrap();

        let candidates = shortlist_relevant_memories(
            &store,
            Some("proj-missing"),
            "release guidance",
            &MemoryRecallOptions::default(),
        )
        .await
        .unwrap();
        assert!(!candidates.is_empty());
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.scope == MemoryScope::Global)
        );
    }
}
