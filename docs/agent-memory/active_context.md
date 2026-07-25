# Active context

Romero v0.1.0 is implemented as a Rust 2024 library and CLI. `romero [--verbose] [ROOT]` uses an explicit existing directory or the current directory, reads only optional `ROOT/romero.yaml`, applies defaults for omitted settings, rejects non-string and unsafe paths, validates DAT input, and then creates missing managed directories.

The repository includes a root-level `romero.yaml` example using the default `library`, `work`, and `dats` paths.

The engine is split across:

- `config.rs`: pure configuration merge/path validation plus production root and symlink inspection.
- `dat.rs`: direct Logiqx DAT and in-memory ZIP entry loading, catalog validation, and newest-catalog selection.
- `cache.rs`: rollback-journal SQLite with exclusive transaction locking, resumable per-file checkpoints, and root-independent cache keys.
- `filesystem.rs`: the production OS adapter and the unit-test-only in-memory adapter with injected failures.
- `cue.rs`, `reconcile.rs`, and `model.rs`: pure parsing, filename assignment, collision, multiset, and report logic.
- `engine.rs`: library quarantine/audit, immediate promotion, recovery, duplicate cleanup, reporting, and summaries.

The library is flat per selected DAT header. Only entries originating in library are quarantined to work, directories and non-regular work entries are ignored, and library-origin entries are never deleted. Games remain in library only when every expected filename independently matches size and SHA-1.

Work matching uses SHA-1 multisets and exact DAT filename assignment. CUE files are examined sequentially in deterministic work filename order. Once a CUE's referenced files and in-memory rewrite form a complete DAT match, that game is immediately applied and work is rescanned; candidates are never globally collected or compared. CUE rewrites preserve all non-filename bytes and line endings and are accepted only when the rewritten size and SHA-1 match the DAT. Recovery copies bytes from verified complete library games; duplicate and redundant cleanup deletes only eligible regular work files.

The engine emits all typed `ProgressEvent` values through `run_with_progress`; CLI verbosity only filters rendering. Default stderr includes phases, hashing paths without byte counts, game-level promotions, quarantine, recovery, cleanup, ignored work entries, and reports. `--verbose` additionally renders hash saves, cache hits, promoted ROM moves, rewritten CUE writes, and rewritten CUE source removals. The original `run` entry point remains quiet, and the final summary stays on stdout.

Recognized non-CUE leftovers use typed game-first diagnostics. Each selected system/game group contains every expected DAT ROM sorted case-insensitively by filename and labels it `OK`, `MISSING`, or `MISMATCH`. Exact content matches print only the DAT filename; renamed content matches print `expected -> actual [OK]`. Work filenames are relative to the work directory and never include the configured work path. A mismatch means the exact DAT filename was assigned to the group but its size or SHA-1 differs.

Each physical non-CUE work file is assigned to the candidate game with the largest number of distinct content- or filename-matching work files before assignment; case-insensitive system/game alphabetical order breaks size ties. Within a group, exact content filenames are assigned first and remaining content matches are assigned deterministically. Exact-name mismatches can select a group, allowing mismatch-only incomplete games to be explained. CUE files do not select candidate groups, but expected CUE entries appear in selected groups with the same diagnostic statuses. Unknown files remain aggregated.

One shared ordering helper applies locale-independent Unicode lowercase comparison to every textual name/path sort, then uses original spelling as a deterministic tie-breaker. It covers DAT and ZIP discovery, production and in-memory directory scans, sequential processing, filename assignment, promotion/removal ordering, recovery source choice, catalog/system/game grouping, `.miss` files, and incomplete diagnostics. Non-text sorting such as hashes, dates, completeness state, and CUE byte ranges retains its natural ordering.

The CLI sends the colored summary through `anstream`: interactive terminals receive yellow `Incomplete:`, green `[OK]`, ANSI-256 orange `[MISMATCH]`, and red `[MISSING]`. Non-terminal stdout automatically receives the unchanged plain summary, and standard `NO_COLOR`, `CLICOLOR`, and `CLICOLOR_FORCE` behavior is honored. `ExecutionSummary::colored()` supplies styled formatting while its ordinary `Display` implementation remains plain.

The fixed cache is `ROOT/.romero.sqlite3`. It stores managed area plus native relative path, size, nanosecond mtime, and SHA-1, so relocating the complete root preserves cache hits. Every newly calculated hash is committed before the next file is scanned, allowing Ctrl+C or failure to resume at the first unfinished file. Cache path changes are likewise checkpointed after successful filesystem moves, copies, and deletions, keeping the database aligned throughout reconciliation. Each checkpoint immediately begins a new exclusive transaction, so concurrent runs still fail.

Unit tests use only the in-memory filesystem and SQLite `:memory:`. Integration tests use temporary roots and cover CLI defaults/rejections and verbose filtering, plain and ZIP DATs, relocation, per-file interruption/resume, symlinks, exclusive locking, production moves, atomic reports, and rollback recovery. CI verifies formatting and tests native runner platforms. Release automation validates an annotated matching tag on `main`, builds five targets, uploads exactly five one-binary ZIPs to a draft, verifies the asset set, and publishes it.

The SQLite dependency is intentionally held at `rusqlite` 0.39 / `libsqlite3-sys` 0.37 because `libsqlite3-sys` 0.38 uses the newer `cfg_select!` macro while declaring no corresponding `rust-version`. `serde-saphyr` is similarly held at 0.0.10 because later releases use let chains and integer APIs unavailable in Rust 1.85. These versions preserve Romero's declared compiler minimum.

Local validation is green for formatting, Clippy with denied warnings, all tests, an optimized release build on the exact Rust 1.85.0 minimum, and a smoke run against the supplied 10,949-game PlayStation DAT. The GitHub-hosted cross-platform workflow itself has not run locally.
