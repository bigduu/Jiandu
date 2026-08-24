# Canonical Store Format `v1alpha1`

> Historical compatibility source: `v1alpha2` succeeded this format,
> [`jiandu.store/v1alpha3`](store-format-v1alpha3.md) is the historical
> forget/tombstone format, and
> [`jiandu.store/v1alpha4`](store-format-v1alpha4.md) is the current writer
> format. This document remains authoritative for validating and recovering
> the legacy state before the explicit migration publishes its capability
> gate.

This document commits the on-disk compatibility boundary implemented by
`jiandu-store`. Paths described here are private implementation details: MCP
clients and model-visible requests use opaque IDs and typed scopes only.

## Store ownership and metadata

One process exclusively owns a data directory through the single persistent
`LOCK` inode plus the opened root-directory handle. On Unix, an advisory lock
on that directory inode keeps a replaced `LOCK` name from creating a second
cooperative owner in the same directory. On Windows, root and `LOCK` handles
deny delete sharing, and the owning `LOCK` writer also denies write sharing
while permitting read-only diagnostics handles. Those names therefore cannot
be replaced and a second writer cannot open the lock while owned. `LOCK`
contains only a canonical instance UUID, process ID, and start timestamp for
secret-safe diagnostics. It must be a no-follow opened regular file with
exactly one hard link. Releasing a store drops the held handles and advisory
locks but deliberately does not unlink `LOCK`; deleting it could let two
processes lock different inodes.

On Unix, initialization makes the data-directory boundary and fixed private
directories mode `0700` and the `LOCK`/metadata control files mode `0600`.
Opening validates those modes before updating owner diagnostics. A supported
store with widened permissions therefore fails closed rather than silently
repairing access policy during a normal open.

Initialization creates the fixed directory layout and this strict, canonical
`store.json` object:

```json
{
  "formatVersion": "jiandu.store/v1alpha1",
  "storeId": "00000000-0000-4000-8000-000000000000",
  "storeRevision": 0,
  "createdAt": "2026-08-24T00:00:00Z"
}
```

The real `storeId` is a random canonical UUID. `storeRevision` is the
authoritative read watermark; zero represents a newly initialized empty store.
Unknown fields, alternate JSON formatting, non-canonical UUIDs or timestamps,
and malformed metadata fail closed.

Initialization never claims an arbitrary non-empty directory. An empty target
receives a persistent `LOCK` ownership marker and a private
`.store.json.init` file before the fixed empty layout is built. The metadata is
fsynced and atomically renamed to `store.json` only after the layout directories
and their parent entries have also crossed the platform durability boundary.
On Unix this includes directory `fsync`. Windows capability directory handles
cannot be upgraded to the `GENERIC_WRITE` access required by
`FlushFileBuffers`, so file contents are flushed before rename while the
directory step is a documented platform no-op.

After an interruption, a canonical init file is completed with the same store
ID; a truncated uncommitted init file is rolled back and recreated. Any entry
outside the exact empty initialization layout fails without adding Jiandu
artifacts.

Opening inspects `formatVersion` before acquiring or updating `LOCK`. A future
format therefore returns `UnsupportedStoreFormat` without mutating any file or
directory. Opening a supported format validates its complete layout, acquires
the existing lock inode, rechecks `store.json` under the lock, and only then
updates lock-owner diagnostics. Before readiness it reconciles at most one
active transaction and runs a private replace/sync capability probe under
`transactions/`; recognized interrupted probe artifacts are removed and
synced on the next open. Opening and ordinary reads do not change canonical
record bytes or modification times.

The configured data directory is opened one component at a time. User-owned
intermediate links and the final symlink are rejected before creation; only
root-owned links reached through a root-owned, non-writable system-directory
chain may be resolved for platform aliases such as macOS `/var`. The store then
retains that opened root capability. Metadata, layout checks, record opens,
directory enumeration, rename, and supported directory flushes operate relative
to opened directory handles with final-component no-follow semantics. A later
rename or replacement of the configured root invalidates the handle instead of
redirecting reads into the replacement tree.

## Canonical private layout

```text
<data-dir>/
├── store.json
├── LOCK
├── records/
│   ├── principal/<principal-key>/<shard>/<memory-key>.md
│   ├── project/<project-key>/<shard>/<memory-key>.md
│   ├── session/<session-key>/<shard>/<memory-key>.md
│   └── instance_global/<shard>/<memory-key>.md
├── lineages/
├── tombstones/
├── transactions/
├── receipts/
│   └── quarantine/
├── audit/
├── index/lexical.sqlite       # derived v1alpha1 index; never canonical
├── quarantine/
└── backups/
```

Each private key is lowercase hexadecimal SHA-256 over
`jiandu.store.path/v1`, a NUL separator, the domain (`principal`, `project`,
`session`, or `memory`), another NUL, and the exact opaque ID bytes. Domain
separation prevents cross-kind aliases; lowercase fixed-length keys keep
case-sensitive IDs distinct on case-insensitive filesystems and avoid unbounded
path components. `<shard>` is the first two characters of `<memory-key>`.
The record's strict frontmatter scope must map back to the owner key represented
by its location. Its ID must map back to the filename key and shard.

The original owner ID in validated frontmatter is authoritative identity, not a
workspace name or filesystem-derived Project identity. A list resolves public
selectors against host-provided `AuthorizedScopes` first, then traverses only
the resulting owner directories. An invalid or symlinked record belonging to
another principal or an ungranted Project/Session is not opened and cannot leak
through an error.

All internal joins reject absolute paths, parent components, non-UTF-8 names,
symlinks, non-regular record entries, and hard-linked canonical files. Regular
files are opened nonblocking before their actual descriptor type and identity
are validated, so a raced FIFO cannot stall a reader. Public store methods
never return a path.

## Canonical Markdown grammar

A record consists of these bytes:

```text
--- LF
<canonical serde YAML for MemoryFrontmatterV1Alpha1>
--- LF
<exact UTF-8 API body>
LF
```

The final LF is the file terminator, not part of the body. Decoding removes
exactly one byte. If the API body ends in LF, the file ends in two LFs and the
body retains one after parsing. No `trim`, line-ending conversion, Unicode
normalization, or other silent rewrite occurs.

Only the first delimiter after the opening marker ends frontmatter. Later
lines equal to `---` are ordinary Markdown body content and round-trip exactly.

The decoder rejects:

- UTF-8 BOMs, invalid UTF-8, or any CR byte;
- a missing final file terminator;
- a missing or malformed frontmatter terminator;
- unknown or missing strict frontmatter fields (an `etag` header is also
  rejected because the store derives it from canonical bytes);
- values outside the `jiandu-core` validation bounds;
- YAML bytes that deserialize but differ from the canonical re-encoding; and
- documents larger than the store document bound.

The returned ETag is `sha256:<lowercase-hex>` of every canonical file byte.
Frontmatter contains ID and revision, so the digest covers both without trusting
a caller-supplied ETag.

## Validated read behavior

`get` accepts an opaque `MemoryId` and authoritative allowed scopes. It computes
only the canonical candidate path for each allowed scope. Zero candidates and a
record that exists only outside those scopes both return the same `NotFound`.
More than one authorized candidate for a globally unique ID returns the
explicit `DuplicateMemoryId` error.

`list` validates the public request, intersects its selectors with authoritative
scope grants, strictly validates every record under only those selected owner
directories, applies type/status/tag/update filters, and sorts deterministically.
Timestamps are compared as instants, and the memory ID is the final ascending
tie-breaker. Results contain `MemorySummary` values; bodies, provenance,
relations, and paths are not retained in list state.

Each page returns the `storeRevision` observed by the handle. The opaque cursor
is versioned and binds:

- store UUID and revision;
- requested and resolved scopes;
- the complete authoritative-scope fingerprint;
- filters, update watermark, sort order, and page limit; and
- the deterministic next offset.

The token also carries an unkeyed corruption checksum over those cursor
components, so an accidentally edited offset is rejected instead of being
interpreted as another page. It is not an authenticity mechanism: a client can
recompute it. Authorization is re-evaluated on every request, so cursor contents
cannot expand host grants. A keyed cursor MAC can be introduced later if an
adapter needs tamper resistance in addition to authorization.

Malformed or differently bound cursors fail closed. A cursor whose store UUID
or revision changed is stale. With unchanged canonical bytes and metadata, a
cursor resumes at the same record after process restart.

## Authorized create and update

Create and update accept an `AuthorizedScope` capability resolved from the
host's `AuthorizedScopes`; callers cannot place a model-selected Principal,
Project, Session, or instance-global scope directly into the mutation. Create
assigns revision 1. It checks the exact private memory-key filename under every
canonical owner directory, without parsing another owner's body, so the
globally unique `MemoryId` invariant is enforced without returning a path or
foreign content.

Update loads the exact authorized canonical record and holds its opened file
identity through the pre-rename check. `expectedRevision` must match; otherwise
`RevisionConflict` contains only the current positive revision. A successful
update increments the record revision exactly once, requires `updatedAt` to be
at least the previous timestamp, and derives a new content-bound ETag. Scope
moves are not part of this operation.

Each successful create/update also advances `storeRevision` once. The record
and `store.json` are therefore a two-file transaction rather than independent
renames.

## Transaction manifest and commit protocol

The immutable write-ahead manifest is strict canonical JSON, at most 64 KiB,
and has format `jiandu.store.transaction/v1alpha1`. Its filename and embedded
canonical UUID must agree. It contains the store UUID, operation, opaque memory
ID, typed authoritative scope, base/target record revision and ETag, and exact
base/target store metadata. It never contains the Markdown body, title,
provenance payload, an ambient/canonical path, credential, prompt, or model
input. Unknown fields, non-canonical bytes, wrong store IDs, invalid hashes,
oversized manifests, foreign transaction entries, and more than one active
manifest fail closed.

Create/update use this order while holding the single owning lock and an
exclusive mutable store handle:

1. write, flush, and directory-sync a staged manifest, then rename and
   directory-sync the published manifest;
2. create the private owner/shard namespace, stage the canonical record in its
   final shard (therefore on the same filesystem), flush it, and sync the
   namespace;
3. stage canonical target `store.json` in the root, flush it, and sync the
   root;
4. atomically rename the record temp over its target after rechecking the held
   old file identity (or absence for create), then sync the shard;
5. atomically rename the metadata temp over `store.json`, then sync the root;
6. remove the manifest and sync `transactions/`; only then return success.

From the first write-ahead byte until the final manifest cleanup, any I/O or
injected-boundary error poisons the current handle. A poisoned handle returns
`RecoveryRequired` even if a post-rename failure left its in-memory watermark
stale; it cannot serve reads or another mutation and must be dropped/reopened.

Startup classifies the canonical record as exact base, exact target, or
ambiguous and independently classifies `store.json` as exact base, exact
target, or ambiguous:

| Record | Metadata | Recovery |
| --- | --- | --- |
| base | base | Remove staged artifacts and roll back. |
| target | base | Durably publish target metadata and complete. |
| target | target | Clean artifacts and complete. |
| base | target | Fail closed as impossible ordering. |
| ambiguous | either | Fail closed without guessing. |

For create, an absent target is the base state. For update, absence is
ambiguous because atomic replacement cannot legitimately remove the old file.
Recovery never commits merely because a temp or manifest exists and never
invents an acknowledgement. A crash after a durable canonical commit but
before the caller receives it can therefore yield a committed mutation that is
not replay-safe in this legacy format. `v1alpha2` extends this exact manifest
boundary with pre-acknowledgement idempotency/audit durability.

Every persistence boundary has a deterministic failpoint. Tests interrupt and
reopen across manifest write/sync/publish, record and metadata temp write/sync,
both renames and directory syncs, cleanup, recovery itself, quarantine, and the
durability probe. The exhaustive boundary list is guarded so adding a new
boundary without a recovery scenario fails the test contract.

At startup and through `doctor`, Jiandu writes two private files in
`transactions/`, flushes them, atomically replaces one with the other, syncs
the directory, validates the replacement bytes, and cleans the probe. Failure
returns `UnsupportedDurability` before readiness. Unix reports explicit
directory sync. Windows reports the existing documented platform best-effort
directory step while the probe still requires file flush and replacement to
succeed.

## Invalid records and quarantine

Ordinary open/get/list operations are read-only and never repair, normalize,
rename, or quarantine a record. They return stable, path-free error categories
for malformed encoding, filename/ID mismatch, scope/path mismatch, shard
mismatch, duplicate ID, unsafe path, and layout failure.

`quarantine_invalid` is a separate operator-facing primitive. It first proves
that the exact canonical candidate is invalid, then renames it into the existing
private quarantine directory using a random opaque token. The source descriptor
remains open across the capability-relative rename, and the destination file
identity must match the invalid inode that was validated; a raced replacement
fails closed. A valid record is refused, and the receipt contains only the
memory ID and token, never a path or body. Authorization of this administrative
action belongs at the future CLI or transport boundary.

Quarantine uses the same durable manifest before rename. The manifest stores a
SHA-256 digest of the invalid inode, not its body. Recovery distinguishes
source-only (rename did not commit), destination-only (complete), an identical
source/destination duplicate (complete after durably removing the source), and
all mismatched/missing combinations (fail closed). Destination and source
directories are synced in that order. A strict, path-free operator receipt is
then published under `receipts/quarantine/` before the manifest is removed.
The pending receipt ledger survives restart until explicit acknowledgement;
acknowledgement removes only the receipt, never quarantined bytes. This
namespace is intentionally separate from `v1alpha2` idempotency receipts.

Acknowledgement is itself a durable one-file deletion. After resolving the
exact receipt, the live handle is poisoned before unlink, the receipt directory
is synced, the in-memory ledger is updated, and only then is the handle made
serviceable again. Any unlink, sync, or injected-boundary failure requires
drop/reopen. If power loss occurs before the directory sync, the receipt may
reappear as pending or remain deleted; startup reconstructs either outcome from
the durable namespace and never serves the failed handle's stale in-memory
ledger. Quarantined bytes are outside this acknowledgement lifecycle and are
not removed.

The immediately preceding `v1alpha1` reader created `receipts/` without its
`quarantine/` child. Opening accepts that exact legacy shape without mutation,
acquires the exclusive writer lock, then idempotently creates and syncs the
namespaced child before transaction recovery. A crash before its parent sync
may leave the child present or absent; the next open revalidates and syncs the
present form or recreates the absent form. No record, store watermark, or
metadata bytes change. Removing the empty child restores the earlier layout,
and the earlier reader ignores private children beneath `receipts/`, so
canonical record reads remain rollback-compatible. Pending quarantine receipts
must still be operated on by a version that understands this ledger.

## Compatibility and non-goals

`jiandu.store/v1alpha1` readers do not migrate unknown future formats.
`v1alpha2` therefore uses an explicit root-locked migration and checked
conformance fixtures; after its metadata-last capability gate is published, an
older writer fails closed.
The canonical YAML serializer and dependency lockfile are part of drift
protection for this alpha format.

This slice implements single-owner canonical create/update CAS, watermark
replacement, deterministic startup recovery, durability diagnostics, and
recoverable operator quarantine. It deliberately does not claim idempotent
request replay, audit-event atomicity, forget/tombstones, import/export,
search/indexing, MCP transport, Bamboo integration, prompt construction, model
calls, or filesystem-path identity. Those remain separate roadmap issues. The
implemented `v1alpha2` successor extends the transaction before
acknowledgement instead of adding a fallible post-commit callback.
