# Data Model and Filesystem

## Canonical record

Each memory is a UTF-8 Markdown document with a strict, machine-validated YAML frontmatter header. The header carries identity, policy, lifecycle, and provenance metadata; the Markdown body carries the human-inspectable content.

Illustrative `v1alpha1` record:

```markdown
---
schema: jiandu.dev/memory/v1alpha1
id: mem_01K3...
revision: 7
scope:
  kind: project
  project_id: prj_01K3...
type: decision
status: active
title: Use opaque project identity
tags:
  - architecture
  - identity
created_at: 2026-08-23T10:00:00Z
updated_at: 2026-08-23T10:05:00Z
provenance:
  agent_id: bamboo
  session_id: ses_01K3...
  branch_id: br_01K3...
  message_ids:
    - msg_41
    - msg_42
relations: []
---

Workspace paths are mutable metadata. Project identity is an opaque ID resolved by the host.
```

The serialized representation is deterministic: normalized line endings, stable field order, UTC timestamps, and a final newline. This allows validation, reviewable exports, and stable content hashing.

## Identity and revisions

- `id` is globally unique and opaque. It does not encode a path, principal, timestamp, or scope.
- `revision` is a positive, monotonically increasing integer per record.
- An ETag is derived from record ID, revision, and canonical content hash.
- Renaming a title, moving a workspace, or changing display metadata never changes the ID.
- A scope move is not an ordinary patch because it changes authorization. It requires a dedicated operation and audit record.

## Memory types

The first schema defines a small closed set:

| Type | Intended content |
| --- | --- |
| `preference` | A durable user or project preference. |
| `decision` | A decision, rationale, and meaningful constraints. |
| `project` | Stable project knowledge that is not merely a decision. |
| `fact` | A durable fact with provenance and an appropriate confidence policy. |
| `feedback` | Feedback about agent behavior or a prior result. |
| `reference` | A pointer or summary that helps retrieve an external source. |

Free-form type strings are not accepted in the canonical store. New types require a schema revision and conformance fixtures.

## Lifecycle status

| Status | Retrieval behavior |
| --- | --- |
| `active` | Eligible for normal recall. |
| `stale` | Retained but excluded unless a query requests stale records. |
| `superseded` | Replaced by a related newer record. |
| `contradicted` | Retained as provenance but not presented as current truth. |
| `archived` | Kept for history and explicit reads only. |

Forgetting is represented separately by a tombstone; a forgotten record is never returned through normal record APIs.

## Relations

Typed relations preserve history without embedding graph logic in record bodies:

- `supersedes`
- `supports`
- `contradicts`
- `derived_from`
- `related_to`

Relations use opaque memory IDs. The store validates syntax and authorization but does not require cross-scope targets to be visible to every client that can read the source record. Query results must not leak inaccessible target metadata.

## Scope model

### Principal

Memory belonging to an authenticated human or service principal across projects. Examples include stable interaction preferences. Principal identity comes from the host authentication boundary, never model input.

### Project

Memory shared by authorized participants in one logical project. A Project uses an opaque ID. Repository URL, workspace path, and display name are mutable aliases maintained by the host or an identity registry; none becomes the canonical ID.

### Session

Memory private to one conversation lineage. Session scope is useful for durable working context that should not automatically affect other sessions. A Session belongs to a Principal and may be associated with a Project.

### Instance global

Operator-managed knowledge available across principals or projects, such as service-level reference material. It is named `instance_global` to avoid implying universal or cross-tenant visibility. Agent clients normally receive read-only access.

Scope precedence is a host concern. Jiandu returns records and ranking metadata; it does not claim that Session content always overrides Project or Principal content.

## Provenance

Provenance is portable metadata, not a Bamboo database foreign key. It may include:

- host/agent identifier;
- opaque Session and branch identifiers;
- message IDs or an inclusive committed-message range;
- source URI and content digest;
- extraction method and optional extractor version;
- creation actor (`model`, `user`, `host`, `operator`, or `import`);
- confidence when the memory type permits it.

The service stores enough provenance for audit and correction while allowing a host to redact inaccessible message bodies.

## Session branch and deep-copy semantics

Conversation copying and memory copying are separate operations. Jiandu models lineage explicitly so a host can implement both without duplicating unrelated scopes.

A branch event contains:

```json
{
  "eventId": "evt_01...",
  "sourceSessionId": "ses_source",
  "sourceBranchId": "br_source",
  "throughMessageId": "msg_42",
  "targetSessionId": "ses_target",
  "targetBranchId": "br_target",
  "mode": "snapshot",
  "occurredAt": "2026-08-23T10:00:00Z"
}
```

For `snapshot` mode:

1. Session-scoped memories whose committed provenance is at or before `throughMessageId` become visible to the target lineage.
2. The target stores a source watermark and immutable lineage link instead of eagerly copying record files.
3. A later target-side mutation uses copy-on-write and receives a new memory ID with `derived_from` provenance.
4. Later source-side memories or edits do not appear automatically in the target.
5. Principal- and Project-scoped memories remain shared by their existing scope and are not duplicated.
6. Draft messages, partial streams, and tool output not committed by the host are excluded.
7. Retention or deletion in either lineage follows explicit policy; a branch link cannot resurrect a forgotten record.

This gives the user-visible behavior of “deep copy through this message” without multiplying canonical data or coupling Jiandu to a particular session database.

## Proposed filesystem layout

```text
<data-dir>/
├── store.json                     # store UUID and format version
├── LOCK                           # exclusive-writer lock metadata
├── records/
│   ├── principal/<shard>/<id>.md
│   ├── project/<shard>/<id>.md
│   ├── session/<shard>/<id>.md
│   └── instance_global/<shard>/<id>.md
├── lineages/<target-session-id>.json
├── tombstones/<shard>/<id>.json
├── transactions/<transaction-id>.json
├── receipts/<principal-shard>/<key-hash>.json
├── audit/<date>/<sequence>.jsonl
├── index/lexical.sqlite           # derived and rebuildable
├── quarantine/
└── backups/
```

Sharding uses a stable prefix of the opaque ID only to bound directory size. Clients never observe these paths. `store.json`, record frontmatter, transaction manifests, and tombstones carry explicit format versions.

## Atomic mutation protocol

A successful mutation follows one state machine:

1. authenticate the connection and authorize the requested operation;
2. validate the public schema and canonicalize the input;
3. find an existing idempotency receipt and replay or reject it;
4. load the visible record and check `expectedRevision` when applicable;
5. validate content, scope, status transitions, relations, and quotas;
6. write and fsync a transaction manifest;
7. write the new record or tombstone to a same-filesystem temporary file and fsync it;
8. atomically rename canonical files, append the audit event, and mark the transaction committed;
9. persist the idempotency receipt before acknowledging success;
10. update the derived index asynchronously from the committed store revision.

Startup recovery examines transaction state and either completes an unambiguous committed mutation or rolls back temporary artifacts. A response is never considered successful before its canonical mutation and idempotency receipt are durable.

## External edits

Human readability is not a promise that arbitrary editor writes are safe. The supported paths are MCP and the administrative CLI.

A later opt-in watcher may import external edits by parsing the full record, preserving ID, requiring a matching revision/ETag marker, and applying the same mutation protocol. Invalid or conflicting files move to `quarantine` with diagnostics; Jiandu never silently overwrites the authoritative version.

## Forget, purge, and retention

- `memory_forget` creates an authorized tombstone for one memory ID.
- Search and reads exclude tombstoned content immediately after commit.
- Audit entries retain metadata necessary to prove the operation without retaining the forgotten body.
- Hard purge timing is operator policy and must account for backups and legal retention requirements.
- Bulk purge, scope deletion, and store reset are administrative commands requiring explicit target resolution and dry-run output.
- Import cannot recreate an ID still protected by a tombstone unless an operator performs an explicit restore workflow.

## Derived data

Lexical indexes, embeddings, caches, ranking statistics, previews, and materialized lineage views are derived. Every derived format declares the store revision and index version from which it was built. `jiandu rebuild-index` can discard and recreate it without losing canonical records or policy state.
