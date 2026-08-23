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

The `score` field is present only in ranked query results and is meaningful only within that response.

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

The service validates scope authority and content policy, then returns the created record. Repeating an identical request with the same idempotency key returns the original result. Reusing the key for different input returns `IDEMPOTENCY_CONFLICT`.

### `memory_update`

Apply an explicit patch to one record.

Input includes `memoryId`, `expectedRevision`, a field-level patch, a reason, and `idempotencyKey`. The first version supports replacement of title/body, tag add/remove, status transition, and relation add/remove. Scope moves require a later, dedicated operation.

A stale revision returns `REVISION_CONFLICT` and current revision metadata without exposing an inaccessible record body.

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

The normal operation creates an auditable tombstone and makes the record unavailable to retrieval. Operator-configured hard purge is a separate administrative lifecycle, never a model tool. Bulk deletion is intentionally absent from the public MCP surface.

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

## Compatibility rules

- New optional response fields may be added within `v1alpha1`.
- Existing fields cannot change meaning without an API-version change.
- Unknown enum values must be surfaced safely rather than coerced.
- Cursors are opaque and short-lived; clients must not parse them.
- Tool schemas and conformance fixtures are generated from the same Rust domain definitions.
- Store migrations cannot silently widen a client's authorization or scope.
