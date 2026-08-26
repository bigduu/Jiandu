# MCP Ordinary Conformance Matrix v0

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

Restart/retry/index-degradation chaos remains #34, bounded shutdown remains
#29, and Bamboo, remote transport, and hosted/load integration remain outside
this matrix.
