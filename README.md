# Jiandu

**Jiandu (简牍)** is an agent-independent, filesystem-backed memory service exposed over the Model Context Protocol (MCP).

The name refers to the bamboo and wooden slips used for durable written records. Jiandu applies the same idea to agents: memory is stored as inspectable records, owned by one standalone service, and shared through a stable protocol instead of being embedded in one agent runtime.

> Status: architecture, agent-neutral `v1alpha1` Rust contracts, and the first canonical-store slice (exclusive ownership plus validated reads). Service implementation is tracked in [the standalone-service epic](https://github.com/bigduu/Jiandu/issues/1) and delivered through small, independently testable issues.

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
                            │ MCP Streamable HTTP  │
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
- Mutations use expected revisions and idempotency keys.
- Forget and purge operations are explicit, authorized, and auditable.
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
- [Bamboo integration and migration](docs/integrations/bamboo.md)
- [Delivery roadmap](docs/roadmap.md)

## Rust layout

Crates are introduced only when their boundary is needed. The current dependency direction is:

```text
crates/jiandu-core/                  agent-neutral domain types and contracts
  fixtures/v1alpha1/                 canonical valid and invalid conformance data
  schemas/v1alpha1/                  checked JSON Schemas generated from Rust types
  src/                               ordinary structs, enums, newtypes, and validation
crates/jiandu-store/                 exclusive filesystem ownership and validated reads
  fixtures/v1alpha1/                 canonical store-document conformance data
  src/                               private paths, strict codec, lock, cursor, and read APIs
```

Future index, MCP adapter, daemon, and CLI crates are introduced only when their
boundary is needed. `jiandu-store` depends on `jiandu-core`; `jiandu-core` has
no storage, transport, Bamboo, prompt, LLM, or filesystem-path identity
dependency. The store's on-disk compatibility rules are documented in
[Canonical store format v1alpha1](docs/store-format-v1alpha1.md).

Run the contract gates from the repository root:

```shell
cargo fmt --all -- --check
cargo metadata --locked --no-deps --format-version 1
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
```

## License

Jiandu is licensed under the [MIT License](LICENSE).
