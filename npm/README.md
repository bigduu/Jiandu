# @bigduu/jiandu

Node launcher for [Jiandu](https://github.com/bigduu/Jiandu) (简牍), a local
shared-memory MCP server. On install, or on first launch if install scripts
were skipped, it downloads the prebuilt **v0.3.3** GitHub Release binary for
this machine and checks the archive against the release `SHA256SUMS`.

Homebrew and `cargo install jiandu-mcp --version 0.3.3 --locked` remain
available. Both build from source and need Rust 1.95 or newer. This package
does not include the Rust source.

MCP Registry name: `io.github.bigduu/jiandu`.

```sh
npx @bigduu/jiandu --help
npm i -g @bigduu/jiandu
```

| Platform | Release asset |
| --- | --- |
| macOS (Apple Silicon and Intel) | `jiandu-v0.3.3-universal-apple-darwin.tar.gz` |
| Windows x64 | `jiandu-v0.3.3-x86_64-pc-windows-msvc.zip` |
| Linux x64 (gnu) | `jiandu-v0.3.3-x86_64-unknown-linux-gnu.tar.gz` |
| Windows ARM64, Linux ARM64, and other machines | not published; the launcher exits with an error |

Requires Node.js 18 or newer. Confirm the install with `npx @bigduu/jiandu --help`.

For an MCP client, pass `-y` so `npx` does not prompt, and pass the same
`--data-dir` to every client that should share memory:

```json
{
  "mcpServers": {
    "jiandu": {
      "command": "npx",
      "args": ["-y", "@bigduu/jiandu", "--data-dir", "/absolute/path/to/.jiandu"]
    }
  }
}
```

The macOS binary is ad-hoc signed, not notarized, so Gatekeeper can block the
first run. Windows binaries are not Authenticode-signed. See the
[install guide](https://github.com/bigduu/Jiandu#install)
([简体中文](https://github.com/bigduu/Jiandu/blob/main/README.zh-CN.md#%E5%AE%89%E8%A3%85)).
