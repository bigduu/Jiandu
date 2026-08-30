# Repository Guidelines

Jiandu is a filesystem-backed memory system for agents. Keep it small,
deterministic, and independent of any agent runtime.

## Architecture Boundary

The workspace contains only `crates/jiandu-memory` and `crates/jiandu-mcp`.
Keep MCP concerns out of the memory crate, and map MCP actions directly to
`MemoryStore` without adding a generic backend or service framework. Callers
provide a stable opaque `ProjectId`; prompt assembly and ranking policy belong
to the consuming host.

## Agent Use Through MCP

Use the single `memory` tool; a host may namespace it as
`mcp__jiandu__memory`. Select behavior with the `action` argument.

- Recall before guessing. Use `query` for relevant prior knowledge and `get`
  when the full returned item is needed; verify recalled facts against current
  repository and tool state.
- Use Session for workstream continuity, Project for project-specific durable
  facts, and Global only for stable cross-project knowledge. Project authority
  comes from the MCP host.
- Query before `write`. Store only confirmed, durable, non-secret facts that
  will help a future session, as concise atomic items with searchable titles.
- Never edit Jiandu data files directly or create a repository memory file as a
  fallback; all reads and mutations go through `jiandu-memory` or the MCP tool.

## Development Gates

Before committing, run:

```shell
cargo fmt --all -- --check
cargo metadata --locked --all-features --format-version 1
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked
```

Use Conventional Commits. Do not push or open a pull request unless explicitly
requested.
