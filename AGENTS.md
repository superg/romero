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
- Use source-adjacent `#[cfg(test)]` unit tests only. Do not add integration tests, end-to-end tests, or a top-level `tests/` directory.
- Unit tests must not access the filesystem, create temporary files, depend on external fixtures, launch Romero or any other subprocess, mutate process environment or the current directory, access the network, sleep, or use containers.
- Use inline data, `MemoryFileSystem`, in-memory readers, injected clocks and failures, and SQLite `:memory:` connections for test isolation.
- Keep production OS adapters thin and untested when testing them would violate these constraints. Delete a noncompliant test rather than retaining it.
- Run `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test --lib` before completing a change.

## AI context and memory protocol

Before modifying application code:

1. Read `docs/agent-memory/active_context.md`.
2. Read `docs/agent-memory/learned_patterns.md`.

At the end of every task or feature:

1. Update `docs/agent-memory/active_context.md` with the resulting architecture, behavior, and remaining work.
2. Update `docs/agent-memory/learned_patterns.md` with reusable technical discoveries and project-specific patterns.
