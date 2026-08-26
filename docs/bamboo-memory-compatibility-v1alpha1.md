# Bamboo Memory Compatibility Corpus v1alpha1

This document freezes the evidence and mapping decisions consumed by the
separate [Bamboo Snapshot Import `v1alpha1`](bamboo-snapshot-import-v1alpha1.md).
The corpus itself does **not** open a Bamboo data directory, mutate either
store, or import Bamboo's Session database.

## Reproducible source evidence

The sanitized corpus models Bamboo commit `f362cc151a2ac097db01178449b9a733929a440f` (tree `a824dfe7e7263c69304ff26ef5826b7abcb0085f`). [`manifest.json`](../crates/jiandu-core/fixtures/migration/bamboo-memory/v1alpha1/manifest.json) records every source artifact as a sorted relative path, byte length, and SHA-256. Its aggregate is SHA-256 over, for each sorted entry, `relative_path`, NUL, decimal byte length, NUL, lowercase digest, newline. It explicitly records `nativeWatermark: null`, `snapshotEvidenceKind: immutable_file_manifest`, and binds the identity/type-override contract digest. The commit, tree, mapping digest, case classification, and corpus schema together are compatibility evidence.

This is format evidence, not a live-store consistency claim. Bamboo has no exclusive snapshot or canonical memory watermark. `generated_at`, `last_reindex`, access logs, `projects/index.json`, and legacy migration journals must not be promoted to one. A future operator must copy a quiesced store and compare complete begin/end scans; changed, missing, symlinked, or non-regular inputs fail closed.

The generator reads only checked-in Jiandu fixtures and writes only stdout:

```sh
cargo run -p jiandu-core --example generate_bamboo_memory_fixture_manifest --quiet -- --check
cargo test -p jiandu-core --test bamboo_memory_fixture_corpus
```

## Format and identity facts

Bamboo durable topics have no frontmatter schema/version field. Version evidence comes from the `memory/v1` path and `scopes/*/state/schema_version.json` value `1`. The pre-granularity/minimal frontmatter shape remains readable. Durable tools support global and project topics; although serde can parse `scope: session`, the durable query/write tools reject it. Session notes and state are therefore separate formats, never a positive durable-topic variant.

`projects/<id>/project.json` is the authoritative Project identity artifact. `projects/index.json` is derived and skipped. Neither a path-hash project key nor a first-class Bamboo `ProjectId` is a Jiandu `projectId`; both require an explicit host/operator mapping. The same applies to principal and Session identity. Workspace paths are provenance aliases only.

Jiandu memory IDs are globally unique. Bamboo's primary-to-legacy alias lookup may silently choose the first matching ID, and same-root `read_dir` order is unstable. Migration must instead inventory the entire candidate set: same-root duplicates are all quarantined; the fixture's divergent primary/legacy pair has no journal authority evidence, so both sides are quarantined; cross-scope collisions are explicit conflicts. Filename/frontmatter ID mismatch and corrupt frontmatter are quarantined. Path-only legacy identity remains unresolved rather than being guessed.

## Outcome vocabulary

| Outcome | Meaning |
| --- | --- |
| `accepted` | Lossless target record after explicit identity resolution. |
| `transformed` | Deterministic, documented semantic conversion. |
| `unresolved` | Valid source needs an operator or future policy decision. |
| `skipped` | Rebuildable, diagnostic, state, view, or out-of-scope artifact. |
| `quarantined` | Corrupt, ambiguous, duplicate, or unsafe input. |

The manifest assigns exactly one outcome and reason code to all 48 source artifacts. It separately covers canonical topics, legacy/pre-granularity and path-only records, duplicate/corrupt records, ledgers, indexes/logs/views, Session notes/state, plans, Project identity artifacts, and migration journals.

## Durable topic field mapping

| Bamboo field | Jiandu field | Rule |
| --- | --- | --- |
| `id` | `id` | Preserve only after syntax and global uniqueness validation; never derive from filename. |
| `title` | `title` | Preserve. |
| Markdown body | `body` | Preserve normalized fixture content. |
| none | `summary` | Leave unset. Bamboo has no authoritative summary; never populate it from derived lexical/index summaries. |
| `type` | `memoryType` | `feedback`, `project`, and `reference` map directly. Bamboo `user` is ambiguous; only a per-record operator override may select `preference`, as the fixture identity map does. |
| `scope` | `scope` | Global requires principal mapping; project requires explicit Project mapping. Session is not accepted as durable memory. |
| `project_key` | `projectId` | Resolve through the checked host identity map; hashes and Bamboo IDs are never copied as authority. |
| `granularity` | tag | Preserve as namespaced `bamboo:granularity:*`; absence is supported. |
| `status` | `status` | Validate and directly map all five shared states: active, stale, superseded, contradicted, and archived. |
| `freshness` | tag | Preserve as namespaced `bamboo:freshness:*`. |
| `confidence` | `confidence` | Explicit high maps to `1.0`, medium to `0.5`, and low to `0.25`; unset remains `None`, because Bamboo's `0.5` recall default is runtime ranking behavior rather than persisted provenance. Unknown strings are unresolved. |
| `created_at`, `updated_at` | timestamps | Parse RFC 3339 and canonicalize offsets; invalid values quarantine the record. |
| `created_by`, `updated_by` | migration evidence | Jiandu `createdBy` is `import`. Preserve Bamboo actor values in the migration report; never guess that a Session actor is a model. The checked identity map states this rule explicitly. |
| `sources[]` | migration evidence/provenance IDs | Preserve raw sources in the migration report. Map only explicitly resolved Session/message IDs into Jiandu provenance; the singular `sourceUri` is reserved for source-file evidence. |
| `message_range` | `provenance.messageIds` | Only with explicit message-ID mapping; never expand or infer a range. Otherwise unresolved. |
| `supersedes` | `supersedes` | Preserve direction after referenced-ID validation. |
| `related` | `related` | Preserve after referenced-ID validation. |
| `contradicted_by` | `contradicts` | **Reverse the edge**: if Bamboo A says `contradicted_by: B`, Jiandu B gets `contradicts: A`. Missing endpoints are unresolved. |
| `tags` | `tags` | Pass through already-valid Jiandu tags. The fixture uses a fixed lowercase-and-hyphen transform for case/space normalization; collisions or otherwise invalid tags require an explicit conflict instead of silent rewriting. |
| retrieval metadata | none | Skip rebuildable counters/index hints. |
| none | `revision`, `etag` | Generated by Jiandu at import time; never copied or fixture-forged as authoritative state. |

Expected mappings are intentionally partial plans. Tests inject synthetic revision/etag values only to validate them against Jiandu's ordinary domain types; no store is opened.

## Other artifact families

| Family | Decision |
| --- | --- |
| Ledger records | `unresolved`: kind, time/recurrence/schedule, dependencies, status transitions, and audit semantics lack a lossless Jiandu memory contract. Minimal legacy and current shapes are both frozen. |
| Ledger indexes/views/audit | `skipped`: derived or diagnostic, never authoritative records. |
| Memory indexes/state/logs and generated views | `skipped`: rebuildable or operational. Tolerant access-log parsing is represented without making logs a watermark. |
| Dream notebook/view prose | `unresolved`: it may be user-authored and cannot be silently treated as derived data. |
| Session notes | `unresolved`: require explicit Session mapping plus split/title/type/provenance policy. Current and legacy note shapes are frozen separately. |
| Session state | `skipped`: lifecycle state, not a durable topic. Legacy dream prose is `unresolved`. |
| Plan markdown/state/cursor/sections | `skipped`: a separate planning subsystem, not memory records. |
| `project.json` | Identity evidence requiring an explicit host/operator map; it is not itself a memory record. |
| `projects/index.json` | `skipped`: rebuildable derived index. |
| Legacy migration journal | `skipped`: operational history, not authoritative source inventory or watermark. |

## Sanitization and future importer boundary

Fixtures contain invented principals, Projects, Sessions, messages, content, and URIs. Validation rejects common credential markers, home-directory paths, real absolute workspace paths, unlisted files, symlinks, and digest drift. No test reads `BAMBOO_DATA_DIR` or depends on a Bamboo crate.

The implemented offline adapter consumes an operator-provided isolated
snapshot and exact identity map, produces this same five-way source report,
consumes the reviewed manifest classifications, and validates every
import-eligible ID for syntax and uniqueness before portable projection. It
leaves Bamboo read-only; this focused four-record slice does not re-parse
quarantined or intentionally unsupported durable artifacts to derive a second
classification.
Its independent destination classifications and reviewed portable-import
contract are documented in
[Bamboo Snapshot Import `v1alpha1`](bamboo-snapshot-import-v1alpha1.md).
Session database migration, live cutover, and dual-write remain outside both
this corpus and the importer slice.
