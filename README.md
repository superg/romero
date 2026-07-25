# Romero

Romero is a conservative command-line ROM set manager. It validates a flat library against Logiqx XML DAT catalogs, quarantines anything that does not form a complete correctly named game, recognizes complete games in a work directory by SHA-1, rewrites compliant CUE filenames, and reports missing games.

Romero never deletes an entry originating in the library. Unexpected files, directories, symlinks, stale reports, and incomplete games are moved intact to the work directory.

## Usage

```text
romero [--verbose] [ROOT]
```

`ROOT` must be an existing directory. When it is omitted, Romero uses the current directory.

`--verbose` adds cache hits, durable hash-save confirmations, and per-file promotion details. It does not change reconciliation behavior or the final summary.

A typical root is:

```text
roms/
├── romero.yaml              # optional
├── .romero.sqlite3          # generated hash cache
├── dats/
├── library/
└── work/
```

Romero looks only for `ROOT/romero.yaml`. Passing the YAML file itself is an error.

Successful reconciliation exits with status 0 even when games are missing. Invalid configuration, invalid DAT input, cache locking, and filesystem failures return a nonzero status.

## Configuration

All settings are optional string paths:

```yaml
library_path: library
work_path: work
dat_path: dats
```

A missing or empty `romero.yaml` uses those defaults. Missing individual settings also use their defaults.

Configured paths must be relative, remain below `ROOT`, contain no `..` or symlink component, and must not overlap even when compared case-insensitively. They cannot start with `romero.yaml` or `.romero.sqlite3`. Romero creates missing managed directories only after validating the complete configuration and all available DAT input.

The configuration, cache, library, work directory, and DAT directory can be moved together to a different absolute location without invalidating unchanged cached ROM hashes.

## DAT catalogs

The DAT directory is flat. Romero loads:

- Direct `.dat` files.
- Every `.dat` entry contained in a direct `.zip` file.

DAT ZIP entries are parsed in memory and are not extracted.

Every catalog must provide a header name and a date formatted as `YYYY-MM-DD HH-MM-SS`. Games must provide a name and at least one ROM with a safe basename, size, and forty-digit SHA-1.

When several catalogs have the same header name, Romero selects the newest date. Equal-date catalogs must be semantically identical. Catalog conflicts, unsafe filenames, case-insensitive target collisions, duplicate game-content multisets, and games with more than one CUE fail before library mutation.

## Library layout

Each selected DAT owns one flat directory:

```text
library/
├── Sony - PlayStation/
│   ├── '98 Koushien (Japan) (Demo).cue
│   ├── '98 Koushien (Japan) (Demo).bin
│   ├── '98 Koushien - Koukou Yakyuu Simulation (Japan).cue
│   └── '98 Koushien - Koukou Yakyuu Simulation (Japan).bin
└── Sony - PlayStation.miss
```

There are no per-game directories. A game is complete only when every expected DAT filename is independently present with the expected size and SHA-1.

Files with equal hashes in different games are separate physical copies. One correctly named file never satisfies another game's differently named entry.

## Reconciliation

Romero performs these phases:

1. Validate configuration and all catalogs.
2. Move every unrecognized library-root entry to work. Unknown directories move with their complete contents.
3. Move nested directories and non-regular entries out of tracked system directories without traversing them.
4. Verify tracked library games. Incorrectly named, unknown, and partial-game files move to work.
5. Recognize complete games in work by SHA-1 multiset and promote each game immediately.
6. Recover uniquely identifiable incomplete games by copying matching content from verified complete library games.
7. Remove remaining non-CUE work files whose content already exists in a verified complete library game.
8. Atomically write alphabetized `.miss` reports.

Every promoted file is renamed to its exact DAT filename. Work filenames are never treated as authoritative.

File and directory moves use ordinary operating-system rename behavior. Cross-filesystem copy/delete fallback is intentionally not implemented. A rejected move stops the run and leaves its source intact.

When moving into work, collisions become:

- `name.ext`, `name.1.ext`, `name.2.ext`
- `directory`, `directory.1`, `directory.2`

Directories and other non-regular entries parked in work are preserved but not processed.

## CUE handling

CUE processing accepts UTF-8 sheets whose `FILE` values are basenames without directory components.

Romero resolves every referenced work file, matches the non-CUE SHA-1 multiset, assigns each source to its expected DAT filename, and rewrites only the filename tokens. Other bytes and original line endings remain unchanged. Promotion occurs only when the rewritten CUE size and SHA-1 equal the DAT values.

Romero examines CUE files one at a time in deterministic work filename order. As soon as one CUE, all of its referenced files, and its in-memory rewrite form a complete DAT match, that game is promoted immediately. Romero then rescans work and continues with the next CUE; it does not collect or compare otherwise valid CUE candidates globally.

Noncompliant or unmatched CUE files remain in work.

## Hash cache

`ROOT/.romero.sqlite3` stores the managed area, relative path, size, high-resolution modification time, and SHA-1 for regular library and work files. Absolute root paths are never stored.

The database uses SQLite rollback journaling and an exclusive transaction. A second Romero process using the same root fails immediately. SQLite may temporarily create `.romero.sqlite3-journal`.

Romero commits each newly calculated SHA-1 immediately before scanning the next file. If Romero is interrupted, an unchanged rerun uses cache hits for every completed file and resumes hashing at the first unfinished file.

Cache path updates are also committed after successful moves, copies, and deletions. This keeps previously saved hashes associated with their current managed paths if reconciliation is interrupted.

The cache is disposable. Delete `.romero.sqlite3` only while Romero is not running; the next run rebuilds it by hashing the managed files.

## Missing reports and console output

For each selected DAT, `<library_path>/<header.name>.miss` contains missing game names sorted in case-insensitive ascending order, one per line. Complete systems receive an empty report.

While a run is in progress, the CLI prints each significant operation to standard error. Default output includes DAT loading, phases, hashing, game-level promotions, quarantine activity, recovery, cleanup, ignored non-regular work entries, and report writes. `--verbose` additionally prints hash saves, cache hits, each promoted ROM move, rewritten CUE write, and rewritten CUE source removal. Paths are shown relative to the Romero root where possible.

The final standard-output summary includes DAT and cache totals, moves, quarantined entries, promotions, recovery copies, duplicate cleanup, complete and missing games, regular leftovers, and ignored work entries. Recognized non-CUE leftovers are grouped by system and game. Each group lists every ROM expected by the DAT, sorted by ROM filename:

```text
Incomplete: IBM - PC compatible / Game Name
  Game Name (Track 01).bin [OK]
  Game Name (Track 02).bin -> downloaded-track.bin [OK]
  Game Name.cue [MISSING]
```

`[OK]` means the size and SHA-1 match; an arrow shows that the matching work filename differs from the expected DAT filename. `[MISSING]` means no assigned work file has the expected content or filename. `[MISMATCH]` means the expected filename exists in work but its size or SHA-1 is wrong. Work paths are omitted because only direct work files are processed.

In an interactive terminal, Romero highlights `Incomplete:` in yellow, `[OK]` in green, `[MISMATCH]` in orange, and `[MISSING]` in red. Colors are automatically removed when standard output is redirected or piped. Set the standard `NO_COLOR` environment variable to disable them explicitly.

All name and path ordering is case-insensitive, including DAT discovery, filesystem processing, deterministic filename assignment, missing reports, incomplete system/game groups, and ROM rows. Original spelling is the final tie-breaker when two names differ only by case, keeping output deterministic.

Each physical work file contributes to only one candidate game: Romero chooses the game with the most distinct content- or filename-matching work files before assignment, then breaks equal-size ties by case-insensitive system and game name. Exact filenames with incorrect content can therefore expose otherwise mismatch-only incomplete games. Unknown files remain aggregated in the totals, and leftover CUE files do not select candidate groups, though each selected game still reports the expected CUE as `OK`, `MISSING`, or `MISMATCH`. Keeping progress on standard error means the final summary can still be redirected separately.

## Development

Unit tests use in-memory filesystem and cache adapters and perform no operating-system filesystem operations. Integration tests synthesize isolated DATs, ZIP archives, CUE sheets, and ROM bytes in temporary directories.

Romero's minimum supported Rust version is 1.85.0. CI tests the committed dependency lockfile with that exact compiler in addition to running formatting and Clippy on stable Rust.

Run:

```sh
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

## Releases

An annotated semantic tag such as `v1.2.3`, matching the version in `Cargo.toml` and pointing to `main`, triggers release builds for:

- `x86_64-unknown-linux-musl`
- `i686-pc-windows-msvc`
- `x86_64-pc-windows-msvc`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`

Every asset is a ZIP containing exactly one `romero` or `romero.exe` binary at its root. The workflow creates a draft release, uploads all five assets, and publishes only after every build succeeds.

Repository administrators should enable immutable releases so a published tag and its attached binaries cannot subsequently be replaced.
