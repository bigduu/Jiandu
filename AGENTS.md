# Repository Guidelines

Jiandu is unreleased and compatibility-free. Its core is a mechanical port of Bamboo's filesystem memory implementation from `origin/dev@6135bb4c`.

The workspace has only `crates/jiandu-memory` and `crates/jiandu-mcp`. Keep MCP concerns out of the memory crate, and map MCP actions directly to `MemoryStore` without a generic backend or service framework.

Use only the current `memory/v1` layout. Never restore Jiandu `v1alpha` contracts, fixtures, schemas, migrations, or legacy aliases/readers. Callers must provide a stable opaque `ProjectId`, and every raw Project key must be validated before it reaches a filesystem path.

Preserve source behavior and avoid redesign or new features, except for narrow path-safety and host-owned Project-authority corrections. LLM reranking, Dream, ledger, plan, budget, prompt assembly, and `workspace_state` stay outside Jiandu's memory core.

Before committing, run:

```shell
cargo fmt --all -- --check
cargo metadata --locked --all-features --format-version 1
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked
```

Use Conventional Commits. Do not push or open a pull request unless explicitly requested.
