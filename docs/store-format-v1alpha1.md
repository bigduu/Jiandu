# Canonical Store Format `v1alpha1`

This document commits the on-disk compatibility boundary implemented by
`jiandu-store`. Paths described here are private implementation details: MCP
clients and model-visible requests use opaque IDs and typed scopes only.

## Store ownership and metadata

One process exclusively owns a data directory through the single persistent
`LOCK` inode plus the opened root-directory handle. On Unix, an advisory lock
on that directory inode keeps a replaced `LOCK` name from creating a second
cooperative owner in the same directory. On Windows, root and `LOCK` handles
deny delete sharing, so those names cannot be replaced while owned. `LOCK`
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
updates lock-owner diagnostics. Opening and ordinary reads do not change
canonical record bytes or modification times.

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
├── audit/
├── index/
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
- unknown or missing strict frontmatter fields, including `etag`;
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

## Compatibility and non-goals

`jiandu.store/v1alpha1` readers do not migrate unknown future formats. A future
format requires an explicit migration implementation and conformance fixtures.
The canonical YAML serializer and dependency lockfile are part of drift
protection for this alpha format.

This slice deliberately does not implement record mutations, transaction
recovery, tombstones, search/indexing, MCP transport, Bamboo integration,
prompt construction, model calls, or filesystem-path identity. Those remain
separate roadmap issues.
