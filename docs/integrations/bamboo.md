# Bamboo Integration and Migration

## Target boundary

Bamboo integrates with Jiandu through a narrow memory adapter. Jiandu has no Bamboo dependency.

```text
┌──────────────────────── Bamboo ─────────────────────────┐
│                                                        │
│  BambooMemoryPlugin                                    │
│  ├── ToolProvider          explicit model operations   │
│  ├── ContextContributor    proactive recall            │
│  └── EventConsumer         committed lifecycle events  │
│             │                                          │
│             └──────── authenticated MCP client ────────┼──► Jiandu
│                                                        │
│  PromptIR / token policy / authority remain in Bamboo  │
└────────────────────────────────────────────────────────┘
```

These names describe responsibilities, not a required universal plugin trait. Bamboo should continue using ordinary Rust structs, enums, and narrow capability traits so the memory integration remains replaceable.

## Prompt boundary

Jiandu returns structured records. Bamboo decides:

- which scopes are relevant for the current request;
- whether proactive recall is allowed;
- which records satisfy authority and trust policy;
- how to rank, deduplicate, and fit records into the token budget;
- how memory is rendered as a dynamic `ContextBlock`;
- where that block enters `PromptIR`;
- how prompt-cache stability is preserved;
- what provenance is shown to the user.

Memory must not be concatenated into a stable system-prompt prefix. Dynamic recalled content changes by query and belongs in a separately identified prompt segment. Stored memory is untrusted evidence, never host policy or an instruction to override the current user.

Jiandu does not know Bamboo's system prompt, provider wire format, context window, cache accounting, or approval mode.

## Identity mapping

| Bamboo concept | Jiandu representation |
| --- | --- |
| Authenticated user/service | `principal_id` established by MCP authentication |
| Logical Project | Opaque `project_id` registered or resolved by Bamboo |
| Workspace path | Mutable Project alias/metadata, never identity |
| Conversation Session | Opaque `session_id` and optional lineage events |
| Message branch | Opaque `branch_id` plus committed branch watermark |
| Plugin instance | `client_id` and granted capabilities |

Bamboo must not derive a Project ID solely by hashing or normalizing a workspace path. A moved workspace and multiple checkouts of the same Project need stable identity; unrelated Projects that reuse a path must not collide.

## Compatibility surface

During migration, Bamboo may retain its current model-facing unified `memory` tool as a compatibility alias. The adapter maps explicit actions to Jiandu's split MCP tools:

| Existing action | Jiandu operation |
| --- | --- |
| search/query | `memory_search` or `memory_list` |
| get | `memory_get` |
| create/remember | `memory_remember` |
| update | `memory_update` |
| delete/forget | `memory_forget` |

This keeps existing prompts and UI stable while allowing new hosts to grant read, write, and destructive capabilities independently. The alias is owned by Bamboo and is not part of the Jiandu protocol.

## Progressive migration

### Phase A: contract and importer

- Use the [versioned Bamboo compatibility corpus](../bamboo-memory-compatibility-v1alpha1.md) as the pinned format, identity, duplicate, sanitization, and classification contract.
- Inventory Bamboo's current filesystem memory schema, IDs, scope assumptions, and error behavior against an isolated, quiesced copy.
- Implement a read-only importer and validator in Jiandu.
- Generate a deterministic migration report: accepted, transformed, skipped, conflicted, and quarantined records.
- Run dry-run imports against fixtures and a sanitized copy, never the live store.
- Preserve original source paths and hashes in import provenance without treating paths as identity.

Exit gate: repeated dry runs produce the same plan and do not mutate Bamboo data.

### Phase B: shadow reads

- Start one Jiandu daemon against an isolated imported store.
- Query Bamboo's existing implementation and Jiandu in parallel for test and opted-in development sessions.
- Keep the existing Bamboo result authoritative.
- Compare visible IDs, ordering, scope, latency, and error classification using secret-safe metrics.

Exit gate: an agreed fixture corpus and representative sanitized sessions meet parity thresholds; no Jiandu result reaches prompts yet.

### Phase C: switch proactive recall

- Enable `ContextContributor` behind a feature flag.
- Jiandu becomes authoritative for recall in opted-in sessions.
- Bamboo enforces its own record selection, trust labels, token budget, and dynamic `ContextBlock` rendering.
- On Jiandu failure, Bamboo follows an explicit fail-open-without-memory or fail-closed host policy and records the degradation.

Exit gate: end-to-end tests prove scope isolation, prompt placement, bounded token use, cache behavior, and unavailable-service handling.

### Phase D: switch mutations

- Quiesce the old writer during a bounded migration window.
- Re-run import from the final Bamboo watermark.
- Route create, update, and forget operations to Jiandu with idempotency keys and expected revisions.
- Keep the old data read-only for rollback during a declared retention window.
- Do not operate indefinite dual-write. It creates two authorities and ambiguous recovery.

Exit gate: mutation, retry, conflict, crash-recovery, and rollback drills pass; Jiandu is the only live writer.

### Phase E: committed events and lineage

- Submit only committed message, Session branch, copy-through-message, archive, and delete events.
- Use stable event IDs so retries cannot duplicate memory effects.
- Add automatic extraction or consolidation only after deterministic event ingestion is proven.

Exit gate: deep-copy fixtures prove the target sees the source snapshot through the selected message, excludes later source changes, and uses copy-on-write for target changes.

### Phase F: retire direct filesystem ownership

- Remove Bamboo code that directly scans, locks, mutates, or migrates the memory directory.
- Retain the adapter, feature configuration, diagnostics, and migration rollback documentation.
- Archive the old store only after backup, restore, and user-visible verification.

Exit gate: no Bamboo runtime path opens Jiandu's canonical files, and another independent MCP client passes the same core read/write conformance suite.

## Acceptance gates across all phases

- Principal, Project, Session, and instance-global isolation has negative tests.
- No credential, record body, or raw query leaks into normal logs.
- Project identity survives workspace relocation and multiple worktrees.
- Idempotent retries return the original result; conflicting key reuse is rejected.
- Stale updates never overwrite a newer revision.
- The agent remains usable under the chosen policy when Jiandu is unavailable.
- Proactive memory appears as dynamic data with bounded size, not trusted system policy.
- The old and new writers are never concurrently authoritative.
- Migration has a documented rollback boundary and backup restore test.

## Explicit non-goals for the first Bamboo integration

- Redesigning Bamboo's whole plugin system.
- Moving Bamboo's conversation database into Jiandu.
- Letting Jiandu make Bamboo authorization or approval decisions.
- Automatic model-based extraction before deterministic storage and event semantics are stable.
- Requiring other agents to implement Bamboo-specific lifecycle hooks to use Jiandu.
