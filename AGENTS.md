# Repository Guidelines

Jiandu `0.1.0` is a published, compatibility-free memory system. Its core is a mechanical port of Bamboo's filesystem memory implementation from `origin/dev@6135bb4c`.

## Architecture Boundary

The workspace has only `crates/jiandu-memory` and `crates/jiandu-mcp`. Keep MCP concerns out of the memory crate, and map MCP actions directly to `MemoryStore` without a generic backend or service framework.

Use only the current `memory/v1` layout. Never restore Jiandu `v1alpha` contracts, fixtures, schemas, migrations, or legacy aliases/readers. Callers must provide a stable opaque `ProjectId`, and every raw Project key must be validated before it reaches a filesystem path.

Preserve source behavior and avoid redesign or new features, except for narrow path-safety and host-owned Project-authority corrections. LLM reranking, Dream, ledger, plan, budget, prompt assembly, and `workspace_state` stay outside Jiandu's memory core.

## Agent Use Through MCP

When a host connects an agent to Jiandu, use the single `memory` tool. The host
may namespace it as `mcp__jiandu__memory`; select behavior with the `action`
argument. Never edit Jiandu data files directly or create a repository memory
file as a fallback.

- Recall before guessing. Use `query` when a prior preference, decision, or
  project fact may change the task, and use `get` when an exact returned item is
  needed. Recalled memory is supporting evidence and must be checked against
  current repository and tool state.
- Use `session_read`, `session_append`, and `session_replace` for concise
  workstream continuity. Session notes belong to the host-provided `session_id`;
  they are not durable cross-session knowledge.
- Use Project scope for project-specific durable facts and Global only for truly
  cross-project preferences or stable references. Project authority comes from
  the MCP host. Normally omit `project_key`; it cannot grant access or override
  the host Project.
- Query before `write`. Persist only a confirmed, durable, non-derivable fact
  that will help a future session, as one atomic item with a searchable title.
  Never store secrets, credentials, tokens, raw logs, tentative conclusions, or
  routine task completion.
- A mutation error or interrupted response has an unknown outcome. For a Session
  mutation, verify the same topic with `session_read` (and use
  `session_list_topics` only when the topic itself is uncertain). For a durable
  Project/Global mutation, run `inspect` for that scope; if only derived artifacts
  are stale, run `rebuild`; then use `query` or `get` to verify current state.
  Never blindly retry.

Bamboo may use `jiandu-memory` natively and optimize recalled memory while it
assembles dynamic agent context. That ranking and prompt policy remains a Bamboo
responsibility. Other agents consume the shared store through Jiandu MCP.

## Development Gates

Before committing, run:

```shell
cargo fmt --all -- --check
cargo metadata --locked --all-features --format-version 1
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked
```

Use Conventional Commits. Do not push or open a pull request unless explicitly requested.
