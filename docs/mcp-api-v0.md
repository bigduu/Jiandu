# MCP API v0

## Status and versioning

This document defines the proposed Jiandu `v1alpha1` public contract. It is an implementation target, not a compatibility promise. Tool names and schemas become stable only after conformance tests exercise at least two independent clients.

The Jiandu API version is independent from the MCP protocol revision negotiated by the client and server. The service advertises both.

## Contract principles

- Public operations exchange structured memory records, not rendered prompt text.
- Authenticated identity is transport context, never model-controlled input.
- Read-only, mutating, and destructive operations are separate tools so hosts can grant them independently.
- Every mutation is idempotent and revision-aware.
- Scope selection uses opaque IDs supplied or resolved by the host.
- Responses include stable domain error codes and correlation metadata.
- Text content may accompany structured output for human-facing clients, but structured content is authoritative.

## Authentication context

Before a tool is invoked, the MCP session establishes:

```json
{
  "principalId": "prn_01...",
  "clientId": "cli_01...",
  "grants": ["memory:read", "memory:write:project"]
}
```

Neither `principalId` nor `clientId` appears as a public tool argument. This prevents a model from impersonating another principal. A host may pass opaque Project and Session IDs only within the scopes authorized for that identity.

## Common types

### Scope selector

```json
{
  "kind": "project",
  "projectId": "prj_01..."
}
```

Supported scope kinds in `v1alpha1`:

- `principal`
- `project`
- `session`
- `instance_global` (operator-controlled; normally read-only to agent clients)

### Memory summary

```json
{
  "id": "mem_01...",
  "revision": 7,
  "etag": "W/\"mem_01...:7\"",
  "scope": { "kind": "project", "projectId": "prj_01..." },
  "type": "decision",
  "status": "active",
  "title": "Use opaque project identity",
  "summary": "Workspace paths are metadata, not Project identity.",
  "tags": ["identity", "architecture"],
  "updatedAt": "2026-08-23T10:00:00Z",
  "score": 0.91
}
```

The `summary` field is optional and reflects canonical record metadata; the
contract layer does not synthesize a body excerpt when it is absent. The
`score` field is required in every ranked query hit and is forbidden in
deterministic list summaries.

### Result envelope

```json
{
  "apiVersion": "jiandu.dev/v1alpha1",
  "correlationId": "req_01...",
  "storeRevision": 42,
  "result": {}
}
```

## Public tools

### `memory_search`

Find ranked memories visible to the authenticated client.

Input:

```json
{
  "query": "project identity decision",
  "scopes": [
    { "kind": "project", "projectId": "prj_01..." },
    { "kind": "principal" }
  ],
  "types": ["decision", "preference"],
  "statuses": ["active"],
  "limit": 10,
  "cursor": null
}
```

Output contains memory summaries, deterministic pagination metadata, and query diagnostics that disclose no inaccessible record.

### `memory_get`

Read one record by opaque memory ID. The output includes the complete record, provenance allowed by policy, revision, and ETag. An inaccessible record is not distinguishable from a nonexistent one unless operator policy explicitly permits that distinction.

Input:

```json
{ "memoryId": "mem_01..." }
```

### `memory_list`

List memories using structured filters without a free-text relevance query. This is intended for deterministic browsing, synchronization, export selection, and testing.

Input supports one or more authorized scopes, record type, status, tags, update watermark, limit, cursor, and a stable sort order.

### `memory_remember`

Create a memory record.

Input:

```json
{
  "scope": { "kind": "project", "projectId": "prj_01..." },
  "type": "decision",
  "title": "Use opaque project identity",
  "body": "Workspace paths remain mutable metadata and never become identity.",
  "tags": ["identity", "architecture"],
  "provenance": {
    "agentId": "bamboo",
    "sessionId": "ses_01...",
    "messageIds": ["msg_41", "msg_42"]
  },
  "idempotencyKey": "evt_01..."
}
```

The service validates current exact-scope authority and content policy, then
returns the created record. Repeating an identical request with the same
authenticated principal, operation, idempotency key, and canonical input
returns the original result with `idempotentReplay: true`. Reusing the key for
different canonical input returns `IDEMPOTENCY_CONFLICT` before a persistent
write. Generated IDs, timestamps, and correlation metadata do not change retry
identity.

### `memory_update`

Apply an explicit patch to one record.

Input includes `memoryId`, `expectedRevision`, a field-level patch, a reason, and `idempotencyKey`. The first version supports replacement of title/body, tag add/remove, status transition, and relation add/remove. Scope moves require a later, dedicated operation.

The patch has this shape; omitted fields are unchanged, and an empty patch is invalid:

```json
{
  "memoryId": "mem_01...",
  "expectedRevision": 7,
  "patch": {
    "title": "Updated title",
    "body": "Updated Markdown body.",
    "status": "stale",
    "tags": { "add": ["reviewed"], "remove": ["draft"] },
    "relations": {
      "add": [{ "kind": "supersedes", "targetMemoryId": "mem_02..." }],
      "remove": []
    }
  },
  "reason": "Source changed",
  "idempotencyKey": "evt_01..."
}
```

The same tag or relation cannot appear in both `add` and `remove`; duplicate
values and self-relations are invalid. `expectedRevision` is a positive integer.

A stale revision returns `REVISION_CONFLICT` and current revision metadata without exposing an inaccessible record body.

Receipt lookup occurs before record lookup and CAS, so retrying the exact
successful update returns its original record/revision/ETag/store revision even
though the supplied `expectedRevision` is now stale. Present authorization is
still required on every retry. Exact replay lasts only while the private result
artifact is retained. Ordinary forget keeps historical mutation replay
artifacts; a future explicit, versioned hard-purge/receipt-GC ledger transition
terminates replay for retired keys. External backups are outside a local
deletion guarantee.

### `memory_forget`

Forget exactly one record.

Input:

```json
{
  "memoryId": "mem_01...",
  "expectedRevision": 7,
  "reason": "User requested deletion",
  "idempotencyKey": "delete_01..."
}
```

The normal operation requires a destructive `memory:forget:{scope-kind}` grant
that is independent from `memory:write:*`. It authenticates and resolves exact
scope before receipt lookup, fingerprints scope/ID/revision/reason, and looks up
an exact retry before record lookup or CAS. A committed retry returns the
original body-free ID/revision/ETag/`forgottenAt` result without another audit;
conflicting key reuse fails before a write. The metadata-last transaction
publishes a protected tombstone before renaming the held record to a private
witness and descriptor-truncating/syncing it to zero bytes. The zero-length
witness remains protected and is not a secure-erase claim. Exact get checks
global tombstone presence before any candidate record open; list filters global
tombstone storage keys before body decode. Thus get/list exclude the ID even if
an ambient actor injects the same name in another authorized scope, and
create/update cannot implicitly resurrect it.

Operator restore/hard purge is a separate administrative lifecycle, never a
model tool. The current store can produce a non-executing, bounded, explicit
target dry-run plan behind separate admin grants; bulk deletion remains absent
from the public MCP surface.

## MCP resources

Jiandu may expose addressable, authorized records as resources:

```text
jiandu://memory/<memory-id>
jiandu://scope/project/<project-id>/memories
jiandu://scope/session/<session-id>/memories
```

Resource reads follow the same authorization and revision rules as tools. Sensitive free-text searches are not encoded in resource URIs. Resource subscriptions are deferred until their consistency and privacy behavior is specified.

## Future event operations

The following are intentionally outside the first public tool set and will be designed after the storage contract is proven:

- submit a committed conversation turn;
- declare Session branch creation;
- declare Session deletion or archival;
- request extraction or consolidation;
- report host-side memory usage feedback.

These operations will be host-to-service integration endpoints, not automatically model-visible tools.

## Domain errors

Errors include a stable code, safe message, correlation ID, retryability, and optional structured details.

Initial codes:

| Code | Meaning |
| --- | --- |
| `INVALID_ARGUMENT` | Input violates the public schema or domain invariant. |
| `UNAUTHENTICATED` | The client identity is missing or invalid. |
| `FORBIDDEN` | The authenticated client lacks the required grant. |
| `NOT_FOUND` | No visible record exists for the opaque ID. |
| `REVISION_CONFLICT` | `expectedRevision` is stale. |
| `IDEMPOTENCY_CONFLICT` | A key was reused for different input. |
| `STORE_UNAVAILABLE` | Canonical storage cannot safely serve the operation. |
| `INDEX_DEGRADED` | Ranked search is temporarily unavailable or incomplete. |
| `RATE_LIMITED` | Client or principal quota is exceeded. |
| `INTERNAL` | An unexpected failure occurred; details remain in secret-safe logs. |

The `retryable` bit is derived from the code rather than chosen by an adapter.
`REVISION_CONFLICT`, `STORE_UNAVAILABLE`, `INDEX_DEGRADED`, `RATE_LIMITED`, and
`INTERNAL` are retryable after the caller performs the action implied by the
code (for example, re-read before retrying a revision conflict). Other initial
codes are not retryable without changing identity, authorization, or input.

## Committed validation bounds

The Rust `Validate` implementation is authoritative for cross-field and UTF-8
byte invariants. Generated JSON Schemas carry all directly expressible bounds.

| Value | `v1alpha1` bound |
| --- | --- |
| Prefixed opaque IDs | 4 or 5–128 ASCII bytes including the declared prefix; suffix uses letters, digits, `_`, or `-` |
| Agent IDs and idempotency keys | 1–128 ASCII bytes using letters, digits, `.`, `_`, `:`, or `-` |
| Opaque page cursor | 1–1,024 base64url-shaped ASCII bytes; clients must not parse it |
| ETag | 1–256 visible ASCII bytes |
| Title | 1–200 Unicode scalar values, trimmed, no control characters |
| Summary | 1–1,000 Unicode scalar values, trimmed, no control characters |
| Markdown body | 1–65,536 UTF-8 bytes, non-whitespace, no NUL |
| Mutation reason | 1–1,000 Unicode scalar values, trimmed, no control characters |
| Search query | 1–4,096 Unicode scalar values, trimmed, no control characters |
| Tags | At most 32 unique lower-case ASCII tags; each tag is 1–64 bytes |
| Relations | At most 128 unique typed targets; no self-relation |
| Provenance message IDs | At most 128 unique IDs; list and range are mutually exclusive |
| Scope selectors | 1–16 unique authorized selectors |
| Type, status, or tag filters | At most 32 unique values per filter |
| Page limit | 1–100 |
| Confidence and search score | Finite number from 0 through 1 inclusive |
| Timestamp | Canonical RFC 3339 UTC with `Z`, optional 1–9 fractional digits |
| Content digest | `[a-z0-9_]+:[A-Fa-f0-9]+`, at most 256 ASCII bytes |
| Source URI | Absolute scheme plus non-empty visible ASCII remainder, at most 2,048 bytes |

Because standard JSON Schema `maxLength` counts characters rather than UTF-8
bytes, body schemas also carry the extension `x-jiandu-maxUtf8Bytes: 65536`;
the Rust validator enforces the byte limit authoritatively.

`active` may transition to `stale`, `superseded`, `contradicted`, or `archived`.
`stale` may return to `active` or move to any of the latter three states.
`superseded` may only move to `archived`; `contradicted` may move to `active`,
`stale`, or `archived`; `archived` is terminal. A no-op transition is valid.

## JSON and frontmatter naming

Public API JSON uses `camelCase`. The canonical Markdown header is a separate,
strict snake_case DTO (`project_id`, `created_at`, `target_memory_id`, and so
on). This makes the on-disk representation explicit without leaking a storage
format into API identity or command types. ETag and body are API/document
metadata outside the YAML header: ETag is derived by storage, while body is the
Markdown after the closing delimiter.

## Compatibility rules

- New optional response fields may be added within `v1alpha1`.
- Input objects, scope variants, provenance objects, patches, and canonical
  frontmatter reject unknown fields. Clients must not send speculative fields.
- Response records, result envelopes, diagnostics, and error payloads may ignore
  newly added optional fields, but their nested closed types remain strict.
  Ranked and unranked summary projections are closed so the ranking-only `score`
  field cannot cross the search/list boundary.
- Existing fields cannot change meaning without an API-version change.
- All current enums are closed: API/schema version, scope kind, memory type,
  lifecycle status, relation kind, creation actor, extraction method, list sort,
  validation code, and domain error code. Unknown values fail explicitly; adding
  or renaming a value requires a contract revision rather than coercion.
- Cursors are opaque and short-lived; clients must not parse them.
- Tool schemas are generated from the Rust domain definitions; conformance
  fixtures are decoded and validated against those types and generated schemas.
- Store migrations cannot silently widen a client's authorization or scope.

The checked schemas live under `crates/jiandu-core/schemas/v1alpha1`, while
canonical valid and invalid JSON/Markdown fixtures live under
`crates/jiandu-core/fixtures/v1alpha1`. CI regenerates schemas in memory and
fails on any semantic difference from the checked files.
