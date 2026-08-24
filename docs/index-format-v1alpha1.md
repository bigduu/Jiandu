# Lexical Index Format `v1alpha1`

Status: implemented by `crates/jiandu-index` for Jiandu Issue #6.

This document commits the deterministic tokenizer, logical index image,
ranking, authorization, cursor, diagnostics, and rebuild compatibility rules.
The index is disposable derived data. Canonical Markdown records, protected
tombstones, store metadata, and the mutation ledger remain authoritative.

## Version and location

- logical format: `jiandu.index.lexical/v1alpha1`;
- fixed private filename: `index/lexical.sqlite` beneath a Jiandu data layout;
- SQLite `application_id`: `0x4a494458` (`JIDX`);
- SQLite `user_version`: `1`;
- journal mode while constructing: `DELETE`;
- page size: 4,096 bytes;
- maximum image size accepted by the reader: 2 GiB;
- maximum indexed records: 10,000.

The caller configures only the private `index/` directory. `LexicalIndex` joins
the fixed filename internally, refuses a symlink/non-directory/shared-mode
parent, and never chmods an existing parent. On Unix the directory must be
owner-private and the final file must be an owner-private, single-link regular
file. These are local protection rules, not public identity.

The carrier has exactly these non-internal SQLite objects and constraints:

```sql
CREATE TABLE index_metadata (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    value BLOB NOT NULL
) STRICT;
CREATE TABLE documents (
    memory_id TEXT PRIMARY KEY NOT NULL,
    scope_key TEXT NOT NULL,
    value BLOB NOT NULL
) STRICT;
CREATE INDEX documents_by_scope ON documents(scope_key, memory_id);
```

Both blobs are strict, canonical, compact JSON. Metadata contains format
version, source store ID/revision, document count, and a lowercase SHA-256
logical content checksum. A document contains the canonical public
`MemorySummary`, one internal exact-scope key, and sorted unique weighted terms.
It contains no raw body, provenance, ambient/canonical path, credential, raw
idempotency key, query, receipt, result, audit, WAL, or witness bytes. Weighted
tokens are still derived from potentially sensitive body text, so the index is
private even though raw bodies are absent.

## Rebuild and tenancy

There is one all-store index, not one index per caller or authority view. The
host authenticates a trusted principal and must independently grant
`memory:admin:rebuild_index`. That private-field capability is the only input
accepted by the `CanonicalRecordReader::read_index_snapshot` seam.
`CanonicalStore` then returns all validated, non-forgotten records at one
stable store ID/revision or returns no snapshot. A tenant-scoped caller cannot
choose the permanent index coverage.

Rebuild sorts documents by opaque memory ID, creates a fresh SQLite image in
the same private directory, commits and closes SQLite, fsyncs the image, and
uses the maintained cross-platform `atomicwrites` replacement primitive. It
then syncs the directory where the platform exposes directory fsync. No live
SQLite connection is carried across replacement. Rebuilding identical logical
input with this format and bundled SQLite version produces byte-identical
output on supported platforms; CI locks an exact SQLite-image SHA-256 and runs
the replacement path on Windows as well as Linux.

If replacement fails, the previous complete image remains authoritative for
health evaluation; canonical storage is unchanged. Deleting the image is safe
and rebuilding it requires no store migration. The canonical store format is
therefore not bumped for an index-only format change.

## Deterministic tokenization

Tokenizer version `v1alpha1` applies, in order:

1. Unicode NFKC normalization;
2. Unicode lowercase expansion;
3. punctuation/whitespace separation;
4. contiguous non-CJK alphanumeric words;
5. CJK/Kana/Hangul unigrams plus adjacent bigrams, in source order.

Query terms are deduplicated before scoring. A query that produces no token is
invalid. The checked
`crates/jiandu-index/fixtures/v1alpha1/tokenization.json` fixture covers full
width Latin, canonical composition, mixed scripts, punctuation, and CJK
unigrams/bigrams.

## Indexed fields and weights

Every token occurrence adds the following integer weight:

| Field | Weight |
| --- | ---: |
| title | 12 |
| tags | 10 |
| summary | 8 |
| body | 4 |
| memory type | 2 |
| lifecycle status | 2 |
| exact scope kind/opaque ID | 2 |
| `updatedAt` and record revision metadata | 1 |

Weights are accumulated per document/token without floating-point ranking.
Search applies structured type, status, all-requested-tags, and strict
`updatedAt > updatedAfter` filters. Nonzero results sort by raw integer score
descending, then opaque memory ID ascending. Only after sorting, public
`SearchScore` is `raw / highest_raw` in the complete filtered result set, in
the committed `[0, 1]` API range.

## Query authorization and privacy

Normal search never accepts a forgeable vector of resolved scopes. The host
provides its current `AuthorizedScopes`; the store resolves the exact public
selectors and returns a private-field `AuthorizedIndexQuery`. It binds:

- exact selected authoritative scopes;
- the authenticated Principal;
- the complete current Project and Session grants;
- current `instance_global` authority; and
- the exact selector sequence used by the public request.

Search first validates the complete private derived image (schema, SQLite
integrity, strict rows, exact count, and checksum). It then filters the
in-memory documents by the capability's exact scope keys before scoring or
producing a hit. No inaccessible summary, term, scope key, count, or diagnostic
is emitted. A cross-tenant unique token returns an ordinary empty result.

The source store watermark is read before and after the search. The index must
match the initial store ID/revision, and the store watermark must remain
unchanged. A concurrent mutation therefore yields a safe stale/degraded error,
not a mixed page.

## Cursor authenticity versus corruption detection

Public page cursors are canonical JSON envelopes encoded as unpadded base64url.
The payload binds offset, normalized query/filter/limit fingerprint, complete
authority fingerprint, source store ID/revision, and index content checksum.

Two mechanisms deliberately have different jobs:

- an unkeyed, domain-separated SHA-256 checksum detects accidental payload
  corruption only;
- a standard HMAC-SHA256 tag, verified with the maintained `hmac` crate's
  constant-time `verify_slice`, provides authenticity.

Recomputing the checksum after changing offset or authority does not create a
valid cursor. A wrong key, changed request, or changed authority is invalid;
a changed store/index watermark is stale. The HMAC key is host configuration,
is never serialized or debug-printed, and should be rotated according to host
policy. Future adapters that expose cursor tokens across a trust boundary must
continue to use a keyed construction; the internal index checksum can never be
promoted into an authenticity mechanism.

## Health, corruption, and compatibility

`LexicalIndex::diagnose` returns a path-free closed health state:

- `Missing` — the fixed image does not exist;
- `Corrupt` — unsafe file shape, SQLite failure, schema/row/canonical/checksum
  mismatch, or unsupported extra objects;
- `IncompatibleVersion` — SQLite user/logical format version is unknown;
- `Stale` — source store ID/revision differs;
- `SourceUnavailable` — the canonical source cannot provide a safe watermark.

All index-only degraded states are observable and rebuildable through the same
admin API. `SourceUnavailable` requires the canonical source to recover before
diagnosis or rebuild can proceed. Search fails safely while degraded.
`CanonicalStore::get` and canonical list do not depend on the index; they
continue to work for index-only degradation, but may independently fail if the
canonical source itself is unavailable.

Readers do not reinterpret an unknown version and there is no index migration:
delete/rebuild is the compatibility path. A future tokenizer, schema, checksum,
or ranking semantic change requires a new `jiandu.index.lexical/*` version and
must not silently accept a `v1alpha1` image.

## Bounds and non-goals

- at most 16 exact query scopes, inherited from the public contract;
- at most 100 hits per page;
- at most 131,072 distinct weighted terms and 2 MiB canonical derived JSON per
  document;
- at most 16 KiB canonical index metadata;
- cursor payload at most 768 decoded bytes and 1,024 public base64url bytes.

This slice adds no CLI, MCP transport, daemon, network request, embedding,
semantic model, LLM/provider credential, Bamboo mapping, remote index, or
canonical-store mutation. Admin rebuild and diagnostics are ordinary Rust APIs
reserved as seams for later CLI/service Issues.
