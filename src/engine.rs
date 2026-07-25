use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fmt::{self, Display, Formatter};
use std::io::Read;
use std::path::{Path, PathBuf};

use sha1::{Digest, Sha1};

use crate::cache::{CacheRecord, CacheStore, SqliteCache, relative_cache_key};
use crate::config::ResolvedConfig;
use crate::cue::CueDocument;
use crate::dat::load_selected_dats;
use crate::error::{Result, RomeroError};
use crate::filesystem::{DirectoryEntry, EntryKind, FileSystem, OsFileSystem};
use crate::model::{DatCatalog, GameSpec, RomSpec};
use crate::ordering;
use crate::reconcile::{
    HashedFile, all_assignments, collision_name, deterministic_assignment, game_is_complete,
    missing_report,
};

const LIBRARY_AREA: &str = "library";
const WORK_AREA: &str = "work";
const ANSI_GREEN: &str = "\x1b[32m";
const ANSI_YELLOW: &str = "\x1b[33m";
const ANSI_RED: &str = "\x1b[31m";
const ANSI_ORANGE: &str = "\x1b[38;5;208m";
const ANSI_RESET: &str = "\x1b[0m";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ProgressMoveKind {
    Quarantine,
    LibraryToWork,
    Promotion,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ProgressRemovalKind {
    ExistingGameDuplicate,
    RedundantLeftover,
    RewrittenCueSource,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ProgressEvent {
    LoadingConfiguration {
        root: PathBuf,
    },
    LoadingDats {
        path: PathBuf,
    },
    DatsLoaded {
        count: usize,
    },
    PreparingDirectories,
    OpeningCache {
        path: PathBuf,
    },
    AuditingLibrary,
    ProcessingWork,
    WritingReports,
    HashSaved {
        path: PathBuf,
    },
    CacheHit {
        path: PathBuf,
    },
    Hashing {
        path: PathBuf,
        size: u64,
    },
    Moving {
        kind: ProgressMoveKind,
        source: PathBuf,
        destination: PathBuf,
    },
    PromotingGame {
        system: String,
        game: String,
    },
    WritingCue {
        source: PathBuf,
        destination: PathBuf,
    },
    CopyingRecovery {
        source: PathBuf,
        destination: PathBuf,
    },
    Removing {
        kind: ProgressRemovalKind,
        path: PathBuf,
    },
    WritingReport {
        path: PathBuf,
        missing_games: usize,
    },
    IgnoringWorkEntry {
        path: PathBuf,
        kind: String,
    },
}

impl Display for ProgressEvent {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::LoadingConfiguration { root } => {
                write!(formatter, "Loading configuration from {}", root.display())
            }
            Self::LoadingDats { path } => {
                write!(formatter, "Loading DAT catalogs from {}", path.display())
            }
            Self::DatsLoaded { count } => {
                write!(formatter, "Loaded {count} selected DAT catalog(s)")
            }
            Self::PreparingDirectories => formatter.write_str("Preparing managed directories"),
            Self::OpeningCache { path } => write!(formatter, "Opening cache {}", path.display()),
            Self::AuditingLibrary => formatter.write_str("Auditing library"),
            Self::ProcessingWork => formatter.write_str("Processing work directory"),
            Self::WritingReports => formatter.write_str("Writing missing-game reports"),
            Self::HashSaved { path } => write!(formatter, "Hash saved: {}", path.display()),
            Self::CacheHit { path } => write!(formatter, "Cache hit: {}", path.display()),
            Self::Hashing { path, .. } => write!(formatter, "Hashing {}", path.display()),
            Self::Moving {
                kind,
                source,
                destination,
            } => {
                let action = match kind {
                    ProgressMoveKind::Quarantine => "Quarantining",
                    ProgressMoveKind::LibraryToWork => "Moving library file",
                    ProgressMoveKind::Promotion => "Moving ROM",
                };
                write!(
                    formatter,
                    "{action}: {} -> {}",
                    source.display(),
                    destination.display()
                )
            }
            Self::PromotingGame { system, game } => {
                write!(formatter, "Promoting game: {system} / {game}")
            }
            Self::WritingCue {
                source,
                destination,
            } => write!(
                formatter,
                "Writing rewritten CUE: {} -> {}",
                source.display(),
                destination.display()
            ),
            Self::CopyingRecovery {
                source,
                destination,
            } => write!(
                formatter,
                "Copying recovery file: {} -> {}",
                source.display(),
                destination.display()
            ),
            Self::Removing { kind, path } => {
                let reason = match kind {
                    ProgressRemovalKind::ExistingGameDuplicate => "existing-game duplicate",
                    ProgressRemovalKind::RedundantLeftover => "redundant leftover",
                    ProgressRemovalKind::RewrittenCueSource => "rewritten CUE source",
                };
                write!(formatter, "Removing {reason}: {}", path.display())
            }
            Self::WritingReport {
                path,
                missing_games,
            } => write!(
                formatter,
                "Writing report: {} ({missing_games} missing game(s))",
                path.display()
            ),
            Self::IgnoringWorkEntry { path, kind } => {
                write!(formatter, "Ignoring work {kind}: {}", path.display())
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum LeftoverStatus {
    Ok,
    Missing,
    Mismatch,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LeftoverMatch {
    pub expected_rom: String,
    pub work_path: Option<String>,
    pub status: LeftoverStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LeftoverDetail {
    pub system: String,
    pub game: String,
    pub matches: Vec<LeftoverMatch>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExecutionSummary {
    pub dats_loaded: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub rom_moves: u64,
    pub quarantined_entries: u64,
    pub quarantined_directories: u64,
    pub promotions: u64,
    pub recovery_copies: u64,
    pub existing_duplicates_removed: u64,
    pub redundant_leftovers_removed: u64,
    pub complete_games: u64,
    pub missing_games: u64,
    pub remaining_leftovers: u64,
    pub unknown_files: u64,
    pub ignored_work_entries: u64,
    pub leftover_details: Vec<LeftoverDetail>,
}

impl ExecutionSummary {
    pub fn colored(&self) -> impl Display + '_ {
        ColoredExecutionSummary(self)
    }

    fn fmt_with_color(&self, formatter: &mut Formatter<'_>, color: bool) -> fmt::Result {
        writeln!(formatter, "DATs loaded: {}", self.dats_loaded)?;
        writeln!(formatter, "Cache hits: {}", self.cache_hits)?;
        writeln!(formatter, "Cache misses: {}", self.cache_misses)?;
        writeln!(formatter, "ROM moves: {}", self.rom_moves)?;
        writeln!(
            formatter,
            "Quarantined entries: {} ({} directories)",
            self.quarantined_entries, self.quarantined_directories
        )?;
        writeln!(formatter, "Promotions: {}", self.promotions)?;
        writeln!(formatter, "Recovery copies: {}", self.recovery_copies)?;
        writeln!(
            formatter,
            "Existing-game duplicates removed: {}",
            self.existing_duplicates_removed
        )?;
        writeln!(
            formatter,
            "Redundant leftovers removed: {}",
            self.redundant_leftovers_removed
        )?;
        writeln!(formatter, "Complete games: {}", self.complete_games)?;
        writeln!(formatter, "Missing games: {}", self.missing_games)?;
        writeln!(
            formatter,
            "Remaining regular leftovers: {} ({} unknown)",
            self.remaining_leftovers, self.unknown_files
        )?;
        writeln!(
            formatter,
            "Ignored work entries: {}",
            self.ignored_work_entries
        )?;
        for leftover in &self.leftover_details {
            let (incomplete_style, incomplete_reset) = ansi_style(color, ANSI_YELLOW);
            writeln!(
                formatter,
                "{incomplete_style}Incomplete:{incomplete_reset} {} / {}",
                leftover.system, leftover.game
            )?;
            for rom in &leftover.matches {
                let (status, status_color) = match rom.status {
                    LeftoverStatus::Ok => ("[OK]", ANSI_GREEN),
                    LeftoverStatus::Missing => ("[MISSING]", ANSI_RED),
                    LeftoverStatus::Mismatch => ("[MISMATCH]", ANSI_ORANGE),
                };
                let (status_style, status_reset) = ansi_style(color, status_color);
                match (&rom.status, rom.work_path.as_deref()) {
                    (LeftoverStatus::Ok, Some(work_path))
                        if work_path != rom.expected_rom.as_str() =>
                    {
                        writeln!(
                            formatter,
                            "  {} -> {} {status_style}{status}{status_reset}",
                            rom.expected_rom, work_path
                        )?;
                    }
                    _ => writeln!(
                        formatter,
                        "  {} {status_style}{status}{status_reset}",
                        rom.expected_rom
                    )?,
                }
            }
        }
        Ok(())
    }
}

fn ansi_style(enabled: bool, style: &'static str) -> (&'static str, &'static str) {
    if enabled {
        (style, ANSI_RESET)
    } else {
        ("", "")
    }
}

struct ColoredExecutionSummary<'a>(&'a ExecutionSummary);

impl Display for ColoredExecutionSummary<'_> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        self.0.fmt_with_color(formatter, true)
    }
}

impl Display for ExecutionSummary {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        self.fmt_with_color(formatter, false)
    }
}

pub fn run(root: &Path) -> Result<ExecutionSummary> {
    run_with_progress(root, |_| {})
}

pub fn run_with_progress(
    root: &Path,
    mut progress: impl FnMut(&ProgressEvent),
) -> Result<ExecutionSummary> {
    progress(&ProgressEvent::LoadingConfiguration {
        root: root.to_path_buf(),
    });
    let config = ResolvedConfig::load(root)?;
    progress(&ProgressEvent::LoadingDats {
        path: relative_to_root(&config.root, &config.dat_path),
    });
    let catalogs = load_selected_dats(&config.dat_path)?;
    progress(&ProgressEvent::DatsLoaded {
        count: catalogs.len(),
    });
    progress(&ProgressEvent::PreparingDirectories);
    config.prepare_managed_directories()?;
    progress(&ProgressEvent::OpeningCache {
        path: relative_to_root(&config.root, &config.database_path),
    });
    let mut cache = SqliteCache::open(&config.database_path)?;
    let filesystem = OsFileSystem;
    let summary =
        execute_with_progress(&filesystem, &mut cache, &config, &catalogs, &mut progress)?;
    cache.commit()?;
    Ok(summary)
}

fn relative_to_root(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root).unwrap_or(path).to_path_buf()
}

fn entry_kind_label(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::File => "file",
        EntryKind::Directory => "directory",
        EntryKind::Symlink => "symlink",
        EntryKind::Other => "non-regular entry",
    }
}

#[derive(Clone, Debug)]
struct RuntimeFile {
    absolute_path: PathBuf,
    relative_path: PathBuf,
    name: OsString,
    cache_key: String,
    size: u64,
    modified_ns: i64,
    sha1: String,
}

impl RuntimeFile {
    fn assignment_id(&self) -> String {
        self.cache_key.clone()
    }

    fn is_cue(&self) -> bool {
        self.absolute_path
            .extension()
            .and_then(OsStr::to_str)
            .is_some_and(|extension| extension.eq_ignore_ascii_case("cue"))
    }
}

fn leftover_diagnostics(
    game: &GameSpec,
    assigned_files: &[RuntimeFile],
    all_work: &[RuntimeFile],
) -> Vec<LeftoverMatch> {
    let mut roms: Vec<_> = game.roms.iter().collect();
    roms.sort_by(|left, right| ordering::text(&left.name, &right.name));

    let mut available: BTreeSet<_> = (0..assigned_files.len()).collect();
    let mut assignments = BTreeMap::<String, usize>::new();

    for rom in roms.iter().copied().filter(|rom| !rom.is_cue()) {
        let exact = available.iter().copied().find(|index| {
            let file = &assigned_files[*index];
            file.name == OsStr::new(&rom.name) && file.size == rom.size && file.sha1 == rom.sha1
        });
        if let Some(index) = exact {
            available.remove(&index);
            assignments.insert(rom.name.clone(), index);
        }
    }

    for rom in roms.iter().copied().filter(|rom| !rom.is_cue()) {
        if assignments.contains_key(&rom.name) {
            continue;
        }
        let matching = available.iter().copied().find(|index| {
            let file = &assigned_files[*index];
            file.size == rom.size && file.sha1 == rom.sha1
        });
        if let Some(index) = matching {
            available.remove(&index);
            assignments.insert(rom.name.clone(), index);
        }
    }

    roms.into_iter()
        .map(|rom| {
            if rom.is_cue() {
                let exact = all_work
                    .iter()
                    .find(|file| file.name == OsStr::new(&rom.name));
                return match exact {
                    Some(file) if file.size == rom.size && file.sha1 == rom.sha1 => LeftoverMatch {
                        expected_rom: rom.name.clone(),
                        work_path: Some(work_relative_path(file)),
                        status: LeftoverStatus::Ok,
                    },
                    Some(file) => LeftoverMatch {
                        expected_rom: rom.name.clone(),
                        work_path: Some(work_relative_path(file)),
                        status: LeftoverStatus::Mismatch,
                    },
                    None => LeftoverMatch {
                        expected_rom: rom.name.clone(),
                        work_path: None,
                        status: LeftoverStatus::Missing,
                    },
                };
            }

            if let Some(index) = assignments.get(&rom.name) {
                let file = &assigned_files[*index];
                LeftoverMatch {
                    expected_rom: rom.name.clone(),
                    work_path: Some(work_relative_path(file)),
                    status: LeftoverStatus::Ok,
                }
            } else if let Some(file) = all_work.iter().find(|file| {
                file.name == OsStr::new(&rom.name)
                    && (file.size != rom.size || file.sha1 != rom.sha1)
            }) {
                LeftoverMatch {
                    expected_rom: rom.name.clone(),
                    work_path: Some(work_relative_path(file)),
                    status: LeftoverStatus::Mismatch,
                }
            } else {
                LeftoverMatch {
                    expected_rom: rom.name.clone(),
                    work_path: None,
                    status: LeftoverStatus::Missing,
                }
            }
        })
        .collect()
}

fn work_relative_path(file: &RuntimeFile) -> String {
    file.relative_path.to_string_lossy().into_owned()
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct GameKey {
    system: String,
    game: String,
}

#[derive(Clone, Debug, Default)]
struct LibraryState {
    complete: BTreeSet<GameKey>,
    members: Vec<RuntimeFile>,
    hashes: BTreeSet<String>,
}

#[derive(Clone, Debug)]
struct PromotionCandidate {
    key: GameKey,
    system: String,
    non_cue: Vec<(RuntimeFile, RomSpec)>,
    cue: Option<(RuntimeFile, RomSpec, Vec<u8>)>,
}

struct Engine<'a, F, C> {
    filesystem: &'a F,
    cache: &'a mut C,
    config: &'a ResolvedConfig,
    catalogs: &'a [DatCatalog],
    progress: &'a mut dyn FnMut(&ProgressEvent),
    summary: ExecutionSummary,
    accounted_cache_keys: BTreeSet<(String, String)>,
}

#[cfg(test)]
fn execute<F: FileSystem, C: CacheStore>(
    filesystem: &F,
    cache: &mut C,
    config: &ResolvedConfig,
    catalogs: &[DatCatalog],
) -> Result<ExecutionSummary> {
    let mut quiet = |_: &ProgressEvent| {};
    execute_with_progress(filesystem, cache, config, catalogs, &mut quiet)
}

fn execute_with_progress<F: FileSystem, C: CacheStore>(
    filesystem: &F,
    cache: &mut C,
    config: &ResolvedConfig,
    catalogs: &[DatCatalog],
    progress: &mut dyn FnMut(&ProgressEvent),
) -> Result<ExecutionSummary> {
    let mut engine = Engine {
        filesystem,
        cache,
        config,
        catalogs,
        progress,
        summary: ExecutionSummary {
            dats_loaded: catalogs.len() as u64,
            ..ExecutionSummary::default()
        },
        accounted_cache_keys: BTreeSet::new(),
    };

    engine.report(ProgressEvent::AuditingLibrary);
    engine.quarantine_library_structure()?;
    engine.audit_library_files()?;
    engine.report(ProgressEvent::ProcessingWork);
    engine.process_work()?;
    engine.report(ProgressEvent::WritingReports);
    engine.finish_reports_and_cache()?;
    Ok(engine.summary)
}

impl<F: FileSystem, C: CacheStore> Engine<'_, F, C> {
    fn report(&mut self, event: ProgressEvent) {
        (self.progress)(&event);
    }

    fn progress_path(&self, path: &Path) -> PathBuf {
        relative_to_root(&self.config.root, path)
    }

    fn quarantine_library_structure(&mut self) -> Result<()> {
        let mut expected_systems: Vec<_> = self
            .catalogs
            .iter()
            .map(|catalog| catalog.name.clone())
            .collect();
        expected_systems.sort_by(|left, right| ordering::text(left, right));
        let expected_system_names: BTreeSet<_> = expected_systems.iter().cloned().collect();
        let expected_reports: BTreeSet<_> = self
            .catalogs
            .iter()
            .map(|catalog| format!("{}.miss", catalog.name))
            .collect();

        for entry in self.filesystem.read_directory(&self.config.library_path)? {
            let name = entry.name.to_str();
            let recognized_system = name.is_some_and(|name| expected_system_names.contains(name))
                && entry.kind == EntryKind::Directory;
            let recognized_report = name.is_some_and(|name| expected_reports.contains(name))
                && entry.kind == EntryKind::File;
            if !recognized_system && !recognized_report {
                self.quarantine_entry(&entry)?;
            }
        }

        for system in expected_systems {
            let system_path = self.config.library_path.join(&system);
            if self.filesystem.metadata(&system_path)?.is_none() {
                self.filesystem.create_directory_all(&system_path)?;
            }
            for entry in self.filesystem.read_directory(&system_path)? {
                if entry.kind != EntryKind::File {
                    self.quarantine_entry(&entry)?;
                }
            }
        }
        Ok(())
    }

    fn quarantine_entry(&mut self, entry: &DirectoryEntry) -> Result<()> {
        let destination = self.collision_destination(&entry.name, entry.kind)?;
        self.report(ProgressEvent::Moving {
            kind: ProgressMoveKind::Quarantine,
            source: self.progress_path(&entry.path),
            destination: self.progress_path(&destination),
        });
        self.filesystem.rename(&entry.path, &destination)?;
        self.summary.quarantined_entries += 1;
        if entry.kind == EntryKind::Directory {
            self.summary.quarantined_directories += 1;
        }
        if let Ok(relative) = entry.path.strip_prefix(&self.config.library_path) {
            let key = relative_cache_key(relative);
            self.cache.remove(LIBRARY_AREA, &key)?;
        }
        self.cache.checkpoint()?;
        Ok(())
    }

    fn audit_library_files(&mut self) -> Result<()> {
        for catalog_index in 0..self.catalogs.len() {
            let system = self.catalogs[catalog_index].name.clone();
            let games = self.catalogs[catalog_index].games.clone();
            let files = self.scan_system(&system)?;
            let files_by_name = files_by_utf8_name(&files);
            let mut keep = BTreeSet::<OsString>::new();
            for game in &games {
                if game_is_complete(game, &files_by_name) {
                    keep.extend(game.roms.iter().map(|rom| OsString::from(&rom.name)));
                }
            }

            for file in files {
                if !keep.contains(&file.name) {
                    self.move_library_file_to_work(&file)?;
                }
            }
        }
        Ok(())
    }

    fn process_work(&mut self) -> Result<()> {
        for entry in self.filesystem.read_directory(&self.config.work_path)? {
            if entry.kind != EntryKind::File {
                self.report(ProgressEvent::IgnoringWorkEntry {
                    path: self.progress_path(&entry.path),
                    kind: entry_kind_label(entry.kind).to_owned(),
                });
            }
        }

        let mut attempted_recovery = BTreeSet::new();
        let mut library = self.read_library_state()?;
        let mut work = self.scan_work()?;
        self.checkpoint_scanned_inventory(&library, &work)?;

        loop {
            if let Some(candidate) = self.find_cue_candidate(&work)? {
                self.apply_candidate(candidate, &library)?;
                library = self.read_library_state()?;
                work = self.scan_work()?;
                continue;
            }
            if let Some(candidate) = self.find_content_candidate(&work, &library)? {
                self.apply_candidate(candidate, &library)?;
                library = self.read_library_state()?;
                work = self.scan_work()?;
                continue;
            }
            if self.try_recovery(&work, &library, &mut attempted_recovery)? {
                work = self.scan_work()?;
                continue;
            }
            break;
        }

        library = self.read_library_state()?;
        self.remove_redundant_leftovers(&library)?;
        Ok(())
    }

    fn checkpoint_scanned_inventory(
        &mut self,
        library: &LibraryState,
        work: &[RuntimeFile],
    ) -> Result<()> {
        let mut seen = BTreeSet::new();
        for member in &library.members {
            seen.insert((LIBRARY_AREA.to_owned(), member.cache_key.clone()));
        }
        for file in work {
            seen.insert((WORK_AREA.to_owned(), file.cache_key.clone()));
        }
        self.cache.retain(&seen)?;
        self.cache.checkpoint()
    }

    fn finish_reports_and_cache(&mut self) -> Result<()> {
        let library = self.read_library_state()?;
        let mut seen = BTreeSet::new();
        for member in &library.members {
            seen.insert((LIBRARY_AREA.to_owned(), member.cache_key.clone()));
        }

        let mut complete_count = 0_u64;
        let mut missing_count = 0_u64;
        for catalog in self.catalogs {
            let mut missing = Vec::new();
            for game in &catalog.games {
                let key = GameKey {
                    system: catalog.name.clone(),
                    game: game.name.clone(),
                };
                if library.complete.contains(&key) {
                    complete_count += 1;
                } else {
                    missing_count += 1;
                    missing.push(game.name.as_str());
                }
            }
            let missing_games = missing.len();
            let report = missing_report(missing.into_iter());
            let report_path = self
                .config
                .library_path
                .join(format!("{}.miss", catalog.name));
            self.report(ProgressEvent::WritingReport {
                path: self.progress_path(&report_path),
                missing_games,
            });
            self.filesystem.write_atomic(&report_path, &report)?;
        }
        self.summary.complete_games = complete_count;
        self.summary.missing_games = missing_count;

        let work_entries = self.filesystem.read_directory(&self.config.work_path)?;
        self.summary.ignored_work_entries = work_entries
            .iter()
            .filter(|entry| entry.kind != EntryKind::File)
            .count() as u64;
        let work = self.scan_work()?;
        self.summary.remaining_leftovers = work.len() as u64;
        self.summary.leftover_details.clear();
        self.summary.unknown_files = 0;
        let mut candidates_by_file = BTreeMap::<usize, BTreeSet<(String, String)>>::new();

        for (file_index, file) in work.iter().enumerate() {
            seen.insert((WORK_AREA.to_owned(), file.cache_key.clone()));
            if file.is_cue() {
                continue;
            }
            let mut recognized = false;
            for catalog in self.catalogs {
                for game in &catalog.games {
                    if game.non_cue_roms().any(|rom| {
                        (rom.sha1 == file.sha1 && rom.size == file.size)
                            || file.name == OsStr::new(&rom.name)
                    }) {
                        recognized = true;
                        candidates_by_file
                            .entry(file_index)
                            .or_default()
                            .insert((catalog.name.clone(), game.name.clone()));
                    }
                }
            }
            if !recognized {
                self.summary.unknown_files += 1;
            }
        }
        let mut candidate_group_sizes = BTreeMap::<(String, String), usize>::new();
        for candidates in candidates_by_file.values() {
            for group in candidates {
                *candidate_group_sizes.entry(group.clone()).or_default() += 1;
            }
        }
        let mut leftover_groups = BTreeMap::<(String, String), Vec<RuntimeFile>>::new();
        for (file_index, candidates) in candidates_by_file {
            let selected_group = candidates
                .iter()
                .min_by(|left, right| {
                    let left_size = candidate_group_sizes
                        .get(*left)
                        .copied()
                        .unwrap_or_default();
                    let right_size = candidate_group_sizes
                        .get(*right)
                        .copied()
                        .unwrap_or_default();
                    right_size.cmp(&left_size).then_with(|| {
                        ordering::text(&left.0, &right.0)
                            .then_with(|| ordering::text(&left.1, &right.1))
                    })
                })
                .expect("recognized leftovers have at least one candidate group")
                .clone();
            leftover_groups
                .entry(selected_group)
                .or_default()
                .push(work[file_index].clone());
        }
        let mut leftover_details: Vec<_> = leftover_groups
            .into_iter()
            .map(|((system, game_name), files)| {
                let game = self
                    .catalogs
                    .iter()
                    .find(|catalog| catalog.name == system)
                    .and_then(|catalog| catalog.games.iter().find(|game| game.name == game_name))
                    .expect("leftover group came from a selected DAT game");
                LeftoverDetail {
                    system,
                    game: game_name,
                    matches: leftover_diagnostics(game, &files, &work),
                }
            })
            .collect();
        leftover_details.sort_by(|left, right| {
            ordering::text(&left.system, &right.system)
                .then_with(|| ordering::text(&left.game, &right.game))
        });
        self.summary.leftover_details = leftover_details;
        self.cache.retain(&seen)?;
        Ok(())
    }

    fn find_cue_candidate(&mut self, work: &[RuntimeFile]) -> Result<Option<PromotionCandidate>> {
        let by_name: BTreeMap<OsString, RuntimeFile> = work
            .iter()
            .map(|file| (file.name.clone(), file.clone()))
            .collect();

        for cue_file in work.iter().filter(|file| file.is_cue()) {
            let bytes = self.filesystem.read(&cue_file.absolute_path)?;
            let Ok(cue) = CueDocument::parse(&bytes) else {
                continue;
            };
            let mut referenced = Vec::new();
            let mut all_exist = true;
            for name in cue.referenced_names() {
                let Some(file) = by_name.get(OsStr::new(name)) else {
                    all_exist = false;
                    break;
                };
                referenced.push((name.to_owned(), file.clone()));
            }
            if !all_exist {
                continue;
            }
            let source_hashes: Vec<_> = referenced
                .iter()
                .map(|(name, file)| (name.clone(), file.sha1.clone()))
                .collect();

            for (catalog_index, game_index) in self.game_indices() {
                let catalog = &self.catalogs[catalog_index];
                let game = &catalog.games[game_index];
                let Some(expected_cue) = game.cue() else {
                    continue;
                };
                let expected: Vec<_> = game.non_cue_roms().collect();
                for assignment in all_assignments(&source_hashes, &expected) {
                    let mut selected = Vec::new();
                    let mut sizes_match = true;
                    for (source_name, file) in &referenced {
                        let target_name = &assignment[source_name];
                        let target = expected
                            .iter()
                            .find(|rom| rom.name == *target_name)
                            .expect("assignment target came from expected ROMs");
                        if file.size != target.size {
                            sizes_match = false;
                            break;
                        }
                        selected.push((file.clone(), (*target).clone()));
                    }
                    if !sizes_match {
                        continue;
                    }
                    let rewritten = cue.rewrite(&assignment)?;
                    if rewritten.len() as u64 != expected_cue.size
                        || sha1_bytes(&rewritten) != expected_cue.sha1
                    {
                        continue;
                    }
                    return Ok(Some(PromotionCandidate {
                        key: GameKey {
                            system: catalog.name.clone(),
                            game: game.name.clone(),
                        },
                        system: catalog.name.clone(),
                        non_cue: selected,
                        cue: Some((cue_file.clone(), expected_cue.clone(), rewritten)),
                    }));
                }
            }
        }

        Ok(None)
    }

    fn find_content_candidate(
        &mut self,
        work: &[RuntimeFile],
        library: &LibraryState,
    ) -> Result<Option<PromotionCandidate>> {
        let non_cue: Vec<_> = work.iter().filter(|file| !file.is_cue()).cloned().collect();
        let cues: Vec<_> = work.iter().filter(|file| file.is_cue()).cloned().collect();

        let mut game_indices = self.game_indices();
        game_indices.sort_by_key(|(catalog_index, game_index)| {
            let catalog = &self.catalogs[*catalog_index];
            let game = &catalog.games[*game_index];
            library.complete.contains(&GameKey {
                system: catalog.name.clone(),
                game: game.name.clone(),
            })
        });
        for (catalog_index, game_index) in game_indices {
            let catalog = &self.catalogs[catalog_index];
            let game = &catalog.games[game_index];
            let expected: Vec<_> = game.non_cue_roms().collect();
            let Some(selected) = select_sources(&non_cue, &expected) else {
                continue;
            };
            let source_hashes: Vec<_> = selected
                .iter()
                .map(|file| (file.assignment_id(), file.sha1.clone()))
                .collect();
            let Some(assignment) = deterministic_assignment(&source_hashes, &expected) else {
                continue;
            };
            let non_cue_assignment = selected
                .into_iter()
                .map(|file| {
                    let target_name = &assignment[&file.assignment_id()];
                    let target = expected
                        .iter()
                        .find(|rom| rom.name == *target_name)
                        .expect("assignment target came from expected ROMs");
                    (file, (*target).clone())
                })
                .collect();

            let cue = if let Some(expected_cue) = game.cue() {
                let mut correct = None;
                for cue_file in &cues {
                    if cue_file.name == OsStr::new(&expected_cue.name)
                        && cue_file.size == expected_cue.size
                        && cue_file.sha1 == expected_cue.sha1
                    {
                        let bytes = self.filesystem.read(&cue_file.absolute_path)?;
                        if CueDocument::parse(&bytes).is_ok() {
                            correct = Some((cue_file.clone(), expected_cue.clone(), bytes));
                            break;
                        }
                    }
                }
                let Some(correct) = correct else {
                    continue;
                };
                Some(correct)
            } else {
                None
            };

            return Ok(Some(PromotionCandidate {
                key: GameKey {
                    system: catalog.name.clone(),
                    game: game.name.clone(),
                },
                system: catalog.name.clone(),
                non_cue: non_cue_assignment,
                cue,
            }));
        }
        Ok(None)
    }

    fn apply_candidate(
        &mut self,
        candidate: PromotionCandidate,
        library: &LibraryState,
    ) -> Result<()> {
        if library.complete.contains(&candidate.key) {
            let mut paths = Vec::new();
            for (file, _) in &candidate.non_cue {
                paths.push((file.absolute_path.clone(), file.cache_key.clone()));
            }
            if let Some((cue, _, _)) = &candidate.cue {
                paths.push((cue.absolute_path.clone(), cue.cache_key.clone()));
            }
            paths.sort_by(|left, right| ordering::path(&left.0, &right.0));
            paths.dedup_by(|left, right| left.0 == right.0);
            for (path, cache_key) in paths {
                self.report(ProgressEvent::Removing {
                    kind: ProgressRemovalKind::ExistingGameDuplicate,
                    path: self.progress_path(&path),
                });
                self.filesystem.remove_file(&path)?;
                self.cache.remove(WORK_AREA, &cache_key)?;
                self.cache.checkpoint()?;
                self.summary.existing_duplicates_removed += 1;
            }
            return Ok(());
        }

        let destination_directory = self.config.library_path.join(&candidate.system);
        self.filesystem
            .create_directory_all(&destination_directory)?;
        let mut non_cue: Vec<_> = candidate.non_cue.iter().collect();
        non_cue.sort_by(|left, right| {
            ordering::text(&left.1.name, &right.1.name)
                .then_with(|| ordering::os(&left.0.name, &right.0.name))
        });
        let mut target_paths = Vec::new();
        for (_, target) in &non_cue {
            target_paths.push(destination_directory.join(&target.name));
        }
        if let Some((_, target, _)) = &candidate.cue {
            target_paths.push(destination_directory.join(&target.name));
        }
        target_paths.sort_by(|left, right| ordering::path(left, right));
        for path in &target_paths {
            if self.filesystem.metadata(path)?.is_some() {
                return Err(RomeroError::Operational(format!(
                    "promotion target already exists: {}",
                    path.display()
                )));
            }
        }

        self.report(ProgressEvent::PromotingGame {
            system: candidate.key.system.clone(),
            game: candidate.key.game.clone(),
        });
        for (source, target) in non_cue {
            let destination = destination_directory.join(&target.name);
            self.report(ProgressEvent::Moving {
                kind: ProgressMoveKind::Promotion,
                source: self.progress_path(&source.absolute_path),
                destination: self.progress_path(&destination),
            });
            self.filesystem
                .rename(&source.absolute_path, &destination)?;
            self.cache.remove(WORK_AREA, &source.cache_key)?;
            self.cache_known_hash(
                LIBRARY_AREA,
                &self.config.library_path,
                &destination,
                &source.sha1,
            )?;
            self.cache.checkpoint()?;
            self.summary.rom_moves += 1;
        }

        if let Some((source, target, bytes)) = &candidate.cue {
            let destination = destination_directory.join(&target.name);
            self.report(ProgressEvent::WritingCue {
                source: self.progress_path(&source.absolute_path),
                destination: self.progress_path(&destination),
            });
            self.filesystem.write_atomic(&destination, bytes)?;
            self.cache_known_hash(
                LIBRARY_AREA,
                &self.config.library_path,
                &destination,
                &target.sha1,
            )?;
            self.cache.checkpoint()?;
            self.report(ProgressEvent::Removing {
                kind: ProgressRemovalKind::RewrittenCueSource,
                path: self.progress_path(&source.absolute_path),
            });
            self.filesystem.remove_file(&source.absolute_path)?;
            self.cache.remove(WORK_AREA, &source.cache_key)?;
            self.cache.checkpoint()?;
        }
        self.summary.promotions += 1;
        Ok(())
    }

    fn try_recovery(
        &mut self,
        work: &[RuntimeFile],
        library: &LibraryState,
        attempted: &mut BTreeSet<GameKey>,
    ) -> Result<bool> {
        let non_cue: Vec<_> = work.iter().filter(|file| !file.is_cue()).cloned().collect();
        for leftover in &non_cue {
            let mut candidates = BTreeSet::new();
            for catalog in self.catalogs {
                for game in &catalog.games {
                    if game
                        .non_cue_roms()
                        .any(|rom| rom.sha1 == leftover.sha1 && rom.size == leftover.size)
                    {
                        candidates.insert(GameKey {
                            system: catalog.name.clone(),
                            game: game.name.clone(),
                        });
                    }
                }
            }
            if candidates.len() != 1 {
                continue;
            }
            let key = candidates.into_iter().next().expect("one candidate exists");
            if attempted.contains(&key) || library.complete.contains(&key) {
                continue;
            }
            attempted.insert(key.clone());
            let game = self
                .find_game(&key)
                .expect("candidate key came from selected catalogs")
                .1
                .clone();
            let missing = missing_non_cue_roms(&game, &non_cue);
            if missing.is_empty() {
                continue;
            }

            let mut sources = Vec::new();
            let mut available = true;
            for rom in &missing {
                let source = library
                    .members
                    .iter()
                    .filter(|member| member.sha1 == rom.sha1 && member.size == rom.size)
                    .min_by(|left, right| {
                        ordering::path(&left.relative_path, &right.relative_path)
                    });
                let Some(source) = source else {
                    available = false;
                    break;
                };
                sources.push((source.clone(), rom.clone()));
            }
            if !available {
                continue;
            }

            for (source, rom) in sources {
                let destination =
                    self.collision_destination(OsStr::new(&rom.name), EntryKind::File)?;
                self.report(ProgressEvent::CopyingRecovery {
                    source: self.progress_path(&source.absolute_path),
                    destination: self.progress_path(&destination),
                });
                self.filesystem.copy(&source.absolute_path, &destination)?;
                self.cache_known_hash(
                    WORK_AREA,
                    &self.config.work_path,
                    &destination,
                    &source.sha1,
                )?;
                self.cache.checkpoint()?;
                self.summary.recovery_copies += 1;
            }
            return Ok(true);
        }
        Ok(false)
    }

    fn remove_redundant_leftovers(&mut self, library: &LibraryState) -> Result<()> {
        for file in self.scan_work()? {
            if !file.is_cue() && library.hashes.contains(&file.sha1) {
                self.report(ProgressEvent::Removing {
                    kind: ProgressRemovalKind::RedundantLeftover,
                    path: self.progress_path(&file.absolute_path),
                });
                self.filesystem.remove_file(&file.absolute_path)?;
                self.cache.remove(WORK_AREA, &file.cache_key)?;
                self.cache.checkpoint()?;
                self.summary.redundant_leftovers_removed += 1;
            }
        }
        Ok(())
    }

    fn read_library_state(&mut self) -> Result<LibraryState> {
        let mut state = LibraryState::default();
        for catalog_index in 0..self.catalogs.len() {
            let catalog_name = self.catalogs[catalog_index].name.clone();
            let games = self.catalogs[catalog_index].games.clone();
            let files = self.scan_system(&catalog_name)?;
            let files_by_name = files_by_utf8_name(&files);
            for game in &games {
                if game_is_complete(game, &files_by_name) {
                    state.complete.insert(GameKey {
                        system: catalog_name.clone(),
                        game: game.name.clone(),
                    });
                    for rom in &game.roms {
                        let file = files_by_name[&rom.name].clone();
                        state.hashes.insert(file.sha1.clone());
                        state.members.push(RuntimeFile {
                            absolute_path: self
                                .config
                                .library_path
                                .join(&catalog_name)
                                .join(&rom.name),
                            relative_path: file.relative_path.clone(),
                            name: OsString::from(&rom.name),
                            cache_key: relative_cache_key(&file.relative_path),
                            size: file.size,
                            modified_ns: file.modified_ns,
                            sha1: file.sha1.clone(),
                        });
                    }
                }
            }
        }
        state
            .members
            .sort_by(|left, right| ordering::path(&left.relative_path, &right.relative_path));
        Ok(state)
    }

    fn scan_system(&mut self, system: &str) -> Result<Vec<RuntimeFile>> {
        let directory = self.config.library_path.join(system);
        self.scan_regular_directory(LIBRARY_AREA, &self.config.library_path, &directory)
    }

    fn scan_work(&mut self) -> Result<Vec<RuntimeFile>> {
        self.scan_regular_directory(WORK_AREA, &self.config.work_path, &self.config.work_path)
    }

    fn scan_regular_directory(
        &mut self,
        area: &str,
        area_root: &Path,
        directory: &Path,
    ) -> Result<Vec<RuntimeFile>> {
        let mut files = Vec::new();
        for entry in self.filesystem.read_directory(directory)? {
            if entry.kind == EntryKind::File {
                files.push(self.hash_file(area, area_root, &entry.path)?);
            }
        }
        files.sort_by(|left, right| ordering::os(&left.name, &right.name));
        Ok(files)
    }

    fn hash_file(&mut self, area: &str, area_root: &Path, path: &Path) -> Result<RuntimeFile> {
        let metadata = self.filesystem.metadata(path)?.ok_or_else(|| {
            RomeroError::Operational(format!("file disappeared: {}", path.display()))
        })?;
        if metadata.kind != EntryKind::File {
            return Err(RomeroError::Operational(format!(
                "expected a regular file: {}",
                path.display()
            )));
        }
        let relative = path.strip_prefix(area_root).map_err(|_| {
            RomeroError::Operational(format!("{} is outside managed {area} path", path.display()))
        })?;
        let cache_key = relative_cache_key(relative);
        let accounting_key = (area.to_owned(), cache_key.clone());
        let first_seen = self.accounted_cache_keys.insert(accounting_key);
        let cached = self.cache.get(area, &cache_key)?;
        let sha1 = if let Some(record) = cached.filter(|record| {
            record.size == metadata.len && record.modified_ns == metadata.modified_ns
        }) {
            if first_seen {
                self.report(ProgressEvent::CacheHit {
                    path: self.progress_path(path),
                });
                self.summary.cache_hits += 1;
            }
            record.sha1
        } else {
            if first_seen {
                self.report(ProgressEvent::Hashing {
                    path: self.progress_path(path),
                    size: metadata.len,
                });
            }
            let sha1 = hash_reader(self.filesystem.open_reader(path)?)?;
            self.cache.put(&CacheRecord {
                area: area.to_owned(),
                path: cache_key.clone(),
                size: metadata.len,
                modified_ns: metadata.modified_ns,
                sha1: sha1.clone(),
            })?;
            self.cache.checkpoint()?;
            if first_seen {
                self.report(ProgressEvent::HashSaved {
                    path: self.progress_path(path),
                });
                self.summary.cache_misses += 1;
            }
            sha1
        };

        Ok(RuntimeFile {
            absolute_path: path.to_path_buf(),
            relative_path: relative.to_path_buf(),
            name: path
                .file_name()
                .ok_or_else(|| {
                    RomeroError::Operational(format!("file has no name: {}", path.display()))
                })?
                .to_os_string(),
            cache_key,
            size: metadata.len,
            modified_ns: metadata.modified_ns,
            sha1,
        })
    }

    fn cache_known_hash(
        &mut self,
        area: &str,
        area_root: &Path,
        path: &Path,
        sha1: &str,
    ) -> Result<()> {
        let metadata = self.filesystem.metadata(path)?.ok_or_else(|| {
            RomeroError::Operational(format!("file disappeared: {}", path.display()))
        })?;
        if metadata.kind != EntryKind::File {
            return Err(RomeroError::Operational(format!(
                "expected a regular file: {}",
                path.display()
            )));
        }
        let relative = path.strip_prefix(area_root).map_err(|_| {
            RomeroError::Operational(format!("{} is outside {area}", path.display()))
        })?;
        self.cache.put(&CacheRecord {
            area: area.to_owned(),
            path: relative_cache_key(relative),
            size: metadata.len,
            modified_ns: metadata.modified_ns,
            sha1: sha1.to_owned(),
        })
    }

    fn move_library_file_to_work(&mut self, file: &RuntimeFile) -> Result<()> {
        let destination = self.collision_destination(&file.name, EntryKind::File)?;
        self.report(ProgressEvent::Moving {
            kind: ProgressMoveKind::LibraryToWork,
            source: self.progress_path(&file.absolute_path),
            destination: self.progress_path(&destination),
        });
        self.filesystem.rename(&file.absolute_path, &destination)?;
        self.cache.remove(LIBRARY_AREA, &file.cache_key)?;
        self.cache_known_hash(WORK_AREA, &self.config.work_path, &destination, &file.sha1)?;
        self.cache.checkpoint()?;
        self.summary.rom_moves += 1;
        Ok(())
    }

    fn collision_destination(&self, name: &OsStr, kind: EntryKind) -> Result<PathBuf> {
        let original = self.config.work_path.join(name);
        if self.filesystem.metadata(&original)?.is_none() {
            return Ok(original);
        }
        for counter in 1..=u32::MAX {
            let candidate = self
                .config
                .work_path
                .join(collision_name(name, kind, counter));
            if self.filesystem.metadata(&candidate)?.is_none() {
                return Ok(candidate);
            }
        }
        Err(RomeroError::Operational(format!(
            "cannot resolve work collision for {:?}",
            name
        )))
    }

    fn game_indices(&self) -> Vec<(usize, usize)> {
        let mut indices = Vec::new();
        for (catalog_index, catalog) in self.catalogs.iter().enumerate() {
            for (game_index, game) in catalog.games.iter().enumerate() {
                indices.push((catalog_index, game_index, game.name.as_str()));
            }
        }
        indices.sort_by(|left, right| {
            ordering::text(&self.catalogs[left.0].name, &self.catalogs[right.0].name)
                .then_with(|| ordering::text(left.2, right.2))
        });
        indices
            .into_iter()
            .map(|(catalog, game, _)| (catalog, game))
            .collect()
    }

    fn find_game(&self, key: &GameKey) -> Option<(&DatCatalog, &GameSpec)> {
        self.catalogs.iter().find_map(|catalog| {
            if catalog.name != key.system {
                return None;
            }
            catalog
                .games
                .iter()
                .find(|game| game.name == key.game)
                .map(|game| (catalog, game))
        })
    }
}

fn files_by_utf8_name(files: &[RuntimeFile]) -> BTreeMap<String, HashedFile> {
    files
        .iter()
        .filter_map(|file| {
            Some((
                file.name.to_str()?.to_owned(),
                HashedFile {
                    relative_path: file.relative_path.clone(),
                    size: file.size,
                    modified_ns: file.modified_ns,
                    sha1: file.sha1.clone(),
                },
            ))
        })
        .collect()
}

fn select_sources(available: &[RuntimeFile], expected: &[&RomSpec]) -> Option<Vec<RuntimeFile>> {
    let mut by_identity = BTreeMap::<(String, u64), Vec<RuntimeFile>>::new();
    for file in available {
        by_identity
            .entry((file.sha1.clone(), file.size))
            .or_default()
            .push(file.clone());
    }
    for files in by_identity.values_mut() {
        files.sort_by(|left, right| ordering::os(&left.name, &right.name));
    }

    let mut selected = Vec::new();
    let mut used = BTreeMap::<(String, u64), usize>::new();
    let mut expected_sorted = expected.to_vec();
    expected_sorted.sort_by(|left, right| ordering::text(&left.name, &right.name));
    for rom in expected_sorted {
        let identity = (rom.sha1.clone(), rom.size);
        let index = used.entry(identity.clone()).or_default();
        let file = by_identity.get(&identity)?.get(*index)?.clone();
        *index += 1;
        selected.push(file);
    }
    Some(selected)
}

fn missing_non_cue_roms(game: &GameSpec, work: &[RuntimeFile]) -> Vec<RomSpec> {
    let mut available = BTreeMap::<(String, u64), usize>::new();
    for file in work {
        *available.entry((file.sha1.clone(), file.size)).or_default() += 1;
    }
    let mut expected: Vec<_> = game.non_cue_roms().cloned().collect();
    expected.sort_by(|left, right| ordering::text(&left.name, &right.name));
    let mut missing = Vec::new();
    for rom in expected {
        let count = available.entry((rom.sha1.clone(), rom.size)).or_default();
        if *count > 0 {
            *count -= 1;
        } else {
            missing.push(rom);
        }
    }
    missing
}

fn hash_reader(mut reader: Box<dyn Read>) -> Result<String> {
    let mut hasher = Sha1::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| RomeroError::io("cannot hash file", error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn sha1_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{CacheRecord, CacheStore, SqliteCache};
    use crate::filesystem::MemoryFileSystem;
    use crate::model::DatDate;

    fn fixture() -> (MemoryFileSystem, ResolvedConfig) {
        let filesystem = MemoryFileSystem::new(Path::new("/root"));
        for directory in ["/root/library", "/root/work", "/root/dats"] {
            filesystem.add_directory(directory);
        }
        let config = ResolvedConfig {
            root: PathBuf::from("/root"),
            library_path: PathBuf::from("/root/library"),
            work_path: PathBuf::from("/root/work"),
            dat_path: PathBuf::from("/root/dats"),
            database_path: PathBuf::from("/root/.romero.sqlite3"),
        };
        (filesystem, config)
    }

    fn rom(name: &str, contents: &[u8]) -> RomSpec {
        RomSpec {
            name: name.into(),
            size: contents.len() as u64,
            sha1: sha1_bytes(contents),
        }
    }

    fn catalog(games: Vec<GameSpec>) -> DatCatalog {
        DatCatalog {
            name: "System".into(),
            date: DatDate([2026, 1, 1, 0, 0, 0]),
            games,
            source: "memory.dat".into(),
        }
    }

    struct FailingCache {
        inner: SqliteCache,
        fail_next_put: bool,
    }

    impl FailingCache {
        fn new() -> Self {
            Self {
                inner: SqliteCache::in_memory().unwrap(),
                fail_next_put: true,
            }
        }
    }

    impl CacheStore for FailingCache {
        fn get(&self, area: &str, path: &str) -> Result<Option<CacheRecord>> {
            self.inner.get(area, path)
        }

        fn put(&mut self, record: &CacheRecord) -> Result<()> {
            if std::mem::take(&mut self.fail_next_put) {
                return Err(RomeroError::Cache("injected cache failure".into()));
            }
            self.inner.put(record)
        }

        fn remove(&mut self, area: &str, path: &str) -> Result<()> {
            self.inner.remove(area, path)
        }

        fn retain(&mut self, seen: &BTreeSet<(String, String)>) -> Result<()> {
            self.inner.retain(seen)
        }

        fn checkpoint(&mut self) -> Result<()> {
            self.inner.checkpoint()
        }

        fn commit(&mut self) -> Result<()> {
            self.inner.commit()
        }
    }

    #[test]
    fn promotes_complete_multiset_to_exact_dat_filenames_without_os_filesystem() {
        let (filesystem, config) = fixture();
        filesystem.add_file("/root/work/acquired-name.bin", b"payload".to_vec());
        let catalogs = [catalog(vec![GameSpec {
            name: "Game".into(),
            roms: vec![rom("Expected Name.bin", b"payload")],
        }])];
        let mut cache = SqliteCache::in_memory().unwrap();

        let summary = execute(&filesystem, &mut cache, &config, &catalogs).unwrap();

        assert!(!filesystem.contains("/root/work/acquired-name.bin"));
        assert_eq!(
            filesystem.contents("/root/library/System/Expected Name.bin"),
            Some(b"payload".to_vec())
        );
        assert_eq!(summary.promotions, 1);
        assert_eq!(
            filesystem.contents("/root/library/System.miss"),
            Some(Vec::new())
        );
    }

    #[test]
    fn emits_deterministic_progress_for_hashing_promotion_and_reports() {
        let (filesystem, config) = fixture();
        filesystem.add_file("/root/work/download.bin", b"payload".to_vec());
        filesystem.add_directory("/root/work/notes");
        let catalogs = [catalog(vec![GameSpec {
            name: "Game".into(),
            roms: vec![rom("Expected.bin", b"payload")],
        }])];
        let mut cache = SqliteCache::in_memory().unwrap();
        let mut events = Vec::new();

        execute_with_progress(&filesystem, &mut cache, &config, &catalogs, &mut |event| {
            events.push(event.clone())
        })
        .unwrap();

        assert!(events.contains(&ProgressEvent::AuditingLibrary));
        assert!(events.contains(&ProgressEvent::ProcessingWork));
        assert!(events.contains(&ProgressEvent::IgnoringWorkEntry {
            path: PathBuf::from("work/notes"),
            kind: "directory".into(),
        }));
        assert_eq!(
            events
                .iter()
                .filter(|event| {
                    matches!(
                        event,
                        ProgressEvent::Hashing { path, .. }
                            if path == Path::new("work/download.bin")
                    )
                })
                .count(),
            1
        );
        assert_eq!(
            ProgressEvent::Hashing {
                path: PathBuf::from("work/download.bin"),
                size: 7,
            }
            .to_string(),
            "Hashing work/download.bin"
        );
        assert!(events.contains(&ProgressEvent::HashSaved {
            path: PathBuf::from("work/download.bin"),
        }));
        assert!(events.contains(&ProgressEvent::CacheHit {
            path: PathBuf::from("library/System/Expected.bin"),
        }));
        assert!(events.contains(&ProgressEvent::PromotingGame {
            system: "System".into(),
            game: "Game".into(),
        }));
        assert!(events.contains(&ProgressEvent::Moving {
            kind: ProgressMoveKind::Promotion,
            source: PathBuf::from("work/download.bin"),
            destination: PathBuf::from("library/System/Expected.bin"),
        }));
        assert!(events.contains(&ProgressEvent::WritingReport {
            path: PathBuf::from("library/System.miss"),
            missing_games: 0,
        }));
    }

    #[test]
    fn rewrites_cue_and_promotes_every_source_to_its_dat_name() {
        let (filesystem, config) = fixture();
        let source_cue = b"FILE \"source.bin\" BINARY\r\n";
        let expected_cue = b"FILE \"Game.bin\" BINARY\r\n";
        filesystem.add_file("/root/work/source.bin", b"disc".to_vec());
        filesystem.add_file("/root/work/template.cue", source_cue.to_vec());
        let catalogs = [catalog(vec![GameSpec {
            name: "Game".into(),
            roms: vec![rom("Game.cue", expected_cue), rom("Game.bin", b"disc")],
        }])];
        let mut cache = SqliteCache::in_memory().unwrap();

        execute(&filesystem, &mut cache, &config, &catalogs).unwrap();

        assert_eq!(
            filesystem.contents("/root/library/System/Game.bin"),
            Some(b"disc".to_vec())
        );
        assert_eq!(
            filesystem.contents("/root/library/System/Game.cue"),
            Some(expected_cue.to_vec())
        );
        assert!(!filesystem.contains("/root/work/template.cue"));
    }

    #[test]
    fn first_full_cue_match_is_applied_without_comparing_other_games() {
        let (filesystem, config) = fixture();
        let source_cue = b"FILE \"download.bin\" BINARY\n";
        let first_cue = b"FILE \"First.bin\" BINARY\n";
        let second_cue = b"FILE \"Second.bin\" BINARY\n";
        filesystem.add_file("/root/work/download.bin", b"disc".to_vec());
        filesystem.add_file("/root/work/template.cue", source_cue.to_vec());
        let catalogs = [catalog(vec![
            GameSpec {
                name: "First Game".into(),
                roms: vec![rom("First.cue", first_cue), rom("First.bin", b"disc")],
            },
            GameSpec {
                name: "Second Game".into(),
                roms: vec![rom("Second.cue", second_cue), rom("Second.bin", b"disc")],
            },
        ])];
        let mut cache = SqliteCache::in_memory().unwrap();

        let summary = execute(&filesystem, &mut cache, &config, &catalogs).unwrap();

        assert_eq!(summary.promotions, 1);
        assert_eq!(summary.complete_games, 1);
        assert_eq!(summary.missing_games, 1);
        assert_eq!(
            filesystem.contents("/root/library/System/First.bin"),
            Some(b"disc".to_vec())
        );
        assert_eq!(
            filesystem.contents("/root/library/System/First.cue"),
            Some(first_cue.to_vec())
        );
        assert!(!filesystem.contains("/root/library/System/Second.bin"));
        assert!(!filesystem.contains("/root/work/download.bin"));
        assert!(!filesystem.contains("/root/work/template.cue"));
    }

    #[test]
    fn quarantines_unknown_directories_whole_and_uses_directory_collisions() {
        let (filesystem, config) = fixture();
        filesystem.add_directory("/root/library/Unknown");
        filesystem.add_file("/root/library/Unknown/nested.bin", b"nested".to_vec());
        filesystem.add_directory("/root/work/Unknown");
        filesystem.add_symlink("/root/library/link");
        filesystem.add_other("/root/library/special");
        let mut cache = SqliteCache::in_memory().unwrap();

        let summary = execute(&filesystem, &mut cache, &config, &[]).unwrap();

        assert!(filesystem.contains("/root/work/Unknown.1"));
        assert_eq!(
            filesystem.contents("/root/work/Unknown.1/nested.bin"),
            Some(b"nested".to_vec())
        );
        assert!(filesystem.contains("/root/work/link"));
        assert!(filesystem.contains("/root/work/special"));
        assert_eq!(summary.quarantined_entries, 3);
        assert_eq!(summary.quarantined_directories, 1);
        assert_eq!(summary.ignored_work_entries, 4);
    }

    #[test]
    fn quarantines_stale_report_regular_file_and_tracked_nested_entries() {
        let (filesystem, config) = fixture();
        filesystem.add_directory("/root/library/System");
        filesystem.add_directory("/root/library/System/nested");
        filesystem.add_file(
            "/root/library/System/nested/evidence.bin",
            b"evidence".to_vec(),
        );
        filesystem.add_symlink("/root/library/System/link");
        filesystem.add_file("/root/library/Stale.miss", b"stale".to_vec());
        filesystem.add_file("/root/library/loose.bin", b"loose".to_vec());
        let catalogs = [catalog(vec![GameSpec {
            name: "Missing".into(),
            roms: vec![rom("Missing.bin", b"missing")],
        }])];
        let mut cache = SqliteCache::in_memory().unwrap();

        let summary = execute(&filesystem, &mut cache, &config, &catalogs).unwrap();

        assert_eq!(summary.quarantined_entries, 4);
        assert_eq!(summary.quarantined_directories, 1);
        assert_eq!(
            filesystem.contents("/root/work/nested/evidence.bin"),
            Some(b"evidence".to_vec())
        );
        assert!(filesystem.contains("/root/work/link"));
        assert_eq!(
            filesystem.contents("/root/work/Stale.miss"),
            Some(b"stale".to_vec())
        );
        assert_eq!(
            filesystem.contents("/root/work/loose.bin"),
            Some(b"loose".to_vec())
        );
    }

    #[test]
    fn evacuates_every_file_from_a_partial_library_game() {
        let (filesystem, config) = fixture();
        filesystem.add_directory("/root/library/System");
        filesystem.add_file("/root/library/System/One.bin", b"one".to_vec());
        let catalogs = [catalog(vec![GameSpec {
            name: "Partial".into(),
            roms: vec![rom("One.bin", b"one"), rom("Two.bin", b"two")],
        }])];
        let mut cache = SqliteCache::in_memory().unwrap();

        execute(&filesystem, &mut cache, &config, &catalogs).unwrap();

        assert!(!filesystem.contains("/root/library/System/One.bin"));
        assert_eq!(
            filesystem.contents("/root/work/One.bin"),
            Some(b"one".to_vec())
        );
    }

    #[test]
    fn recovery_copies_shared_content_then_promotes_the_unique_game() {
        let (filesystem, config) = fixture();
        filesystem.add_directory("/root/library/System");
        filesystem.add_file("/root/library/System/Existing.bin", b"shared".to_vec());
        filesystem.add_file("/root/work/random.bin", b"unique".to_vec());
        let catalogs = [catalog(vec![
            GameSpec {
                name: "Existing".into(),
                roms: vec![rom("Existing.bin", b"shared")],
            },
            GameSpec {
                name: "Target".into(),
                roms: vec![
                    rom("Target.bin", b"unique"),
                    rom("Target.shared.bin", b"shared"),
                ],
            },
        ])];
        let mut cache = SqliteCache::in_memory().unwrap();

        let summary = execute(&filesystem, &mut cache, &config, &catalogs).unwrap();

        assert_eq!(summary.recovery_copies, 1);
        assert_eq!(summary.promotions, 1);
        assert_eq!(
            filesystem.contents("/root/library/System/Target.bin"),
            Some(b"unique".to_vec())
        );
        assert_eq!(
            filesystem.contents("/root/library/System/Target.shared.bin"),
            Some(b"shared".to_vec())
        );
        assert_eq!(
            filesystem.contents("/root/library/System/Existing.bin"),
            Some(b"shared".to_vec())
        );
    }

    #[test]
    fn deletes_redundant_non_cue_leftover_but_not_a_cue() {
        let (filesystem, config) = fixture();
        filesystem.add_directory("/root/library/System");
        filesystem.add_file("/root/library/System/One.bin", b"shared".to_vec());
        filesystem.add_file("/root/library/System/Two.bin", b"other".to_vec());
        filesystem.add_file("/root/work/lone-copy.bin", b"shared".to_vec());
        filesystem.add_file(
            "/root/work/leftover.cue",
            b"FILE \"missing.bin\" BINARY\n".to_vec(),
        );
        let catalogs = [catalog(vec![GameSpec {
            name: "Existing".into(),
            roms: vec![rom("One.bin", b"shared"), rom("Two.bin", b"other")],
        }])];
        let mut cache = SqliteCache::in_memory().unwrap();

        let summary = execute(&filesystem, &mut cache, &config, &catalogs).unwrap();

        assert!(!filesystem.contains("/root/work/lone-copy.bin"));
        assert!(filesystem.contains("/root/work/leftover.cue"));
        assert_eq!(summary.redundant_leftovers_removed, 1);
    }

    #[test]
    fn removes_only_selected_work_copies_of_a_verified_existing_game() {
        let (filesystem, config) = fixture();
        filesystem.add_directory("/root/library/System");
        filesystem.add_file("/root/library/System/Game.bin", b"payload".to_vec());
        filesystem.add_file("/root/work/copy.bin", b"payload".to_vec());
        filesystem.add_file("/root/work/unrelated.bin", b"unrelated".to_vec());
        let catalogs = [catalog(vec![GameSpec {
            name: "Game".into(),
            roms: vec![rom("Game.bin", b"payload")],
        }])];
        let mut cache = SqliteCache::in_memory().unwrap();

        let summary = execute(&filesystem, &mut cache, &config, &catalogs).unwrap();

        assert!(!filesystem.contains("/root/work/copy.bin"));
        assert!(filesystem.contains("/root/work/unrelated.bin"));
        assert_eq!(summary.existing_duplicates_removed, 1);
        assert_eq!(summary.redundant_leftovers_removed, 0);
    }

    #[test]
    fn assigns_shared_leftovers_to_the_largest_game_group() {
        let (filesystem, config) = fixture();
        filesystem.add_file("/root/work/a-shared.bin", b"shared".to_vec());
        filesystem.add_file("/root/work/b-secondary.bin", b"secondary".to_vec());
        filesystem.add_file("/root/work/c-tie.bin", b"tie".to_vec());
        filesystem.add_file("/root/work/mystery.bin", b"mystery".to_vec());
        filesystem.add_file(
            "/root/work/unmatched.cue",
            b"FILE \"missing.bin\" BINARY\n".to_vec(),
        );
        let system = catalog(vec![
            GameSpec {
                name: "Alpha".into(),
                roms: vec![
                    rom("Alpha Shared.bin", b"shared"),
                    rom("Alpha Secondary.bin", b"secondary"),
                    rom("Alpha Missing.bin", b"missing"),
                ],
            },
            GameSpec {
                name: "Beta".into(),
                roms: vec![
                    rom("Beta Shared.bin", b"shared"),
                    rom("Beta Missing.bin", b"beta missing"),
                ],
            },
            GameSpec {
                name: "Tie".into(),
                roms: vec![
                    rom("System Tie.bin", b"tie"),
                    rom("System Tie Missing.bin", b"system tie missing"),
                ],
            },
        ]);
        let other = DatCatalog {
            name: "alpha system".into(),
            date: DatDate([2026, 1, 1, 0, 0, 0]),
            games: vec![
                GameSpec {
                    name: "Alpha".into(),
                    roms: vec![
                        rom("Other Shared.bin", b"shared"),
                        rom("Other Missing.bin", b"other missing"),
                    ],
                },
                GameSpec {
                    name: "Tie".into(),
                    roms: vec![
                        rom("Other Tie.bin", b"tie"),
                        rom("Other Tie Missing.bin", b"other tie missing"),
                    ],
                },
            ],
            source: "other.dat".into(),
        };
        let catalogs = [system, other];
        let mut cache = SqliteCache::in_memory().unwrap();

        let summary = execute(&filesystem, &mut cache, &config, &catalogs).unwrap();

        assert_eq!(summary.remaining_leftovers, 5);
        assert_eq!(summary.unknown_files, 1);
        assert_eq!(
            summary.leftover_details,
            vec![
                LeftoverDetail {
                    system: "alpha system".into(),
                    game: "Tie".into(),
                    matches: vec![
                        LeftoverMatch {
                            expected_rom: "Other Tie Missing.bin".into(),
                            work_path: None,
                            status: LeftoverStatus::Missing,
                        },
                        LeftoverMatch {
                            expected_rom: "Other Tie.bin".into(),
                            work_path: Some("c-tie.bin".into()),
                            status: LeftoverStatus::Ok,
                        },
                    ],
                },
                LeftoverDetail {
                    system: "System".into(),
                    game: "Alpha".into(),
                    matches: vec![
                        LeftoverMatch {
                            expected_rom: "Alpha Missing.bin".into(),
                            work_path: None,
                            status: LeftoverStatus::Missing,
                        },
                        LeftoverMatch {
                            expected_rom: "Alpha Secondary.bin".into(),
                            work_path: Some("b-secondary.bin".into()),
                            status: LeftoverStatus::Ok,
                        },
                        LeftoverMatch {
                            expected_rom: "Alpha Shared.bin".into(),
                            work_path: Some("a-shared.bin".into()),
                            status: LeftoverStatus::Ok,
                        },
                    ],
                },
            ]
        );
        assert!(format!("{summary}").contains(concat!(
            "Incomplete: alpha system / Tie\n",
            "  Other Tie Missing.bin [MISSING]\n",
            "  Other Tie.bin -> c-tie.bin [OK]\n",
            "Incomplete: System / Alpha\n",
            "  Alpha Missing.bin [MISSING]\n",
            "  Alpha Secondary.bin -> b-secondary.bin [OK]\n",
            "  Alpha Shared.bin -> a-shared.bin [OK]\n",
        )));
    }

    #[test]
    fn explains_exact_renamed_missing_and_mismatched_leftover_roms() {
        let (filesystem, config) = fixture();
        filesystem.add_file("/root/work/zBad.bin", b"wrong".to_vec());
        filesystem.add_file("/root/work/Exact.bin", b"exact".to_vec());
        filesystem.add_file("/root/work/download.bin", b"renamed".to_vec());
        filesystem.add_file("/root/work/Game.cue", b"wrong cue".to_vec());
        let catalogs = [catalog(vec![GameSpec {
            name: "Game".into(),
            roms: vec![
                rom("alpha.bin", b"renamed"),
                rom("Game.cue", b"expected cue"),
                rom("Missing.bin", b"missing"),
                rom("Exact.bin", b"exact"),
                rom("zBad.bin", b"expected bad"),
            ],
        }])];
        let mut cache = SqliteCache::in_memory().unwrap();

        let summary = execute(&filesystem, &mut cache, &config, &catalogs).unwrap();

        assert_eq!(summary.unknown_files, 0);
        assert_eq!(
            summary.leftover_details,
            vec![LeftoverDetail {
                system: "System".into(),
                game: "Game".into(),
                matches: vec![
                    LeftoverMatch {
                        expected_rom: "alpha.bin".into(),
                        work_path: Some("download.bin".into()),
                        status: LeftoverStatus::Ok,
                    },
                    LeftoverMatch {
                        expected_rom: "Exact.bin".into(),
                        work_path: Some("Exact.bin".into()),
                        status: LeftoverStatus::Ok,
                    },
                    LeftoverMatch {
                        expected_rom: "Game.cue".into(),
                        work_path: Some("Game.cue".into()),
                        status: LeftoverStatus::Mismatch,
                    },
                    LeftoverMatch {
                        expected_rom: "Missing.bin".into(),
                        work_path: None,
                        status: LeftoverStatus::Missing,
                    },
                    LeftoverMatch {
                        expected_rom: "zBad.bin".into(),
                        work_path: Some("zBad.bin".into()),
                        status: LeftoverStatus::Mismatch,
                    },
                ],
            }]
        );
        assert!(format!("{summary}").contains(concat!(
            "Incomplete: System / Game\n",
            "  alpha.bin -> download.bin [OK]\n",
            "  Exact.bin [OK]\n",
            "  Game.cue [MISMATCH]\n",
            "  Missing.bin [MISSING]\n",
            "  zBad.bin [MISMATCH]\n",
        )));
        let colored = format!("{}", summary.colored());
        assert!(colored.contains("\x1b[33mIncomplete:\x1b[0m System / Game"));
        assert!(colored.contains("zBad.bin \x1b[38;5;208m[MISMATCH]\x1b[0m"));
        assert!(colored.contains("Exact.bin \x1b[32m[OK]\x1b[0m"));
        assert!(colored.contains("Missing.bin \x1b[31m[MISSING]\x1b[0m"));
        assert!(!format!("{summary}").contains('\x1b'));
    }

    #[test]
    fn reports_missing_games_case_insensitively() {
        let (filesystem, config) = fixture();
        let catalogs = [catalog(vec![
            GameSpec {
                name: "Zulu".into(),
                roms: vec![rom("Zulu.bin", b"z")],
            },
            GameSpec {
                name: "alpha".into(),
                roms: vec![rom("alpha.bin", b"a")],
            },
            GameSpec {
                name: "Beta".into(),
                roms: vec![rom("Beta.bin", b"b")],
            },
        ])];
        let mut cache = SqliteCache::in_memory().unwrap();

        execute(&filesystem, &mut cache, &config, &catalogs).unwrap();

        assert_eq!(
            filesystem.contents("/root/library/System.miss"),
            Some(b"alpha\nBeta\nZulu\n".to_vec())
        );
    }

    #[test]
    fn injected_move_failure_leaves_source_intact() {
        let (filesystem, config) = fixture();
        filesystem.add_file("/root/work/source.bin", b"payload".to_vec());
        filesystem.fail_next_rename();
        let catalogs = [catalog(vec![GameSpec {
            name: "Game".into(),
            roms: vec![rom("Target.bin", b"payload")],
        }])];
        let mut cache = SqliteCache::in_memory().unwrap();

        assert!(execute(&filesystem, &mut cache, &config, &catalogs).is_err());
        assert!(filesystem.contains("/root/work/source.bin"));
        assert!(!filesystem.contains("/root/library/System/Target.bin"));
    }

    #[test]
    fn cache_hit_and_miss_accounting_reuses_unchanged_metadata() {
        let (filesystem, config) = fixture();
        filesystem.add_file("/root/work/unknown.bin", b"payload".to_vec());
        let mut cache = SqliteCache::in_memory().unwrap();

        let first = execute(&filesystem, &mut cache, &config, &[]).unwrap();
        let second = execute(&filesystem, &mut cache, &config, &[]).unwrap();

        assert_eq!((first.cache_hits, first.cache_misses), (0, 1));
        assert_eq!((second.cache_hits, second.cache_misses), (1, 0));
    }

    #[test]
    fn injected_hash_and_cache_failures_leave_work_source_intact() {
        let (filesystem, config) = fixture();
        filesystem.add_file("/root/work/source.bin", b"payload".to_vec());
        filesystem.fail_next_hash();
        let mut cache = SqliteCache::in_memory().unwrap();

        assert!(execute(&filesystem, &mut cache, &config, &[]).is_err());
        assert!(filesystem.contains("/root/work/source.bin"));

        let mut cache = FailingCache::new();
        assert!(execute(&filesystem, &mut cache, &config, &[]).is_err());
        assert!(filesystem.contains("/root/work/source.bin"));
    }

    #[test]
    fn injected_cue_read_failure_leaves_all_work_sources_intact() {
        let (filesystem, config) = fixture();
        filesystem.add_file("/root/work/disc.bin", b"disc".to_vec());
        filesystem.add_file(
            "/root/work/template.cue",
            b"FILE \"disc.bin\" BINARY\n".to_vec(),
        );
        filesystem.fail_next_read();
        let catalogs = [catalog(vec![GameSpec {
            name: "Game".into(),
            roms: vec![
                rom("Game.cue", b"FILE \"Game.bin\" BINARY\n"),
                rom("Game.bin", b"disc"),
            ],
        }])];
        let mut cache = SqliteCache::in_memory().unwrap();

        assert!(execute(&filesystem, &mut cache, &config, &catalogs).is_err());
        assert!(filesystem.contains("/root/work/disc.bin"));
        assert!(filesystem.contains("/root/work/template.cue"));
    }

    #[test]
    fn injected_cue_write_failure_preserves_the_source_cue() {
        let (filesystem, config) = fixture();
        filesystem.add_file("/root/work/disc.bin", b"disc".to_vec());
        filesystem.add_file(
            "/root/work/template.cue",
            b"FILE \"disc.bin\" BINARY\n".to_vec(),
        );
        filesystem.fail_next_write();
        let catalogs = [catalog(vec![GameSpec {
            name: "Game".into(),
            roms: vec![
                rom("Game.cue", b"FILE \"Game.bin\" BINARY\n"),
                rom("Game.bin", b"disc"),
            ],
        }])];
        let mut cache = SqliteCache::in_memory().unwrap();

        assert!(execute(&filesystem, &mut cache, &config, &catalogs).is_err());
        assert!(filesystem.contains("/root/work/template.cue"));
        assert!(!filesystem.contains("/root/library/System/Game.cue"));
    }
}
