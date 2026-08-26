# Jiandu

**Jiandu (简牍)** is an agent-independent, filesystem-backed memory service exposed over the Model Context Protocol (MCP).

The name refers to the bamboo and wooden slips used for durable written records. Jiandu applies the same idea to agents: memory is stored as inspectable records, owned by one standalone service, and shared through a stable protocol instead of being embedded in one agent runtime.

> Status: architecture, agent-neutral `v1alpha1` Rust contracts, a canonical-store core with exclusive ownership, validated reads, atomic/idempotent mutations, validation/export/import/recovery support, a reviewed offline Bamboo snapshot adapter, a deterministic disposable Unicode/CJK lexical index, an authenticated MCP read/mutation handler, and a singleton loopback Streamable HTTP daemon with bounded response drain. Remaining service work is tracked in [the standalone-service epic](https://github.com/bigduu/Jiandu/issues/1) and delivered through small, independently testable issues.

## Why Jiandu exists

Agent memory should not belong to a particular prompt implementation or application process. Bamboo, Codex, Claude, and other MCP-capable agents should be able to use the same durable memories without directly sharing mutable files or depending on Bamboo-specific session types.

Jiandu therefore separates three responsibilities:

1. **Jiandu owns memory data**: canonical files, indexes, revisions, migrations, scopes, and audit history.
2. **MCP carries structured memory**: agents search, read, remember, update, and forget through versioned contracts.
3. **Each agent host owns prompt composition**: a host may proactively recall memory and place it in its own dynamic context, but Jiandu never writes a system prompt.

```text
                            ┌──────────────────────┐
                            │       Jiandu         │
                            │ filesystem + index   │
                            │ MCP tool handler     │
                            │ transport comes next │
                            └──────────┬───────────┘
                                       │
                  ┌────────────────────┼────────────────────┐
                  │                    │                    │
           Bamboo adapter        Generic MCP agent    Another host adapter
          automatic recall         model tools         automatic recall
          context injection       explicit memory      post-turn events
```

## Core guarantees

- One authoritative service owns and mutates a Jiandu data directory.
- Canonical memory remains human-inspectable on the filesystem.
- Search indexes and caches are derived and rebuildable.
- Agents never directly mutate canonical memory files.
- Public contracts use opaque IDs, never workspace paths as identity.
- Principal, Project, Session, and operator-global scopes remain distinct.
- Read and write results are structured data, not pre-rendered prompt instructions.
- Canonical create/update uses expected-revision CAS plus principal/operation-scoped durable receipts. Identical retries replay the original result without another mutation or audit event.
- Ordinary forget is exact-scope, revision-aware, independently destructive-authorized, idempotent, and audited; it retains a descriptor-erased zero-length logical witness rather than claiming secure physical erasure. Restore/hard-purge remain separate administrative lifecycles.
- MCP mutation identity comes only from trusted connection context. Operation-specific write and forget grants are resolved before private receipt access; configurable admission runs only for a fresh canonical target and before the WAL. A strict transport correlation maps to the transaction ID already bound across WAL/result/receipt/audit, while replay returns the original committed correlation.
- Daemon shutdown closes authenticated HTTP and backend admission atomically, bounds finite response/session grace, and force-cancels every accepted socket on timeout, including a peer still uploading its body. Readiness owns only a sanitized health snapshot, sessions keep a weak canonical facade, synchronous reads run off Tokio workers, and normal shutdown leaves the force-I/O token untouched so Axum can flush every produced response before reporting `Drained`. Shutdown still waits for every entered canonical worker before releasing the singleton lock; a late durable mutation is observed only by same-key replay.
- Live owners and coordinated offline inspectors share one bounded, read-only validation engine. Portable export is canonical, deterministic, scope-authorized, complete for public record/provenance fields, and excludes paths and private replay/WAL/audit/witness bytes.
- Portable import strictly decodes before write, produces a deterministic zero-write authority plan, and commits at most 100 records/tombstones in one metadata-last v4 WAL. Exact retries replay one receipt-bound result and backup metadata without another mutation or audit event.
- Jiandu remains useful without an LLM provider; extraction and reranking are optional later capabilities.
- If Jiandu is unavailable, an agent can continue without recalled memory according to host policy.

## Integration levels

| Client capability | Result |
| --- | --- |
| MCP tools only | The model can explicitly search and mutate shared memory. |
| MCP plus a host recall hook | The host can proactively recall and inject dynamic context before an LLM call. |
| MCP plus committed-event integration | The host can submit durable turns and branch events for automatic memory maintenance. |

MCP does not force a client to inject context. Jiandu returns memory records; the client decides whether, where, and with what authority to use them.

## Design documents

- [Architecture](docs/architecture.md)
- [MCP API v0](docs/mcp-api-v0.md)
- [Data model, filesystem, scopes, and lineage](docs/data-model.md)
- [Canonical store format v1alpha4](docs/store-format-v1alpha4.md)
- [Validation report and portable export v1alpha1](docs/portable-export-v1alpha1.md)
- [Portable import and backup metadata v1alpha1](docs/portable-import-v1alpha1.md)
- [Deterministic lexical index format v1alpha1](docs/index-format-v1alpha1.md)
- [Historical forget/tombstone store format v1alpha3](docs/store-format-v1alpha3.md)
- [Historical create/update store format v1alpha2](docs/store-format-v1alpha2.md)
- [Bamboo integration and migration](docs/integrations/bamboo.md)
- [Reviewed Bamboo snapshot import v1alpha1](docs/bamboo-snapshot-import-v1alpha1.md)
- [Delivery roadmap](docs/roadmap.md)

## Rust layout

Crates are introduced only when their boundary is needed. The current dependency direction is:

```text
crates/jiandu-core/                  agent-neutral domain types and contracts
  fixtures/v1alpha1/                 canonical valid and invalid conformance data
  schemas/v1alpha1/                  checked JSON Schemas generated from Rust types
  src/                               ordinary structs, enums, newtypes, and validation
crates/jiandu-store/                 exclusive ownership, reads, atomic CAS, and recovery
  fixtures/v1alpha1/                 canonical store-document conformance data
  fixtures/v1alpha2/                 strict metadata, WAL, receipt, result, and audit fixtures
  fixtures/v1alpha3/                 strict forget WAL, tombstone, result, receipt, and audit fixtures
  fixtures/v1alpha4/                 current store capability metadata fixture
  fixtures/inspection/v1alpha1/      deterministic validation/export fixtures
  fixtures/import/v1alpha1/          canonical import plan/result/backup fixtures
  schemas/inspection/v1alpha1/       generated strict report/export JSON Schemas
  schemas/import/v1alpha1/           generated strict import/backup JSON Schemas
  src/                               private paths, strict codecs, lock, inspection, import, tombstones, logical-erasure witnesses, transactions, and recovery
crates/jiandu-bamboo-import/         read-only Bamboo snapshot planning and reviewed portable import
  src/                               strict report/plan/evidence contracts, capability scan, mapping, commit, and recovery acknowledgement
crates/jiandu-index/                 deterministic, derived Unicode/CJK lexical retrieval
  fixtures/v1alpha1/                 tokenizer and logical index-format conformance fixtures
  src/                               strict format, SQLite rebuild, HMAC cursor, ranking, diagnostics
crates/jiandu-mcp/                   transport-independent authenticated MCP adapter
  src/                               fixed read/mutation tools, resources, policy, safe health, backend seams
  tests/                             in-process protocol, authorization, schema, retry, cancellation, degradation fixtures
crates/jiandu-service/               singleton loopback Streamable HTTP daemon
  src/                               strict local config, bearer auth, lifecycle admission, bounded drain
  tests/                             two-client conformance and restart/degradation resilience
```

Future administrative CLI boundaries are introduced only when needed. `jiandu-service`
composes `jiandu-mcp` with one daemon-owned canonical backend; `jiandu-mcp`
depends on the three existing domain/store/index crates
but owns no transport listener or canonical data. `jiandu-index` depends narrowly on `jiandu-store` and
`jiandu-core`; canonical storage never depends on the index. `jiandu-core` has
no storage, transport, Bamboo, prompt, LLM, or filesystem-path identity
dependency. `jiandu-bamboo-import` is a leaf adapter over core/store canonical
APIs; neither crate depends on Bamboo contracts or the adapter. The current
canonical on-disk compatibility rules are documented in
[Canonical store format v1alpha4](docs/store-format-v1alpha4.md); the
[v1alpha3 document](docs/store-format-v1alpha3.md) preserves the historical
forget/tombstone contract, the [v1alpha2 document](docs/store-format-v1alpha2.md)
preserves the historical create/update receipt/audit contract, and the
[v1alpha1 document](docs/store-format-v1alpha1.md) remains the migration source
contract.

Run the contract gates from the repository root:

```shell
cargo fmt --all -- --check
cargo metadata --locked --all-features --format-version 1
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
```

## License

Jiandu is licensed under the [MIT License](LICENSE).
