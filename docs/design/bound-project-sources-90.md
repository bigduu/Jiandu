# Native Project source references (#90)

This first slice supplies a read-only Jiandu adapter seam for a host's single
Project/Work context. It does not implement Bamboo prompt assembly or recovery.
The workspace still has two crates, one store, and one unchanged memory MCP tool.

`MemoryStore::query_project_sources_read_only(project_id, query)` returns at most
three Active compact hits, without Global fallback, Session reads, access-log
writes, automatic maintenance, model calls, or document hydration. Blank queries
are rejected. Its result has `items` and `more_available`, with no pagination or
incremental cursor. A hit is discovery; its index summary is not versioned proof.

`MemoryStore::get_project_source_read_only(expected)` reads exactly the reference's
Project locator. It never substitutes a same-ID Global memory. It returns:

- `version_matched`: complete title/body and the current reference, with unknown
  confidence. The body is limited to 6000 Unicode characters.
- `version_mismatch`: the observed reference, with no body; the host must choose
  whether to adopt a new source through its existing context authority.
- `missing`, `inactive`, or `over_budget`: no body and a clear reason. Inactive
  includes Stale, Contradicted, Superseded, and Archived even if the host pins the
  inactive record's version. Evidence is never silently truncated.

`ProjectMemoryRef` serializes only `project_id`, `memory_id`, and `content_version`.
Its constructor/deserializer validates opaque identity and the version's shape.
The `source_key()` form is `jiandu:project:<ProjectId>:memory:<MemoryId>`. Neither a
key nor a digest grants Project access. Native callers are trusted host code;
the existing MCP host metadata and model argument policy remain unchanged.

To capture a reference for explicit source adoption, the host can call
`ProjectMemoryRef::from_document` on a host-selected current Project document.
Global documents are rejected. The reference must be placed into the host's
existing authoritative source map only after the host's normal validation/CAS.
The read adapter never adopts it on the host's behalf.

The v1 version is `jiandu-content-v1:sha256:<64 lowercase hex characters>`. It
hashes the domain separator `jiandu-project-memory-content-v1\0`, followed by
UTF-8 `serde_json` serialization of the tuple `(complete typed frontmatter, body)`.
Field ordering follows the current typed structs; filesystem paths are excluded.
Metadata, status, source coverage, and content changes invalidate the reference.
Incompatible semantic serialization changes require a new version prefix.
This is a consistency digest, not a signature, event watermark, source truth
rating, permission, checkpoint, or guarantee that the source cannot change later.

For Bamboo's next integration slice, an outer adapter can select a current Work
through TicketService, derive the authorized Project, and select a hit using the
short query. It then constructs `ProjectMemoryRef` from the already-adopted
`ProjectContextSnapshot.sources` key/version and performs exact get. The resulting
reference can travel in a host handoff alongside canonical Assignment/context/
receipt/checkpoint locators. Jiandu knows none of those Task states or locators.
A missing source pin is unverified, never an invitation to silently adopt the
latest observation. Worker reads retain the host's existing redaction rules.

Under Bamboo #2050, tickets owns these persistent facts and lawful transitions;
orchestration chooses retrieval/recovery policy; an outer adapter calls this
native API. This Jiandu slice has no Bamboo dependency and does not create an
orchestration crate, duplicate Task state, or add a new runtime loop.

Version-matched content remains supporting context. Legacy high confidence and
Session message ranges cannot establish truth or acceptance. The adapter returns
unknown confidence and does not expose message ranges as claim evidence. Before
execution, the host must recheck the actual adopted context, source relevance,
generation, fresh permissions, original receipts, resource claims, and budget.
Unknown side effects are reconciled through the original operation key. No
summary/reference can grant tools, accept a Submission, or replay a checkpoint.

No durable schema, metadata writer, ingestion receipt, invalidation authority,
incremental feed, model policy, or installation is changed. Source metadata and
the live Bamboo consumer remain separate follow-up slices of #90.

Synthetic integration tests in `bound_project_sources.rs` cover query/get/handoff
restart, content updates between reads, source-map references, top-three and
inactive filtering, strict Project isolation without Global/Session fallback,
unknown legacy confidence, body budgets, cold-root non-mutation, and malformed
reference rejection. They do not claim an actual Bamboo runtime recovery test.
