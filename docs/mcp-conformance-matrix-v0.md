# MCP Public Conformance Matrix v0

## Ordinary contract (#10)

Issue #10 runs one public-contract suite independently through two clients and
then connects both to one temporary daemon/store for interoperability. The
versioned source of expectations is
`crates/jiandu-service/fixtures/conformance/v1alpha1/manifest.json`.

| Boundary | Locked version | Driver |
| --- | --- | --- |
| MCP | `2025-11-25` | both |
| Jiandu API | `jiandu.dev/v1alpha1` | both |
| Jiandu harness/package | `0.1.0` | both |
| Official client | `rmcp 3.1.4` | A |
| Independent HTTP client | `reqwest 0.13.4` | B |

Both drivers cover discovery and checked schemas, resources, exact/list/search
reads and pagination, single-record mutations, closed ordinary errors, fixed
HTTP 401, and exact scope isolation. The joint case covers bidirectional
create/read/update, ordinary concurrent reads and nonconflicting writes,
revision CAS, independent forget authority, and post-forget convergence.

The raw driver uses only HTTP and `serde_json`. Current rmcp legacy sessions
frame each POST as one bounded SSE response event even with JSON-response
preference enabled; the driver decodes exactly that event and implements no GET
stream, reconnect, resume, `Last-Event-ID`, or keepalive lifecycle.

## Resilience contract (#34)

The same isolated harness now also runs a separate black-box resilience slice:

| Boundary | Public evidence |
| --- | --- |
| Concurrent CAS | Official rmcp and raw HTTP race one shared revision; exactly one update commits and the loser receives `REVISION_CONFLICT`. |
| Concurrent idempotency | Official rmcp and a raw credential for the same principal produce one durable result plus one exact replay; conflicting input under one concurrent key produces one commit plus one `IDEMPOTENCY_CONFLICT`. |
| Exact scope | An authorized private update can race an inaccessible update without widening the latter beyond path/body/identity-free `NOT_FOUND`. |
| Lost acknowledgement and restart | The raw requester drops an unread successful response body, the official client observes the durable record, the session terminates, and repeated daemon restarts replay the exact record, revision, watermark, and durable correlation. |
| Disposable index loss | Removing only the sandbox `index/lexical.sqlite` keeps readiness, get, and list available while search returns retryable, path/query-free `INDEX_DEGRADED`. |
| Disposable index corruption | Corrupt bytes produce the same public search failure as complete loss; an operator-authorized public rebuild restores byte-identical index bytes and deterministic public search results. |
| Writer contention | A second daemon using the same isolated data directory returns only the permitted owner tuple and leaves every store byte unchanged. |
| Complete service absence | Official and raw initialization plus known/absent raw requests receive no fabricated HTTP/MCP memory response and disclose no identity or record-existence distinction. |

Every destructive fixture action is guarded by an exact fixed-file assertion
beneath a nested `TempDir` store, and every daemon binds `127.0.0.1:0`. The
matrix closes sessions explicitly before lifecycle transitions. Arbitrary
active-session admission/drain bounds remain #29; Bamboo behavior, remote
transport, load testing, and semantic-quality evaluation remain out of scope.
