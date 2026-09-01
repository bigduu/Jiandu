# Jiandu

Jiandu (简牍) is a small, filesystem-backed memory system for AI agents. It stores
session notes and durable knowledge with deterministic lexical recall and MCP.

Jiandu owns one authoritative data root, normally `~/.jiandu`. Bamboo native
memory and every MCP client must use that same Jiandu-owned root after cutover;
`~/.bamboo` must not remain a second authoritative durable-memory store.

## Components

- `jiandu-memory` provides persistence, maintenance, and lexical/BM25/CJK recall.
- `jiandu-mcp` exposes the store over stdio as one MCP tool named `memory`.
  Its `action` argument selects one of 19 memory operations.

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
        "--data-dir", "/absolute/path/to/.jiandu",
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

## Optional agent Skill

The canonical [`jiandu-memory` Skill](skills/jiandu-memory/SKILL.md) teaches a
host model the lowest-cost query/get/write, Dream, and deterministic maintenance
workflows. It is optional: Jiandu's live MCP description, schema, and structured
responses remain the correctness contract when the Skill is absent.

Copy or symlink the canonical `skills/jiandu-memory/` directory into one native
host location:

| Host | Personal | Repository |
| --- | --- | --- |
| Codex | `$HOME/.agents/skills/jiandu-memory/` | `$REPO_ROOT/.agents/skills/jiandu-memory/` |
| Claude Code | `~/.claude/skills/jiandu-memory/` | `$REPO_ROOT/.claude/skills/jiandu-memory/` |

The host discovers and activates the Skill under its own permission model;
Jiandu does not install or enable it. See the official
[Codex Skill locations](https://developers.openai.com/codex/skills#where-codex-loads-local-skills)
and [Claude Code Skill locations](https://code.claude.com/docs/en/slash-commands#where-skills-live).

## Dream orientation

Dream is one compact host-generated orientation snapshot for Global memory and
each authorized Project. It is derived prose, not canonical truth, and never
enters topic counts, lifecycle operations, or lexical recall.

First call `dream_read`. It returns a missing cold state or the current snapshot,
plus the canonical-memory `current_generation` and an advisory `stale` flag. A
host that owns a model, prompt, cadence, and budget may synthesize up to 12,000
characters of Markdown, then call `dream_publish` with the generation observed
before synthesis began:

```json
{
  "action": "dream_publish",
  "scope": "project",
  "source_generation": "<current_generation from dream_read>",
  "content": "## Current orientation\n\n- ..."
}
```

Jiandu publishes the body and metadata atomically and rejects the result if
canonical memory changed meanwhile. A missing generation marker after upgrading
an existing store requires one `rebuild` for that authorized scope. For factual
decisions, always use `query`/`get` and current tools; Dream is only a cheap first
orientation. Jiandu never selects or calls a model/provider and does not schedule
Dream generation.

## One-time import from Bamboo

Before Bamboo switches its native memory store to the Jiandu root, stop Bamboo
memory writes or take a static snapshot. The destination must be absent or an
empty directory and must not be inside the Bamboo source:

```shell
jiandu import-bamboo \
  --source-data-dir /absolute/path/to/.bamboo \
  --data-dir /absolute/path/to/.jiandu
```

The command reads the Bamboo source without modifying it. It imports only the
current canonical durable topics under `memory/v1/scopes/global/topics/*.md`
and `projects/<ProjectId>/memory/v1/topics/*.md`; Session notes, Dream/Ledger/
plan data, indexes, views, logs, locks, state, and migration administration are
not copied. Every topic is validated before a sibling staging store is built,
then rendered once through Jiandu's current typed schema so retired metadata such
as `embedding_ready` does not become new Jiandu state. Each imported scope is
rebuilt once, and the completed store is published only after the raw source and
current-schema staged identities match their validated expectations. The JSON
result reports scanned/imported/failed counts plus separate SHA-256 identities
for the raw source topic paths/bytes and imported current-schema paths/bytes.
The command never deletes the Bamboo source and refuses to overwrite an
initialized Jiandu root.

After a successful cutover, Bamboo should construct its native
`jiandu-memory::MemoryStore` with this Jiandu-owned root. Other agents should
launch `jiandu` over stdio with the same `--data-dir`; do not dual-write or keep
using the Bamboo source as a fallback memory root.

## Host integration

Bamboo can use `jiandu-memory` directly and optimize recall while assembling
its dynamic context. Ranking, prompt placement, and token budgeting remain
Bamboo responsibilities. Bamboo also owns Dream synthesis prompts, model choice,
cadence, failure policy, and prompt placement; Jiandu only persists the resulting
generation-stamped snapshot. Other agents use `jiandu-mcp` as shared memory
without depending on Bamboo runtime types.

## Verify

```shell
cargo fmt --all -- --check
cargo metadata --locked --all-features --format-version 1
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked
```

Jiandu is licensed under the [MIT License](LICENSE).
