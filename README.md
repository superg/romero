# Romero — Fast CLI ROM Manager

Romero is a fast, automatic, command-line ROM manager for keeping Redump-style
collections complete, verified, and correctly named. Choose a directory as
`ROOT`, add your DAT catalogs, drop ROMs and CUE sheets into `work`, and let
Romero sort out the rest.

## 💡 Motivation

For quite a while, I managed my own collection with an unfinished C++ ROM
manager I had written for personal use. It did what I needed, but it was never
something I could comfortably hand to anyone else. Eventually, I decided to
revisit the idea and build the user-friendly tool I always wanted.

The new version would stay command-line only. More importantly, it had to be
fast enough for large disc collections, careful with the files already in the
library, and able to automate as much of the process as content-based matching
allows.

That is what Romero is built for: one CLI, one managed `ROOT`, and a repeatable
workflow for bringing a ROM set back in line with its latest catalogs.

## ✨ Features

- **Multiple systems**: Track any number of systems in one `ROOT`
- **Zipped DATs**: Read DAT files directly from `.zip` archives
- **Newest catalog selection**: Automatically use the newest DAT for each
  system
- **Content-first matching**: Identify ROMs by size and SHA-1
- **CUE-aware promotion**: Match CUE sheets to their content and rewrite file
  references to the exact DAT names
- **Automatic organization**: Move complete games into the correct system
  library as soon as they are recognized
- **Conservative library cleanup**: Move anything that does not belong in the
  verified library back to `work`
- **Persistent hash cache**: Reuse hashes through a relocatable SQLite cache
  instead of reading large ROMs again on every run
- **Missing-game reports**: Write one readable `.miss` file per selected system
- **Duplicate-game diagnostics**: Report complete games in `work` that already
  exist in the library without removing them

> **Note:** ZIP support applies to DAT archives only. Compressed ROM sets are
> not supported at this time; place individual ROM files and CUE sheets in
> `work`.

## 🚀 Quick Start

### Prerequisites

- Rust 1.97.1 or newer

### Installation

Install the current version directly from GitHub:

```bash
cargo install --git https://github.com/superg/romero --locked
```

> **Note:** Prebuilt binaries are not available yet. Automated CI release builds
> will be added later.

### Workflow

1. **Prepare `ROOT`**

   Create an empty `ROOT`. You can optionally add a
   [`romero.yaml`](#configuration) to customize the managed paths, or simply run
   Romero once to create the default `dats`, `work`, and `library` directories:

   ```bash
   mkdir -p ROOT
   romero ROOT
   ```

   The `ROOT` argument is optional when Romero is run from inside that
   directory:

   ```bash
   cd ROOT
   romero
   ```

2. **Add DATs and ROMs**

   Put `.dat` files or DAT `.zip` archives into `dats`. Put incoming ROMs and
   their CUE sheets into `work`, then run Romero:

   ```bash
   romero ROOT
   ```

3. **Update the catalogs**

   Whenever new DATs are released, add them to `dats` and run Romero again.
   Older catalogs can remain in place; Romero automatically selects the newest
   DAT for each system. Or delete them to keep things tidy.

4. **Add new ROMs**

   Whenever you acquire new ROMs, put them and their CUE sheets into `work` and
   run Romero again.

5. **Review incomplete ROMs**

   Check Romero's console output and the files left in `work`. Romero explains
   incomplete candidates, leaves anything it cannot safely promote for you to
   inspect, and updates each system's `.miss` report.

6. **Rinse and repeat**

   Keep adding updated DATs and new ROMs, run Romero, and review whatever
   remains in `work`.

## 📁 Directory Layout

With the default configuration, a populated `ROOT` looks like this:

```text
ROOT/
├── dats/
│   ├── Sony - PlayStation - Datfile (10949) (2026-07-24 13-57-31).dat
│   └── IBM - PC compatible - Datfile (60000) (2026-07-26 20-01-41).zip
├── work/
├── library/
│   ├── IBM - PC compatible/
│   ├── Sony - PlayStation/
│   ├── IBM - PC compatible.miss
│   └── Sony - PlayStation.miss
├── romero.yaml
└── .romero.sqlite3
```

- `dats` contains the catalogs Romero tracks
- `work` is the inbox for new, unmatched, incomplete, or displaced files
- `library` contains only correctly named files belonging to complete games,
  grouped by DAT system name
- `romero.yaml` is optional and changes the three managed paths
- `.romero.sqlite3` is Romero's automatic SHA-1 cache
- `library/*.miss` lists the games still missing from each system

System directories and `.miss` filenames come from the selected DAT's
`datafile/header/name`.

## ⚙️ Configuration

Romero looks only for an optional `romero.yaml` in `ROOT`. Without one, it uses
`library`, `work`, and `dats`.

To customize those paths, create `ROOT/romero.yaml`:

```yaml
library_path: verified
work_path: incoming
dat_path: catalogs
```

The available settings are:

| Setting | Purpose | Default |
|---|---|---|
| `library_path` | Verified, correctly named ROM library | `library` |
| `work_path` | Inbox and holding area for unmatched content | `work` |
| `dat_path` | DAT files and DAT ZIP archives | `dats` |

All configured paths are relative to `ROOT`. They must stay inside `ROOT` and
must not overlap one another. Romero creates missing managed directories after
the configuration and DAT inputs have been validated.

See the checked-in [`romero.yaml`](romero.yaml) for a complete default example.

## 🔄 How It Works

### 1. Select the catalogs

Romero loads top-level `.dat` files and every DAT contained in a top-level `.zip`
archive. Catalogs are grouped by their `datafile/header/name`, which lets one
`ROOT` manage multiple systems at once.

When several catalogs describe the same system, Romero compares their
`datafile/header/date` values and selects the newest. If equally new catalogs
disagree about their contents, Romero stops instead of guessing.

### 2. Audit the library

Each selected system has a flat directory under `library`. A game remains there
only when every expected file is present under its exact DAT filename and
matches both the expected size and SHA-1.

Foreign entries, incorrectly named files, and files belonging to incomplete
games are moved back to `work`. If the destination name already exists, Romero
automatically increments a numeric suffix until it finds an available name.

### 3. Inventory the work directory

After loading the DATs and opening the cache, Romero hashes files during the
initial library audit and `work` inventory. It reuses a cached SHA-1 when a
file's size and modification time have not changed; only new or changed files
need to be read and hashed again. Content matching begins after this inventory
is ready.

You do not need to pre-sort or correctly name incoming files. Put them all in
`work`; their contents are authoritative.

### 4. Match content and CUE sheets

Non-CUE content is matched to incomplete games by size and SHA-1. Once Romero
has identified the content for a game, it finds a suitable CUE sheet, rewrites
its file references to the DAT filenames, and accepts it only when the resulting
CUE has the exact size and SHA-1 required by the catalog.

When a game needs content that already exists in another verified library game,
Romero can copy that known-good content rather than requiring another physical
source in `work`.

### 5. Promote complete games

As soon as a complete game is recognized, Romero moves its work files into the
selected system directory under their exact DAT names. Verified shared content
coming from the library is copied, and a validated CUE is written directly to
its final destination.

### 6. Report what remains

Complete games in `work` that already exist in the library are reported as
`Duplicate`, with the matched work files shown as `OK`. Romero leaves those
files untouched.

Anything Romero cannot safely complete stays in `work`. Incomplete candidates
are explained in the console as `OK`, `LIBRARY`, `MISSING`, or `MISMATCH`, and
each system receives a `library/<system>.miss` text file containing one missing
game name per line.

At the end of the run, Romero prints a summary of DATs, cache hits and misses,
moves, promotions, complete games, missing games, and leftovers.

## 🛡️ Data Safety

Romero does not discard ROM payloads. Content rejected from the library is
always moved back to `work`, never deleted. Name collisions in `work` are
resolved by choosing a new filename so the existing file is preserved.

There are two important distinctions behind that guarantee:

- When a CUE sheet from `work` is successfully rewritten and validated, Romero
  writes the DAT-valid replacement to the library before removing the source CUE
- Generated state is replaceable: `.miss` reports are rewritten atomically and
  cache records are updated as files move

In short: library problems go back to `work`, unresolved and duplicate input
stays in `work`, and complete verified content moves forward to the library.

## 📄 License

Romero is licensed under the [GNU General Public License v3.0 or
later](LICENSE).

## 👨‍💻 Author

**Hennadiy Brych** — [gennadiy.brych@gmail.com](mailto:gennadiy.brych@gmail.com)

---

**Need help?** [Open an issue](https://github.com/superg/romero/issues).
