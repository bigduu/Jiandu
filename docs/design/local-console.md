# Read-only local console

## Acceptance slice

Issue #70 is a 4–8 hour slice: `jiandu ui` launches a local browser view of the
same canonical data root as every agent. Its scope is the existing MCP crate's
CLI, a small loopback HTTP adapter, embedded HTML/CSS/JS, the memory crate's
read-only query/Session inventory APIs, focused tests and browser acceptance.
Project discovery is the separate prerequisite in Issue #69. The per-call MCP
context work in PR #68 is independent; the console does not bind a host context.

Editing, lifecycle operations, rebuild controls, remote serving/accounts,
MCP-over-HTTP, storage migration, an extra crate/store/index, agent integrations,
Dream model/scheduling, publication and installed-runtime replacement are out
of scope. The first console lists first-class Projects only. Legacy directory
inventory and native access remain unchanged.

## Launch and authority

`jiandu ui [--data-dir PATH] [--port PORT]` selects the canonical root (default
the user's `.jiandu`) and binds `127.0.0.1`. An OS-selected port is the default;
the command prints its actual URL and runs until Ctrl-C. Missing roots remain
missing when browsed. Identity and host/bind flags are rejected for this command.

The user launching a local console chooses the root to inspect and may switch
among its discovered Projects and Sessions. This human-facing scope selection
does not change the MCP tool schema, host context, or Project authority. Each
HTTP request chooses its own scope and the adapter creates a typed Project
view explicitly; no last-selected Project is retained by the server. Detail
lookup also verifies the returned document's scope, avoiding the native API's
Project-to-Global fallback in a Project-only view.

HTTP accepts only the printed Host authority, the matching Origin when present,
and same-origin/none Fetch Metadata. API calls require the console request
header; there is no CORS allowance. Only GET/HEAD are exposed. Bundled assets
receive no-store, nosniff, no-referrer and a self-only CSP with frame embedding
disabled. This is a local UI rather than a remote authenticated service.

## Read path

The HTTP routes map directly to MemoryStore catalogue, query, get, inspect,
index, Dream and Session reads. Axum handles HTTP parsing/routing only; there is
no generic backend abstraction, persistence layer or Node build pipeline. See
[the upstream Axum API](https://docs.rs/axum/0.8.9/axum/) for the transport API.

`query_scope_read_only` shares all query validation, lexical/BM25/CJK ranking,
filters, limits and pagination with `query_scope`. The only difference is that
it omits access-log writes, so human browsing does not affect agent usage or
maintenance signals. Ordinary agent query behavior remains unchanged.

Project catalogue entries come from the typed-only inventory API. Session
inventory accepts validated, real directories with persisted notes/state,
ignoring incomplete/invalid entries and symlinked components. Inventory APIs
do not grant an MCP host broader access or introduce new metadata on disk.

Durable browsing returns twenty compact records per page; details fetch only
the selected body. Blank queries read canonical records even if the index is
missing. Nonblank queries use the existing lexical index and surface its
missing/invalid error for nonempty stores. Cold empty scopes return no matches
without building anything. Session search reads existing notes, filters topic
names/content, and pages summaries; details use the validated topic reader.

Status independently reports counts, index readability and Dream freshness.
Missing Dream is a normal cold state; an invalid/missing generation marker is
reported without blocking canonical browsing. Refresh observes current files;
the routes do not claim a transaction across concurrent agent writes.

## Browser behavior

Global, Project and Session have separate navigation and explicit selectors.
Search/filter/scope changes reset paging and clear prior results/details/status.
Request cancellation and generation checks prevent late list, status or detail
responses from overwriting the current selection. Status failure is independent
from list failure. Refresh also reloads the catalogue for newly persisted IDs.

Bodies, notes, Dream prose, titles, tags and provenance are rendered as text.
Stored Markdown is displayed with wrapping rather than executing stored HTML
or loading remote resources. The layout adapts to a narrow phone viewport;
loading, empty, missing-index and server-error states have visible explanations.

## Verification

Native tests verify read-only query bytes and retained agent-access behavior,
and Session discovery/cold-store/symlink semantics. Real executable tests start
the CLI, use bounded TCP HTTP calls and clean up child processes. They cover
embedded assets, cold roots, Project and Session switching, CJK search,
status filtering/paging, Dream freshness, invalid/foreign/write requests and
byte-for-byte unchanged canonical/derived data. Browser acceptance uses an
isolated fixture root populated through the memory API/MCP and captures
desktop and phone layouts. All required Rust workspace gates still apply.

Local acceptance passed all Rust gates (157 tests, one existing ignored manual
benchmark), independent review, desktop/phone interaction, CJK search, Session
switching, stored-HTML text safety and delayed-response isolation. All 58 fixture
files retained identical hashes. Ctrl-C released the listener. The screenshots
use isolated demonstration data: [desktop](../images/local-console-desktop.png)
and [phone](../images/local-console-phone.png).
