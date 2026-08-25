# Canonical Store Format `v1alpha3`

> Historical contract: [`jiandu.store/v1alpha4`](store-format-v1alpha4.md) is
> the current writer format. This document and its checked fixtures remain the
> immutable forget/tombstone and `v1alpha3` migration-source contract.

`jiandu.store/v1alpha3` adds ordinary, idempotent single-record forget and
protected tombstones to the receipt/audit guarantees of
[`v1alpha2`](store-format-v1alpha2.md). Record/frontmatter and public API schema
versions remain independent. A `v1alpha2` writer must reject the `v1alpha3`
store marker and cannot silently resurrect an identity protected by this
format.

## Capability gate and private layout

`store.json` keeps the `v1alpha2` fields and changes only `formatVersion`:

```json
{
  "formatVersion": "jiandu.store/v1alpha3",
  "storeId": "00000000-0000-4000-8000-000000000018",
  "storeRevision": 51,
  "auditSequence": 10,
  "createdAt": "2026-08-24T00:00:00Z"
}
```

Protected tombstones use the same authoritative owner segmentation as records:

```text
tombstones/
├── principal/<principal-key>/<shard>/<memory-key>.json
├── project/<project-key>/<shard>/<memory-key>.json
├── session/<session-key>/<shard>/<memory-key>.json
└── instance_global/<shard>/<memory-key>.json
```

Owner and memory keys are domain-separated lowercase SHA-256 storage keys.
Paths are private implementation details and never appear in public results,
manifests, receipts, audits, plans, or diagnostics. Startup rejects symlinks,
hard links, unknown entries, malformed owner/shard/file names, duplicate IDs,
scope/path mismatches, and orphan transaction temps. Each committed forget also
retains exactly one private, single-link, zero-length
`.forgotten-<transaction-id>.erased` witness beside the former record name.
The witness name is derived from the strict receipt/WAL identity; it is never a
client-visible path or enumerable result.

## Authorization and retry identity

Forget requires an independently grantable destructive permission:

- `memory:forget:principal`
- `memory:forget:project`
- `memory:forget:session`
- `memory:forget:instance_global`

`memory:write:*` cannot forget, and `memory:forget:*` cannot create or update.
The host authenticates trusted principal/client context, validates the command,
and mints the nonempty forget-specific set of exact authoritative scopes before
looking up private receipt metadata. A receipt hit's bound scope must remain
in that set before its result, audit, or full fingerprint is loaded. On a miss,
target discovery scans only the set and then narrows to one exact mutation
capability; an out-of-set receipt or target returns the same `NotFound` as an
absent opaque ID.
For a new commit, the trusted `forgottenAt` value must be at or after the
record's current `updatedAt`; an exact committed replay returns its original
time before consulting current record state.

The receipt identity is the existing principal + operation + raw-key-digest
identity. The request fingerprint binds exact authoritative scope, memory ID,
expected revision, and the canonical raw reason. It excludes generated
transaction IDs, `forgottenAt`, correlation data, and other server values.
Thus an exact retry can return the original time and result; different input
with the same key returns `IDEMPOTENCY_CONFLICT`.

Lookup occurs before record lookup, not-found, and CAS. A committed retry
therefore succeeds after timeout, disconnect, restart, or recovery even though
the canonical record is now absent. Current exact-scope authority is checked
again on every replay. A new key sees the ordinary non-disclosing `NotFound`.

## Strict private formats

All codecs deny unknown fields, require canonical pretty JSON plus one final
LF, private permissions, one link, and digest/store bindings. The manifest,
tombstone, receipt, and audit codecs enforce a 64 KiB bound. The mixed
historical result ledger retains its existing 1 MiB bound because v1alpha2
create/update replay artifacts can contain a record body; the new forget result
is body-free.

- tombstone: `jiandu.store.tombstone/v1alpha1`
- forget WAL: `jiandu.store.transaction/v1alpha3`
- body-free forget result: `jiandu.store.forget-result/v1alpha1`
- forget receipt: `jiandu.store.idempotency-receipt/v1alpha2`
- forget audit: `jiandu.store.mutation-audit/v1alpha2`

The tombstone contains only store/transaction identity, memory ID, exact scope,
forgotten revision and ETag, `forgottenAt`, target store revision, and audit
sequence. The result repeats the strict mutation binding and `forgottenAt`.
The receipt and audit repeat the binding and result digest. The manifest binds
the base/target metadata, receipt/result/audit digests, and tombstone digest.

Raw reason, raw idempotency key, body, title, summary, provenance payload,
query, credential, ambient/canonical path, and correlation metadata are absent
from all five artifacts and from diagnostics. Historical `v1alpha2`
create/update results remain the only private artifacts allowed to carry a
record body.

The operation-set authorization seam and the reversible mapping from an MCP
`req_txn_` UUID correlation to the already persisted transaction ID add no
field to v1alpha3 or v1alpha4 manifest/tombstone/result/receipt/audit codecs.
The standalone correlation remains absent from artifacts, existing strict
fixtures remain byte compatible, and the transaction ID remains the durable
anchor.

Executable byte fixtures are checked under
`crates/jiandu-store/fixtures/v1alpha3/`. The separate `v1alpha2` fixture set is
unchanged and remains a historical decode contract.

## Metadata-last forget transaction

One exclusive owner performs the operation in this order:

1. persist and sync the body-free manifest;
2. stage and fsync the exact tombstone, target `store.json`, result, receipt,
   and audit bytes on their final filesystems;
3. publish and sync the protected tombstone;
4. rename the held, validated canonical record into a transaction-private
   same-directory erasure-witness name, verify that the destination still has
   the held file identity, and sync the record shard; truncate the held file
   descriptor to zero bytes, sync that descriptor, and verify that the private
   name still identifies the held single-link file;
5. publish and sync result, receipt, and audit;
6. publish and sync `store.json` last; and
7. remove the manifest and sync `transactions/` before acknowledging success.

The descriptor-bound truncation prevents a pathname replacement after rename
from erasing a replacement inode: the held original is the only file whose
length can change. A replacement makes the transaction fail closed and poisons
the handle. This is a **logical erasure witness**, not a secure-erase claim;
filesystem snapshots, copy-on-write blocks, storage media, and external backups
may retain prior bytes. Ordinary reads skip only a syntactically valid hidden
witness after startup has validated the exact ledger. Every write, sync,
rename, descriptor erasure, publication, recovery, and cleanup boundary has a
deterministic failpoint/reopen scenario.

## Recovery matrix

Recovery classifies canonical/full-witness/zero-witness/absent body state, tombstone
absent/target/ambiguous state, metadata base/target/ambiguous state, and any
published result/receipt/audit:

| Record/body | Tombstone | Metadata | Artifact state | Recovery |
| --- | --- | --- | --- | --- |
| canonical base | absent | base | none | remove exact staged temps; retain record |
| canonical base | target | base | none | sync protection, rename and descriptor-erase the held record, then complete artifacts and metadata |
| full exact witness | target | base/target | any consistent prefix | re-sync its shard namespace, descriptor-erase and sync the held witness, then complete artifacts and metadata |
| zero witness | target | base/target | any consistent prefix | re-sync its shard namespace and held witness, verify it, then complete artifacts and metadata |
| canonical base | target/absent | target | any | fail closed |
| canonical base | any | base | published artifact | fail closed |
| absent witness after tombstone publication | any | any | any | fail closed |
| witness | absent | any | any | fail closed |
| any ambiguous/foreign state | any | any | any | fail closed |

With `metadata=target`, tombstone/zero-witness/result/receipt/audit must match
the exact target. Missing artifacts may be reconstructed only from the strict
manifest, exact tombstone, and a full or already-zero exact witness; recovery
never guesses. Orphan, duplicate, nonzero committed, hard-linked, symlinked,
absent, partial, or foreign witnesses fail closed. The manifest is removed only
after all committed parents have been synced.

## Read, resurrection, and retention behavior

After commit the canonical record is absent, so exact get/list (and future
search/export) exclude it immediately. Exact get checks global tombstone
presence before opening any candidate record. List collects the global hashed
tombstone-key set once and filters candidates before opening or decoding a
`.md` body, including when the tombstone is outside the caller's allowed
scopes. Create checks protected IDs globally and returns ordinary `NotFound`;
update also returns `NotFound`. Neither can implicitly resurrect or change
tombstone scope authority. The v4 successor's import protocol honors the same
rule; see [Canonical Store Format `v1alpha4`](store-format-v1alpha4.md).

Ordinary forget retains historical create/update private replay results and its
own body-free result while their receipts remain live. Those mutation replays
still require current exact-scope authority; this does not make the record
visible through normal reads. This slice does not delete receipts or results.
A future hard-purge/receipt-GC executor must use a versioned atomic ledger
retirement transition before deleting a result, because the current startup
invariant requires every receipt/result/audit triple. Once that explicit
transition deletes an artifact, exact replay for the retired key ends.
That future atomic retirement must remove the corresponding logical-erasure
witness as part of the same lifecycle. External backups are outside a local
deletion guarantee.

## Administrative dry-run seam

Restore and hard purge execution, retention schedules, remote purge, and bulk
model tools are not implemented. The store exposes only a non-executing,
host-only planner behind separate private-field capabilities and grants:

- `memory:admin:restore`
- `memory:admin:hard_purge`

The planner accepts 1–100 explicit opaque IDs in one exact scope. Duplicate,
missing, inaccessible, mismatched, or non-tombstoned input makes the whole
request fail. It sorts targets and returns action, every target ID/scope/
revision/ETag, count, current store watermark, and a domain-separated
confirmation digest. The digest binds action, store ID/revision, principal,
exact scope, and each sorted tombstone identity including transaction/time/
watermarks. It exposes no body or path and grants no execution authority.

## Explicit migration from `v1alpha2`

Migration is explicit under the root lock:

1. validate strict `v1alpha2` metadata/layout;
2. recover any active `v1alpha2` WAL while the old marker is authoritative;
3. validate audit genesis and the complete historical v2 ledger;
4. create and sync the fixed scope-safe tombstone directories;
5. stage, fsync, atomically publish, and sync `v1alpha3` `store.json` last.

A crash before step 5 leaves the v2 marker, so migration repeats safely and a
v2 writer remains within its declared capabilities. A crash after publication
leaves the v3 marker, so the current open path converges and every v2 writer
fails closed. Canonical record bytes and mtimes are unchanged. Normal open
never migrates an old or unknown format.

`v1alpha1` explicit migration recovers its legacy WAL, creates both historical
receipt/audit and then-current tombstone layouts, writes audit genesis, and
then publishes the historical v3 marker metadata-last.

## Read-only validation and portable export

Side-effect-free validation and portable export use independent
`jiandu.validation-report/v1alpha1`, `jiandu.portable-export/v1alpha1`, and
`jiandu.portable-tombstone/v1alpha1` format domains. They do not change the
`jiandu.store/v1alpha3` capability marker or historical fixtures. Inspection
does not initialize, migrate, recover, quarantine, rewrite, or advance this
store; active WAL, an inconsistent private ledger in admin whole-store mode, an
unstable snapshot, or an unsupported source refuses export. Scoped mode does
not traverse the principal-global private ledger because it cannot be safely
partitioned by a requested memory scope.

Scoped inspection opens and decodes records and tombstone metadata only under
explicitly authorized owner segments. It preserves this format's global
non-resurrection rule with one bounded namespace-only pass over tombstone
storage keys before opening an authorized record candidate. That pass validates
strict entry names and filesystem type/link/permission metadata, but does not
open or decode
an unauthorized tombstone or expose its scope, ID, metadata, or count. A match
is a record/tombstone conflict and export fails closed; it is never silently
normalized into a valid bundle.

Portable tombstones contain body-free protection metadata only. The bundle
adds no canonical path/internal key fields, private receipts/results/audits/WAL,
erasure-witness bytes, raw keys/reasons/queries, host/store credential fields,
or forgotten bodies. Authorized public record fields remain exact user content
and are not a path- or credential-redaction boundary. Full coordination,
authorization, strict-codec, ordering, digest, schema, bound, and compatibility
rules are specified in
[Validation Report and Portable Export `v1alpha1`](portable-export-v1alpha1.md).

## Non-goals

This historical format does not itself implement restore/hard-purge execution,
receipt GC, retention scheduling, remote backup deletion, committed import,
backup metadata, search/indexing, MCP/CLI transport, Bamboo integration, prompt
construction, or model calls. Committed import/backup metadata require the
explicit v4 capability migration. Validation/export remain separate read-only
output formats, not v3 mutation capabilities.
