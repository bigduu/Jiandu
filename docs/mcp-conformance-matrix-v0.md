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
resilience matrix closes sessions explicitly before lifecycle transitions.
Bamboo behavior, remote transport, load testing, and semantic-quality
evaluation remain out of scope.

## Bounded shutdown contract (#29)

The service library adds a deterministic in-process lifecycle matrix around
the same real rmcp transport and canonical store:

| Boundary | Evidence |
| --- | --- |
| Admission linearization | A paused authenticated initialize owns the atomic HTTP permit; synchronous drain closes the gate, preserves invalid-token 401 precedence, and returns one fixed redacted 503 to a later valid request. |
| Concurrent normal drain | Independent rmcp sessions enter a canonical mutation and exact list; shutdown waits for both finite response bodies, returns `Drained`, and releases the store at the committed watermark. |
| Normal final-frame flush | A deterministic pause occurs after `PermitBody` produces its terminal frame but before Hyper can finish the response. Shutdown does not cancel connection I/O or report `Drained` until release lets the client receive the complete body. |
| Whole-grace deadline | A deterministic post-idle cleanup delay exceeds the configured response grace; outcome is `ForcedAfterTimeout`, sessions close, and the delay cannot be misreported as `Drained`. |
| Incomplete authenticated upload | A raw HTTP/1.1 peer sends valid authorization and headers but only part of its declared JSON body. Forced timeout cancels the accepted socket, releases its HTTP permit, writes nothing, and reopens the singleton store while the peer object is still alive. |
| Readiness ownership | A cloned sanitized health observer remains live after shutdown while the singleton store is reopened immediately, proving readiness cannot retain the canonical backend or writer lock. |
| Runtime-worker saturation | With one Tokio worker, a mutation holds the canonical writer while an exact read waits behind it on the blocking pool. The async deadline still reaches forced state before an independent OS-thread fail-safe releases the writer. |
| Forced before WAL | Policy pauses after normal admission but before the final lifecycle check. Timeout removes HTTP/session work, the final check writes no WAL/artifact/watermark, and restart sees revision zero. |
| Forced after WAL | A mutation pauses at the metadata-rename durability boundary. Timeout removes transport acknowledgement while a competing owner still sees `StoreLocked`; after release, the detached supervisor quiesces the worker and immediate restart recovers one exact replay. |
| Cancelled shutdown waiter | Dropping the caller's shutdown future cannot drop the already-spawned supervisor; the singleton lock is eventually released. |

All blocking pause fixtures have bounded waits plus idempotent release-on-drop
guards, so an assertion failure cannot strand CI. These tests do not claim that
Tokio can kill a synchronous fsync/rename worker. The configured deadline
bounds response/session grace; explicit shutdown returns only after any entered
canonical lease has ended and the store owner has been dropped.
