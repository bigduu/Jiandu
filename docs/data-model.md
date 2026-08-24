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
  created_by: host
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

The serialized representation is deterministic: LF-only line endings, stable field order, UTC timestamps, and a required file-terminator LF. Parsing removes exactly that one terminator; it never trims the API body. Thus an API body that ends in LF produces a file ending in two LFs and round-trips with its body LF intact. Markdown horizontal-rule lines such as `---` remain ordinary body content after the first frontmatter terminator. BOMs, CRLF, missing terminators, unknown frontmatter fields, and semantically valid but non-canonical YAML encodings are rejected. This allows validation, reviewable exports, and stable content hashing without silently rewriting user content.

## Identity and revisions

- `id` is globally unique and opaque. It does not encode a path, principal, timestamp, or scope.
- `revision` is a positive, monotonically increasing integer per record.
- An ETag is `sha256:<lowercase-hex>` over the complete canonical file bytes. Because those bytes include record ID and revision, either changes the ETag. `etag` is never accepted as a frontmatter field.
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
├── store.json                     # store UUID, format, storeRevision, auditSequence
├── LOCK                           # exclusive-writer lock metadata
├── records/
│   ├── principal/<principal-key>/<shard>/<memory-key>.md
│   ├── project/<project-key>/<shard>/<memory-key>.md
│   ├── session/<session-key>/<shard>/<memory-key>.md
│   └── instance_global/<shard>/<memory-key>.md
├── lineages/<target-session-key>.json
├── tombstones/<shard>/<memory-key>.json
├── transactions/<transaction-id>.json
├── receipts/
│   ├── quarantine/quarantine-<transaction-id>.json
│   └── idempotency/
│       ├── metadata/<principal-digest>/<operation>/<shard>/<receipt-id>.json
│       └── results/<shard>/<receipt-id>.json
├── audit/
│   ├── genesis.json
│   └── mutations/<20-digit-audit-sequence>.json
├── index/lexical.sqlite           # derived and rebuildable
├── quarantine/
└── backups/
```

The authoritative owner segment is part of the private layout for Principal, Project, and Session records. Every `<owner-key>` and `<memory-key>` is a domain-separated lowercase SHA-256 storage key of its opaque ID. This keeps distinct case-sensitive IDs separate on case-insensitive filesystems and bounds component length; the original IDs remain authoritative in validated frontmatter. The owner segment lets an authorized list traverse only granted owners instead of parsing other tenants' records. It is never inferred from a workspace path.

Sharding uses the first two characters of the memory storage key only to bound directory size. Clients never observe these paths. Exact reads receive typed memory IDs plus host-authorized scopes, recompute the private key, and still validate the original ID/scope in frontmatter. Absent and inaccessible IDs share the same public not-found result. `store.json`, record frontmatter, transaction manifests, and tombstones carry explicit format versions.

## Atomic idempotent mutation protocol

The implemented #4/#17 create/update core follows one state machine:

1. the host authenticates the trusted request context, resolves an
   operation-specific `AuthorizedMutation` for one exact scope, and validates
   the public command;
2. Jiandu fingerprints authoritative scope plus canonical caller input and
   looks up the principal/operation/key-digest receipt before generated values,
   create existence checks, update `NotFound`, or CAS;
3. Jiandu durably publishes a strict, body/path-free versioned transaction
   manifest binding the target record, metadata, private result, body-free
   receipt, and body-free audit event by digest;
4. it stages and fsyncs all five target files on their final filesystems;
5. it atomically publishes record, private result, receipt, and audit in order,
   then publishes `store.json` last as the commit watermark; and
6. it removes and syncs the manifest before acknowledging the original
   create/update result.

Startup recovery compares both record state (base/target/ambiguous) and store
metadata state (base/target/ambiguous). It rolls back only base/base, completes
target/base or target/target only after reconstructing/verifying every artifact
against the strict manifest digests, and fails closed for impossible or unknown
combinations. Base/base with any published result/receipt/audit is impossible.
A post-boundary error poisons the current handle until this startup path runs.

An identical retry with fresh exact-scope authority returns the original
record/revision/ETag/store revision and does not rewrite state or advance
`auditSequence`. Conflicting key reuse fails before any write. The complete
result is retained only in a bounded, non-enumerable private artifact; metadata,
WAL, audit, and diagnostics contain no body, update reason, raw key/query,
credential, or path. Full byte/state/migration/failpoint details are committed
in [the `v1alpha2` store-format document](store-format-v1alpha2.md).

Forget/tombstone retention and hard-purge/receipt-GC lifecycle are Issue #18.
Derived index maintenance remains asynchronous and non-authoritative in its own
milestone.

## External edits

Human readability is not a promise that arbitrary editor writes are safe. The supported paths are MCP and the administrative CLI.

A later opt-in watcher may import external edits by parsing the full record, preserving ID, requiring a matching revision/ETag marker, and applying the same mutation protocol. Invalid or conflicting imported files may move to `quarantine` through an explicit operator action with diagnostics; Jiandu never silently overwrites the authoritative version. Ordinary exact/list reads are side-effect free: they report invalid canonical data but never rewrite or quarantine it.

## Forget, purge, and retention

- `memory_forget` creates an authorized tombstone for one memory ID.
- Search and reads exclude tombstoned content immediately after commit.
- Audit entries retain metadata necessary to prove the operation without retaining the forgotten body.
- Hard purge timing is operator policy and must account for backups and legal retention requirements.
- Bulk purge, scope deletion, and store reset are administrative commands requiring explicit target resolution and dry-run output.
- Import cannot recreate an ID still protected by a tombstone unless an operator performs an explicit restore workflow.

## Derived data

Lexical indexes, embeddings, caches, ranking statistics, previews, and materialized lineage views are derived. Every derived format declares the store revision and index version from which it was built. `jiandu rebuild-index` can discard and recreate it without losing canonical records or policy state.
