# Jiandu

**Jiandu (简牍)** is an agent-independent, filesystem-backed memory service exposed over the Model Context Protocol (MCP).

The name refers to the bamboo and wooden slips used for durable written records. Jiandu applies the same idea to agents: memory is stored as inspectable records, owned by one standalone service, and shared through a stable protocol instead of being embedded in one agent runtime.

> Status: architecture and contract definition. Implementation is tracked in [the standalone-service epic](https://github.com/bigduu/Jiandu/issues/1) and delivered through small, independently testable issues.

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

## Planned Rust layout

The exact crate split will be introduced only when each boundary is needed. The intended dependency direction is:

```text
jiandu-core       agent-neutral domain types and contracts
jiandu-store      canonical filesystem persistence and migrations
jiandu-index      rebuildable lexical/search indexes
jiandu-mcp        MCP protocol adapter
jiandu            daemon and administrative CLI
```

No core crate may depend on Bamboo or on a specific model provider.

## License

Jiandu is licensed under the [MIT License](LICENSE).
