use serde::Deserialize;
use std::collections::BTreeMap;

use super::{StdioClient, success};
use serde_json::{Value, json};

#[derive(Deserialize)]
struct Corpus {
    version: u32,
    facts: Vec<Fact>,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Fact {
    name: String,
    project: String,
    title: String,
    body: String,
    keywords: Vec<String>,
    #[serde(default)]
    supersedes: Option<String>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    project: String,
    query: String,
    expected: Vec<String>,
    #[serde(default)]
    allowed_weak_hits: Vec<String>,
}

fn project_context(project: &str) -> Option<Value> {
    Some(json!({"project_id":project}))
}

#[test]
fn stdio_recall_baseline_for_coding_facts() {
    let corpus: Corpus = serde_json::from_str(include_str!("coding-v1.json")).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let mut client = StdioClient::start(directory.path(), &[]);
    let mut ids = BTreeMap::new();
    for fact in &corpus.facts {
        let written = success(client.call(
            json!({
                "action":"write", "scope":"project", "type":"reference",
                "title":fact.title, "content":fact.body, "keywords":fact.keywords,
                "options":{"allow_merge_if_similar":false}
            }),
            project_context(&fact.project),
        ));
        let id = written["memory"]["id"].as_str().unwrap().to_owned();
        if let Some(source) = &fact.supersedes {
            success(client.call(
                json!({
                    "action":"merge", "id":id, "content":fact.body,
                    "source_memory_ids":[ids.get(source).unwrap()], "keywords":fact.keywords
                }),
                project_context(&fact.project),
            ));
        }
        assert!(ids.insert(&fact.name, id).is_none(), "duplicate fact name");
    }

    let mut expected_count = 0;
    let mut recalled_count = 0;
    let mut no_answer_cases = 0;
    let mut nonempty_negatives = 0;
    let mut unexpected_hits = 0;
    for case in &corpus.cases {
        let expected = case
            .expected
            .iter()
            .map(|name| ids[name].as_str())
            .collect::<Vec<_>>();
        // MCP also serves management queries that intentionally include older states.
        // Evaluate the current-fact policy with the existing status filter.
        let result = success(client.call(
            json!({
                "action":"query", "scope":"project", "query":case.query,
                "filters":{"status":["active"]}
            }),
            project_context(&case.project),
        ));
        let hits = result["data"]["items"].as_array().unwrap();
        assert!(
            hits.len() <= 3,
            "{} exceeded the compact default",
            case.name
        );
        let actual = hits
            .iter()
            .map(|hit| hit["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        expected_count += expected.len();
        let matched = expected.iter().filter(|id| actual.contains(id)).count();
        recalled_count += matched;
        let allowed_weak = case
            .allowed_weak_hits
            .iter()
            .map(|name| ids[name].as_str())
            .collect::<Vec<_>>();
        let unexpected = actual
            .iter()
            .filter(|id| !expected.contains(id) && !allowed_weak.contains(id))
            .count();
        unexpected_hits += unexpected;
        if expected.is_empty() {
            no_answer_cases += 1;
            nonempty_negatives += usize::from(!actual.is_empty());
        }
        eprintln!(
            "{}: expected={matched}/{} unexpected={unexpected}",
            case.name,
            expected.len()
        );
    }
    eprintln!(
        "coding-v{}: recall@3={recalled_count}/{expected_count} negative_queries_with_candidates={nonempty_negatives}/{no_answer_cases} unexpected_hits={unexpected_hits}",
        corpus.version
    );
    assert!(expected_count > 0 && no_answer_cases > 0);
    assert_eq!(recalled_count, expected_count);
    assert_eq!(unexpected_hits, 0);
}
