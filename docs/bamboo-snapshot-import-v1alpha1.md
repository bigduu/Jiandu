# Bamboo Snapshot Import `v1alpha1`

`jiandu-bamboo-import` is the host/operator adapter for importing the frozen
Bamboo filesystem-memory corpus into a new Jiandu store. It is deliberately
outside `jiandu-core` and `jiandu-store`: Bamboo DTOs, paths, actor shapes, and
classification policy do not become agent-neutral domain or persistence
contracts.

This slice implements deterministic planning and one all-or-none import. It
does not perform a Bamboo cutover, read a live Bamboo directory, run shadow
reads, add proactive recall, dual-write, import conversations, or expose a CLI
or MCP tool.

## Entry points and authority

The adapter has two host-facing functions:

- `plan_bamboo_snapshot` reads an explicit isolated snapshot and returns a
  `BambooDryRun` containing a strict reviewed plan, an authorized migration
  report, the canonical Jiandu portable-import plan, and a private canonical
  bundle;
- `commit_bamboo_snapshot` accepts the exact canonical reviewed-plan bytes and
  an idempotency key, regenerates all source-derived artifacts, and delegates
  the only durable mutation to `CanonicalStore::import_portable`.

Planning and commit require fresh authority for every projected Principal and
Project scope, the corresponding `memory:import:*` grants, and the independent
`memory:admin:validate_store` and `memory:admin:export_all` grants. The adapter
does not infer Principal, Project, or Session identity from a path.

The API accepts only the explicit snapshot root supplied by the operator. It
does not read `BAMBOO_DATA_DIR`, discover a Bamboo installation, acquire a
Bamboo lock, remove a Bamboo temporary, or invoke Bamboo cleanup/migration
code. The snapshot must therefore be an already quiesced, isolated copy. The
separate Bamboo operational procedure owns creation and retention of that
copy.

## Read-only snapshot contract

The expected root contains the checked `manifest.json`, `source/` tree, and
canonical identity map from the compatibility corpus. Bamboo has no native
canonical watermark, so `nativeWatermark` remains `null`. Evidence instead
binds the frozen Bamboo repository commit/tree, mapping-contract digest,
compatibility-manifest digest, and the aggregate of every source artifact.
This `v1alpha1` adapter exact-pins repository `bigduu/Bamboo-agent`, commit
`f362cc151a2ac097db01178449b9a733929a440f`, and tree
`a824dfe7e7263c69304ff26ef5826b7abcb0085f`; a different Bamboo codec/version
requires a separately reviewed compatibility contract rather than relabelling
its commit as this corpus.

The adapter holds a directory capability and performs a bounded, sorted scan
of all 48 `source/` files, including cases classified as skipped, unresolved,
or quarantined. It:

- rejects absolute, parent, backslash, non-UTF-8, duplicate, or changed names;
- opens directories and files relative to the held capability without
  following symlinks;
- accepts only single-link regular files and ordinary directories;
- bounds traversal to 1,000 total directory entries (files and directories),
  at most 1,000 regular source artifacts, 16 directory levels, and 64 MiB;
- requires the scanned directory set to equal the parent directories implied by
  the frozen file manifest, so an extra empty directory is source drift;
- captures and rechecks root, directory, filename, and opened-file identity;
- reads with nonblocking semantics on Unix so a substituted special file
  cannot stall the scan; and
- repeats the full source scan and exact manifest/identity-map reads before
  returning.

Missing files, byte/length/digest changes, root or name replacement, and
begin/end disagreement fail closed. Planning has no source write handle and
does not create a lock, marker, report, temporary, or cleanup artifact in the
snapshot.

## Two independent classification axes

The migration report intentionally keeps two axes. They must not be collapsed
or translated into each other.

The frozen source-mapping axis from the compatibility corpus is:

| Source outcome | Meaning |
| --- | --- |
| `accepted` | Lossless durable mapping after explicit identity resolution. |
| `transformed` | Durable mapping with the documented deterministic conversion. |
| `unresolved` | Valid source outside this import slice or requiring future policy. |
| `skipped` | Rebuildable, operational, derived, or intentionally out-of-scope input. |
| `quarantined` | Corrupt, ambiguous, duplicate, or otherwise unsafe source. |

The independent Jiandu destination axis is the existing canonical portable
import classification:

| Destination classification | Meaning |
| --- | --- |
| `accepted` | The mapped ID is absent and the exact scope is authorized. |
| `conflicting` | A target record already owns the global ID. |
| `unauthorized` | Fresh exact-scope import authority is absent. |
| `tombstone_protected` | A target tombstone protects the global ID. |
| `invalid` | The portable batch violates its committed bound. |

Every one of the 48 source artifacts remains in the authorized report with its
relative logical name, SHA-256, frozen source outcome/reason, and mapping
disposition. Only the four `accepted`/`transformed` durable-topic cases are
import-eligible. Intentionally unsupported source `unresolved` cases do not by
themselves make planning fail. Commit is noncommittable specifically when an
import-eligible record has unresolved identity or an invalid mapping; it is
never silently relabelled `skipped`.

The reviewed manifest is the classification input for this focused slice.
The adapter validates syntax and uniqueness across every import-eligible ID,
but does not re-parse quarantined or intentionally unsupported durable files to
invent a second classification. Their exact bytes are still fingerprinted and
drift-bound like every other source artifact.

The manifest's repository, source inventory, case IDs/outcomes/reasons, format
evidence, and expected mappings are byte-frozen to #36. Only the three fields
that bind the separately reviewed host identity-map bytes (its digest and byte
metadata in the two manifest locations) may vary; the adapter normalizes just
those fields and requires the remainder to match the frozen manifest digest.
The actual unnormalized manifest and actual identity-map digests are both
carried into the reviewed plan, so changing or removing one mapping still
changes the high-level plan and must be reviewed.

`conflicting` and `tombstone_protected` describe only target state. The frozen
Bamboo corpus contains no source tombstones.

## Deterministic projection and mapping

The four eligible records are projected through the rules frozen in
[Bamboo Memory Compatibility Corpus `v1alpha1`](bamboo-memory-compatibility-v1alpha1.md).
The implementation preserves the exact expected mapping for ID, scope, type,
status, title, absent summary, trimmed fixture body, normalized tags,
timestamps, provenance, confidence, and relations. In particular:

- the canonical identity-map bytes and digest select the Principal, Project,
  Session, and per-record `user`-type override;
- tag normalization is deterministic, but a normalization collision fails
  instead of silently deduplicating values;
- RFC 3339 offsets canonicalize to UTC `Z`;
- `createdBy` becomes `import`, while raw `created_by`, `updated_by`, and
  `sources[]` values remain authorized report evidence;
- the logical `bamboo-memory://snapshot-v1/...` source URI and exact source
  digest retain source-file provenance without making a path authoritative;
- `contradicted_by` is reversed onto the contradicting Jiandu record; and
- revision `1` and the exact canonical ETag are produced by Jiandu's ordinary
  canonical document round trip, not copied from Bamboo.

The adapter builds an in-memory `jiandu.portable-export/v1alpha1` projection.
Its deterministic `sourceStoreId` identifies this canonical virtual
projection; it does **not** claim that Bamboo is a Jiandu store. Independent
Bamboo repository, manifest, aggregate, and mapping evidence remains bound in
the adapter plan and report.

## Reviewed plan and pristine destination

`BambooReviewedPlan` uses strict pretty JSON plus one final LF, denies unknown
fields, and carries a domain-separated SHA-256 digest. It is body- and
path-free. It binds:

- the complete source evidence and exact mapping/manifest digests;
- projection identity and watermark;
- target store ID and base store/audit watermark;
- target whole-store validation and export digests;
- authorized report digest;
- canonical portable bundle and portable-import plan digests;
- eligible/mapped counts; and
- pristine, mapping-complete, portable-committable, and final committable
  decisions.

A commit target must be a compatible canonical v4 store in its pristine state,
not merely a store without conflicting IDs. Whole-store validation must be
complete and clean; revision and audit sequence must both be zero; and the
portable export must contain no scopes, records, or tombstones. An unrelated
record with a different ID therefore still rejects commit before portable WAL
entry.

For a fresh commit, the adapter decodes exact reviewed bytes, rescans the
snapshot and identity map, regenerates the report, bundle, and portable plan,
and requires byte-identical reviewed-plan bytes before its first durable
action. An acknowledgement-recovery retry cannot regenerate a pristine target
plan after the original commit; instead it reconstructs every pristine-base
field that remains derivable and uses the canonical store's read-only replay
seam to require the receipt fingerprint to match the original bundle and
portable-plan digest. Receipt absence returns to adapter preflight without
fresh portable planning or WAL entry. Source drift,
changed or removed mapping bytes, stale target evidence, nonpristine or
incompatible target state, unresolved eligible identity, target conflict, and
tombstone resurrection are preflight rejections with no adapter partial apply.

## Commit, acknowledgement, and evidence recovery

The only write path is the canonical v4 portable-import WAL. It commits all
four records as one metadata-last batch and returns its receipt-bound
`ImportCommit` and backup metadata. Jiandu startup recovery and receipt replay
remain authoritative for old-or-complete crash semantics.

After a durable import, validation/export evidence generation is an
acknowledgement step, not a second persistence protocol. If that step fails,
the adapter returns `CommittedEvidenceUnavailable`, explicitly carrying the
body/path-free transaction, import-result, and backup-metadata digests. It does
not return an ordinary error that implies the destination is unchanged.

Retrying the exact source, reviewed plan, principal, idempotency key, bundle,
and portable-plan digest gives canonical receipt replay precedence over fresh
target conflicts. The adapter reconstructs and verifies every pristine-base
field in the reviewed plan, including the original whole-store validation and
export digests, before replay. A changed reviewed plan is stale even if a
receipt exists. Exact replay returns the original transaction and does not add
a second store revision or audit event.

On success, the adapter runs authoritative whole-store validation and export,
then returns strict `jiandu.bamboo-cutover-evidence/v1alpha1`. The evidence is
body/path-free and binds source/mapping, reviewed plan/report/bundle/portable
plan, base/target watermarks, transaction, import result, backup metadata,
validation, and export digests. It provides rollback review and Bamboo cutover
inputs only. This issue adds no restore, rollback, retention, or cutover
executor.

## Privacy and diagnostics

Raw standalone relative-path fields and raw Bamboo actor/source values are
confined to the explicitly authorized migration report. Imported records—and
therefore an explicitly authorized portable export—retain only the documented
logical `bamboo-memory://snapshot-v1/...` source URI; it is never an ambient or
physical user path. Reviewed plans, cutover evidence, commit debug output, and
normal errors contain no record body, raw relative/ambient path, credential,
raw idempotency key, or raw private store artifact. Store failures are reduced
to closed `StoreErrorCode` categories. The dry-run and commit types use
manually redacted `Debug` implementations with only digests, counts, and
transaction metadata.

## Verification boundary

Behavior tests cover byte-identical repeated 48-artifact planning, exact four
record mapping, source and destination zero-write planning, ignored ambient
`BAMBOO_DATA_DIR` tripwires, source byte/symlink/hardlink drift, exact directory
inventory, global entry and recursion-depth bounds, changed mapping bytes,
unrelated nonempty and incompatible targets, target conflict and tombstone
overlays, strict plan tampering, authoritative validation/export, safe
diagnostics, explicit post-commit evidence failure, and exact-key receipt
recovery.

The acceptance slice ends at the deterministic offline import boundary. Shadow
reads, Bamboo runtime adapters, proactive prompt injection, dual-write,
conversation/Session database migration, final quiescence/copy tooling,
cutover, rollback execution, and an administrative CLI remain separate issues.
