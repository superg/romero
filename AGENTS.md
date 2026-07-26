# Romero development rules

## Stack

- Rust 2024.
- `clap` for the CLI.
- Serde plus `serde-saphyr` for optional YAML configuration.
- `quick-xml` for Logiqx DAT parsing.
- `zip` for DAT archives only.
- `sha1` for ROM identity.
- Bundled `rusqlite` for the relocatable hash cache.

## Engineering rules

- Follow red-green-refactor whenever a behavior can be expressed as a focused test.
- Run `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test` before completing a change.

## AI context and memory protocol

Before modifying application code:

1. Read `docs/agent-memory/active_context.md`.
2. Read `docs/agent-memory/learned_patterns.md`.

At the end of every task or feature:

1. Update `docs/agent-memory/active_context.md` with the resulting architecture, behavior, and remaining work.
2. Update `docs/agent-memory/learned_patterns.md` with reusable technical discoveries and project-specific patterns.
