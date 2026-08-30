# Jiandu

Jiandu (简牍) is unreleased and has no compatibility promise. The repository is being reset to a small Rust baseline mechanically ported from Bamboo `origin/dev@6135bb4c`.

The intended workspace contains two crates:

- `crates/jiandu-memory`: atomic filesystem operations and deterministic memory-store behavior.
- `crates/jiandu-mcp`: the separately integrated MCP boundary.

`jiandu-memory` supports Bamboo's current `memory/v1` layout only. Callers supply a stable, opaque, path-safe `ProjectId`; Jiandu does not derive Project identity from workspace paths.

The phase-one port preserves Session operations, durable memory CRUD and maintenance, rebuildable artifacts, lexical/BM25/CJK recall, and concurrency behavior. It deliberately excludes LLM reranking, Dream, ledger, plan, budget, prompt assembly, `workspace_state`, and all historical readers, aliases, migrations, schemas, and Jiandu `v1alpha` code.

Run the core gates from the repository root:

```shell
cargo fmt --all -- --check
cargo clippy -p jiandu-memory --all-targets -- -D warnings
cargo test -p jiandu-memory
```

Jiandu is licensed under the [MIT License](LICENSE).
