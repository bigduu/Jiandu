# Session Snapshot Contracts `v1alpha1`

Issue [#44](https://github.com/bigduu/Jiandu/issues/44) defines the first
agent-neutral contract slice for Session lineage. It contains no persistence,
MCP endpoint, Bamboo adapter, prompt policy, or committed-message ledger.

Two independent schema domains are checked in and generated from ordinary Rust
types:

- `jiandu.dev/branch-snapshot-event/v1alpha1` is strict host intent;
- `jiandu.dev/session-snapshot-manifest/v1alpha1` is the immutable source view
  that a future authoritative Jiandu resolver may mint from verified committed
  evidence.

## Host-declared event

```json
{
  "schema": "jiandu.dev/branch-snapshot-event/v1alpha1",
  "eventId": "evt_01K4SNAPSHOT",
  "sourceSessionId": "ses_01K4SOURCE",
  "sourceBranchId": "br_01K4SOURCE",
  "throughMessageId": "msg_000042",
  "targetSessionId": "ses_01K4TARGET",
  "targetBranchId": "br_01K4TARGET",
  "mode": "snapshot",
  "occurredAt": "2026-08-30T04:17:10Z"
}
```

All identities are opaque. Event, Session, branch, and message IDs never encode
a filesystem path, workspace, repository, prompt, or host database key. The
target Session must differ from the source Session so the target has a distinct
Session scope. Branch IDs are preserved as host lineage labels and are not
assumed to be globally unique across different Sessions.

The event is a declaration, not proof. In particular:

- `throughMessageId` does not prove that the message is committed;
- `occurredAt` does not establish message order;
- a syntactically valid source tuple does not prove that the message belongs to
  that Session and branch; and
- successful Rust/JSON validation authorizes no read or mutation.

A later host-only ingestion and resolver boundary must authenticate the caller,
verify durable committed-message ordering, verify source membership, and reject
drafts, partial streams, and uncommitted tool output before it emits a manifest.

## Resolved immutable manifest

```json
{
  "schema": "jiandu.dev/session-snapshot-manifest/v1alpha1",
  "event": {
    "schema": "jiandu.dev/branch-snapshot-event/v1alpha1",
    "eventId": "evt_01K4SNAPSHOT",
    "sourceSessionId": "ses_01K4SOURCE",
    "sourceBranchId": "br_01K4SOURCE",
    "throughMessageId": "msg_000042",
    "targetSessionId": "ses_01K4TARGET",
    "targetBranchId": "br_01K4TARGET",
    "mode": "snapshot",
    "occurredAt": "2026-08-30T04:17:10Z"
  },
  "sourceStoreRevision": 42,
  "visibleRecords": [
    {
      "memoryId": "mem_01K4ALPHA",
      "revision": 3,
      "etag": "sha256:1111111111111111"
    },
    {
      "memoryId": "mem_01K4OMEGA",
      "revision": 7,
      "etag": "sha256:9999999999999999"
    }
  ]
}
```

`visibleRecords` contains only source Session-scoped memories selected through
the verified message watermark. Principal and Project records remain visible
through their existing shared scopes and are not represented in this list.
Forgotten records are not visible anchors.

Every anchor binds one memory ID to its exact record revision and content-bound
ETag at `sourceStoreRevision`. Anchors are strictly ascending by case-sensitive
`memoryId` and IDs are unique. A record revision cannot exceed the source store
revision. An empty list is valid when no source Session record is eligible.

The manifest is immutable evidence, not historical content storage. A future
store implementation must retain or recover the exact anchored revision. If it
cannot load bytes that validate to the anchored revision and ETag, it fails
closed; it must never substitute a later source revision. The persistence,
recovery, copy-on-write, and tombstone protocols are deliberately separate
Issue #11 children.

## Compatibility

- Both schema identifiers and `mode` are closed. Unknown versions or modes fail
  instead of being coerced.
- Both objects reject unknown fields. Adding or changing an input or manifest
  field requires a new schema version.
- The generated JSON Schemas enforce structural bounds and full-anchor
  uniqueness. Rust `Validate` additionally enforces cross-field source/target
  Session separation, strict memory-ID order, unique memory IDs even when other
  anchor fields differ, and the record/store revision relationship.
- `eventId` is durable event identity. It is not authentication context, an
  idempotency receipt, or proof that a store commit occurred.
- Neither contract is an MCP tool in `v1alpha1`. A future host-only endpoint may
  reuse these types only after defining authentication, authorization,
  idempotency, recovery, and stable error behavior.

The checked schemas and fixtures live under
`crates/jiandu-core/schemas/v1alpha1` and
`crates/jiandu-core/fixtures/v1alpha1`.
