# Memory console recording

`memory-console.gif` is a real Chromium recording of Jiandu's bundled, unmodified
read-only console: select the demo Project, search `release checklist`, open the
matching memory, and scroll to its full content. `memory-console.png` is the
static alternative. All content is synthetic demonstration data, explicitly
labelled in the stored title and body. No provider, credentials, personal store,
production configuration or desktop data is used.

Recorded from source commit `5b50834cc2e54adf506e72628620f5e028bf2aae`
(manifest version `0.2.0`), on Linux with Rust 1.95.0, Node 24.19.0,
Playwright and system Chromium. This demonstrates that source revision; it does
not establish packaged-release, macOS, Windows or native-desktop acceptance.
The browser UI is read-only: setup writes happen through real stdio MCP before
recording, not through the browser. No screenshots are animated to imitate use.

## Reproduce

From the Jiandu checkout, build the exact source and use a new temporary root:

```sh
cargo build --locked -p jiandu-mcp
JIANDU_BIN="${CARGO_TARGET_DIR:-target}/debug/jiandu" \
  python3 docs/demos/seed-memory-demo.py > /tmp/jiandu-demo-evidence.jsonl
DEMO_ROOT=$(python3 -c 'import json,sys; print(next(row["demo_data_dir"] for row in map(json.loads, open(sys.argv[1])) if "demo_data_dir" in row))' /tmp/jiandu-demo-evidence.jsonl)
"${CARGO_TARGET_DIR:-target}/debug/jiandu" ui --data-dir "$DEMO_ROOT" --port 9123
```

The seed script acts as a minimal MCP host and grants only its dedicated
`readme-demo` Project through per-call `_meta`. It initializes MCP, queries the
empty scope, follows the server's explicit missing-index instruction to rebuild,
queries again, writes one reference, recalls it and retrieves its full body by
ID. It asserts that lexical recall includes the saved ID and that the returned
body contains the demo's rollback instruction. Every stdout line is JSON,
including the expected missing-index diagnostic and final demo-root record.
All memory access uses MCP; no canonical data files are edited directly.

In another terminal, with Playwright installed in an existing tool environment:

```sh
# PLAYWRIGHT_MODULE may be an absolute path to an existing playwright package.
PLAYWRIGHT_MODULE=playwright node docs/demos/record-console.cjs
# Use the VIDEO path printed by that command:
ffmpeg -i /tmp/jiandu-demo-video/REPLACE_WITH_RECORDED_VIDEO.webm \
  -filter_complex '[0:v]fps=10,split[a][b];[a]palettegen=max_colors=96[p];[b][p]paletteuse=dither=bayer:bayer_scale=3' \
  -loop 0 docs/demos/memory-console.gif
```

The script uses genuine browser input and Playwright video capture. Pauses allow
reading; no responses, timestamps, UI state or output are substituted. The final
PNG is taken from the live page. Stop the console after recording. Temporary
demo roots can be removed once no process is using them.

## Verification

The recording uses 1120 × 800 pixels to preserve the console's two-column layout
and readable text. The GIF is palette-compressed, 10 fps, 16.6 seconds (166 frames), 3,093,178 bytes
and loops indefinitely (verified Netscape loop count 0).
The initial view, searched results and final opened body were visually reviewed.
The video/GIF covers only the browser flow; the seed script reproduces the
separate successful MCP write → query → get flow. No product code was changed.
