# Jiandu

Jiandu (简牍) is a small, filesystem-backed memory system for AI agents. It stores
session notes and durable knowledge with deterministic lexical recall and MCP.

## Components

- `jiandu-memory` provides persistence, maintenance, and lexical/BM25/CJK recall.
- `jiandu-mcp` exposes the store over stdio as one MCP tool named `memory`.
  Its `action` argument selects one of 17 memory operations.

## Memory scopes

- **Session** is temporary continuity for one host-identified agent workstream.
- **Project** is durable knowledge shared by agents working on the same project.
  The MCP host grants access with a stable, opaque `project-id`.
- **Global** is durable knowledge that is genuinely useful across projects.

## Install and connect

```shell
cargo install jiandu-mcp --locked
```

Configure an MCP host to launch it:

```json
{
  "mcpServers": {
    "jiandu": {
      "command": "jiandu",
      "args": [
        "--data-dir", "/absolute/path/to/shared-memory",
        "--session-id", "agent-session-1",
        "--project-id", "project-1"
      ]
    }
  }
}
```

The host may namespace the tool as `mcp__jiandu__memory`. A typical Project
recall call still uses the same tool arguments:

```json
{"action":"query","scope":"project","query":"release decision"}
```

Use a different `session-id` for each workstream. Agents that should share
Project memory use the same data directory and host-authorized `project-id`.
Query before writing, keep durable items concise, and never edit Jiandu's data
files directly.

## Host integration

Bamboo can use `jiandu-memory` directly and optimize recall while assembling
its dynamic context. Ranking, prompt placement, and token budgeting remain
Bamboo responsibilities. Other agents use `jiandu-mcp` as shared memory without
depending on Bamboo runtime types.

## Verify

```shell
cargo fmt --all -- --check
cargo metadata --locked --all-features --format-version 1
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked
```

Jiandu is licensed under the [MIT License](LICENSE).
