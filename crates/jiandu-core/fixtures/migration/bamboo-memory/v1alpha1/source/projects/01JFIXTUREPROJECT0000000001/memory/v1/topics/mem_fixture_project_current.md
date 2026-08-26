---
id: mem_fixture_project_current
title: Keep the memory service agent-neutral
type: project
scope: project
project_key: 01JFIXTUREPROJECT0000000001
granularity: quarter
status: active
freshness: high
confidence: high
created_at: 2026-08-20T08:00:00Z
updated_at: 2026-08-21T09:30:00Z
created_by:
  kind: session
  id: sesfixture1
updated_by:
  kind: memory_write
  actor: fixture-agent
sources:
- kind: session
  id: sesfixture1
relations:
  supersedes:
  - mem_fixture_project_previous
  contradicted_by:
  - mem_fixture_global_user
  related:
  - mem_fixture_global_feedback
tags:
- architecture
- memory
retrieval:
  keywords:
  - agent-neutral
  - memory
  entities:
  - Jiandu
  embedding_ready: true
---

The standalone memory service must expose structured records without depending on a host agent runtime.
