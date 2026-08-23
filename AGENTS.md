# Repository Guidelines

## Purpose

Jiandu is a standalone, agent-independent, filesystem-backed memory service. It exposes structured memory through MCP and must not depend on Bamboo-specific runtime types or prompt assembly.

Read these documents before changing contracts or persistence:

- `docs/architecture.md`
- `docs/mcp-api-v0.md`
- `docs/data-model.md`
- `docs/roadmap.md`

## Architectural invariants

- A Jiandu data directory has one authoritative writer service.
- Canonical records live on the filesystem; indexes and caches are disposable derivatives.
- Clients access memory through APIs and never directly mutate canonical files.
- Public identities are opaque. A workspace path, repository path, display name, or hash of a path is not a Project identity.
- Principal ownership, Project membership, Session lineage, and operator-global data are separate scopes.
- MCP responses contain structured records and provenance, not system-prompt text.
- Prompt placement, instruction authority, and token budgeting belong to the consuming host.
- Every mutation is revision-aware, idempotent, and auditable.
- Destructive operations are narrow and explicit. Broad purge is an administrative operation, not a normal model tool.
- The storage and retrieval core must work without LLM credentials. Optional extraction, embeddings, and reranking are layered capabilities.
- Protocol, API, storage-schema, and package versions are independent.

## Implementation style

- Prefer ordinary Rust structs, enums, and narrow traits.
- Keep dependency direction from adapters toward core; never import an agent runtime into core contracts.
- Use behavior-focused tests next to code and integration/conformance tests under `tests/`.
- Require `cargo fmt --check`, strict Clippy, and the full affected test suite before merging.
- Preserve human-readable error messages while returning stable machine-readable error codes over MCP.

## Change discipline

- Contract changes require examples and compatibility notes in `docs/mcp-api-v0.md`.
- Storage changes require migration, crash-recovery, rebuild, and rollback tests.
- Scope changes require cross-principal isolation and branch-lineage tests.
- A change that renders memory into a prompt belongs in a host integration repository, not Jiandu core.
