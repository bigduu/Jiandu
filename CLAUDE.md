@AGENTS.md

Use the single Jiandu-owned data root for memory. After the one-shot
`jiandu import-bamboo` cutover, point MCP at that root and never read, write, or
fall back to Bamboo's old data root. Recall through the `memory` tool before
guessing, and let the host provide Project and Session identity per invocation
through `_meta["io.github.bigduu.jiandu/context"]`. Only `--data-dir` is required
to connect; optional identity flags are defaults for dedicated processes. Treat
`dream_read` as lower-trust orientation only; use `query`/`get` and live tools
for facts. If this host synthesizes Dream, publish it with the generation read
before synthesis; Jiandu rejects stale results and never makes the model call.
