# Canonical Store Format `v1alpha4`

`jiandu.store/v1alpha4` adds bounded portable batch import and receipt-bound
recovery-safe backup metadata to the create/update/forget guarantees of
[`v1alpha3`](store-format-v1alpha3.md). Record/frontmatter, public API,
portable-export, import-plan/result, and backup-metadata versions remain
independent. A v3 writer rejects the v4 marker and cannot mutate a store whose
batch-WAL and import-ledger invariants it does not understand.

## Capability gate and layout

`store.json` retains the same strict fields and changes `formatVersion` to
`jiandu.store/v1alpha4`. The migration preserves store identity, creation time,
store revision, audit sequence, canonical record/tombstone bytes, and all
historical private ledgers.

The current private layout adds:

```text
receipts/import/
├── metadata/<principal-digest>/<shard>/<receipt-id>.json
└── results/<shard>/<receipt-id>.json
audit/imports/<20-digit-audit-sequence>.json
backups/imports/<transaction-id>.json
```

Directories and files are private, single-link, bounded, and accessed beneath
the held root capability. Principal/key digests, receipt IDs, shards, and
memory owner/storage keys are domain-separated lowercase SHA-256 values.
Internal paths and keys never appear in public plan/result/backup contracts or
diagnostics.

Strict private formats are:

- batch WAL: `jiandu.store.transaction/v1alpha4`;
- receipt: `jiandu.store.import-receipt/v1alpha1`;
- audit: `jiandu.store.import-audit/v1alpha1`;
- result: `jiandu.import-result/v1alpha1`;
- backup metadata: `jiandu.backup-metadata/v1alpha1`.

The WAL is body-free and capped at 256 KiB only for the v4 import intent; the
historical 64 KiB bounds remain unchanged for non-import and v1/v2/v3
manifests. It binds exact base/target metadata, source/bundle/plan lineage,
fresh authority scopes, sorted record/tombstone intents and their content
digests, result/receipt/audit/backup digests, receipt identity, transaction ID,
counts, and the one target audit sequence. Raw keys, bodies, forget reasons,
queries, credentials, and ambient/canonical paths are forbidden.

## Metadata-last batch transaction

One exclusive owner commits a batch in this order:

1. strictly validate the portable bundle, current exact-scope authority, and
   target plan; perform receipt lookup and manifest-size preflight before write;
2. persist and sync the strict manifest;
3. prepare/sync final owner/shard and private-ledger directories;
4. create, fsync, and parent-sync every same-filesystem record/tombstone temp,
   backup temp, result temp, receipt temp, audit temp, and target metadata temp;
5. atomically rename and parent-sync all canonical records, then protected
   tombstones;
6. atomically rename and parent-sync backup metadata, result, receipt, and audit
   in that order;
7. atomically replace and root-sync `store.json` last; and
8. remove the manifest and sync `transactions/` before acknowledging success.

No serviceable handle exists between steps 2 and 8: any error poisons the live
owner until startup recovery. The metadata marker is the commit watermark.
Imported record revisions/ETags remain the source values, so the target store
revision may advance to the greater of checked base-plus-one and the source
watermark. The independent audit sequence advances exactly once.

## Recovery matrix

Startup first accepts exactly one strict v4 manifest and classifies every
expected record, tombstone, backup, result, receipt, audit, and metadata path as
absent, safe staged, exact target, or ambiguous. Every staged file must first be
opened no-follow and proven regular, private, and single-link. A staged file may
be incomplete only when metadata is exact base and no canonical target or
body-free artifact has been published: this is the real crash window after
`write_all` and before file sync. Recovery identity-rechecks and removes those
manifest-bound temps, syncs their parents, and returns to the old store. Once
any target/artifact is published, every remaining staged record, tombstone,
backup, result, receipt, audit, and metadata byte required for roll-forward must
be digest-exact (except that target metadata can be reconstructed from the
strict WAL after the proven metadata-rename window). Symlink, hardlink,
special/foreign entry, duplicate staged+target, missing required byte, or
roll-forward digest mismatch fails closed.

Recovery retains the opened inode identity for every exact staged or published
target. It reopens and rechecks that identity, privacy, single-link state, and
exact digest before a target is reused or renamed, then rechecks the complete
published target set immediately before target metadata is published or an
already-committed manifest is removed. A validation-to-commit inode replacement
or added hardlink therefore fails closed instead of becoming ready state.

| Metadata | Published target/artifact state | Recovery |
| --- | --- | --- |
| exact base | no target and no artifact | identity-check and remove exact or safe-incomplete manifest-bound temps, sync parents, remove/sync manifest; old store remains |
| exact base | any canonical target, with every remaining byte exact staged or exact target | publish/sync every remaining target and artifact, then target metadata, then remove/sync manifest |
| exact base | artifact before all canonical record/tombstone targets | fail closed |
| exact target | every target and artifact exact, no metadata temp | remove/sync manifest; imported store remains |
| exact target | anything missing, staged, foreign, or ambiguous | fail closed |
| neither base nor target | any | fail closed |

Publication order is also validated: result cannot precede backup, receipt
cannot precede result, audit cannot precede receipt, and no body-free artifact
may exist before the complete canonical target set. When all targets/artifacts
are durable but a crash after metadata rename exposes base `store.json` and no
metadata temp, the strict manifest and exact target digests authorize
deterministic reconstruction and metadata-last publication. Recovery never
guesses from a filename or a self-signed orphan temp.

Every live and recovery persistence boundary has a fail-once/reopen scenario.
The declared `PersistenceBoundary::ALL` set must equal the exercised test set,
so adding an untested boundary is a test failure.

## Startup ledger

After WAL recovery and before readiness, startup validates an exact-set import
ledger:

- each receipt is under its principal digest and receipt-ID shard;
- receipt, result, audit, and backup metadata reconstruct the same exact
  binding, transaction, base/source/target snapshots, item/count set, and
  digests;
- import and ordinary mutation audit sequence sets form one contiguous ledger
  ending at `store.json.auditSequence`;
- every imported protected tombstone and its local watermark remains exact;
  duplicate or resurrected IDs fail closed; and
- no orphan receipt/result/audit/backup/temp exists.

Imported records are not permanently exact-bound to the historical import
result: a later ordinary update or forget is valid. The historical result,
receipt, audit, and backup metadata continue to prove what the import committed.
Imported tombstones remain exact until a future explicit versioned retirement
protocol.

The `ImportBinding` requires a nonempty sorted scope set and enforces all item,
count, revision, digest, base/source/target watermark, and audit-lineage
invariants during manifest, receipt, audit, and startup decode. A forged empty
scope binding cannot become an authority-free replay record.

## Backup metadata

Backup metadata is published by the same pre-acknowledgement transaction. It
contains only format/store/transaction identities, exact base/source/target
watermarks, bundle and plan digests, item counts, and its own digest. It never
contains a record or forgotten body, path, raw key/query, credential, private
result, upload location, schedule, or retention decision.

`ImportCommit` returns the exact persisted value, including on replay. A
separately authorized host may read it by transaction ID only after complete
ledger and file-identity verification. There is no independent unaudited
writer and no recovery rule that promotes an orphan backup temp.

## Explicit `v1alpha3` to `v1alpha4` migration

Migration is explicit under the root lock:

1. validate strict v3 metadata/layout and lock identity;
2. recover any active v3 WAL while the v3 marker remains authoritative;
3. validate audit genesis and the complete v3 receipt/result/audit/tombstone/
   witness ledger;
4. create, validate, and sync the fixed import receipt/result/audit/backup
   directories and their ancestors;
5. stage and fsync exact v4 metadata; and
6. atomically publish and root-sync v4 `store.json` last.

A crash before step 6 leaves the v3 marker, so migration safely repeats and a
v3 writer remains within its declared capability. A crash after publication
leaves the v4 marker, which every v3 writer rejects. Migration failpoints cover
layout sync, metadata staging/sync/rename/root sync, and restart. Direct
v1alpha1/v1alpha2 migrations first recover and validate their historical WAL
and ledgers, then create every intermediate/current fixed layout before the same
v4 metadata-last publication. Historical v1/v2/v3 fixture bytes remain decode
contracts and are not rewritten.

## Read-only validation and export compatibility

Validation report `v1alpha1` keeps its closed code/artifact taxonomy. Import
backup metadata is receipt-bound, so corruption maps to the existing
`receipt_inconsistent` / `receipt` public finding; the internal startup stage
remains distinct and fail-closed.

Portable export `v1alpha1` can strictly decode supported v3 and v4 source
markers. Export contains public records and portable tombstones only; it never
exports the private import receipt/result/audit/backup/WAL namespaces.
Portable export and its historical v3 fixture do not change when the store
capability advances to v4.

## Non-goals

This store format does not implement import/export CLI or MCP transport,
Bamboo legacy mapping, remote backup storage, backup scheduling, full backup
archives, restore, hard purge, receipt GC, retention execution, search/index
maintenance, filesystem watching, prompt construction, or model calls.
