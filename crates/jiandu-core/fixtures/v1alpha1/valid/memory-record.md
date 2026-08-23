---
schema: jiandu.dev/memory/v1alpha1
id: mem_01K3IDENTITY
revision: 7
scope:
  kind: project
  project_id: prj_01K3PROJECT
type: decision
status: active
title: Use opaque project identity
summary: Workspace paths are metadata, not Project identity.
tags:
  - architecture
  - identity
created_at: 2026-08-23T10:00:00Z
updated_at: 2026-08-23T10:05:00Z
provenance:
  created_by: host
  agent_id: bamboo
  session_id: ses_01K3SESSION
  branch_id: br_01K3BRANCH
  message_ids:
    - msg_41
    - msg_42
  source_uri: https://example.invalid/decisions/identity
  content_digest: sha256:0123456789abcdef
  extraction:
    method: explicit
    extractor_version: 1.0.0
  confidence: 0.98
relations:
  - kind: supports
    target_memory_id: mem_01K3ARCHITECTURE
---
Workspace paths remain mutable metadata and never become identity.
