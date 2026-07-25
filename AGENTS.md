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
- Unit tests must not access the operating-system filesystem. Use pure functions, in-memory data, mocked filesystem operations, or in-memory SQLite.
- Integration tests may use isolated temporary directories.
- Never delete an entry originating in the library. Quarantine it into work instead.
- Never move an entry into work unless it originated inside the configured library.
- Never follow symlinks.
- Validate the complete configuration and all DAT catalogs before mutating library data.
- Promote complete games immediately and rename every promoted ROM to its exact DAT filename.
- Keep matching, reconciliation, CUE rewriting, collision naming, and report generation independent from operating-system I/O.
- Run `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test` before completing a change.

## AI context and memory protocol

Before modifying application code:

1. Read `docs/agent-memory/active_context.md`.
2. Read `docs/agent-memory/learned_patterns.md`.

At the end of every task or feature:

1. Update `docs/agent-memory/active_context.md` with the resulting architecture, behavior, and remaining work.
2. Append reusable technical discoveries and project-specific patterns to `docs/agent-memory/learned_patterns.md`.
