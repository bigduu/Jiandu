# Repository Guidelines

Jiandu is unreleased and compatibility-free. Its core is a mechanical port of Bamboo's filesystem memory implementation from `origin/dev@6135bb4c`.

The intended workspace has only `crates/jiandu-memory` and `crates/jiandu-mcp`. Keep MCP concerns out of the memory crate.

Use only the current `memory/v1` layout. Never restore Jiandu `v1alpha` contracts, fixtures, schemas, migrations, or legacy aliases/readers. Callers must provide a stable opaque `ProjectId`, and every raw Project key must be validated before it reaches a filesystem path.

Preserve source behavior and avoid redesign or new features. LLM reranking, Dream, ledger, plan, budget, prompt assembly, and `workspace_state` stay outside Jiandu's memory core.

Before committing, run:

```shell
cargo fmt --all -- --check
cargo clippy -p jiandu-memory --all-targets -- -D warnings
cargo test -p jiandu-memory
```

Use Conventional Commits. Do not push or open a pull request unless explicitly requested.
