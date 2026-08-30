# Jiandu

Jiandu (简牍) is a compatibility-free memory system mechanically ported from Bamboo `origin/dev@6135bb4c`.

The workspace contains exactly two crates:

- `crates/jiandu-memory`: atomic filesystem persistence, deterministic memory operations, and lexical/BM25/CJK recall.
- `crates/jiandu-mcp`: one stdio MCP server exposing one unified `memory` tool with Bamboo's current 17 actions.

Callers provide a stable, opaque, path-safe `ProjectId`; Jiandu never derives Project identity from a workspace path. A request parameter may confirm the host-provided Project identity, but cannot grant Project access.

`memory/v1` is only the name of the current internal on-disk layout. It is not a public `v1alpha` API or a compatibility lifecycle. Jiandu contains no historical readers, aliases, migrations, versioned schemas, or old `v1alpha` crates.

The baseline preserves Session notes, durable memory CRUD and maintenance, rebuildable artifacts, recall, and concurrency behavior. Dream, LLM reranking, ledger, plan, budget, prompt assembly, workspace discovery, and `workspace_state` remain Bamboo responsibilities.

Run the server:

```shell
cargo run -p jiandu-mcp --bin jiandu -- \
  --data-dir /path/to/data \
  --session-id session_1
```

Add `--project-id project_1` when the host grants this server access to that Project's memory.

An MCP host can launch the compiled binary with the same arguments. For example:

```json
{
  "mcpServers": {
    "jiandu": {
      "command": "/absolute/path/to/jiandu",
      "args": [
        "--data-dir", "/absolute/path/to/shared-memory",
        "--session-id", "agent_session_1",
        "--project-id", "project_1"
      ]
    }
  }
}
```

Use a distinct `session-id` for each agent workstream. Agents trusted for the
same Project may use the same opaque `project-id` and data directory to share
durable Project memory.

Durable concurrency is scoped, not record-local. Every mutation holds both a
process-local mutex and an OS advisory lock for the complete canonical
read-modify-write, audit append, and derived-artifact refresh. Separate Jiandu
processes can therefore safely mutate different records in the same Global or
Project scope; mutations in different scopes remain independent. Lock files are
internal to each scope and require no host configuration.

Advisory locking is cooperative: every writer to a Jiandu data directory must
use Jiandu's API. Direct edits to canonical files are unsupported and can bypass
the concurrency contract. Session notes are intentionally isolated by
`session-id`; do not share one Session identity across independent processes.

Recall access logging remains a best-effort soft signal outside the durable
scope lock. Under cross-process recall load, a sample can be lost (especially
during rare log compaction); this can only make later capacity ranking slightly
stale. It cannot change canonical memory, corrupt derived recall artifacts, or
fail the recall that produced the sample.

A mutation is not a filesystem-wide transaction. A call can return an error
after its canonical memory document was committed but before an audit or
rebuildable artifact completed. After any mutation failure, run `inspect` first.
If canonical documents committed but derived artifacts are stale, run `rebuild`,
then use `query` or `get` to verify current state before choosing the next
action; never blindly retry.

During MCP initialization Jiandu returns concise usage instructions, so hosts
that surface server instructions can teach the connected agent when to recall,
write, and choose Session, Project, or Global scope. A host may namespace the
tool name, for example as `mcp__jiandu__memory`; the server itself exposes only
the single `memory` tool.

Run all gates from the repository root:

```shell
cargo fmt --all -- --check
cargo metadata --locked --all-features --format-version 1
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked
```

Jiandu is licensed under the [MIT License](LICENSE).
