# Jiandu

![Jiandu brand illustration: bamboo slips beside a stream, representing records and shared memory.](docs/assets/jiandu-nature-hero.png)

*Brand illustration, not a software screenshot. Bamboo slips represent records and shared memory.*

**Give your agents a shared memory that survives a chat.** Jiandu (简牍) stores
project decisions, reusable knowledge, and temporary session notes in one local
filesystem store. Recall uses deterministic lexical search, including BM25/CJK;
no embedding service or model provider is required.

Use Project memory to carry confirmed decisions between agents, Session notes
to resume a workstream, and Global memory for facts useful across projects.
Bamboo can embed the memory crate; other runtimes connect through one MCP tool.
Your host decides what to remember and how to place recalled facts in context.

## Watch a memory lookup from source

![Jiandu searches a demo project for a release checklist and opens the saved memory.](docs/demos/memory-console.gif)

[Static image](docs/demos/memory-console.png) · [Reproduce the MCP write and browser lookup](docs/demos/reproduction.md)

Real Linux Chromium capture of the **source-only read-only console**, which is
not included in published `v0.2.0`. The clearly labelled demo memory was written
through actual stdio MCP before recording. The browser searches and opens it;
it does not write memory or run a model. No personal memory store was used.

## Choose a version

| Path | Available workflow |
| --- | --- |
| [Published v0.2.0](https://github.com/bigduu/Jiandu/releases/tag/v0.2.0) | Shared memory over stdio MCP, fixed per-process Project/Session defaults, Dream snapshots, and one-time Bamboo import. |
| Current source | Also includes the read-only browser console and per-call host identity metadata described below. These additions are not in v0.2.0. |

The source manifest still says `0.2.0`. Install that release for the published
contract; build this checkout for the console and per-call context. Both require
Rust 1.95 or newer to build. [Audit evidence](docs/readme-audit.md).

Jiandu owns one authoritative data root, normally `~/.jiandu`. Bamboo native
memory and every MCP client must use that same Jiandu-owned root after cutover;
`~/.bamboo` must not remain a second authoritative durable-memory store.

## Components

- `jiandu-memory` provides persistence, maintenance, and lexical/BM25/CJK recall.
- `jiandu-mcp` exposes the store over stdio as one MCP tool named `memory`.
  Its `action` argument selects one of 19 memory operations.
  The same binary also serves the local read-only browser console.

## Memory scopes

- **Session** is temporary continuity for one host-identified agent workstream.
- **Project** is durable knowledge shared by agents working on the same project.
  The MCP host grants each call access with a stable, opaque `project_id`.
- **Global** is durable knowledge that is genuinely useful across projects.

## Install and connect

```shell
cargo install jiandu-mcp --version 0.2.0 --locked
```

For the source-only features, build this checkout instead:

```shell
cargo build --release --locked -p jiandu-mcp --bin jiandu
# Use the absolute path to target/release/jiandu in your MCP configuration.
```

Configure an MCP host to launch it:

```json
{
  "mcpServers": {
    "jiandu": {
      "command": "jiandu",
      "args": [
        "--data-dir", "/absolute/path/to/.jiandu",
        "--project-id", "demo-project",
        "--session-id", "demo-session"
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

For a first trial, use a new dedicated data directory and the example identities
above. Ask the host to query Project memory, write one confirmed non-sensitive
fact, then query it from a new connection with the same Project identity. For
example, the `memory` tool accepts:

```json
{"action":"query","scope":"project","query":"demo preview port"}
{"action":"write","scope":"project","type":"reference","title":"Demo preview port","content":"The fictional demo project uses port 4173 for its local preview."}
{"action":"query","scope":"project","query":"demo preview port"}
```

These are separate tool calls with deliberately fictional demo data. Reuse the
Project identity when sharing memory; choose a fresh Session identity for each
independent workstream. The fixed flags work with v0.2.0 and current source.

### Per-call identity (current source)

Only `--data-dir` is required to connect. Global memory needs no Session or
Project identity. One connection can serve multiple projects and workstreams:
the host supplies identity separately for each `tools/call` request, outside
model-generated tool arguments:

```json
{
  "name": "memory",
  "arguments": {"action":"query","scope":"project","query":"release decision"},
  "_meta": {
    "io.github.bigduu.jiandu/context": {
      "project_id": "project-1",
      "session_id": "agent-session-1"
    }
  }
}
```

This is the `params` object of a `tools/call` request. The metadata key is a
Jiandu extension that the host must explicitly support and populate; MCP does
not automatically provide current Project or Session identity. Project actions
require a host-authorized `project_id`; `session_*` actions require a host
`session_id`. Either field can be omitted when the operation does not need it.
`project_key` in tool arguments can only assert the current host Project and
cannot grant access. Use a distinct Session id for each independent workstream.

For hosts that dedicate one process to one workstream, `--session-id <ID>` and
`--project-id <ID>` remain optional startup defaults. A per-call context replaces
both defaults completely; `{}` explicitly clears them for that call. Context
is never retained between calls, and malformed context is rejected instead of
falling back to defaults. Clients without this metadata support can use Global
memory or those fixed defaults.

Rust hosts can construct `MemoryExecutionContext::default()` and use
`MemoryServer::execute_with_context` to provide a trusted context per invocation.
See the [per-call identity design](docs/design/mcp-call-context.md) for resolution,
authority, and compatibility details. Agents sharing Project memory use the
same data directory and Project identity. Query before writing, keep durable
items concise, and never edit Jiandu's data files directly.

## Local console (current source)

```shell
jiandu ui
# Or select the same canonical root used by your agents:
jiandu ui --data-dir /absolute/path/to/.jiandu --port 9123
```

Open the printed `http://127.0.0.1:<port>/` URL. The default root is `~/.jiandu`
and port `0` asks the OS for an available port. No Session or Project identity
is required. Press Ctrl-C to stop; the command serves only IPv4 loopback.

Browse Global knowledge, switch first-class Projects, or select persisted
Sessions. Search and page memories, filter their status, and read their full
content, tags, timestamps and provenance. Durable scopes also show lexical-index
availability and Dream snapshot freshness/content. Sessions show temporary
notes and are not assigned to a Project by inference.

The console uses the existing MemoryStore and bundled assets, without a
separate frontend build or agent runtime. It does not write records, access
signals, indexes, Session state or Dream snapshots. A missing/invalid index
still permits blank-query browsing; rebuild it through the existing authorized
memory tool when needed. This version has no edit, rebuild or Dream-generation
controls, and lists only first-class Projects rather than legacy directories.
See the [console design](docs/design/local-console.md).

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
