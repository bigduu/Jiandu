# Validation Report and Portable Export `v1alpha1`

Jiandu exposes side-effect-free store validation and deterministic portable
export as host/operator store APIs. These formats are independent of the MCP
protocol, public memory API, canonical store, and future import/backup format
versions:

- validation report: `jiandu.validation-report/v1alpha1`;
- portable export: `jiandu.portable-export/v1alpha1`;
- portable tombstone projection: `jiandu.portable-tombstone/v1alpha1`.

The checked JSON Schemas and canonical fixtures live under
`crates/jiandu-store/schemas/inspection/v1alpha1/` and
`crates/jiandu-store/fixtures/inspection/v1alpha1/`. They are generated and
strictly decoded from the same Rust definitions. Schema drift and canonical
fixture decode are test failures.

## Entry points and coordination

The same pure read-only inspection engine serves two entry points:

- a live `CanonicalStore` owner calls `validate_scopes`, `export_scopes`,
  `validate_all`, or `export_all` while retaining its existing root capability
  and exclusive ownership; and
- `ReadOnlyStoreInspector::open` coordinates with the same kernel store lock
  for offline inspection while the daemon is stopped. It does not publish or
  rewrite lock-owner diagnostics.

Offline inspection refuses a live owner. Both paths read the canonical
`store.json` watermark at the beginning and end, validate the fixed root and
lock identities, recheck traversed namespace names and opened file identities,
and fail export if the snapshot changed. A legitimate single writer therefore
produces either one complete before/after snapshot or a safe failure, never a
mixed bundle. Validation may return a bounded `snapshot_changed` finding.

Inspection never initializes a missing store, migrates an old store, recovers
an active WAL, quarantines or repairs a record, creates a directory or lock,
removes a temporary file, advances a watermark, or changes canonical bytes or
mtimes. An active/foreign transaction is reported and export is refused. A
future store version can produce a safe validation report with no asserted
store identity or watermark, but it cannot be exported.

## Authorization and scope privacy

Scoped validation/export accepts a non-empty, unique, bounded list of exact
scopes and an `AuthorizedScopes` capability. It opens and decodes records and
tombstone metadata only beneath those explicitly authorized owner segments.
The report may identify only those expected scopes; a forged record or
tombstone cannot make a scoped report echo its decoded scope or an unbound ID.

Whole-store operations use private-field, principal-bound capabilities with
separate grants:

- `memory:admin:validate_store` for `validate_all`;
- `memory:admin:export_all` for `export_all`.

Write, forget, lifecycle-plan, or scoped-read authority does not imply either
admin grant. These host/operator APIs are not model-visible MCP tool schemas.

There is one deliberately narrow global check required by the v3/v4
non-resurrection invariant. Before any authorized candidate record body is
opened, inspection performs a bounded traversal of tombstone namespace names
and collects only domain-separated hashed memory storage keys. It checks strict
entry names plus filesystem type/link/permission metadata but never opens or
decodes an unauthorized tombstone, learns its logical scope/ID/revision/ETag,
or exposes its logical metadata/count in a scoped report, bundle, or digest. A
matching key causes
the authorized candidate to be treated as a record/tombstone conflict before
body decode and export fails closed. This protection-key-only check is not an
authorization bypass; it is the same cross-scope ambient-resurrection defense
required by exact get/list.

Private receipt/result/audit/witness ledger validation is deliberately a
whole-store operation. Those artifacts are not safely partitionable by a
requested memory scope without opening principal-global replay state, so
`validate_scopes` and `export_scopes` do not traverse them. Scoped operations
still validate store metadata and audit genesis, active WAL markers, selected
records/tombstones, and the non-decoding protection-key set. Operators that
need a statement about the complete private ledger use `validate_all` (and
`export_all` refuses an inconsistent ledger) under the distinct admin
capability. This separation prevents a scoped caller from using validation as
a foreign-ledger oracle.

## Validation report

A report contains its format, mode, canonically sorted inspected scopes,
canonically sorted and deduplicated safe findings, truncation flag, and a
domain-separated digest. `sourceStoreId` and `snapshot` are present together
only when `store.json` is authoritative. The snapshot binds both
`storeRevision` and the independent `auditSequence`, with
`auditSequence <= storeRevision`.

Findings contain only a closed code, logical artifact class, and an optional
authorized/admin-visible opaque scope and memory ID. They never contain a
canonical or ambient path, body, title, summary, provenance payload, query,
credential, raw idempotency key, forget reason, transaction name, or private
artifact bytes.

| Stable code | Meaning |
| --- | --- |
| `store_metadata_inconsistent` | Metadata is missing, malformed, noncanonical, or has impossible watermarks. |
| `unsupported_store_version` | The store format is not supported by this reader. |
| `layout_inconsistent` | A fixed directory or entry shape is invalid. |
| `unsafe_entry` | A symlink, hard link, special file, unsafe permission, or raced identity was observed. |
| `active_transaction` | A WAL/temporary transaction state exists; inspection does not recover it. |
| `record_malformed` | A bounded record is not strict canonical Markdown/frontmatter or fails domain validation. |
| `record_id_mismatch` | A decoded ID does not bind the canonical hashed filename. |
| `scope_path_mismatch` | Decoded scope does not bind the authoritative owner segment. |
| `shard_mismatch` | A storage key or decoded ID is under the wrong shard. |
| `duplicate_memory_id` | One opaque ID appears in more than one canonical record. |
| `record_tombstone_conflict` | A canonical candidate exists for a globally protected ID. |
| `tombstone_inconsistent` | A protected tombstone is malformed, misplaced, duplicated, or ledger-inconsistent. |
| `receipt_inconsistent` | The exact receipt set or binding is inconsistent with metadata. |
| `result_inconsistent` | A private replay result is missing, foreign, malformed, or digest-mismatched. |
| `audit_inconsistent` | Audit genesis/sequence/content does not exactly match the receipt ledger and watermark. |
| `witness_inconsistent` | A logical-erasure witness is missing, extra, linked, nonzero, or otherwise invalid. |
| `snapshot_changed` | A coordinated name, identity, namespace, root, lock, or metadata recheck changed. |
| `scan_limit_exceeded` | A hostile or unexpectedly large traversal exhausted a hard budget. |
| `finding_limit_reached` | The report stopped collecting after its bounded finding limit. |

`truncated=true` is valid only with exactly one final
`finding_limit_reached`; the marker is invalid when `truncated=false`. A scoped
finding may name only one of `inspectedScopes`. Canonical decode rejects all
unknown fields, duplicate/unsorted values, impossible identity/watermark
combinations, noncanonical JSON, and digest mismatch.

## Portable export bundle

Export succeeds only for a supported, stable, finding-free snapshot. The
bundle contains:

- format and source-store format identifiers, opaque store ID, and snapshot;
- canonical sorted exact scopes;
- canonical sorted records with ID, revision, ETag, exact scope, type, status,
  title, optional summary, exact Markdown body, tags, timestamps, relations,
  and every portable provenance field; and
- canonical sorted body-free protected-tombstone projections containing only
  ID, exact scope, forgotten revision/ETag/time, and committed store/audit
  watermarks.

The closed portable record DTO intentionally does not inherit the public
response type's forward-compatible unknown-field behavior. On strict decode it
converts back to the public record, runs the complete domain validator,
canonical Markdown/frontmatter round-trip, and verifies ETag, scope, ordering,
global ID uniqueness, and watermark bounds. In particular, record revision may
not exceed the bundle snapshot; tombstone revision and audit sequence may not
exceed its own store revision; and tombstone store/audit watermarks may not
exceed the bundle snapshot.

The bundle adds no internal ambient/canonical path fields, internal storage
keys, host/store credential fields, queries, raw idempotency keys, receipts,
private replay results, audit events, WAL/temporary files, logical-erasure
witness bytes, or forgotten bodies. A portable tombstone is protection
metadata, not a record body and not an import authorization. Authorized public
record fields (including `sourceUri` and the active body) are preserved exactly;
Jiandu does not inspect or redact arbitrary user content, so callers must still
treat an export as sensitive if those fields themselves contain a path,
credential, or other sensitive text.

## Canonical encoding, ordering, and bounds

Both formats use strict pretty JSON plus exactly one final LF. Struct field
order is committed; scopes, records, tombstones, and findings have committed
logical sort keys and are unique. Digests cover every preceding field using
SHA-256 with the domain prefixes `jiandu/validation-report/v1\0` and
`jiandu/portable-export/v1\0`. The digest itself is lowercase
`sha256:<64-hex>`.

Inspection stops before unbounded collection or I/O. The current limits are:

- 64 explicitly requested scopes;
- 10,000 discovered scopes or portable record+tombstone items;
- 100,000 visited directory entries;
- 64 MiB cumulative canonical file bytes;
- 256 ordinary findings plus one required truncation marker;
- 1 MiB canonical report and 64 MiB canonical bundle.

Symlinks and special files are never followed, opens are capability-relative
and nonblocking where supported, hard links are rejected, canonical file sizes
are charged before allocation, and file/name identities are rechecked after
read. Exceeding a budget yields a deterministic safe report and refuses export.

## Compatibility and deferred work

Adding these read-only formats does not change the canonical store marker, the
public memory API, or historical `v1alpha1`/`v1alpha2`/`v1alpha3` fixtures. A
reader must match the exact format identifiers and strict canonical bytes;
future fields require a new export/report version rather than being silently
ignored. The strict source-store field accepts supported v3 and v4 producers;
the checked v3 fixture bytes remain unchanged. The checked fixtures exercise both provenance representations
(`messageIds` and `messageRange`), multiple records/scopes, canonical ordering,
one protected tombstone, restart stability, and strict decode.

Committed import and receipt-bound backup metadata are specified separately in
[Portable Import and Backup Metadata `v1alpha1`](portable-import-v1alpha1.md).
Remote backup, restore/hard-purge execution, receipt retirement, CLI/MCP
transport, indexes/search, filesystem watchers, and Bamboo mapping remain
separate issues. This bundle itself grants no import, resurrection, scope
reassignment, or deletion authority.
