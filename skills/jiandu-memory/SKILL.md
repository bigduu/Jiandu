---
name: jiandu-memory
description: Use when an agent must recall, checkpoint, orient from, or maintain shared Jiandu memory through its single MCP memory tool. Do not use for generic file search, directly observable live state, secret storage, Skill installation, or as a replacement for current verification.
---

# Use Jiandu memory

Use the host's single Jiandu `memory` tool. It may be namespaced, such as
`mcp__jiandu__memory`. Follow its live schema and structured responses; do not
copy an action catalog into the prompt or invent host-specific fields.

## Choose the scope

- Use Session for temporary workstream continuity. Read it with `session_read`;
  append concise progress or blockers with `session_append`.
- Use Project for durable knowledge specific to the authorized project.
- Use Global only for stable facts or preferences that genuinely apply across
  projects.
- Normally omit `project_key`. Project authority comes from the MCP host, and an
  argument cannot grant or widen access.
- Do not use durable `query` as a Session lookup.

## Recall with the least work

1. Derive a short non-empty query from discriminative terms already understood
   in the current turn: names, aliases, identifiers, error text, or entities.
   Do not call another model to rewrite the query and do not use embeddings.
2. Query the relevant durable scope. Omit `limit` to keep the compact default
   top three. A blank query is for management/filter listing, not normal recall.
3. Read the compact hits and their ids. Call `get(id)` only for a candidate whose
   full body is actually needed; do not expand every hit.
4. Check truncation signals, then verify recalled claims against current files,
   tools, or services before treating them as true now.

Use Project first for project work. Use Global only when the needed knowledge is
cross-project or Project recall has no useful result.

## Use Dream only for orientation

Start with `query` when the turn already provides usable retrieval terms. Use
`dream_read` only for a cold start with too little direction or after query has
no useful hit.

- A missing Dream is a normal cold state.
- A stale Dream is advisory orientation, never factual authority.
- Resolve any promising Dream statement through `query` and selected `get`, then
  verify it against live state.
- Call `dream_publish` only when the host workflow already owns Dream synthesis.
  Read `current_generation` before synthesis, let the host use its own model, and
  publish with that generation. Jiandu chooses no model, prompt, cadence, retry,
  or scheduler.
- If publication rejects a stale generation, do not present the synthesis as
  fresh and do not blindly retry it.

## Write only confirmed durable facts

Before a durable write:

1. Run a short query for the fact and its aliases.
2. If a hit may be the same fact, get that candidate.
3. Use the live schema's merge path for the same fact; otherwise write one new
   atomic fact.

The canonical body must be a complete, confirmed fact, not a lossy or unverified
model-authored conversation summary. Give it a concise searchable title and a
small set of model-known `keywords`, `entities`, and `tags`. Include useful
bilingual aliases when they distinguish the fact. Do not make another model call
to generate retrieval metadata or a nonexistent summary field.

Do not store secrets, credentials, tokens, raw private conversations, transient
chat, or facts that are cheaper and safer to derive from current project files.

## Maintain deterministically

Use one bounded maintenance pass:

1. Call `scan_blobs` or `scan_duplicates`.
2. If the worklist is empty, stop immediately. Make no follow-up model call.
3. Get only the selected records.
4. Let the current host model decide whether to `split` or `consolidate` through
   the live schema.

Do not create an autonomous scan loop, scheduler, Gardener, or background model
inside Jiandu.

## Keep Skills separate from memory

A durable memory may reference `skill:<stable-id>` or record a non-executable
`skill-candidate` rationale. It must not contain a full executable `SKILL.md`,
script, credential, or installation payload. Never install, enable, modify, or
execute another Skill as a side effect of memory work; that requires a separate,
explicitly authorized host workflow.
