# Canonical Store Format `v1alpha2`

> Historical contract: [`jiandu.store/v1alpha3`](store-format-v1alpha3.md) is
> the forget/tombstone successor and
> [`jiandu.store/v1alpha4`](store-format-v1alpha4.md) is the current writer
> format. This document and its checked fixtures remain the
> immutable create/update receipt/audit and `v1alpha2` migration-source
> contract.

This document commits the `jiandu-store` on-disk extension that makes public
create and update durably idempotent and sequence-audited. It builds on the
record grammar, scope-safe layout, exclusive ownership, atomic replacement,
quarantine behavior, and durability rules in
[`v1alpha1`](store-format-v1alpha1.md). The public Jiandu API and canonical
Markdown record schema remain `jiandu.dev/v1alpha1`; store-format versions are
an independent compatibility domain.

All paths in this document are private implementation details. No MCP request,
result, model-visible schema, error, or diagnostic exposes them.

## Capability gate and metadata

`store.json` is strict canonical JSON:

```json
{
  "formatVersion": "jiandu.store/v1alpha2",
  "storeId": "00000000-0000-4000-8000-000000000017",
  "storeRevision": 42,
  "auditSequence": 7,
  "createdAt": "2026-08-24T00:00:00Z"
}
```

`storeRevision` remains the authoritative read watermark. `auditSequence` is
an independent non-negative watermark for committed mutation audit events. A
new create or update advances both values exactly once. A replay advances
neither. The two counters are independent so later store operations can define
their own audit policy without overloading the read watermark.

An implementation that understands only `jiandu.store/v1alpha1` must reject
this format before acquiring or updating `LOCK`. It cannot reopen a migrated
store and write a record without the receipt/audit extension.

## Private layout

The `v1alpha1` directories remain, with these additions:

```text
<data-dir>/
├── store.json
├── records/...
├── transactions/<transaction-id>.json
├── receipts/
│   ├── quarantine/...
│   └── idempotency/
│       ├── metadata/
│       │   └── <principal-digest>/<operation>/<shard>/<receipt-id>.json
│       └── results/
│           └── <shard>/<receipt-id>.json
└── audit/
    ├── genesis.json
    └── mutations/<20-digit-audit-sequence>.json
```

`<operation>` is `create` or `update`. Digests, receipt IDs, shards, and audit
filenames use fixed-width lowercase ASCII. Every file is opened no-follow,
must be regular, private, and single-linked, and is decoded with an exact size
bound and canonical re-encode check. Startup recomputes each expected path from
the artifact's strict typed contents. It rejects misplaced, missing,
duplicated, orphaned, foreign, malformed, hard-linked, symlinked, oversized,
or partially published artifacts before readiness.

The result subtree is operator-private and has no public enumeration API. The
metadata and audit subtrees contain no record body.

## Authentication, identity, and request fingerprint

Receipt lookup is possible only after this order:

1. validate the trusted request context;
2. verify that its principal matches host authority;
3. authorize the operation for one exact authoritative scope;
4. validate the caller command and canonicalize its scope selector;
5. derive and look up the private receipt identity; and
6. on a match, recheck that the fresh capability still authorizes the exact
   scope stored in the receipt before loading the private result.

Mutation capabilities are created by the host and have private fields. Public
create/update entry points cannot accept a caller-constructed Principal or a
bare `AuthorizedScope`. The required host grants are
`memory:write:principal`, `memory:write:project`, `memory:write:session`, or
`memory:write:instance_global` for the selected exact scope.

The receipt identity is derived from:

- a domain-separated digest of the authenticated Principal ID;
- the operation; and
- a domain-separated digest of the raw idempotency key.

The raw Principal ID and raw key are never persisted in receipt metadata,
audit, or a transaction manifest. Principal and operation separation allow the
same raw key to be used independently by another principal or by the other
operation.

The request fingerprint hashes canonical caller input plus the authoritative
scope. Create includes type, title, summary, body, tags, caller provenance, and
relations. Update includes memory ID, expected revision, patch, and reason.
Generated memory ID, trusted creation actor, timestamps, client/correlation
metadata, and the raw idempotency key are excluded. A retry can therefore use
new generated values and still receive the original success, while any change
to caller-controlled semantic input is a conflict.

Receipt lookup precedes create's generated-ID/global-existence check and
update's record lookup, `NotFound`, and CAS check. A successful update retry
therefore replays even though its original expected revision is now stale.

## Strict artifact schemas

Every receipt, result, audit event, and record transaction repeats one strict
binding:

| JSON name | Meaning |
| --- | --- |
| `receiptId` | Lowercase 64-hex derived lookup ID. |
| `transactionId` | Canonical UUID shared with the WAL. |
| `principalDigest` | Domain-separated authenticated-principal digest. |
| `keyDigest` | Domain-separated raw-key digest. |
| `operation` | Closed `create` or `update` value. |
| `scope` | Exact authoritative typed scope. |
| `requestFingerprint` | Digest of canonical caller input and scope. |
| `memoryId` | Opaque target ID. |
| `targetRevision`, `targetEtag` | Exact committed result identity. |
| `storeRevision`, `auditSequence` | Exact target watermarks. |

The canonical checked fixtures under
`crates/jiandu-store/fixtures/v1alpha2/` are executable examples of every
strict JSON shape:

- `store-metadata.json` and `audit-genesis.json`;
- `record-transaction.json`;
- `idempotency-receipt.json`;
- `mutation-audit.json`; and
- `mutation-result.json`.

Tests build the same values through the Rust codecs, compare every byte to the
checked fixtures, and decode the fixtures back through the strict types. This
makes JSON-name, unknown-field, canonical-format, and digest drift fail CI.

### Transaction manifest

The record WAL format is `jiandu.store.transaction/v1alpha2` and is bounded to
64 KiB. In addition to the `v1alpha1` base/target record and metadata identity,
it contains the binding and SHA-256 digests of the exact result, receipt, and
audit bytes. It contains no body, title, summary, provenance payload, update
reason, raw key, raw query, credential, prompt, ambient path, or canonical
path.

### Private replay result

The result format is `jiandu.store.mutation-result/v1alpha1`, bounded to 1 MiB.
It contains the binding, optional previous revision, and the complete original
record. This is the only idempotency artifact allowed to contain the record
body. Its canonical bytes are digest-bound by both the WAL and receipt.

### Receipt metadata

The receipt format is `jiandu.store.idempotency-receipt/v1alpha1`, bounded to
64 KiB. It contains the binding and result digest, but no complete record,
body, reason, raw key, query, credential, or path. Its derived namespace is the
only lookup route; no public list API exists.

### Mutation audit

The audit format is `jiandu.store.mutation-audit/v1alpha1`, bounded to 64 KiB.
It contains the binding and result digest. Filename sequence, embedded
`auditSequence`, receipt binding, result digest, and store UUID must agree.
Exactly one sequence-addressed event exists for each committed create/update.
Authentication, authorization, validation, not-found, CAS, and idempotency
conflicts do not create durable audit-only transactions in this slice.

## Pre-acknowledgement transaction

The single owner performs one WAL transaction in this order:

1. write, flush, publish, and directory-sync the strict manifest;
2. stage and flush the record and target `store.json` on their final
   filesystems;
3. create and sync the derived result, receipt, and audit namespaces;
4. stage and flush the exact result, receipt, and audit bytes;
5. atomically publish the target record and sync its shard;
6. publish and sync result, then receipt, then audit;
7. publish `store.json` last and sync the root; and
8. remove the manifest and sync `transactions/` before returning success.

Metadata-last is the commit watermark. Any error after WAL persistence poisons
the live handle, including errors after record/artifact/metadata rename. A
poisoned handle cannot read, mutate, report a watermark, run doctor, or inspect
operator receipt state until it is dropped and startup recovery runs.

## Recovery states

Recovery classifies the record and `store.json` against exact base/target
values in the manifest:

| Record | Metadata | Published receipt/result/audit | Recovery |
| --- | --- | --- | --- |
| base | base | none | Remove staged files and roll back. |
| base | base | any | Fail closed; publication before the record is impossible. |
| target | base | partial or complete | Rebuild/verify exact artifacts, publish all, then metadata. |
| target | target | partial or complete | Rebuild/verify exact artifacts, then clean up. |
| base | target | any | Fail closed as impossible ordering. |
| ambiguous | either | any | Fail closed without guessing. |

Artifact reconstruction is permitted only from the verified exact target
record plus the strict body-free manifest intent. Every reconstructed byte must
match its digest already committed in the manifest. Recovery never guesses a
body, reason, scope, principal, key, result, or revision. Missing or mismatched
data that cannot satisfy those proofs fails closed.

Recovery is restartable at every artifact namespace, publish, sync, metadata,
and cleanup boundary. Before readiness, the store then validates the entire
committed ledger as an exact set: sequences `1..=auditSequence`, one receipt and
one result per event, recomputed paths, consistent bindings/digests, no foreign
temps, and no orphan artifacts.

## Replay and conflict behavior

An identical authorized retry returns the original transaction ID, record,
revision, ETag, previous revision, and store revision with
`idempotentReplay=true`. It works after success, a lost acknowledgement,
timeout/disconnect, process restart, and recovery. It does not rewrite a file,
advance either watermark, or append an audit event.

If the same receipt identity exists but the exact scope or request fingerprint
differs, the operation returns `IDEMPOTENCY_CONFLICT` before CAS, record lookup,
or any persistent write. Diagnostics contain only the stable error category.

Exact replay is guaranteed only while the private result artifact remains and
the caller still has current authority for its exact scope. `v1alpha3`
ordinary forget retains historical create/update results. A future explicit
hard-purge/receipt-GC executor must atomically retire this strict ledger before
deleting artifacts; deletion then terminates replay for the old key. External
backups are outside a local hard-purge guarantee. Portable export must exclude
private results and the live audit ledger; full-backup semantics are separate
work.

## Explicit migration from `v1alpha1`

Migration is explicit and runs under the exclusive root lock:

1. validate `v1alpha1` metadata/layout and recover its one legacy `#4` WAL
   transaction while the old format remains authoritative;
2. idempotently create and sync the `v1alpha2` directories;
3. write, flush, publish, and sync strict `audit/genesis.json`, recording the
   legacy `storeRevision` at which the new audit sequence begins; and
4. publish and sync `v1alpha2` `store.json` last.

A crash before step 4 leaves the `v1alpha1` capability marker. Migration can
remove/reuse its recognized temps and continue; a legacy writer remains within
its declared format and a later migration regenerates genesis from the latest
legacy watermark. A crash after metadata rename leaves the `v1alpha2` marker,
so the normal `v1alpha2` open/recovery path converges and an older writer fails
closed. Migration never rewrites canonical record bytes or modification times.

## Compatibility and non-goals

`v1alpha2` readers decode legacy `v1alpha1` transaction manifests only during
the explicit migration recovery window. They never silently open a
`v1alpha1` store as writable `v1alpha2`, and normal open never migrates an
unknown format.

This historical slice implements create/update idempotency, private replay results,
sequence-addressed mutation audit, strict startup ledger validation, and
explicit `v1alpha1` migration. Forget/tombstones are added by `v1alpha3`;
restore/hard-purge execution, validation,
portable export/import, backup metadata, search/indexing, MCP/CLI transport,
Bamboo integration, prompt construction, and model calls remain out of scope.
