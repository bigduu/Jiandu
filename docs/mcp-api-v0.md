# MCP API v0

## Status and versioning

This document defines the Jiandu `v1alpha1` public contract. Issue #7 ships the
transport-independent read slice in `jiandu-mcp`: `memory_search`,
`memory_get`, `memory_list`, and read-only resources. Mutation tools, a daemon,
HTTP, and two-client conformance remain later issues, so the complete document
is still an implementation target rather than a stable compatibility promise.

The read handler supports exactly MCP revision `2025-11-25`. This is
independent from `jiandu.dev/v1alpha1`; initialization advertises both and tests
prevent the values from being conflated.

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

Issue #6 implements the retrieval engine and host-facing Rust APIs. Issue #7
adds the read-only MCP adapter. The adapter first asks `jiandu-store` to mint an
`AuthorizedIndexQuery` for the exact request selectors; passing an arbitrary
vector of scopes to the index is not supported. Search validates the complete
private derived image but only authorized scope intersections can become hits.
The stable order is integer relevance descending and memory ID ascending.

The opaque cursor uses a host-held HMAC-SHA256 key and binds the complete fresh
authority fingerprint, normalized query/filters/limit, source store
ID/revision, and index checksum. Its embedded unkeyed checksum detects storage
or transport corruption only and is never an authorization mechanism. Key
rotation, authority change, or watermark change invalidates the cursor safely.
Exact weights, tokenization, format compatibility, and degraded behavior are
defined in [Lexical Index Format `v1alpha1`](index-format-v1alpha1.md).

### `memory_get`

Read one record by opaque memory ID. The output includes the complete record, provenance allowed by policy, revision, and ETag. An inaccessible record is not distinguishable from a nonexistent one unless operator policy explicitly permits that distinction.

Input:

```json
{ "memoryId": "mem_01..." }
```

### `memory_list`

List memories using structured filters without a free-text relevance query. This is intended for deterministic browsing, synchronization, export selection, and testing.

Input supports one or more authorized scopes, record type, status, tags, update watermark, limit, cursor, and a stable sort order.

For all three read tools, the checked `jiandu-core` request schema is the MCP
`inputSchema` without an adapter-owned identity wrapper. The handler strictly
decodes and runs the core validator before calling its backend. Every success
uses `ResultEnvelope` as authoritative `structuredContent`; every routed
domain/input failure uses `ErrorEnvelope`. The accompanying text is only a
short body/query/ID-free summary. Mixed authorized and inaccessible selector
sets fail as a whole instead of silently returning a narrower page.

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

## Host/operator validation and portable export

Issue #19 implements validation and export at the canonical-store Rust
boundary; it does not add model-visible MCP tools or transport request schemas.
Scoped calls use the host's `AuthorizedScopes` and explicit exact scopes.
Whole-store calls require independent, principal-bound
`memory:admin:validate_store` and `memory:admin:export_all` grants. Authenticated
principal/client identity remains trusted context and is never accepted from a
model argument.

Scoped inspection does not traverse another tenant's record owner or open or
decode another tenant's record/tombstone content. Before reading an authorized
candidate body it does one bounded global tombstone namespace pass that
collects only hashed storage keys. It checks strict names and filesystem
type/link/permission metadata without opening the file. This narrow,
non-decoding check is required by the `v1alpha3`/`v1alpha4`
cross-scope non-resurrection rule; it exposes no unauthorized tombstone
scope/metadata/count and cannot add one to a report or bundle.

Private receipt/result/audit/witness artifacts are not safely partitionable by
memory scope. Scoped calls therefore do not traverse that ledger; its exact
cross-artifact invariant is checked only by the separately authorized
whole-store validation/export path. This prevents a scoped caller from probing
foreign replay state.

Validation is bounded and side-effect free. Portable export contains canonical
public records and full portable provenance plus body-free protected tombstone
projections. It adds no internal path/storage-key fields, host/store credential
fields, queries, raw idempotency keys, private replay
results/receipts/audit/WAL, logical-erasure witness bytes, or forgotten body.
Authorized public record fields remain exact user content and are not a
path- or credential-redaction boundary. Exact formats, stable finding codes,
snapshot/read-only coordination, schemas, and compatibility rules are defined
in [Validation Report and Portable Export `v1alpha1`](portable-export-v1alpha1.md).

## Host/operator portable import and backup metadata

Issue #20 is implemented at the canonical-store Rust boundary and adds no
model-visible MCP request schema. A host passes strict portable-export bytes to
a deterministic zero-write planner with fresh exact-scope authority and
independent `memory:import:{scope-kind}` grants. Commit accepts only the same
fully committable plan, rechecks every scope, and uses one bounded
metadata-last v4 WAL for all records/tombstones plus body-free backup metadata,
result, receipt, and audit.

Idempotency identity is trusted principal plus a digest of the raw key;
fingerprinting binds exact canonical source and plan input. Authorization and
exact replay lookup precede target conflict checks. An acknowledgement-loss
retry returns the original result/backup metadata without a second audit;
conflicting reuse or missing scope authority writes nothing.

Backup metadata is readable only through the distinct host grant
`memory:admin:backup_metadata`. It is receipt/audit-bound recovery information,
not the portable bundle, a remote backup, a scheduled job, or a model tool.
The current slice has no import/backup MCP tool, CLI transport, upload, restore,
or retention executor. See
[Portable Import and Backup Metadata `v1alpha1`](portable-import-v1alpha1.md)
and [Canonical Store Format `v1alpha4`](store-format-v1alpha4.md).

## MCP resources

Jiandu may expose addressable, authorized records as resources:

```text
jiandu://memory/<memory-id>
jiandu://scope/principal/memories
jiandu://scope/project/<project-id>/memories
jiandu://scope/session/<session-id>/memories
jiandu://scope/instance_global/memories
```

The exact-ID and project/session shapes are resource templates. The principal
scope list is also advertised as the one concrete scope resource guaranteed by
every authenticated read capability; other selectors remain readable only
when current authority permits them. Scope resources return the first
deterministic `id_asc` page with limit 100, including an opaque next cursor in
the normal result envelope when more records exist. Clients continue paging
through `memory_list`; resource URIs never carry a cursor or free-text query.

Resource reads use the same authority and watermark rules as tools. Malformed,
absent, and inaccessible exact resources share one generic resource-not-found
response. The handler implements neither subscriptions nor list-change
notifications in this revision. Resource results use the `2025-11-25` wire
shape and therefore do not emit later `resultType`, `cacheScope`, or `ttlMs`
fields.

## Read-handler initialization metadata

The official `rmcp` `ServerHandler` exposes only tools, resources, and this
safe authenticated snapshot under `capabilities.experimental.jiandu`:

```json
{
  "apiVersion": "jiandu.dev/v1alpha1",
  "health": {
    "store": "ready",
    "index": "ready",
    "exactRead": true,
    "list": true,
    "search": true
  },
  "optionalCapabilities": ["resources"]
}
```

Store health is the closed set `ready | degraded`; index health is
`ready | degraded | missing`. Operation flags are derived rather than accepted
from the host. This metadata never includes paths, counts, watermarks, internal
reasons, credentials, bodies, or queries. It is supplied through the trusted
host/backend seam and does not invoke operator-only index diagnostics.

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
