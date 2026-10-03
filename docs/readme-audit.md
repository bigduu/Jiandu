# README audit — 2026-10-03

- Zenith pin and remote HEAD: `5b50834cc2e54adf506e72628620f5e028bf2aae`.
- Latest public release redirect: <https://github.com/bigduu/Jiandu/releases/latest>
  → `v0.2.0`, tag `60a1d2bf19676bc69fa5ccca058b211badde3ebf`.
- The public Cargo sparse index <https://index.crates.io/ji/an/jiandu-mcp>
  lists `0.2.0`, published 2026-09-01, non-yanked, requiring Rust 1.95.
- Current source is eight commits after `v0.2.0`; its manifest still uses
  `0.2.0`, which does not make those later features part of the release.
- Checked with `git ls-remote origin HEAD`, `git log v0.2.0..HEAD`,
  `git show v0.2.0:README.md`, release HTTP redirects and the Cargo index.
  GitHub/crates API hosts were unavailable through the environment proxy.

## Capability evidence

- `crates/jiandu-mcp/src/args.rs`: one tool, 19 actions; the introductory
  query/write/query example uses the actual `reference` write schema.
- `crates/jiandu-mcp/src/context.rs` and `tests/call_context.rs`: per-call
  host metadata; this arrived after v0.2.0. Release users can use process
  identity flags, as shown by the tagged README.
- `crates/jiandu-mcp/src/ui.rs` and `src/ui/`: embedded read-only console,
  added after v0.2.0. It does not require a separate JavaScript build.
- `Cargo.toml`: the two crates, Rust 1.95 minimum and runtime independence.
- `skills/jiandu-memory/SKILL.md`: agent memory use; this audit did not read
  any existing user's Jiandu root or use it for task notes.

Only documentation and dedicated demonstration assets are in scope. Console
availability in source is not presented as shipped in `cargo install` v0.2.0.

## Validation

- `cargo fmt --all -- --check`: passed with Rust 1.95.
- `cargo metadata --locked --all-features --format-version 1`: passed.
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`:
  failed on pre-existing `clippy::nonminimal_bool` in
  `crates/jiandu-memory/src/memory_store/paths.rs:245`, `:255` and
  `crates/jiandu-memory/src/memory_store/store.rs:2756`. This documentation
  change does not modify those expressions. Reproduce with the command above;
  product cleanup is outside this task.
- Relative README file links and `git diff --check`: passed.
- `cargo test --workspace --all-targets --all-features --locked`: passed.

## Approved brand illustration

The user-approved nature illustration is saved at `docs/assets/jiandu-nature-hero.png`.
The original PNG was visually inspected and decoded, and its SHA-256 matched
the approved image package. It is a brand illustration, not a software screenshot;
the README alt text and visible caption say so. Existing source/release and
recording limits still apply. The older artwork remains in repository history
and any existing SVG asset is preserved.

- Pixels: 1672 × 941 (RGB PNG)
- Bytes: 2057987
- SHA-256: `3f7d1f7712c07a7ff9f7f4adc061017097c4515fc0023425492f1e89962fc2e7`
