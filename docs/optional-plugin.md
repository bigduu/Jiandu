# Optional Jiandu connector plugin

Bodhi keeps its native `jiandu-memory` integration whether or not this plugin is
installed. The optional plugin installs the independent `jiandu` CLI for other
MCP clients and the read-only console. Its `provides` is empty: installation does
not register another memory server in Bamboo, start a service, enable a Skill,
initialize memory, or change any client's configuration.

This repository supplies the producer. Candidate artifacts are not a published
release or an active plugin-center entry. A maintainer must publish a new,
immutable version and accept the generated bundle URL/checksum in Bodhi's
catalog before users can download that version from the plugin center.
Candidates are unsigned and cannot pass Bamboo's default strict publisher
trust policy. Production also requires a provisioned signing key, verified
Ed25519 sidecars and separate acceptance of Jiandu's reviewed public key in the
consumer. A bundle checksum alone does not satisfy that trust policy.

## Installed executable and shared data

The Bamboo installer places the executable at an absolute package-owned path:

| Host | Executable relative to the installed `jiandu` plugin directory |
| --- | --- |
| macOS, Apple Silicon or Intel | `bin/macos/jiandu` (universal Mach-O) |
| Windows x86_64 | `bin/windows/jiandu.exe` |
| Linux x86_64, glibc 2.35 or newer | `bin/linux/jiandu` |

For the default Bamboo data directory, an example macOS path is
`/Users/you/.bamboo/plugins/jiandu/bin/macos/jiandu`. Derive the **actual absolute
installed path** from the host's configured Bamboo data directory, followed by
`plugins/jiandu/` and the platform suffix above; a custom data directory changes
it. The current host API/CLI need not expose `plugin_dir`. A copyable executable
path in Bodhi is part of the separate [consumer activation work in Zenith #375](https://github.com/bigduu/Zenith/issues/375).
Do not rely on `PATH`, Homebrew, a Rust compiler, or shell expansion of `~`.

Configure an external client, for example with a generic MCP JSON configuration:

```json
{
  "mcpServers": {
    "jiandu": {
      "command": "/Users/you/.bamboo/plugins/jiandu/bin/macos/jiandu",
      "args": ["--data-dir", "/Users/you/.jiandu"]
    }
  }
}
```

Replace both paths with the user's actual paths. On Windows JSON must escape
backslashes, for example `C:\\Users\\you\\.jiandu`, or use forward slashes.
Every client and Bodhi's native integration must use the same canonical
Jiandu-owned data root, normally `~/.jiandu`. Installation and removal touch
only package-owned artifacts; removal preserves this root. Close external
clients before removing/upgrading the executable they use. Their configurations
remain under the user's control and will need updating after removal.

Global operations need no identity. Project and Session operations require the
trusted host to provide `_meta["io.github.bigduu.jiandu/context"]` on each
`tools/call`; sharing a data directory does not grant Project authority.
For a dedicated process whose host cannot send this metadata, the host can
explicitly configure optional `--project-id` and `--session-id` defaults.
Never ask the model to invent an identity or move Project facts to Global to
bypass missing authority. See the [host context contract](design/mcp-call-context.md).

The installed CLI can also run a local read-only console:

```shell
/absolute/installed/path/jiandu ui --data-dir /absolute/path/to/.jiandu
```

## Producer contract

`plugin-release.yml` is manually dispatched. By default it uploads candidate
workflow artifacts only. It builds the current Cargo workspace version without
changing crate manifests: macOS arm64 + x86_64 combined by `lipo`, Windows
x86_64 MSVC, and Linux x86_64 GNU on Ubuntu 22.04. Linux requires glibc 2.35+;
Windows ARM and Linux ARM are outside this release matrix. macOS is built with
an 11.0 deployment target. Each runner executes CLI help and a real stdio MCP
handshake against a new temporary root, checks the exact server version and
single `memory` tool, and confirms startup/discovery did not create the root.

The Python standard-library producer creates deterministic `.tar.gz` / `.zip`
archives with exactly one root `jiandu` / `jiandu.exe`, matching Bamboo's
`PluginArtifact` contract. It generates:

- `plugin.json`: OS-gated manifest with exact `v<version>` artifact URLs and
  SHA-256 over each archive's raw bytes.
- `jiandu-plugin-v<version>.tar.gz`: `plugin.json`, this guide and `LICENSE`.
- `catalog-entry.json`: the bundle's exact URL and SHA-256 for catalog review.
- `SHA256SUMS`: complete inventory for the manifest, bundle, catalog entry and
  three binary archives.

The caller pins the **bundle checksum** independently. The installer then
checks the platform archive against the checksum inside that verified bundle
before extraction. Checksums establish byte identity; publisher trust remains
the host's existing plugin policy. Publication signs both the bundle and bare
manifest: `<asset>.sig` contains the raw 64-byte Ed25519 signature as 128
lowercase hex characters, matching Bamboo's verifier. `publisher-key.json`
contains the corresponding raw 32-byte public key as 64 hex characters; it is
review evidence, not an instruction to auto-trust a new key. The consumer owner
must independently verify its origin/fingerprint and explicitly accept that
key under label `jiandu-official`. Do not set `allow_unsigned` or `insecure`,
generate a new production key as part of installation, or disable policy.

Local packaging and validation, after compiling the binary:

```shell
python3 -B -m unittest discover -s scripts -p 'test_*.py'
python3 scripts/smoke_plugin.py --binary target/release/jiandu --version 0.3.1
python3 scripts/package_plugin.py pack --platform macos \
  --binary /path/to/universal/jiandu --output target/plugin-artifacts
# Repeat pack with the real Windows/Linux binaries produced by their runners.
python3 scripts/package_plugin.py assemble \
  --artifacts target/plugin-artifacts --output target/plugin-release
python3 scripts/package_plugin.py verify --directory target/plugin-release
```

The version defaults to `[workspace.package].version`; the smoke version must
match the actual candidate. These commands do not install a plugin or touch a
memory store. Unit-test fixture bytes validate archives only and do not prove
that another platform's executable runs.

## Publication boundary

Only dispatch with `publish=true` after the protected source PR and required
checks have passed, candidate artifacts have been reviewed, and the maintainer
has chosen an **unused version** through the normal source/version review.
Publication is restricted to `main` and the `release` environment. It first
requires the environment's `JIANDU_PLUGIN_SIGNING_KEY` secret (Ed25519 PEM),
signs and verifies the assets with OpenSSL 3, and includes the sidecars/public
key in `SHA256SUMS`. The private key is used only in a temporary file; the
producer never creates or publishes it. Configure the environment's approval
rules and key outside this workflow. It then creates the new exact source tag
after rejecting an existing remote release/tag (lookup errors also stop publication).
The annotated tag records the exact source/run; a non-forced push prevents a
concurrent publisher from replacing it. It then creates a release with the
verified candidate assets.
It never uploads additional bytes to an existing release or reuses a tag.
If publication is interrupted after the tag is created, stop and inspect the
remote tag/release before a maintainer repairs it; rerunning is deliberately
refused. Publishing does not run `cargo publish`, update Homebrew, or activate
Bodhi's catalog. Those are separate reviewed actions.
