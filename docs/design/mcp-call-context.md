# Per-call identity for Jiandu MCP

## Problem and acceptance slice

Jiandu serves Global, Project, and Session memory through one `memory` tool.
The current stdio process requires a Session id at startup and binds Project
authority to one optional startup Project id. This prevents Global-only clients
from connecting without inventing a Session and requires a new process when
one agent moves between projects.

This 4–8 hour slice changes only the Jiandu MCP adapter, its connection
documentation, and its tests. A single process started with only `--data-dir`
must serve Global memory and accept different host-authorized Project and
Session identities on successive or concurrent calls. Scope isolation, mutation
ownership, and existing fixed-context clients must remain correct.

The memory crate, canonical layout, indexes, import, Dream policy, Bamboo native
integration, consuming host configuration, and publication are outside this
slice. No second store, discovery service, authorization registry, or new
transport is required.

## Connection and invocation

The connection specifies the canonical Jiandu data directory:

```shell
jiandu --data-dir /absolute/path/to/.jiandu
```

`--session-id` and `--project-id` remain optional defaults for existing hosts
that intentionally dedicate one process to one workstream. Neither is required
to start the server. Jiandu never generates a shared `default` identity or
derives a Project id from a filesystem path.

For a host that handles several workstreams, identity belongs to each
`tools/call` request. The host supplies this Jiandu-specific metadata extension
outside model-generated tool arguments:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "tools/call",
  "params": {
    "name": "memory",
    "arguments": {
      "action": "query",
      "scope": "project",
      "query": "release decision"
    },
    "_meta": {
      "io.github.bigduu.jiandu/context": {
        "project_id": "project-a",
        "session_id": "workstream-1"
      }
    }
  }
}
```

MCP reserves `_meta` for additional client/server metadata and recommends
vendor prefixes in reverse DNS form. This key is a Jiandu extension, not an
automatically supplied MCP or Codex field. Hosts must authorize the target
Project and inject the context before sending the request. A client that does
not support this extension can still use Global memory or the optional fixed
startup defaults.

Reference: [MCP 2025-11-25 metadata contract](https://modelcontextprotocol.io/specification/2025-11-25/basic/index#meta).

## Resolution and authority

Each call resolves one immutable `MemoryExecutionContext`:

1. If the Jiandu metadata key is absent, use the optional startup defaults.
2. If the key is present, validate its object and use it as the entire context
   for this call. Omitted or null identities are absent; they do not inherit
   startup defaults. An empty object explicitly selects no identities.
3. Reject malformed objects, unknown context fields, invalid Session ids, and
   invalid opaque Project ids before executing an action.
4. Ignore unrelated protocol metadata.

Replacing the whole context prevents a Project-only call from accidentally
attributing its write to a startup Session. Request contexts are never saved
back to the server, so one call cannot grant authority to a later call.

| Operation | Required context | Missing identity |
| --- | --- | --- |
| Global durable actions | None | Operate normally; Session provenance is optional |
| Project durable actions | Host-authorized `project_id` | Reject only this operation |
| `session_*` actions | Host-provided `session_id` | Reject only this operation |
| Id-based durable actions | Optional current Project plus Global | Search only the current authorized Project and Global |

The existing `project_key` argument remains an assertion against the current
host context. It cannot select or grant an unauthorized Project. Model-supplied
`session_id`, `project_id`, or `_meta` inside `arguments` cannot become trusted
context. They are rejected as unknown tool fields. The host may authorize a
different Project on its next request without changing the connection.

The stdio host is the existing trust boundary. Metadata conveys that host's
decision; it is not a credential and does not protect against a malicious host
that already controls the Jiandu process and its data directory.

## Execution and compatibility

Resolve and clone context before dispatch. Read-only calls and owned mutation
tasks share the original server's in-flight mutation barrier. A cancelled call
must not abort an accepted mutation or discard its captured identity. Context
switching adds no mutable current-project state and changes no store locks.

Rust embedders can use `MemoryExecutionContext::default()` for an unbound
server and `execute_with_context` for a trusted per-call context. Existing
`MemoryExecutionContext::new(session_id)`, `with_project_id`, and `execute`
continue to support fixed-context callers. The `session_id` getter now returns
`Option<&str>` because durable calls can legitimately have no Session.

## Validation

- Launch the real stdio binary with only `--data-dir`, initialize it, and run
  Global write/query/get without a Session or Project identity.
- Use one connection for two Projects and two Sessions; verify lexical recall,
  id lookup, topic reads, and write provenance remain isolated.
- Interleave calls with different contexts and verify no identity is retained.
- Verify a Project-only context needs no Session, and a Session-only context
  needs no Project.
- Verify missing/invalid context and model-argument spoofing cannot grant
  access, and an empty context does not inherit startup authority.
- Preserve the existing fixed-context, mutation cancellation, Dream, import,
  and schema suites; run locked metadata, formatting, strict Clippy, and all
  workspace tests.

## Local acceptance result

Verified on 2026-10-02 in the isolated implementation worktree:

- `cargo fmt --all -- --check` passed.
- `cargo metadata --locked --all-features --format-version 1` passed.
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`
  passed.
- `cargo test --workspace --all-targets --all-features --locked` passed:
  147 tests passed, with one existing manual benchmark ignored. This includes
  five new real-stdio context tests and the existing cancellation, schema,
  Dream, import, and memory-isolation suites.

The running host and installed MCP executable have not been replaced by this
local source change. Dynamic context support requires host adoption of the
extension above.
