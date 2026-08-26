# Portable Import and Backup Metadata `v1alpha1`

Jiandu's host-facing portable-import contracts are independent from the public
memory API, MCP transport, portable-export format, and canonical store format:

- dry-run plan: `jiandu.import-plan/v1alpha1`;
- committed result: `jiandu.import-result/v1alpha1`;
- recovery-safe backup metadata: `jiandu.backup-metadata/v1alpha1`.

The authoritative ordinary Rust definitions, generated JSON Schemas, and
canonical fixtures live under `crates/jiandu-store/src/portable_import.rs`,
`crates/jiandu-store/schemas/import/v1alpha1/`, and
`crates/jiandu-store/fixtures/import/v1alpha1/`. Schema drift, unknown fields,
noncanonical JSON, invalid digests, impossible watermarks, and fixture drift are
test failures.

These are host/operator store APIs. Issue #20 does not add model-visible MCP
arguments, CLI commands, Bamboo mapping, remote storage, upload scheduling, or
retention execution.

## Input and authorization

`CanonicalStore::plan_import` and `CanonicalStore::import_portable` accept the
exact canonical bytes of `jiandu.portable-export/v1alpha1`. Decode and full
portable-domain validation finish before persistent write. This includes
record/frontmatter/ETag round-trip, opaque identity and scope validation,
global ID uniqueness, tombstone invariants, ordering, watermarks, and the
bundle digest.

The trusted request principal must match `AuthorizedScopes`. Each bundle scope
is then resolved against two independent facts:

1. the host-provided exact-scope capability; and
2. an operation-specific `memory:import:{principal|project|session|instance_global}`
   grant.

Planning records the fresh decision for every scope. Commit requires every
scope to remain authorized; authorization is checked before private receipt
lookup. Import never accepts a destination scope separately, widens authority,
moves a record, derives Project identity from a path, or changes an opaque ID.

`CanonicalStore::replay_portable_import` is the narrower read-only
acknowledgement-recovery seam. With the same trusted principal, complete scope
authority, canonical bundle, expected plan digest, and idempotency key, it
returns an existing verified `ImportCommit` or `None` without fresh target
planning or WAL entry. Conflicting key reuse remains an idempotency conflict;
absence never authorizes a new commit. Host adapters use this only to
distinguish a lost committed acknowledgement from unrelated later target
state before deciding whether a fresh import is still admissible.

## Deterministic zero-write dry run

The dry run returns source and target identities/watermarks, sorted unique
scope decisions, sorted body-free item decisions, exact category counts, a
`committable` bit, and a domain-separated SHA-256 digest over every preceding
field. Each input item is classified exactly once as:

- `accepted`: authorized and absent from the target's global record/tombstone
  namespaces;
- `conflicting`: a canonical target record already uses the ID;
- `unauthorized`: current exact-scope import authority is absent;
- `tombstone_protected`: any protected tombstone already owns the global ID;
- `invalid`: the strict source is structurally valid but exceeds the committed
  batch bound.

Two calls over identical source bytes, authority, and target state are
byte-identical. Planning never creates a directory or lock, stages a file,
publishes a manifest, changes metadata, or updates an mtime. A plan may contain
up to 1,000 body-free decisions so the dry run can explain an oversized batch;
a committed batch is limited to 100 total records plus tombstones and 100
scopes. A noncommittable plan and a commit-time authorization failure both exit
before WAL publication with the complete tree and watermark unchanged.

## Idempotency and acknowledgement

The durable receipt identity binds the authenticated principal and a
domain-separated digest of the raw idempotency key. The request fingerprint
binds the canonical bundle digest, authoritative scopes/source snapshot, and
expected plan digest. It excludes generated transaction IDs, target times,
correlation data, and ambient paths.

Commit ordering is:

1. authenticate, authorize every scope, and strictly decode the bundle;
2. look up an existing receipt before target-state conflict checks or writes;
3. on an exact match, revalidate current scope authority and return the original
   strict result and backup metadata;
4. on different canonical input under the same receipt identity, return
   `IDEMPOTENCY_CONFLICT` without a write; or
5. on no receipt, recompute the target plan and enter the batch WAL only when
   its digest matches and it remains committable.

An exact retry after timeout, disconnect, recovery, or restart returns
`idempotentReplay=true` with the original transaction ID, source/target
watermarks, counts, result digest, and backup metadata. Replay does not publish
another record, receipt, audit event, or metadata revision.

## Canonical import semantics

For every accepted record Jiandu reconstructs canonical Markdown/frontmatter,
runs the full domain validator, and requires the exact public record to survive
the round trip. Import preserves ID, record revision, ETag, exact scope,
type/status, title/summary/body, tags, relations, timestamps, and every portable
provenance field. It does not rewrite text, trim Markdown, or replace source
identity with an internal storage key.

For every portable tombstone Jiandu creates a target-store protected tombstone
with the same logical ID, scope, revision, ETag, and `forgottenAt`, but binds it
to the new local transaction and local store/audit watermark. No forgotten
body exists in the portable source or target artifact. Imported tombstones
remain part of the exact non-resurrection ledger; imported records may later be
updated or forgotten normally without invalidating historical import replay.

The target metadata is deterministic from the authoritative base and source
snapshot:

```text
targetStoreRevision = max(baseStoreRevision + 1, sourceStoreRevision)
targetAuditSequence = baseAuditSequence + 1
```

Both additions are checked and overflow fails before WAL entry. Every imported
item revision is at most both the source snapshot and target store revision.
One committed batch contributes exactly one audit sequence regardless of item
count; replay contributes zero.

## Public formats and bounds

All three formats use strict pretty JSON plus exactly one final LF, deny
unknown fields, commit struct field order, and use lowercase
`sha256:<64-hex>` digests with distinct domains. UUID fields accept exactly the
canonical lowercase hyphenated representation accepted by the Rust validator.

The plan digest covers the entire plan except its digest field. The committed
result binds source store/snapshot, target store, exact base and target
snapshots, bundle and plan digests, backup digest, transaction ID, and counts.
Backup metadata independently binds the same lineage and counts plus its own
digest. Strict decode rejects, among other impossible states:

- empty or duplicate/unsorted scope decisions;
- invalid category counts or a `committable` bit inconsistent with both those
  counts and fresh authorization for every declared scope (including an
  unused scope);
- `auditSequence > storeRevision` in any snapshot;
- target revision other than the formula above;
- target audit sequence other than checked base plus one;
- more than 100 committed items or 100 scopes;
- malformed UUID/digest/version fields; and
- noncanonical bytes or digest mismatch.

The plan/result/backup byte limits are 1 MiB, 256 KiB, and 64 KiB respectively.
The private v4 import manifest is capped at 256 KiB; a maximum-length 100-item
fixture proves the public batch bound remains representable. Manifest
canonicalization and the size preflight occur before the live handle is
poisoned or any WAL file is written.

## Backup metadata API

Backup metadata is not an unaudited post-commit callback. The same import WAL
stages, fsyncs, publishes, and receipt/audit-binds it before metadata commit.
`ImportCommit` returns that exact persisted value for both the first success
and replay.

A future host adapter may read the exact artifact by canonical transaction ID
only after creating the private-field capability through the independent
`memory:admin:backup_metadata` grant. `read_backup_metadata` validates the full
ledger before and after the read, opens only a private single-link regular file,
captures and rechecks file identity, strictly decodes it, and verifies the
receipt/audit/store binding. Invalid transaction syntax is `InvalidRequest`;
ordinary write/import/export authority does not imply this grant.

There is deliberately no standalone self-authorizing backup writer. A staged
file or filename cannot authorize publication after a crash. Backup metadata
contains no record/tombstone body, credential, raw idempotency key, query,
canonical/ambient path, or private result bytes. It is local recovery metadata,
not a bundle copy, remote backup, retention policy, or proof that external
backups were deleted.

## Privacy and audit

The v4 manifest, import receipt, import audit, committed result, and backup
metadata are body/reason/query/credential/raw-key/path-free. They contain only
opaque logical IDs/scopes where needed, counts, watermarks, format and store
identities, domain-separated key/principal/request digests, and content
digests. Record bodies occur only in same-filesystem staged/canonical record
files. Tombstone import has no body-bearing representation.

The existing validation-report `v1alpha1` code/artifact enums are closed.
Because backup metadata is receipt-bound, whole-store validation reports its
corruption under the existing `receipt_inconsistent` / `receipt` category
rather than changing that historical wire format. Startup still uses the
stage-specific internal ledger error and fails closed.

## Compatibility

Portable export remains `v1alpha1`; its strict source-store field accepts
supported `jiandu.store/v1alpha3` and `jiandu.store/v1alpha4` producers. Its
historical v3 fixture bytes are unchanged. Import/backup formats are separate
version domains; new fields or semantics require a new version rather than
unknown-field tolerance.

Committed import is a storage capability and therefore requires
`jiandu.store/v1alpha4`. A v3 store must be explicitly migrated under the root
lock before these APIs can be used. Older writers reject the v4 marker. See
[Canonical Store Format `v1alpha4`](store-format-v1alpha4.md) for layout,
transaction order, recovery, and migration.

## Out of scope

This contract does not implement CLI/MCP transport, Bamboo mapping, filesystem
watch import, index/search updates, remote upload, backup scheduling, full
backup archives, committed restore, hard purge, receipt GC, retention policy,
or model/LLM integration.
