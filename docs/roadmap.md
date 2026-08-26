# Delivery Roadmap

Jiandu will be delivered as small vertical milestones. Each implementation issue must preserve the architecture invariants in [AGENTS.md](../AGENTS.md), include focused tests, and avoid introducing Bamboo-specific types into the core.

The complete dependency graph and project-level acceptance criteria are tracked in [Jiandu #1](https://github.com/bigduu/Jiandu/issues/1).

## Milestone 0: Contract foundation

Tracking issue: [#2 — agent-neutral memory contracts and conformance fixtures](https://github.com/bigduu/Jiandu/issues/2)

- Define agent-neutral Rust domain types for records, scopes, provenance, revisions, queries, mutations, and errors.
- Publish JSON Schemas and canonical serialization fixtures from the same definitions.
- Establish API, store, and index version boundaries.
- Add invariant and round-trip tests.

Outcome: downstream work compiles against a small contract with no persistence or MCP dependency.

## Milestone 1: Canonical filesystem store

Tracking issues:

- [#3 — canonical filesystem initialization and validated reads](https://github.com/bigduu/Jiandu/issues/3)
- [#4 — singleton atomic CAS mutations and crash recovery](https://github.com/bigduu/Jiandu/issues/4)
- [#5 — idempotency, audit, tombstones, and portable import/export](https://github.com/bigduu/Jiandu/issues/5) (parent tracker), split into:
  - [#17 — durable create/update idempotency and audit](https://github.com/bigduu/Jiandu/issues/17)
  - [#18 — idempotent forget and protected tombstones](https://github.com/bigduu/Jiandu/issues/18) — implemented in the canonical store core
  - [#19 — deterministic validation and portable export](https://github.com/bigduu/Jiandu/issues/19) — implemented in the canonical store core
  - [#20 — committed import and recovery-safe backup metadata](https://github.com/bigduu/Jiandu/issues/20) — implemented in the canonical store core

- Implement data-directory initialization and exclusive ownership.
- Add canonical record serialization, validation, and scope-safe paths.
- Implement atomic create/update CAS with versioned manifests, durability diagnostics, and deterministic crash recovery.
- Add durable create/update idempotency receipts, private replay results,
  sequence-addressed audit events, and an explicit capability-gated store
  migration.
- Add independently authorized idempotent forget/protected tombstones with a
  descriptor-erased, ledger-bound zero-length witness (done),
  followed by bounded read-only validation and deterministic authorized
  portable export (done), followed by deterministic import planning,
  all-or-none committed import, and receipt-bound recovery-safe backup metadata
  (done).
- Preserve the explicit v3-to-v4 capability migration and strict import/backup
  schemas, fixtures, failpoint recovery, and authority tests delivered in #20.

Outcome: one local process can safely own durable memory without a model or MCP transport.

## Milestone 2: Derived retrieval

Tracking issue: [#6 — deterministic lexical retrieval and rebuild tooling](https://github.com/bigduu/Jiandu/issues/6)

- Build a deterministic lexical index from canonical records (done).
- Implement structured filters, stable ranking, and authenticated cursor
  pagination (done).
- Track store watermarks and rebuild after missing/corrupt/version-mismatched
  derived state (done).
- Add retrieval conformance, cross-platform replacement, and scope-leakage
  tests (done).

Outcome: search is useful and rebuildable without making derived data authoritative.

## Milestone 3: Standalone MCP service

Tracking issues:

- [#7 — structured MCP read operations and resources](https://github.com/bigduu/Jiandu/issues/7) — implemented as a transport-independent handler
- [#8 — authorized MCP mutation operations](https://github.com/bigduu/Jiandu/issues/8) — implemented as independently granted, idempotent single-record tools
- [#9 — standalone MCP service](https://github.com/bigduu/Jiandu/issues/9) (thin v0.1 tracker), delivered by:
  - [#28 — authenticated singleton loopback MCP daemon](https://github.com/bigduu/Jiandu/issues/28) — implemented foundation
  - [#33 — thin v0.1 daemon authority configuration](https://github.com/bigduu/Jiandu/issues/33) — implemented compact typed permission profile
- [#10 — ordinary two-client public conformance](https://github.com/bigduu/Jiandu/issues/10) — implemented with shared official-rmcp/raw-HTTP fixtures
- Non-blocking service follow-ups:
  - [#29 — bounded shutdown admission and durable drain](https://github.com/bigduu/Jiandu/issues/29)
  - [#34 — restart, retry, and degraded-index conformance](https://github.com/bigduu/Jiandu/issues/34) — implemented black-box resilience matrix
  - [#35 — secret-safe diagnostics after integration demand](https://github.com/bigduu/Jiandu/issues/35)
  - [#30 — administrative maintenance commands on demand](https://github.com/bigduu/Jiandu/issues/30)
  - [#31 — stdio proxy only for clients without HTTP](https://github.com/bigduu/Jiandu/issues/31)

- Expose structured read tools and resources (done).
- Add mutation tools with independent capability grants (done).
- Run a singleton loopback Streamable HTTP daemon with digest-based local
  bearer authentication and one shared existing-store backend (Issue #28,
  done).
- Publish separate closed liveness/readiness probes without making disposable
  search health authoritative (Issue #28, done).
- Replace the unreleased raw grant/policy daemon inputs with one versioned,
  typed read/write/forget permission profile and closed service defaults (Issue
  #33, done).
- Prove the ordinary public contract with two independent clients (Issue #10,
  done; exact matrix in `mcp-conformance-matrix-v0.md`).
- Add restart/retry/degraded-index conformance (#34, done; same exact matrix).
- Add bounded shutdown (#29), diagnostics (#35), administrative commands (#30),
  and a `stdio` proxy (#31) only as separately scheduled follow-ups; none
  blocks the thin #9 service.

Outcome: at least two independent MCP clients can use the same Jiandu service safely.

## Milestone 4: Lineage and interoperability

Tracking issues:

- [#11 — Session snapshots and copy-on-write branch semantics](https://github.com/bigduu/Jiandu/issues/11)
- [#12 — committed turn and branch lifecycle event ingestion](https://github.com/bigduu/Jiandu/issues/12)

- Define idempotent committed-turn and branch event contracts.
- Implement Session snapshot, copy-through-message watermarks, and copy-on-write memory semantics.
- Add a two-client conformance harness and transport failure tests.
- Document remote deployment requirements without enabling an insecure default.

Outcome: Session branches are portable, deterministic, and independent from one host's database schema.

## Milestone 5: Bamboo migration

Tracking issues:

- [#13 — import and validate existing Bamboo filesystem memory](https://github.com/bigduu/Jiandu/issues/13)
- [Bamboo #940 — integrate Jiandu and retire direct filesystem ownership](https://github.com/bigduu/Bamboo-agent/issues/940)

- Import and validate current Bamboo filesystem memory.
- Run shadow reads and measure parity.
- Add Bamboo's host-owned proactive recall/context adapter.
- Switch mutations during a bounded single-writer cutover.
- Integrate committed events and remove Bamboo's direct filesystem ownership.

Outcome: Bamboo consumes Jiandu as a replaceable MCP-backed capability, while generic clients remain first-class.

## Milestone 6: Optional smart memory

Only after deterministic contracts and cross-client conformance are stable:

- pluggable embeddings and semantic reranking;
- host- or service-initiated candidate extraction;
- explicit consolidation and contradiction workflows;
- usage feedback and retention recommendations;
- remote multi-tenant deployment.

All model-assisted features remain optional, observable, reversible, and subordinate to canonical records and operator policy.

## Project definition of done

The standalone foundation is complete when:

- Jiandu is the only writer for its data directory and recovers from interrupted mutations;
- canonical records survive index deletion and rebuild;
- read, write, and destructive grants are independently enforceable;
- scope isolation and Project identity behavior have negative tests;
- idempotent retry and revision conflict behavior pass conformance fixtures;
- two independent MCP clients pass the public contract suite;
- Bamboo completes a reversible cutover without indefinite dual-write;
- another agent can use Jiandu without linking or understanding Bamboo;
- backup, export, import, validation, and forget/purge behavior are documented and tested.
