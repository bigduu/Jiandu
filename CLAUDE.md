@AGENTS.md

Use the single Jiandu-owned data root for memory. After the one-shot
`jiandu import-bamboo` cutover, point MCP at that root and never read, write, or
fall back to Bamboo's old data root. Recall through the `memory` tool before
guessing, and let the host provide Project authority with `project-id`.
