use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fmt::{self, Display, Formatter};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha1::{Digest, Sha1};

use crate::cache::{CacheRecord, CacheStore, SqliteCache, relative_cache_key};
use crate::config::ResolvedConfig;
use crate::cue::CueDocument;
use crate::dat::load_selected_dats;
use crate::error::{Result, RomeroError};
use crate::filesystem::{DirectoryEntry, EntryKind, FileSystem, OsFileSystem};
use crate::model::{DatCatalog, GameSpec, RomSpec};
use crate::ordering;
use crate::reconcile::{HashedFile, collision_name, game_is_complete, missing_report};

const LIBRARY_AREA: &str = "library";
const WORK_AREA: &str = "work";
const CACHE_CHECKPOINT_INTERVAL: Duration = Duration::from_secs(60);
const ANSI_GREEN: &str = "\x1b[32m";
const ANSI_LIGHT_CYAN: &str = "\x1b[96m";
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
    RewrittenCueSource,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CacheCommitReason {
    PeriodicCheckpoint,
    RunComplete,
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
    MatchingContent,
    WritingReports,
    HashSaved {
        path: PathBuf,
    },
    CacheCommitted {
        reason: CacheCommitReason,
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
    CopyingLibraryRom {
        source: PathBuf,
        destination: PathBuf,
    },
    Incomplete {
        detail: LeftoverDetail,
    },
    Duplicate {
        detail: DuplicateDetail,
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
            Self::MatchingContent => formatter.write_str("Content matching"),
            Self::WritingReports => formatter.write_str("Writing missing-game reports"),
            Self::HashSaved { path } => write!(formatter, "Hash saved: {}", path.display()),
            Self::CacheCommitted { reason } => match reason {
                CacheCommitReason::PeriodicCheckpoint => {
                    formatter.write_str("Cache committed: periodic checkpoint")
                }
                CacheCommitReason::RunComplete => {
                    formatter.write_str("Cache committed: run complete")
                }
            },
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
            Self::CopyingLibraryRom {
                source,
                destination,
            } => write!(
                formatter,
                "Copying library ROM: {} -> {}",
                source.display(),
                destination.display()
            ),
            Self::Incomplete { detail } => write!(formatter, "{detail}"),
            Self::Duplicate { detail } => write!(formatter, "{detail}"),
            Self::Removing { kind, path } => {
                let reason = match kind {
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
    Library,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DuplicateDetail {
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
    pub library_copies: u64,
    pub complete_games: u64,
    pub missing_games: u64,
    pub remaining_leftovers: u64,
    pub unknown_files: u64,
    pub ignored_work_entries: u64,
    pub leftover_details: Vec<LeftoverDetail>,
    pub duplicate_details: Vec<DuplicateDetail>,
}

impl ExecutionSummary {
    pub fn colored(&self) -> impl Display + '_ {
        ColoredExecutionSummary(self)
    }

    fn fmt_with_color(&self, formatter: &mut Formatter<'_>, _color: bool) -> fmt::Result {
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
        writeln!(formatter, "Library copies: {}", self.library_copies)?;
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
        Ok(())
    }
}

impl LeftoverDetail {
    pub fn colored(&self) -> impl Display + '_ {
        ColoredLeftoverDetail(self)
    }

    fn fmt_with_color(&self, formatter: &mut Formatter<'_>, color: bool) -> fmt::Result {
        fmt_game_detail(
            formatter,
            color,
            "Incomplete",
            &self.system,
            &self.game,
            &self.matches,
        )
    }
}

impl DuplicateDetail {
    pub fn colored(&self) -> impl Display + '_ {
        ColoredDuplicateDetail(self)
    }

    fn fmt_with_color(&self, formatter: &mut Formatter<'_>, color: bool) -> fmt::Result {
        fmt_game_detail(
            formatter,
            color,
            "Duplicate",
            &self.system,
            &self.game,
            &self.matches,
        )
    }
}

impl Display for LeftoverDetail {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        self.fmt_with_color(formatter, false)
    }
}

impl Display for DuplicateDetail {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        self.fmt_with_color(formatter, false)
    }
}

fn fmt_game_detail(
    formatter: &mut Formatter<'_>,
    color: bool,
    heading: &str,
    system: &str,
    game: &str,
    matches: &[LeftoverMatch],
) -> fmt::Result {
    let (heading_style, heading_reset) = ansi_style(color, ANSI_LIGHT_CYAN);
    writeln!(
        formatter,
        "{heading_style}{heading}:{heading_reset} {system} / {game}"
    )?;
    for rom in matches {
        let (status, status_color) = match rom.status {
            LeftoverStatus::Ok => ("[OK]", ANSI_GREEN),
            LeftoverStatus::Library => ("[LIBRARY]", ANSI_YELLOW),
            LeftoverStatus::Missing => ("[MISSING]", ANSI_RED),
            LeftoverStatus::Mismatch => ("[MISMATCH]", ANSI_ORANGE),
        };
        let (status_style, status_reset) = ansi_style(color, status_color);
        match (&rom.status, rom.work_path.as_deref()) {
            (LeftoverStatus::Ok, Some(work_path)) if work_path != rom.expected_rom.as_str() => {
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
    Ok(())
}

fn ansi_style(enabled: bool, style: &'static str) -> (&'static str, &'static str) {
    if enabled {
        (style, ANSI_RESET)
    } else {
        ("", "")
    }
}

struct ColoredExecutionSummary<'a>(&'a ExecutionSummary);

struct ColoredLeftoverDetail<'a>(&'a LeftoverDetail);

struct ColoredDuplicateDetail<'a>(&'a DuplicateDetail);

impl Display for ColoredExecutionSummary<'_> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        self.0.fmt_with_color(formatter, true)
    }
}

impl Display for ColoredLeftoverDetail<'_> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        self.0.fmt_with_color(formatter, true)
    }
}

impl Display for ColoredDuplicateDetail<'_> {
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
    let catalogs = load_selected_dats(&OsFileSystem, &config.dat_path)?;
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
    commit_completed_run(&mut cache, &mut progress)?;
    Ok(summary)
}

fn commit_completed_run<C: CacheStore>(
    cache: &mut C,
    progress: &mut dyn FnMut(&ProgressEvent),
) -> Result<()> {
    cache.commit()?;
    progress(&ProgressEvent::CacheCommitted {
        reason: CacheCommitReason::RunComplete,
    });
    Ok(())
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
    fn is_cue(&self) -> bool {
        self.absolute_path
            .extension()
            .and_then(OsStr::to_str)
            .is_some_and(|extension| extension.eq_ignore_ascii_case("cue"))
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ContentId {
    size: u64,
    sha1: String,
}

impl ContentId {
    fn from_file(file: &RuntimeFile) -> Self {
        Self {
            size: file.size,
            sha1: file.sha1.clone(),
        }
    }

    fn from_rom(rom: &RomSpec) -> Self {
        Self {
            size: rom.size,
            sha1: rom.sha1.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct GameId {
    catalog: usize,
    game: usize,
}

#[derive(Clone, Copy, Debug)]
struct RomLocation {
    game: GameId,
    rom: usize,
}

struct DatIndex {
    ordered_games: Vec<GameId>,
    games_by_content: BTreeMap<ContentId, BTreeSet<GameId>>,
    roms_by_content: BTreeMap<ContentId, Vec<RomLocation>>,
}

impl DatIndex {
    fn new(catalogs: &[DatCatalog]) -> Self {
        let mut ordered_games = Vec::new();
        let mut games_by_content = BTreeMap::<ContentId, BTreeSet<GameId>>::new();
        let mut roms_by_content = BTreeMap::<ContentId, Vec<RomLocation>>::new();

        for (catalog_index, catalog) in catalogs.iter().enumerate() {
            for (game_index, game) in catalog.games.iter().enumerate() {
                let game_id = GameId {
                    catalog: catalog_index,
                    game: game_index,
                };
                ordered_games.push(game_id);
                for (rom_index, rom) in game.roms.iter().enumerate() {
                    let content = ContentId::from_rom(rom);
                    roms_by_content
                        .entry(content.clone())
                        .or_default()
                        .push(RomLocation {
                            game: game_id,
                            rom: rom_index,
                        });
                    if !rom.is_cue() {
                        games_by_content.entry(content).or_default().insert(game_id);
                    }
                }
            }
        }

        ordered_games.sort_by(|left, right| {
            let left_catalog = &catalogs[left.catalog];
            let right_catalog = &catalogs[right.catalog];
            ordering::text(&left_catalog.name, &right_catalog.name).then_with(|| {
                ordering::text(
                    &left_catalog.games[left.game].name,
                    &right_catalog.games[right.game].name,
                )
            })
        });
        for locations in roms_by_content.values_mut() {
            locations.sort_by(|left, right| {
                let left_catalog = &catalogs[left.game.catalog];
                let right_catalog = &catalogs[right.game.catalog];
                let left_path = Path::new(&left_catalog.name)
                    .join(&left_catalog.games[left.game.game].roms[left.rom].name);
                let right_path = Path::new(&right_catalog.name)
                    .join(&right_catalog.games[right.game.game].roms[right.rom].name);
                ordering::path(&left_path, &right_path)
            });
        }

        Self {
            ordered_games,
            games_by_content,
            roms_by_content,
        }
    }

    fn candidate_games_for_content(&self, content: &ContentId) -> Option<&BTreeSet<GameId>> {
        self.games_by_content.get(content)
    }
}

#[derive(Clone)]
enum CachedCue {
    Valid {
        bytes: Vec<u8>,
        document: CueDocument,
    },
    Invalid {
        bytes: Vec<u8>,
    },
}

struct WorkInventory {
    files: BTreeMap<String, RuntimeFile>,
    by_name: BTreeMap<OsString, String>,
    by_content: BTreeMap<ContentId, BTreeSet<String>>,
    cues: BTreeMap<String, CachedCue>,
    cue_keys_by_file_count: BTreeMap<usize, Vec<String>>,
}

impl WorkInventory {
    fn new(files: Vec<RuntimeFile>) -> Self {
        let mut inventory = Self {
            files: BTreeMap::new(),
            by_name: BTreeMap::new(),
            by_content: BTreeMap::new(),
            cues: BTreeMap::new(),
            cue_keys_by_file_count: BTreeMap::new(),
        };
        for file in files {
            inventory.insert(file);
        }
        inventory
    }

    fn insert(&mut self, file: RuntimeFile) {
        let key = file.cache_key.clone();
        let content = ContentId::from_file(&file);
        self.by_name.insert(file.name.clone(), key.clone());
        self.by_content
            .entry(content)
            .or_default()
            .insert(key.clone());
        self.files.insert(key, file);
    }

    fn remove(&mut self, key: &str) -> Option<RuntimeFile> {
        let file = self.files.remove(key)?;
        self.by_name.remove(&file.name);
        let content = ContentId::from_file(&file);
        if let Some(keys) = self.by_content.get_mut(&content) {
            keys.remove(key);
            if keys.is_empty() {
                self.by_content.remove(&content);
            }
        }
        if let Some(CachedCue::Valid { document, .. }) = self.cues.remove(key) {
            let file_count = document.file_count();
            if let Some(keys) = self.cue_keys_by_file_count.get_mut(&file_count) {
                keys.retain(|candidate| candidate != key);
                if keys.is_empty() {
                    self.cue_keys_by_file_count.remove(&file_count);
                }
            }
        }
        Some(file)
    }

    fn get_by_name(&self, name: &OsStr) -> Option<&RuntimeFile> {
        let key = self.by_name.get(name)?;
        self.files.get(key)
    }

    fn sorted_files(&self) -> Vec<&RuntimeFile> {
        let mut files: Vec<_> = self.files.values().collect();
        files.sort_by(|left, right| ordering::os(&left.name, &right.name));
        files
    }

    fn sorted_non_cue_files(&self) -> Vec<&RuntimeFile> {
        self.sorted_files()
            .into_iter()
            .filter(|file| !file.is_cue())
            .collect()
    }

    fn sorted_cue_keys(&self) -> Vec<String> {
        self.sorted_files()
            .into_iter()
            .filter(|file| file.is_cue())
            .map(|file| file.cache_key.clone())
            .collect()
    }

    fn cue_keys_for_file_count(&self, file_count: usize) -> Vec<String> {
        self.cue_keys_by_file_count
            .get(&file_count)
            .cloned()
            .unwrap_or_default()
    }

    fn files_for_content(&self, content: &ContentId, non_cue_only: bool) -> Vec<RuntimeFile> {
        let mut files: Vec<_> = self
            .by_content
            .get(content)
            .into_iter()
            .flatten()
            .filter_map(|key| self.files.get(key))
            .filter(|file| !non_cue_only || !file.is_cue())
            .cloned()
            .collect();
        files.sort_by(|left, right| {
            ordering::os(&left.name, &right.name)
                .then_with(|| ordering::path(&left.relative_path, &right.relative_path))
        });
        files
    }

    fn len(&self) -> usize {
        self.files.len()
    }

    fn cache_keys(&self) -> impl Iterator<Item = &String> {
        self.files.keys()
    }

    fn load_cue<F: FileSystem>(
        &mut self,
        filesystem: &F,
        key: &str,
    ) -> Result<(RuntimeFile, Vec<u8>, Option<CueDocument>)> {
        let file = self
            .files
            .get(key)
            .cloned()
            .expect("CUE key came from work inventory");
        if !self.cues.contains_key(key) {
            let bytes = filesystem.read(&file.absolute_path)?;
            let cached = match CueDocument::parse(&bytes) {
                Ok(document) => CachedCue::Valid { bytes, document },
                Err(_) => CachedCue::Invalid { bytes },
            };
            self.cues.insert(key.to_owned(), cached);
        }
        Ok(match self.cues.get(key).expect("CUE was cached") {
            CachedCue::Valid { bytes, document } => (file, bytes.clone(), Some(document.clone())),
            CachedCue::Invalid { bytes } => (file, bytes.clone(), None),
        })
    }

    fn index_cues<F: FileSystem>(&mut self, filesystem: &F) -> Result<()> {
        self.cue_keys_by_file_count.clear();
        for key in self.sorted_cue_keys() {
            let (_, _, document) = self.load_cue(filesystem, &key)?;
            if let Some(document) = document {
                self.cue_keys_by_file_count
                    .entry(document.file_count())
                    .or_default()
                    .push(key);
            }
        }
        Ok(())
    }
}

fn assign_partial_work_sources(
    game: &GameSpec,
    available: &[RuntimeFile],
    seed: Option<&RuntimeFile>,
) -> Vec<(RuntimeFile, RomSpec)> {
    let mut expected_by_content = BTreeMap::<ContentId, Vec<RomSpec>>::new();
    for rom in game.non_cue_roms() {
        expected_by_content
            .entry(ContentId::from_rom(rom))
            .or_default()
            .push(rom.clone());
    }
    let mut assignments = Vec::new();
    for (content, mut targets) in expected_by_content {
        targets.sort_by(|left, right| ordering::text(&left.name, &right.name));
        let mut sources: Vec<_> = available
            .iter()
            .filter(|file| ContentId::from_file(file) == content)
            .cloned()
            .collect();
        sources.sort_by(|left, right| {
            ordering::os(&left.name, &right.name)
                .then_with(|| ordering::path(&left.relative_path, &right.relative_path))
        });

        let mut selected = Vec::new();
        if let Some(seed) = seed.filter(|seed| ContentId::from_file(seed) == content) {
            selected.push(seed.clone());
        }
        for target in &targets {
            if selected.len() == targets.len() {
                break;
            }
            if let Some(source) = sources.iter().find(|source| {
                source.name == OsStr::new(&target.name)
                    && !selected
                        .iter()
                        .any(|selected: &RuntimeFile| selected.cache_key == source.cache_key)
            }) {
                selected.push(source.clone());
            }
        }
        for source in sources {
            if selected.len() == targets.len() {
                break;
            }
            if !selected
                .iter()
                .any(|selected| selected.cache_key == source.cache_key)
            {
                selected.push(source);
            }
        }

        let mut remaining_sources = selected;
        let mut remaining_targets = targets;
        let mut exact = Vec::new();
        let mut target_index = 0;
        while target_index < remaining_targets.len() {
            let source_index = remaining_sources.iter().position(|source| {
                source.name == OsStr::new(&remaining_targets[target_index].name)
            });
            if let Some(source_index) = source_index {
                exact.push((
                    remaining_sources.remove(source_index),
                    remaining_targets.remove(target_index),
                ));
            } else {
                target_index += 1;
            }
        }
        exact.extend(remaining_sources.into_iter().zip(remaining_targets));
        assignments.extend(exact);
    }
    assignments.sort_by(|left, right| ordering::text(&left.1.name, &right.1.name));
    assignments
}

fn exact_mismatches(
    game: &GameSpec,
    available: &[RuntimeFile],
    assigned_targets: &BTreeSet<String>,
    selected_keys: &BTreeSet<&str>,
) -> Vec<(RuntimeFile, RomSpec)> {
    let mut mismatches = Vec::new();
    for rom in game.non_cue_roms() {
        if assigned_targets.contains(&rom.name) {
            continue;
        }
        if let Some(file) = available.iter().find(|file| {
            file.name == OsStr::new(&rom.name)
                && !selected_keys.contains(file.cache_key.as_str())
                && (file.size != rom.size || file.sha1 != rom.sha1)
        }) {
            mismatches.push((file.clone(), rom.clone()));
        }
    }
    mismatches.sort_by(|left, right| ordering::text(&left.1.name, &right.1.name));
    mismatches
}

fn rewrite_selected_cue(
    cue: &CueDocument,
    non_cue: &[(RomSource, RomSpec)],
    expected_cue: &RomSpec,
) -> Result<Option<Vec<u8>>> {
    let referenced_names: Vec<_> = cue.referenced_names().collect();

    let mut work_names = BTreeMap::<String, String>::new();
    let mut library_names = BTreeSet::new();
    for (source, target) in non_cue {
        match source {
            RomSource::Work(file) => {
                if let Some(name) = file.name.to_str() {
                    work_names.insert(name.to_owned(), target.name.clone());
                }
            }
            RomSource::Library(_) => {
                library_names.insert(target.name.clone());
            }
        }
    }
    let mut replacements = BTreeMap::new();
    for source in referenced_names {
        let target = if let Some(target) = work_names.get(source) {
            target.clone()
        } else if library_names.contains(source) {
            source.to_owned()
        } else {
            return Ok(None);
        };
        replacements.insert(source.to_owned(), target);
    }
    let rewritten = cue.rewrite(&replacements)?;
    Ok(
        (rewritten.len() as u64 == expected_cue.size
            && sha1_bytes(&rewritten) == expected_cue.sha1)
            .then_some(rewritten),
    )
}

fn work_relative_path(file: &RuntimeFile) -> String {
    file.relative_path.to_string_lossy().into_owned()
}

struct LibraryAudit {
    complete_games: BTreeSet<GameId>,
    cache_keys: BTreeSet<String>,
}

#[derive(Clone, Debug)]
struct PromotionCandidate {
    game: GameId,
    system: String,
    non_cue: Vec<(RomSource, RomSpec)>,
    cue: Option<SelectedCue>,
}

#[derive(Clone, Debug)]
enum RomSource {
    Work(RuntimeFile),
    Library(PathBuf),
}

type SelectedCue = (RuntimeFile, RomSpec, Vec<u8>);
type CueMismatch = (RuntimeFile, RomSpec);

#[derive(Clone, Debug)]
struct CandidateEvaluation {
    game: GameId,
    non_cue: Vec<(RomSource, RomSpec)>,
    cue: Option<SelectedCue>,
    cue_mismatch: Option<CueMismatch>,
    mismatches: Vec<(RuntimeFile, RomSpec)>,
    score: usize,
    complete: bool,
}

#[derive(Clone, Debug)]
struct QueueItem {
    file: RuntimeFile,
    candidates: Vec<GameId>,
}

#[derive(Debug)]
struct CacheCheckpointScheduler {
    interval: Duration,
    dirty_since: Option<Instant>,
}

impl CacheCheckpointScheduler {
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            dirty_since: None,
        }
    }

    fn checkpoint_due(&self, now: Instant) -> bool {
        self.dirty_since
            .is_some_and(|dirty_since| now.duration_since(dirty_since) >= self.interval)
    }

    fn mark_dirty(&mut self, now: Instant) {
        self.dirty_since.get_or_insert(now);
    }

    fn committed(&mut self) {
        self.dirty_since = None;
    }
}

struct Engine<'a, F, C> {
    filesystem: &'a F,
    cache: &'a mut C,
    config: &'a ResolvedConfig,
    catalogs: &'a [DatCatalog],
    dat_index: DatIndex,
    progress: &'a mut dyn FnMut(&ProgressEvent),
    now: &'a mut dyn FnMut() -> Instant,
    summary: ExecutionSummary,
    accounted_cache_keys: BTreeSet<(String, String)>,
    cache_checkpoints: CacheCheckpointScheduler,
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
    let mut now = Instant::now;
    execute_with_progress_and_clock(filesystem, cache, config, catalogs, progress, &mut now)
}

fn execute_with_progress_and_clock<F: FileSystem, C: CacheStore>(
    filesystem: &F,
    cache: &mut C,
    config: &ResolvedConfig,
    catalogs: &[DatCatalog],
    progress: &mut dyn FnMut(&ProgressEvent),
    now: &mut dyn FnMut() -> Instant,
) -> Result<ExecutionSummary> {
    let mut engine = Engine {
        filesystem,
        cache,
        config,
        catalogs,
        dat_index: DatIndex::new(catalogs),
        progress,
        now,
        summary: ExecutionSummary {
            dats_loaded: catalogs.len() as u64,
            ..ExecutionSummary::default()
        },
        accounted_cache_keys: BTreeSet::new(),
        cache_checkpoints: CacheCheckpointScheduler::new(CACHE_CHECKPOINT_INTERVAL),
    };

    engine.report(ProgressEvent::AuditingLibrary);
    engine.quarantine_library_structure()?;
    let audit = engine.audit_library_files()?;
    engine.report(ProgressEvent::ProcessingWork);
    let mut work = engine.scan_initial_work()?;
    engine.retain_initial_inventory(&audit.cache_keys, &work)?;
    let mut complete_games = audit.complete_games;
    engine.process_work(&mut work, &mut complete_games)?;
    engine.report(ProgressEvent::WritingReports);
    engine.finish_reports(&work, &complete_games)?;
    Ok(engine.summary)
}

impl<F: FileSystem, C: CacheStore> Engine<'_, F, C> {
    fn report(&mut self, event: ProgressEvent) {
        (self.progress)(&event);
    }

    fn prepare_cache_mutation(&mut self) -> Result<Instant> {
        let now = (self.now)();
        if self.cache_checkpoints.checkpoint_due(now) {
            self.cache.checkpoint()?;
            self.cache_checkpoints.committed();
            self.report(ProgressEvent::CacheCommitted {
                reason: CacheCommitReason::PeriodicCheckpoint,
            });
        }
        Ok(now)
    }

    fn cache_put(&mut self, record: &CacheRecord) -> Result<()> {
        let now = self.prepare_cache_mutation()?;
        self.cache.put(record)?;
        self.cache_checkpoints.mark_dirty(now);
        Ok(())
    }

    fn cache_remove(&mut self, area: &str, path: &str) -> Result<()> {
        let now = self.prepare_cache_mutation()?;
        if self.cache.remove(area, path)? {
            self.cache_checkpoints.mark_dirty(now);
        }
        Ok(())
    }

    fn cache_retain(&mut self, seen: &BTreeSet<(String, String)>) -> Result<()> {
        let now = self.prepare_cache_mutation()?;
        if self.cache.retain(seen)? {
            self.cache_checkpoints.mark_dirty(now);
        }
        Ok(())
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
            let key = relative_cache_key(relative)?;
            self.cache_remove(LIBRARY_AREA, &key)?;
        }
        Ok(())
    }

    fn audit_library_files(&mut self) -> Result<LibraryAudit> {
        let mut complete_games = BTreeSet::new();
        let mut cache_keys = BTreeSet::new();
        for catalog_index in 0..self.catalogs.len() {
            let system = self.catalogs[catalog_index].name.clone();
            let system_path = self.config.library_path.join(&system);
            let mut files = Vec::new();
            for entry in self.filesystem.read_directory(&system_path)? {
                if entry.kind == EntryKind::File {
                    files.push(self.hash_file(
                        LIBRARY_AREA,
                        &self.config.library_path,
                        &entry.path,
                    )?);
                } else {
                    self.quarantine_entry(&entry)?;
                }
            }
            files.sort_by(|left, right| ordering::os(&left.name, &right.name));
            let files_by_name = files_by_utf8_name(&files);
            let mut keep = BTreeSet::<OsString>::new();
            for (game_index, game) in self.catalogs[catalog_index].games.iter().enumerate() {
                if game_is_complete(game, &files_by_name) {
                    complete_games.insert(GameId {
                        catalog: catalog_index,
                        game: game_index,
                    });
                    keep.extend(game.roms.iter().map(|rom| OsString::from(&rom.name)));
                }
            }

            for file in files {
                if keep.contains(&file.name) {
                    cache_keys.insert(file.cache_key.clone());
                } else {
                    self.move_library_file_to_work(&file)?;
                }
            }
        }
        Ok(LibraryAudit {
            complete_games,
            cache_keys,
        })
    }

    fn scan_initial_work(&mut self) -> Result<WorkInventory> {
        let mut files = Vec::new();
        for entry in self.filesystem.read_directory(&self.config.work_path)? {
            if entry.kind == EntryKind::File {
                files.push(self.hash_file(WORK_AREA, &self.config.work_path, &entry.path)?);
            } else {
                self.summary.ignored_work_entries += 1;
                self.report(ProgressEvent::IgnoringWorkEntry {
                    path: self.progress_path(&entry.path),
                    kind: entry_kind_label(entry.kind).to_owned(),
                });
            }
        }
        files.sort_by(|left, right| ordering::os(&left.name, &right.name));
        Ok(WorkInventory::new(files))
    }

    fn process_work(
        &mut self,
        work: &mut WorkInventory,
        complete_games: &mut BTreeSet<GameId>,
    ) -> Result<()> {
        self.report(ProgressEvent::MatchingContent);
        work.index_cues(self.filesystem)?;
        let mut processed_data = BTreeSet::new();
        let mut processed_cues = BTreeSet::new();
        self.process_content_queue(
            work,
            complete_games,
            &mut processed_data,
            &mut processed_cues,
        )?;
        self.report_duplicate_games(work, complete_games)
    }

    fn retain_initial_inventory(
        &mut self,
        library_cache_keys: &BTreeSet<String>,
        work: &WorkInventory,
    ) -> Result<()> {
        let mut seen = BTreeSet::new();
        for cache_key in library_cache_keys {
            seen.insert((LIBRARY_AREA.to_owned(), cache_key.clone()));
        }
        for cache_key in work.cache_keys() {
            seen.insert((WORK_AREA.to_owned(), cache_key.clone()));
        }
        self.cache_retain(&seen)
    }

    fn finish_reports(
        &mut self,
        work: &WorkInventory,
        complete_games: &BTreeSet<GameId>,
    ) -> Result<()> {
        let mut complete_count = 0_u64;
        let mut missing_count = 0_u64;
        for (catalog_index, catalog) in self.catalogs.iter().enumerate() {
            let mut missing = Vec::new();
            for (game_index, game) in catalog.games.iter().enumerate() {
                if complete_games.contains(&GameId {
                    catalog: catalog_index,
                    game: game_index,
                }) {
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
        self.summary.remaining_leftovers = work.len() as u64;
        self.summary.unknown_files = work
            .sorted_non_cue_files()
            .into_iter()
            .filter(|file| {
                self.dat_index
                    .candidate_games_for_content(&ContentId::from_file(file))
                    .is_none()
            })
            .count() as u64;
        Ok(())
    }

    fn process_content_queue(
        &mut self,
        work: &mut WorkInventory,
        complete_games: &mut BTreeSet<GameId>,
        processed_data: &mut BTreeSet<String>,
        processed_cues: &mut BTreeSet<String>,
    ) -> Result<()> {
        let mut queue = Vec::new();
        for file in work.sorted_non_cue_files() {
            if processed_data.contains(&file.cache_key) {
                continue;
            }
            let Some(candidates) = self
                .dat_index
                .candidate_games_for_content(&ContentId::from_file(file))
            else {
                continue;
            };
            let candidates: Vec<_> = self
                .dat_index
                .ordered_games
                .iter()
                .copied()
                .filter(|game_id| candidates.contains(game_id) && !complete_games.contains(game_id))
                .collect();
            if !candidates.is_empty() {
                queue.push(QueueItem {
                    file: file.clone(),
                    candidates,
                });
            }
        }
        queue.sort_by(|left, right| {
            left.candidates
                .len()
                .cmp(&right.candidates.len())
                .then_with(|| ordering::os(&left.file.name, &right.file.name))
                .then_with(|| ordering::path(&left.file.relative_path, &right.file.relative_path))
        });

        for item in queue {
            if processed_data.contains(&item.file.cache_key)
                || !work.files.contains_key(&item.file.cache_key)
            {
                continue;
            }
            let mut evaluations = Vec::new();
            for game_id in item
                .candidates
                .into_iter()
                .filter(|game_id| !complete_games.contains(game_id))
            {
                evaluations.push(self.evaluate_content_candidate(
                    game_id,
                    &item.file,
                    work,
                    complete_games,
                    processed_data,
                    processed_cues,
                )?);
            }
            let Some(winner) = evaluations.into_iter().min_by(|left, right| {
                match (left.complete, right.complete) {
                    (true, true) => self.compare_games(left.game, right.game),
                    (true, false) => std::cmp::Ordering::Less,
                    (false, true) => std::cmp::Ordering::Greater,
                    (false, false) => right
                        .score
                        .cmp(&left.score)
                        .then_with(|| self.compare_games(left.game, right.game)),
                }
            }) else {
                continue;
            };

            if winner.complete {
                let catalog = &self.catalogs[winner.game.catalog];
                self.apply_candidate(
                    PromotionCandidate {
                        game: winner.game,
                        system: catalog.name.clone(),
                        non_cue: winner.non_cue,
                        cue: winner.cue,
                    },
                    work,
                    complete_games,
                )?;
            } else {
                self.emit_incomplete(self.evaluation_detail(&winner));
                processed_data.insert(item.file.cache_key);
                for (source, _) in &winner.non_cue {
                    if let RomSource::Work(file) = source {
                        processed_data.insert(file.cache_key.clone());
                    }
                }
                for (file, _) in &winner.mismatches {
                    processed_data.insert(file.cache_key.clone());
                }
                if let Some((file, _, _)) = &winner.cue {
                    processed_cues.insert(file.cache_key.clone());
                }
                if let Some((file, _)) = &winner.cue_mismatch {
                    processed_cues.insert(file.cache_key.clone());
                }
            }
        }
        Ok(())
    }

    fn report_duplicate_games(
        &mut self,
        work: &mut WorkInventory,
        complete_games: &BTreeSet<GameId>,
    ) -> Result<()> {
        let ordered_games = self.dat_index.ordered_games.clone();
        let mut reported_data = BTreeSet::new();
        let mut reported_cues = BTreeSet::new();
        for game_id in ordered_games {
            if !complete_games.contains(&game_id) {
                continue;
            }
            let game = self.catalogs[game_id.catalog].games[game_id.game].clone();
            let expected_count = game.non_cue_roms().count();
            if expected_count == 0 {
                continue;
            }

            let mut available_by_key = BTreeMap::new();
            for rom in game.non_cue_roms() {
                for file in work.files_for_content(&ContentId::from_rom(rom), true) {
                    if !reported_data.contains(&file.cache_key) {
                        available_by_key.insert(file.cache_key.clone(), file);
                    }
                }
            }
            let mut available: Vec<_> = available_by_key.into_values().collect();
            available.sort_by(|left, right| {
                ordering::os(&left.name, &right.name)
                    .then_with(|| ordering::path(&left.relative_path, &right.relative_path))
            });
            let selected_work = assign_partial_work_sources(&game, &available, None);
            if selected_work.len() != expected_count {
                continue;
            }

            let non_cue: Vec<_> = selected_work
                .into_iter()
                .map(|(file, target)| (RomSource::Work(file), target))
                .collect();
            let (cue, _) = self.select_content_cue(&game, &non_cue, work, &reported_cues)?;
            if game.cue().is_some() && cue.is_none() {
                continue;
            }

            let evaluation = CandidateEvaluation {
                game: game_id,
                score: non_cue.len() + usize::from(cue.is_some()),
                non_cue,
                cue,
                cue_mismatch: None,
                mismatches: Vec::new(),
                complete: true,
            };
            for (source, _) in &evaluation.non_cue {
                let RomSource::Work(file) = source else {
                    unreachable!("duplicate detection uses only work sources");
                };
                reported_data.insert(file.cache_key.clone());
            }
            if let Some((file, _, _)) = &evaluation.cue {
                reported_cues.insert(file.cache_key.clone());
            }
            let detail = self.evaluation_detail(&evaluation);
            self.emit_duplicate(DuplicateDetail {
                system: detail.system,
                game: detail.game,
                matches: detail.matches,
            });
        }
        Ok(())
    }

    fn evaluate_content_candidate(
        &mut self,
        game_id: GameId,
        seed: &RuntimeFile,
        work: &mut WorkInventory,
        complete_games: &BTreeSet<GameId>,
        processed_data: &BTreeSet<String>,
        processed_cues: &BTreeSet<String>,
    ) -> Result<CandidateEvaluation> {
        let game = self.catalogs[game_id.catalog].games[game_id.game].clone();
        let mut available_by_key = BTreeMap::new();
        for rom in game.non_cue_roms() {
            for file in work.files_for_content(&ContentId::from_rom(rom), true) {
                if !processed_data.contains(&file.cache_key) {
                    available_by_key.insert(file.cache_key.clone(), file);
                }
            }
            if let Some(file) = work.get_by_name(OsStr::new(&rom.name))
                && !file.is_cue()
                && !processed_data.contains(&file.cache_key)
            {
                available_by_key.insert(file.cache_key.clone(), file.clone());
            }
        }
        let mut available: Vec<_> = available_by_key.into_values().collect();
        available.sort_by(|left, right| {
            ordering::os(&left.name, &right.name)
                .then_with(|| ordering::path(&left.relative_path, &right.relative_path))
        });
        let selected_work = assign_partial_work_sources(&game, &available, Some(seed));
        let mut non_cue: Vec<_> = selected_work
            .iter()
            .cloned()
            .map(|(file, target)| (RomSource::Work(file), target))
            .collect();
        let mut assigned_targets: BTreeSet<_> = selected_work
            .iter()
            .map(|(_, target)| target.name.clone())
            .collect();
        let mut expected: Vec<_> = game.non_cue_roms().cloned().collect();
        expected.sort_by(|left, right| ordering::text(&left.name, &right.name));
        for target in &expected {
            if assigned_targets.contains(&target.name) {
                continue;
            }
            if let Some(source) = self.library_source_for(target, complete_games) {
                non_cue.push((RomSource::Library(source), target.clone()));
                assigned_targets.insert(target.name.clone());
            }
        }
        non_cue.sort_by(|left, right| ordering::text(&left.1.name, &right.1.name));

        let selected_keys: BTreeSet<_> = selected_work
            .iter()
            .map(|(file, _)| file.cache_key.as_str())
            .collect();
        let mismatches = exact_mismatches(&game, &available, &assigned_targets, &selected_keys);
        let (cue, cue_mismatch) = self.select_content_cue(&game, &non_cue, work, processed_cues)?;
        let score = non_cue.len() + usize::from(cue.is_some());
        let complete = non_cue.len() == expected.len() && (game.cue().is_none() || cue.is_some());
        Ok(CandidateEvaluation {
            game: game_id,
            non_cue,
            cue,
            cue_mismatch,
            mismatches,
            score,
            complete,
        })
    }

    fn library_source_for(
        &self,
        target: &RomSpec,
        complete_games: &BTreeSet<GameId>,
    ) -> Option<PathBuf> {
        self.dat_index
            .roms_by_content
            .get(&ContentId::from_rom(target))?
            .iter()
            .find_map(|location| {
                if !complete_games.contains(&location.game) {
                    return None;
                }
                let catalog = &self.catalogs[location.game.catalog];
                let source_rom = &catalog.games[location.game.game].roms[location.rom];
                (!source_rom.is_cue()).then(|| {
                    self.config
                        .library_path
                        .join(&catalog.name)
                        .join(&source_rom.name)
                })
            })
    }

    fn select_content_cue(
        &mut self,
        game: &GameSpec,
        non_cue: &[(RomSource, RomSpec)],
        work: &mut WorkInventory,
        processed_cues: &BTreeSet<String>,
    ) -> Result<(Option<SelectedCue>, Option<CueMismatch>)> {
        let Some(expected) = game.cue().cloned() else {
            return Ok((None, None));
        };
        let exact_key = work
            .get_by_name(OsStr::new(&expected.name))
            .filter(|file| file.is_cue() && !processed_cues.contains(&file.cache_key))
            .map(|file| file.cache_key.clone());
        if let Some(key) = &exact_key {
            let (file, bytes, _) = work.load_cue(self.filesystem, key)?;
            if file.size == expected.size && file.sha1 == expected.sha1 {
                return Ok((Some((file, expected, bytes)), None));
            }
        }

        let mut cue_keys = work.cue_keys_for_file_count(game.non_cue_roms().count());
        if let Some(exact_key) = &exact_key
            && let Some(position) = cue_keys.iter().position(|key| key == exact_key)
        {
            let exact = cue_keys.remove(position);
            cue_keys.insert(0, exact);
        }
        for key in cue_keys {
            if processed_cues.contains(&key) || !work.files.contains_key(&key) {
                continue;
            }
            let (file, _, document) = work.load_cue(self.filesystem, &key)?;
            let Some(document) = document else {
                continue;
            };
            if let Some(bytes) = rewrite_selected_cue(&document, non_cue, &expected)? {
                return Ok((Some((file, expected, bytes)), None));
            }
        }

        let mismatch = exact_key
            .and_then(|key| work.files.get(&key).cloned())
            .map(|file| (file, expected));
        Ok((None, mismatch))
    }

    fn evaluation_detail(&self, evaluation: &CandidateEvaluation) -> LeftoverDetail {
        let catalog = &self.catalogs[evaluation.game.catalog];
        let game = &catalog.games[evaluation.game.game];
        let mut roms = game.roms.clone();
        roms.sort_by(|left, right| ordering::text(&left.name, &right.name));
        let matches = roms
            .into_iter()
            .map(|rom| {
                if rom.is_cue() {
                    if let Some((file, target, _)) = &evaluation.cue
                        && target.name == rom.name
                    {
                        return LeftoverMatch {
                            expected_rom: rom.name,
                            work_path: Some(work_relative_path(file)),
                            status: LeftoverStatus::Ok,
                        };
                    }
                    if let Some((file, target)) = &evaluation.cue_mismatch
                        && target.name == rom.name
                    {
                        return LeftoverMatch {
                            expected_rom: rom.name,
                            work_path: Some(work_relative_path(file)),
                            status: LeftoverStatus::Mismatch,
                        };
                    }
                } else if let Some((source, _)) = evaluation
                    .non_cue
                    .iter()
                    .find(|(_, target)| target.name == rom.name)
                {
                    let (work_path, status) = match source {
                        RomSource::Work(file) => {
                            (Some(work_relative_path(file)), LeftoverStatus::Ok)
                        }
                        RomSource::Library(_) => (None, LeftoverStatus::Library),
                    };
                    return LeftoverMatch {
                        expected_rom: rom.name,
                        work_path,
                        status,
                    };
                } else if let Some((file, _)) = evaluation
                    .mismatches
                    .iter()
                    .find(|(_, target)| target.name == rom.name)
                {
                    return LeftoverMatch {
                        expected_rom: rom.name,
                        work_path: Some(work_relative_path(file)),
                        status: LeftoverStatus::Mismatch,
                    };
                }
                LeftoverMatch {
                    expected_rom: rom.name,
                    work_path: None,
                    status: LeftoverStatus::Missing,
                }
            })
            .collect();
        LeftoverDetail {
            system: catalog.name.clone(),
            game: game.name.clone(),
            matches,
        }
    }

    fn emit_incomplete(&mut self, detail: LeftoverDetail) {
        self.summary.leftover_details.push(detail.clone());
        self.report(ProgressEvent::Incomplete { detail });
    }

    fn emit_duplicate(&mut self, detail: DuplicateDetail) {
        self.summary.duplicate_details.push(detail.clone());
        self.report(ProgressEvent::Duplicate { detail });
    }

    fn apply_candidate(
        &mut self,
        candidate: PromotionCandidate,
        work: &mut WorkInventory,
        complete_games: &mut BTreeSet<GameId>,
    ) -> Result<()> {
        let game_name = self.catalogs[candidate.game.catalog].games[candidate.game.game]
            .name
            .clone();
        self.report(ProgressEvent::PromotingGame {
            system: candidate.system.clone(),
            game: game_name,
        });
        let destination_directory = self.config.library_path.join(&candidate.system);
        self.filesystem
            .create_directory_all(&destination_directory)?;
        let mut non_cue: Vec<_> = candidate.non_cue.iter().collect();
        non_cue.sort_by(|left, right| ordering::text(&left.1.name, &right.1.name));
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

        for (source, target) in non_cue {
            let destination = destination_directory.join(&target.name);
            match source {
                RomSource::Work(source) => {
                    self.report(ProgressEvent::Moving {
                        kind: ProgressMoveKind::Promotion,
                        source: self.progress_path(&source.absolute_path),
                        destination: self.progress_path(&destination),
                    });
                    self.filesystem
                        .rename(&source.absolute_path, &destination)?;
                    self.cache_remove(WORK_AREA, &source.cache_key)?;
                    self.cache_moved_file(
                        LIBRARY_AREA,
                        &self.config.library_path,
                        &destination,
                        source,
                    )?;
                    work.remove(&source.cache_key)
                        .expect("candidate source exists in work inventory");
                    self.summary.rom_moves += 1;
                }
                RomSource::Library(source) => {
                    self.report(ProgressEvent::CopyingLibraryRom {
                        source: self.progress_path(source),
                        destination: self.progress_path(&destination),
                    });
                    self.filesystem.copy(source, &destination)?;
                    self.cache_known_file(
                        LIBRARY_AREA,
                        &self.config.library_path,
                        &destination,
                        &target.sha1,
                    )?;
                    self.summary.library_copies += 1;
                }
            }
        }

        if let Some((source, target, bytes)) = &candidate.cue {
            let destination = destination_directory.join(&target.name);
            self.report(ProgressEvent::WritingCue {
                source: self.progress_path(&source.absolute_path),
                destination: self.progress_path(&destination),
            });
            self.filesystem.write_atomic(&destination, bytes)?;
            self.cache_known_file(
                LIBRARY_AREA,
                &self.config.library_path,
                &destination,
                &target.sha1,
            )?;
            self.report(ProgressEvent::Removing {
                kind: ProgressRemovalKind::RewrittenCueSource,
                path: self.progress_path(&source.absolute_path),
            });
            self.filesystem.remove_file(&source.absolute_path)?;
            self.cache_remove(WORK_AREA, &source.cache_key)?;
            work.remove(&source.cache_key)
                .expect("candidate CUE exists in work inventory");
        }
        complete_games.insert(candidate.game);
        self.summary.promotions += 1;
        Ok(())
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
        let cache_key = relative_cache_key(relative)?;
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
            self.cache_put(&CacheRecord {
                area: area.to_owned(),
                path: cache_key.clone(),
                size: metadata.len,
                modified_ns: metadata.modified_ns,
                sha1: sha1.clone(),
            })?;
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

    fn cache_known_file(
        &mut self,
        area: &str,
        area_root: &Path,
        path: &Path,
        sha1: &str,
    ) -> Result<RuntimeFile> {
        let metadata = self.filesystem.metadata(path)?.ok_or_else(|| {
            RomeroError::Operational(format!("file disappeared: {}", path.display()))
        })?;
        if metadata.kind != EntryKind::File {
            return Err(RomeroError::Operational(format!(
                "expected a regular file: {}",
                path.display()
            )));
        }
        self.cache_file_record(
            area,
            area_root,
            path,
            metadata.len,
            metadata.modified_ns,
            sha1,
        )
    }

    fn cache_moved_file(
        &mut self,
        area: &str,
        area_root: &Path,
        path: &Path,
        source: &RuntimeFile,
    ) -> Result<RuntimeFile> {
        self.cache_file_record(
            area,
            area_root,
            path,
            source.size,
            source.modified_ns,
            &source.sha1,
        )
    }

    fn cache_file_record(
        &mut self,
        area: &str,
        area_root: &Path,
        path: &Path,
        size: u64,
        modified_ns: i64,
        sha1: &str,
    ) -> Result<RuntimeFile> {
        let relative = path.strip_prefix(area_root).map_err(|_| {
            RomeroError::Operational(format!("{} is outside {area}", path.display()))
        })?;
        let cache_key = relative_cache_key(relative)?;
        self.cache_put(&CacheRecord {
            area: area.to_owned(),
            path: cache_key.clone(),
            size,
            modified_ns,
            sha1: sha1.to_owned(),
        })?;
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
            size,
            modified_ns,
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
        self.cache_remove(LIBRARY_AREA, &file.cache_key)?;
        self.cache_moved_file(WORK_AREA, &self.config.work_path, &destination, file)?;
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

    fn compare_games(&self, left: GameId, right: GameId) -> std::cmp::Ordering {
        let left_catalog = &self.catalogs[left.catalog];
        let right_catalog = &self.catalogs[right.catalog];
        ordering::text(&left_catalog.name, &right_catalog.name).then_with(|| {
            ordering::text(
                &left_catalog.games[left.game].name,
                &right_catalog.games[right.game].name,
            )
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
    Ok(hex_digest(hasher.finalize().as_ref()))
}

fn sha1_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(bytes);
    hex_digest(hasher.finalize().as_ref())
}

fn hex_digest(digest: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let mut encoded = String::with_capacity(digest.len() * 2);
    for &byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
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

    fn runtime_work_file(name: &str, contents: &[u8]) -> RuntimeFile {
        RuntimeFile {
            absolute_path: Path::new("/root/work").join(name),
            relative_path: PathBuf::from(name),
            name: OsString::from(name),
            cache_key: relative_cache_key(Path::new(name)).unwrap(),
            size: contents.len() as u64,
            modified_ns: 1,
            sha1: sha1_bytes(contents),
        }
    }

    fn catalog(games: Vec<GameSpec>) -> DatCatalog {
        named_catalog("System", games)
    }

    fn named_catalog(name: &str, games: Vec<GameSpec>) -> DatCatalog {
        DatCatalog {
            name: name.into(),
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

        fn remove(&mut self, area: &str, path: &str) -> Result<bool> {
            self.inner.remove(area, path)
        }

        fn retain(&mut self, seen: &BTreeSet<(String, String)>) -> Result<bool> {
            self.inner.retain(seen)
        }

        fn checkpoint(&mut self) -> Result<()> {
            self.inner.checkpoint()
        }

        fn commit(&mut self) -> Result<()> {
            self.inner.commit()
        }
    }

    struct CountingCache {
        inner: SqliteCache,
        gets: std::cell::Cell<usize>,
        checkpoints: usize,
        commits: usize,
        fail_checkpoint: bool,
        fail_commit: bool,
    }

    impl CountingCache {
        fn new() -> Self {
            Self {
                inner: SqliteCache::in_memory().unwrap(),
                gets: std::cell::Cell::new(0),
                checkpoints: 0,
                commits: 0,
                fail_checkpoint: false,
                fail_commit: false,
            }
        }

        fn failing_checkpoint() -> Self {
            Self {
                fail_checkpoint: true,
                ..Self::new()
            }
        }

        fn failing_commit() -> Self {
            Self {
                fail_commit: true,
                ..Self::new()
            }
        }
    }

    impl CacheStore for CountingCache {
        fn get(&self, area: &str, path: &str) -> Result<Option<CacheRecord>> {
            self.gets.set(self.gets.get() + 1);
            self.inner.get(area, path)
        }

        fn put(&mut self, record: &CacheRecord) -> Result<()> {
            self.inner.put(record)
        }

        fn remove(&mut self, area: &str, path: &str) -> Result<bool> {
            self.inner.remove(area, path)
        }

        fn retain(&mut self, seen: &BTreeSet<(String, String)>) -> Result<bool> {
            self.inner.retain(seen)
        }

        fn checkpoint(&mut self) -> Result<()> {
            if std::mem::take(&mut self.fail_checkpoint) {
                return Err(RomeroError::Cache(
                    "injected cache checkpoint failure".into(),
                ));
            }
            self.inner.checkpoint()?;
            self.checkpoints += 1;
            Ok(())
        }

        fn commit(&mut self) -> Result<()> {
            if std::mem::take(&mut self.fail_commit) {
                return Err(RomeroError::Cache("injected cache commit failure".into()));
            }
            self.inner.commit()?;
            self.commits += 1;
            Ok(())
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
    fn batches_hash_and_promotion_cache_mutations_without_forced_checkpoints() {
        let (filesystem, config) = fixture();
        filesystem.add_file("/root/work/first-download.bin", b"first".to_vec());
        filesystem.add_file("/root/work/second-download.bin", b"second".to_vec());
        let catalogs = [catalog(vec![
            GameSpec {
                name: "First".into(),
                roms: vec![rom("First.bin", b"first")],
            },
            GameSpec {
                name: "Second".into(),
                roms: vec![rom("Second.bin", b"second")],
            },
        ])];
        let mut cache = CountingCache::new();

        let summary = execute(&filesystem, &mut cache, &config, &catalogs).unwrap();

        assert_eq!(summary.promotions, 2);
        assert_eq!(filesystem.read_directory_calls(), 3);
        assert_eq!(filesystem.metadata_calls(), 5);
        assert_eq!(cache.gets.get(), 2);
        assert_eq!(
            cache.checkpoints, 0,
            "sub-minute cache mutations must remain in the active transaction"
        );
        assert!(
            cache
                .get(
                    LIBRARY_AREA,
                    &relative_cache_key(Path::new("System/First.bin")).unwrap()
                )
                .unwrap()
                .is_some()
        );
        assert!(
            cache
                .get(
                    LIBRARY_AREA,
                    &relative_cache_key(Path::new("System/Second.bin")).unwrap()
                )
                .unwrap()
                .is_some()
        );
        assert!(
            cache
                .get(
                    WORK_AREA,
                    &relative_cache_key(Path::new("first-download.bin")).unwrap()
                )
                .unwrap()
                .is_none()
        );
        assert!(
            cache
                .get(
                    WORK_AREA,
                    &relative_cache_key(Path::new("second-download.bin")).unwrap()
                )
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn cache_checkpoint_scheduler_uses_first_dirty_time_and_resets_after_commit() {
        let start = Instant::now();
        let mut scheduler = CacheCheckpointScheduler::new(CACHE_CHECKPOINT_INTERVAL);

        assert!(!scheduler.checkpoint_due(start + Duration::from_secs(120)));
        scheduler.mark_dirty(start);
        assert!(!scheduler.checkpoint_due(start + Duration::from_secs(59)));
        assert!(scheduler.checkpoint_due(start + Duration::from_secs(60)));

        scheduler.committed();
        assert!(!scheduler.checkpoint_due(start + Duration::from_secs(180)));
        scheduler.mark_dirty(start + Duration::from_secs(180));
        assert!(!scheduler.checkpoint_due(start + Duration::from_secs(239)));
        assert!(scheduler.checkpoint_due(start + Duration::from_secs(240)));
    }

    #[test]
    fn no_op_inventory_cleanup_does_not_start_the_commit_timer() {
        use std::cell::Cell;

        let (filesystem, config) = fixture();
        let mut cache = CountingCache::new();
        let start = Instant::now();
        let clock = Cell::new(start);
        let mut now = || clock.get();
        let mut progress = |event: &ProgressEvent| {
            if matches!(event, ProgressEvent::WritingReports) {
                clock.set(start + CACHE_CHECKPOINT_INTERVAL);
            }
        };

        execute_with_progress_and_clock(
            &filesystem,
            &mut cache,
            &config,
            &[],
            &mut progress,
            &mut now,
        )
        .unwrap();

        assert_eq!(cache.checkpoints, 0);
    }

    #[test]
    fn periodic_checkpoint_batches_hashes_and_emits_progress_after_success() {
        use std::cell::Cell;

        let (filesystem, config) = fixture();
        filesystem.add_file("/root/work/first.bin", b"first".to_vec());
        filesystem.add_file("/root/work/second.bin", b"second".to_vec());
        let mut cache = CountingCache::new();
        let start = Instant::now();
        let clock = Cell::new(start);
        let mut now = || clock.get();
        let mut events = Vec::new();
        let mut progress = |event: &ProgressEvent| {
            events.push(event.clone());
            if matches!(
                event,
                ProgressEvent::HashSaved { path } if path == Path::new("work/first.bin")
            ) {
                clock.set(start + CACHE_CHECKPOINT_INTERVAL);
            }
        };

        execute_with_progress_and_clock(
            &filesystem,
            &mut cache,
            &config,
            &[],
            &mut progress,
            &mut now,
        )
        .unwrap();

        assert_eq!(cache.checkpoints, 1);
        assert_eq!(
            events
                .iter()
                .filter(|event| {
                    matches!(
                        event,
                        ProgressEvent::CacheCommitted {
                            reason: CacheCommitReason::PeriodicCheckpoint
                        }
                    )
                })
                .count(),
            1
        );
        assert_eq!(
            ProgressEvent::CacheCommitted {
                reason: CacheCommitReason::PeriodicCheckpoint,
            }
            .to_string(),
            "Cache committed: periodic checkpoint"
        );
    }

    #[test]
    fn completed_run_commit_emits_progress_after_success() {
        let mut cache = CountingCache::new();
        let mut events = Vec::new();

        commit_completed_run(&mut cache, &mut |event| events.push(event.clone())).unwrap();

        assert_eq!(cache.commits, 1);
        assert_eq!(
            events,
            vec![ProgressEvent::CacheCommitted {
                reason: CacheCommitReason::RunComplete,
            }]
        );
        assert_eq!(events[0].to_string(), "Cache committed: run complete");
    }

    #[test]
    fn presentation_filters_only_mechanical_progress_events_by_default() {
        use crate::presentation::verbose_only;

        let path = PathBuf::from("work/file.bin");
        let destination = PathBuf::from("library/System/file.bin");
        let verbose_events = [
            ProgressEvent::HashSaved { path: path.clone() },
            ProgressEvent::CacheCommitted {
                reason: CacheCommitReason::PeriodicCheckpoint,
            },
            ProgressEvent::CacheCommitted {
                reason: CacheCommitReason::RunComplete,
            },
            ProgressEvent::CacheHit { path: path.clone() },
            ProgressEvent::Moving {
                kind: ProgressMoveKind::Promotion,
                source: path.clone(),
                destination: destination.clone(),
            },
            ProgressEvent::WritingCue {
                source: path.clone(),
                destination: destination.clone(),
            },
            ProgressEvent::Removing {
                kind: ProgressRemovalKind::RewrittenCueSource,
                path: path.clone(),
            },
        ];
        assert!(verbose_events.iter().all(verbose_only));

        let visible_events = [
            ProgressEvent::LoadingConfiguration {
                root: PathBuf::from("/root"),
            },
            ProgressEvent::LoadingDats {
                path: PathBuf::from("dats"),
            },
            ProgressEvent::DatsLoaded { count: 1 },
            ProgressEvent::PreparingDirectories,
            ProgressEvent::OpeningCache {
                path: PathBuf::from(".romero.sqlite3"),
            },
            ProgressEvent::AuditingLibrary,
            ProgressEvent::ProcessingWork,
            ProgressEvent::MatchingContent,
            ProgressEvent::WritingReports,
            ProgressEvent::Hashing {
                path: path.clone(),
                size: 1,
            },
            ProgressEvent::Moving {
                kind: ProgressMoveKind::Quarantine,
                source: path.clone(),
                destination: destination.clone(),
            },
            ProgressEvent::Moving {
                kind: ProgressMoveKind::LibraryToWork,
                source: path.clone(),
                destination: destination.clone(),
            },
            ProgressEvent::PromotingGame {
                system: "System".into(),
                game: "Game".into(),
            },
            ProgressEvent::CopyingLibraryRom {
                source: path.clone(),
                destination: destination.clone(),
            },
            ProgressEvent::Incomplete {
                detail: LeftoverDetail {
                    system: "System".into(),
                    game: "Game".into(),
                    matches: Vec::new(),
                },
            },
            ProgressEvent::Duplicate {
                detail: DuplicateDetail {
                    system: "System".into(),
                    game: "Game".into(),
                    matches: Vec::new(),
                },
            },
            ProgressEvent::WritingReport {
                path,
                missing_games: 1,
            },
            ProgressEvent::IgnoringWorkEntry {
                path: destination,
                kind: "directory".into(),
            },
        ];
        assert!(visible_events.iter().all(|event| !verbose_only(event)));
    }

    #[test]
    fn periodic_checkpoint_survives_interruption_and_the_next_run_reuses_it() {
        use std::cell::Cell;
        use std::panic::{AssertUnwindSafe, catch_unwind};

        let (filesystem, config) = fixture();
        filesystem.add_file("/root/work/first.bin", b"first".to_vec());
        filesystem.add_file("/root/work/second.bin", b"second".to_vec());
        let mut cache = CountingCache::new();
        let start = Instant::now();
        let clock = Cell::new(start);
        let mut now = || clock.get();
        let interrupted = catch_unwind(AssertUnwindSafe(|| {
            let mut progress = |event: &ProgressEvent| match event {
                ProgressEvent::HashSaved { path } if path == Path::new("work/first.bin") => {
                    clock.set(start + CACHE_CHECKPOINT_INTERVAL);
                }
                ProgressEvent::CacheCommitted {
                    reason: CacheCommitReason::PeriodicCheckpoint,
                } => panic!("simulated interruption after checkpoint"),
                _ => {}
            };
            let _ = execute_with_progress_and_clock(
                &filesystem,
                &mut cache,
                &config,
                &[],
                &mut progress,
                &mut now,
            );
        }));

        assert!(interrupted.is_err());
        assert_eq!(cache.checkpoints, 1);
        assert!(
            cache
                .get(
                    WORK_AREA,
                    &relative_cache_key(Path::new("first.bin")).unwrap(),
                )
                .unwrap()
                .is_some()
        );
        assert!(
            cache
                .get(
                    WORK_AREA,
                    &relative_cache_key(Path::new("second.bin")).unwrap(),
                )
                .unwrap()
                .is_none()
        );

        let resumed = execute(&filesystem, &mut cache, &config, &[]).unwrap();
        assert_eq!(resumed.cache_hits, 1);
        assert_eq!(resumed.cache_misses, 1);
    }

    #[test]
    fn failed_periodic_and_final_commits_emit_no_success_progress() {
        use std::cell::Cell;

        let (filesystem, config) = fixture();
        filesystem.add_file("/root/work/first.bin", b"first".to_vec());
        filesystem.add_file("/root/work/second.bin", b"second".to_vec());
        let mut cache = CountingCache::failing_checkpoint();
        let start = Instant::now();
        let clock = Cell::new(start);
        let mut now = || clock.get();
        let mut events = Vec::new();
        let mut progress = |event: &ProgressEvent| {
            events.push(event.clone());
            if matches!(
                event,
                ProgressEvent::HashSaved { path } if path == Path::new("work/first.bin")
            ) {
                clock.set(start + CACHE_CHECKPOINT_INTERVAL);
            }
        };

        assert!(
            execute_with_progress_and_clock(
                &filesystem,
                &mut cache,
                &config,
                &[],
                &mut progress,
                &mut now,
            )
            .is_err()
        );
        assert!(!events.iter().any(|event| {
            matches!(
                event,
                ProgressEvent::CacheCommitted {
                    reason: CacheCommitReason::PeriodicCheckpoint
                }
            )
        }));

        let mut cache = CountingCache::failing_commit();
        let mut events = Vec::new();
        assert!(commit_completed_run(&mut cache, &mut |event| events.push(event.clone())).is_err());
        assert!(events.is_empty());
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
        let content_matching = events
            .iter()
            .position(|event| *event == ProgressEvent::MatchingContent)
            .expect("content matching event");
        assert_eq!(
            ProgressEvent::MatchingContent.to_string(),
            "Content matching"
        );
        let promotion = events
            .iter()
            .position(|event| matches!(event, ProgressEvent::PromotingGame { .. }))
            .expect("promotion event");
        assert!(content_matching < promotion);
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
        assert!(!events.contains(&ProgressEvent::CacheHit {
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
        let source_cue = b"FILE \"source.bin\" BINARY\n";
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
    fn indexes_each_work_cue_once_before_content_matching() {
        let (filesystem, config) = fixture();
        let source_cue = b"FILE \"download.bin\" BINARY\n";
        let expected_cue = b"FILE \"Game.bin\" BINARY\r\n";
        filesystem.add_file(
            "/root/work/a-unmatched.cue",
            b"FILE \"absent.bin\" BINARY\n".to_vec(),
        );
        filesystem.add_file("/root/work/download.bin", b"disc".to_vec());
        filesystem.add_file("/root/work/z-matching.cue", source_cue.to_vec());
        let catalogs = [catalog(vec![GameSpec {
            name: "Game".into(),
            roms: vec![rom("Game.cue", expected_cue), rom("Game.bin", b"disc")],
        }])];
        let mut cache = SqliteCache::in_memory().unwrap();

        let summary = execute(&filesystem, &mut cache, &config, &catalogs).unwrap();

        assert_eq!(summary.promotions, 1);
        assert_eq!(
            filesystem.read_calls(),
            2,
            "each CUE must be read exactly once while building the index"
        );
        assert!(filesystem.contains("/root/work/a-unmatched.cue"));
    }

    #[test]
    fn cue_index_groups_valid_sheets_by_file_count_in_filename_order() {
        let (filesystem, _) = fixture();
        let inputs: [(&str, &[u8]); 4] = [
            ("Zulu.cue", b"FILE \"z.bin\" BINARY\n"),
            ("alpha.cue", b"FILE \"a.bin\" BINARY\n"),
            (
                "two.cue",
                b"FILE \"one.bin\" BINARY\nFILE \"two.bin\" BINARY\n",
            ),
            ("invalid.cue", b"TRACK 01 AUDIO\n"),
        ];
        for (name, contents) in inputs {
            filesystem.add_file(Path::new("/root/work").join(name), contents.to_vec());
        }
        let mut work = WorkInventory::new(
            inputs
                .into_iter()
                .map(|(name, contents)| runtime_work_file(name, contents))
                .collect(),
        );

        work.index_cues(&filesystem).unwrap();

        let names_for = |count| {
            work.cue_keys_for_file_count(count)
                .into_iter()
                .map(|key| work.files[&key].name.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(names_for(1), ["alpha.cue", "Zulu.cue"]);
        assert_eq!(names_for(2), ["two.cue"]);
        assert!(names_for(0).is_empty());
        assert_eq!(work.cues.len(), 4, "invalid CUE bytes are cached too");
        assert_eq!(filesystem.read_calls(), 4);
    }

    #[test]
    fn exact_hash_matching_cue_bypasses_file_count_filter() {
        let (filesystem, config) = fixture();
        let exact_cue = b"FILE \"Game One.bin\" BINARY\n";
        filesystem.add_file("/root/work/Game.cue", exact_cue.to_vec());
        filesystem.add_file("/root/work/one.bin", b"one".to_vec());
        filesystem.add_file("/root/work/two.bin", b"two".to_vec());
        let catalogs = [catalog(vec![GameSpec {
            name: "Game".into(),
            roms: vec![
                rom("Game.cue", exact_cue),
                rom("Game One.bin", b"one"),
                rom("Game Two.bin", b"two"),
            ],
        }])];
        let mut cache = SqliteCache::in_memory().unwrap();

        let summary = execute(&filesystem, &mut cache, &config, &catalogs).unwrap();

        assert_eq!(summary.promotions, 1);
        assert_eq!(
            filesystem.contents("/root/library/System/Game.cue"),
            Some(exact_cue.to_vec())
        );
    }

    #[test]
    fn exact_name_wrong_hash_cue_is_rewritten_before_other_same_count_cues() {
        let (filesystem, config) = fixture();
        let source_cue = b"FILE \"download.bin\" BINARY\n";
        let expected_cue = b"FILE \"Game.bin\" BINARY\r\n";
        filesystem.add_file("/root/work/download.bin", b"disc".to_vec());
        filesystem.add_file("/root/work/a-template.cue", source_cue.to_vec());
        filesystem.add_file("/root/work/Game.cue", source_cue.to_vec());
        let catalogs = [catalog(vec![GameSpec {
            name: "Game".into(),
            roms: vec![rom("Game.cue", expected_cue), rom("Game.bin", b"disc")],
        }])];
        let mut cache = SqliteCache::in_memory().unwrap();

        let summary = execute(&filesystem, &mut cache, &config, &catalogs).unwrap();

        assert_eq!(summary.promotions, 1);
        assert!(!filesystem.contains("/root/work/Game.cue"));
        assert!(filesystem.contains("/root/work/a-template.cue"));
        assert_eq!(
            filesystem.contents("/root/library/System/Game.cue"),
            Some(expected_cue.to_vec())
        );
    }

    #[test]
    fn alternate_cue_search_uses_first_same_count_sheet_only() {
        let (filesystem, config) = fixture();
        let wrong_count = b"FILE \"download-one.bin\" BINARY\n";
        let source_cue = b"FILE \"download-one.bin\" BINARY\nFILE \"download-two.bin\" BINARY\n";
        let expected_cue = b"FILE \"Game One.bin\" BINARY\r\nFILE \"Game Two.bin\" BINARY\r\n";
        filesystem.add_file("/root/work/download-one.bin", b"one".to_vec());
        filesystem.add_file("/root/work/download-two.bin", b"two".to_vec());
        filesystem.add_file("/root/work/a-wrong-count.cue", wrong_count.to_vec());
        filesystem.add_file("/root/work/b-first-match.cue", source_cue.to_vec());
        filesystem.add_file("/root/work/z-later-match.cue", source_cue.to_vec());
        let catalogs = [catalog(vec![GameSpec {
            name: "Game".into(),
            roms: vec![
                rom("Game.cue", expected_cue),
                rom("Game One.bin", b"one"),
                rom("Game Two.bin", b"two"),
            ],
        }])];
        let mut cache = SqliteCache::in_memory().unwrap();

        let summary = execute(&filesystem, &mut cache, &config, &catalogs).unwrap();

        assert_eq!(summary.promotions, 1);
        assert!(filesystem.contains("/root/work/a-wrong-count.cue"));
        assert!(!filesystem.contains("/root/work/b-first-match.cue"));
        assert!(filesystem.contains("/root/work/z-later-match.cue"));
        assert_eq!(
            filesystem.contents("/root/library/System/Game.cue"),
            Some(expected_cue.to_vec())
        );
    }

    #[test]
    fn content_queue_emits_an_incomplete_result_before_a_later_promotion() {
        let (filesystem, config) = fixture();
        filesystem.add_file("/root/work/a-partial.bin", b"partial".to_vec());
        filesystem.add_file(
            "/root/work/a-partial.cue",
            b"FILE \"a-partial.bin\" BINARY\n".to_vec(),
        );
        filesystem.add_file("/root/work/z-complete.bin", b"complete".to_vec());
        filesystem.add_file(
            "/root/work/z-complete.cue",
            b"FILE \"z-complete.bin\" BINARY\n".to_vec(),
        );
        let partial_cue = b"FILE \"Partial One.bin\" BINARY\nFILE \"Partial Two.bin\" BINARY\n";
        let complete_cue = b"FILE \"Complete.bin\" BINARY\r\n";
        let catalogs = [catalog(vec![
            GameSpec {
                name: "Partial".into(),
                roms: vec![
                    rom("Partial.cue", partial_cue),
                    rom("Partial One.bin", b"partial"),
                    rom("Partial Two.bin", b"missing"),
                ],
            },
            GameSpec {
                name: "Complete".into(),
                roms: vec![
                    rom("Complete.cue", complete_cue),
                    rom("Complete.bin", b"complete"),
                ],
            },
        ])];
        let mut cache = SqliteCache::in_memory().unwrap();
        let mut events = Vec::new();

        let summary =
            execute_with_progress(&filesystem, &mut cache, &config, &catalogs, &mut |event| {
                events.push(event.clone())
            })
            .unwrap();

        let incomplete_position = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    ProgressEvent::Incomplete { detail } if detail.game == "Partial"
                )
            })
            .expect("partial content candidate emits immediately");
        let promotion_position = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    ProgressEvent::PromotingGame { game, .. } if game == "Complete"
                )
            })
            .expect("later complete content candidate promotes");
        assert!(incomplete_position < promotion_position);
        assert_eq!(summary.leftover_details.len(), 1);
        assert_eq!(summary.leftover_details[0].game, "Partial");
        assert!(filesystem.contains("/root/work/a-partial.bin"));
        assert!(filesystem.contains("/root/work/a-partial.cue"));
        assert_eq!(summary.promotions, 1);
    }

    #[test]
    fn cue_only_work_does_not_seed_content_matching() {
        let (filesystem, config) = fixture();
        let cue = b"FILE \"missing-download.bin\" BINARY\n";
        filesystem.add_file("/root/work/Game.cue", cue.to_vec());
        let catalogs = [catalog(vec![GameSpec {
            name: "Game".into(),
            roms: vec![rom("Game.cue", cue), rom("Game.bin", b"missing")],
        }])];
        let mut cache = SqliteCache::in_memory().unwrap();

        let summary = execute(&filesystem, &mut cache, &config, &catalogs).unwrap();

        assert!(summary.leftover_details.is_empty());
        assert_eq!(summary.promotions, 0);
        assert_eq!(summary.missing_games, 1);
        assert!(filesystem.contains("/root/work/Game.cue"));
    }

    #[test]
    fn cue_references_do_not_reserve_data_from_content_matching() {
        let (filesystem, config) = fixture();
        filesystem.add_file("/root/work/shared.bin", b"shared".to_vec());
        for cue in ["a.cue", "b.cue"] {
            filesystem.add_file(
                Path::new("/root/work").join(cue),
                b"FILE \"shared.bin\" BINARY\n".to_vec(),
            );
        }
        let expected_cue = b"FILE \"Game Shared.bin\" BINARY\nFILE \"Game Missing.bin\" BINARY\n";
        let catalogs = [catalog(vec![GameSpec {
            name: "Game".into(),
            roms: vec![
                rom("Game.cue", expected_cue),
                rom("Game Shared.bin", b"shared"),
                rom("Game Missing.bin", b"missing"),
            ],
        }])];
        let mut cache = SqliteCache::in_memory().unwrap();

        let summary = execute(&filesystem, &mut cache, &config, &catalogs).unwrap();

        assert_eq!(summary.leftover_details.len(), 1);
        assert_eq!(summary.leftover_details[0].game, "Game");
        assert_eq!(
            summary.leftover_details[0]
                .matches
                .iter()
                .find(|rom| rom.expected_rom == "Game Shared.bin")
                .map(|rom| &rom.status),
            Some(&LeftoverStatus::Ok)
        );
        assert!(filesystem.contains("/root/work/shared.bin"));
        assert!(filesystem.contains("/root/work/a.cue"));
        assert!(filesystem.contains("/root/work/b.cue"));
    }

    #[test]
    fn first_complete_content_candidate_wins_the_game_order_tie() {
        let (filesystem, config) = fixture();
        let source_cue = b"FILE \"download.bin\" BINARY\n";
        let first_cue = b"FILE \"First.bin\" BINARY\r\n";
        let second_cue = b"FILE \"Second.bin\" BINARY\r\n";
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
    fn content_matching_copies_shared_library_content_then_promotes() {
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

        assert_eq!(summary.library_copies, 1);
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
    fn content_matching_prefers_work_content_over_an_equal_library_source() {
        let (filesystem, config) = fixture();
        filesystem.add_directory("/root/library/System");
        filesystem.add_file("/root/library/System/Source.bin", b"shared".to_vec());
        filesystem.add_file("/root/work/a-unique.bin", b"unique".to_vec());
        filesystem.add_file("/root/work/z-shared.bin", b"shared".to_vec());
        let catalogs = [catalog(vec![
            GameSpec {
                name: "Source".into(),
                roms: vec![rom("Source.bin", b"shared")],
            },
            GameSpec {
                name: "Target".into(),
                roms: vec![
                    rom("Target Unique.bin", b"unique"),
                    rom("Target Shared.bin", b"shared"),
                ],
            },
        ])];
        let mut cache = SqliteCache::in_memory().unwrap();

        let summary = execute(&filesystem, &mut cache, &config, &catalogs).unwrap();

        assert_eq!(summary.promotions, 1);
        assert_eq!(summary.library_copies, 0);
        assert!(!filesystem.contains("/root/work/z-shared.bin"));
        assert_eq!(
            filesystem.contents("/root/library/System/Target Shared.bin"),
            Some(b"shared".to_vec())
        );
    }

    #[test]
    fn alternate_cue_can_reference_an_exact_dat_name_supplied_from_library() {
        let (filesystem, config) = fixture();
        filesystem.add_directory("/root/library/System");
        filesystem.add_file("/root/library/System/Source Data.bin", b"data".to_vec());
        filesystem.add_file("/root/work/seed.bin", b"seed".to_vec());
        let source_cue = b"FILE \"seed.bin\" BINARY\nFILE \"Target Data.bin\" BINARY\n";
        let expected_cue =
            b"FILE \"Target Seed.bin\" BINARY\r\nFILE \"Target Data.bin\" BINARY\r\n";
        filesystem.add_file("/root/work/template.cue", source_cue.to_vec());
        let catalogs = [catalog(vec![
            GameSpec {
                name: "Source".into(),
                roms: vec![rom("Source Data.bin", b"data")],
            },
            GameSpec {
                name: "Target".into(),
                roms: vec![
                    rom("Target.cue", expected_cue),
                    rom("Target Seed.bin", b"seed"),
                    rom("Target Data.bin", b"data"),
                ],
            },
        ])];
        let mut cache = SqliteCache::in_memory().unwrap();

        let summary = execute(&filesystem, &mut cache, &config, &catalogs).unwrap();

        assert_eq!(summary.promotions, 1);
        assert_eq!(summary.library_copies, 1);
        assert_eq!(
            filesystem.contents("/root/library/System/Target.cue"),
            Some(expected_cue.to_vec())
        );
        assert!(!filesystem.contains("/root/work/template.cue"));
    }

    #[test]
    fn library_copy_derives_a_source_from_another_system_and_dat_filename() {
        let (filesystem, config) = fixture();
        filesystem.add_directory("/root/library/Alpha System");
        filesystem.add_file(
            "/root/library/Alpha System/Source Name.bin",
            b"shared".to_vec(),
        );
        filesystem.add_file("/root/work/download.bin", b"unique".to_vec());
        let catalogs = [
            named_catalog(
                "Alpha System",
                vec![GameSpec {
                    name: "Source".into(),
                    roms: vec![rom("Source Name.bin", b"shared")],
                }],
            ),
            named_catalog(
                "Beta System",
                vec![GameSpec {
                    name: "Target".into(),
                    roms: vec![
                        rom("Target Unique.bin", b"unique"),
                        rom("Different Target Name.bin", b"shared"),
                    ],
                }],
            ),
        ];
        let mut cache = SqliteCache::in_memory().unwrap();

        let summary = execute(&filesystem, &mut cache, &config, &catalogs).unwrap();

        assert_eq!(summary.library_copies, 1);
        assert_eq!(summary.promotions, 1);
        assert_eq!(
            filesystem.contents("/root/library/Beta System/Different Target Name.bin"),
            Some(b"shared".to_vec())
        );
    }

    #[test]
    fn content_matching_rejects_library_sources_from_incomplete_games() {
        let (filesystem, config) = fixture();
        filesystem.add_directory("/root/library/Alpha System");
        filesystem.add_file(
            "/root/library/Alpha System/Source Name.bin",
            b"wrong".to_vec(),
        );
        filesystem.add_file("/root/work/download.bin", b"unique".to_vec());
        let catalogs = [
            named_catalog(
                "Alpha System",
                vec![GameSpec {
                    name: "Incomplete Source".into(),
                    roms: vec![
                        rom("Source Name.bin", b"shared"),
                        rom("Missing Companion.bin", b"companion"),
                    ],
                }],
            ),
            named_catalog(
                "Beta System",
                vec![GameSpec {
                    name: "Target".into(),
                    roms: vec![
                        rom("Target Unique.bin", b"unique"),
                        rom("Target Shared.bin", b"shared"),
                    ],
                }],
            ),
        ];
        let mut cache = SqliteCache::in_memory().unwrap();

        let summary = execute(&filesystem, &mut cache, &config, &catalogs).unwrap();

        assert_eq!(summary.library_copies, 0);
        assert_eq!(summary.promotions, 0);
        assert!(filesystem.contains("/root/work/download.bin"));
        assert!(!filesystem.contains("/root/library/Beta System/Target Shared.bin"));
    }

    #[test]
    fn library_copy_chooses_the_first_complete_source_path_case_insensitively() {
        let (filesystem, config) = fixture();
        filesystem.add_directory("/root/library/alpha system");
        filesystem.add_file(
            "/root/library/alpha system/Alpha Shared.bin",
            b"shared".to_vec(),
        );
        filesystem.add_file(
            "/root/library/alpha system/Alpha Extra.bin",
            b"alpha".to_vec(),
        );
        filesystem.add_directory("/root/library/Zulu System");
        filesystem.add_file(
            "/root/library/Zulu System/Zulu Shared.bin",
            b"shared".to_vec(),
        );
        filesystem.add_file("/root/library/Zulu System/Zulu Extra.bin", b"zulu".to_vec());
        filesystem.add_file("/root/work/download.bin", b"unique".to_vec());
        let catalogs = [
            named_catalog(
                "Zulu System",
                vec![GameSpec {
                    name: "Zulu Source".into(),
                    roms: vec![
                        rom("Zulu Shared.bin", b"shared"),
                        rom("Zulu Extra.bin", b"zulu"),
                    ],
                }],
            ),
            named_catalog(
                "alpha system",
                vec![GameSpec {
                    name: "Alpha Source".into(),
                    roms: vec![
                        rom("Alpha Shared.bin", b"shared"),
                        rom("Alpha Extra.bin", b"alpha"),
                    ],
                }],
            ),
            named_catalog(
                "Target System",
                vec![GameSpec {
                    name: "Target".into(),
                    roms: vec![
                        rom("Target Unique.bin", b"unique"),
                        rom("Target Shared.bin", b"shared"),
                    ],
                }],
            ),
        ];
        let mut cache = SqliteCache::in_memory().unwrap();
        let mut library_sources = Vec::new();

        execute_with_progress(&filesystem, &mut cache, &config, &catalogs, &mut |event| {
            if let ProgressEvent::CopyingLibraryRom { source, .. } = event {
                library_sources.push(source.clone());
            }
        })
        .unwrap();

        assert_eq!(
            library_sources,
            vec![PathBuf::from("library/alpha system/Alpha Shared.bin")]
        );
    }

    #[test]
    fn library_copy_repeats_content_the_required_number_of_times() {
        let (filesystem, config) = fixture();
        filesystem.add_directory("/root/library/System");
        filesystem.add_file("/root/library/System/Source.bin", b"shared".to_vec());
        filesystem.add_file("/root/work/download.bin", b"unique".to_vec());
        let catalogs = [catalog(vec![
            GameSpec {
                name: "Source".into(),
                roms: vec![rom("Source.bin", b"shared")],
            },
            GameSpec {
                name: "Target".into(),
                roms: vec![
                    rom("Target Unique.bin", b"unique"),
                    rom("Target Copy A.bin", b"shared"),
                    rom("Target Copy B.bin", b"shared"),
                ],
            },
        ])];
        let mut cache = SqliteCache::in_memory().unwrap();

        let summary = execute(&filesystem, &mut cache, &config, &catalogs).unwrap();

        assert_eq!(summary.library_copies, 2);
        assert_eq!(summary.promotions, 1);
        for filename in ["Target Copy A.bin", "Target Copy B.bin"] {
            assert_eq!(
                filesystem.contents(Path::new("/root/library/System").join(filename)),
                Some(b"shared".to_vec())
            );
        }
    }

    #[test]
    fn newly_promoted_game_is_an_immediate_library_source_without_a_rescan() {
        let (filesystem, config) = fixture();
        filesystem.add_file("/root/work/a-source-seed.bin", b"source seed".to_vec());
        filesystem.add_file("/root/work/b-source-shared.bin", b"shared".to_vec());
        filesystem.add_file("/root/work/z-target-download.bin", b"unique".to_vec());
        let catalogs = [catalog(vec![
            GameSpec {
                name: "A Source".into(),
                roms: vec![
                    rom("Source Seed.bin", b"source seed"),
                    rom("Source Shared.bin", b"shared"),
                ],
            },
            GameSpec {
                name: "B Target".into(),
                roms: vec![
                    rom("Target Unique.bin", b"unique"),
                    rom("Target Shared.bin", b"shared"),
                ],
            },
        ])];
        let mut cache = SqliteCache::in_memory().unwrap();

        let summary = execute(&filesystem, &mut cache, &config, &catalogs).unwrap();

        assert_eq!(summary.promotions, 2);
        assert_eq!(summary.library_copies, 1);
        assert_eq!(filesystem.read_directory_calls(), 3);
        assert_eq!(
            filesystem.contents("/root/library/System/Target Shared.bin"),
            Some(b"shared".to_vec())
        );
    }

    #[test]
    fn preserves_leftovers_even_when_their_hash_exists_in_the_library() {
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

        assert!(filesystem.contains("/root/work/lone-copy.bin"));
        assert!(filesystem.contains("/root/work/leftover.cue"));
        assert_eq!(summary.remaining_leftovers, 2);
        assert!(summary.duplicate_details.is_empty());
    }

    #[test]
    fn leaves_work_copies_of_a_verified_existing_game_untouched() {
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

        assert!(filesystem.contains("/root/work/copy.bin"));
        assert!(filesystem.contains("/root/work/unrelated.bin"));
        assert_eq!(summary.promotions, 0);
        assert_eq!(summary.duplicate_details.len(), 1);
    }

    #[test]
    fn reports_a_complete_work_copy_of_a_verified_game_as_duplicate() {
        let (filesystem, config) = fixture();
        let expected_cue = b"FILE \"Game.bin\" BINARY\r\n";
        let work_cue = b"FILE \"copy.bin\" BINARY\n";
        filesystem.add_directory("/root/library/System");
        filesystem.add_file("/root/library/System/Game.bin", b"payload".to_vec());
        filesystem.add_file("/root/library/System/Game.cue", expected_cue.to_vec());
        filesystem.add_file("/root/work/copy.bin", b"payload".to_vec());
        filesystem.add_file("/root/work/copy.cue", work_cue.to_vec());
        let catalogs = [catalog(vec![GameSpec {
            name: "Game".into(),
            roms: vec![rom("Game.cue", expected_cue), rom("Game.bin", b"payload")],
        }])];
        let mut cache = SqliteCache::in_memory().unwrap();
        let mut events = Vec::new();

        let summary =
            execute_with_progress(&filesystem, &mut cache, &config, &catalogs, &mut |event| {
                events.push(event.clone())
            })
            .unwrap();

        let duplicate = events
            .iter()
            .find(|event| event.to_string().starts_with("Duplicate:"))
            .expect("complete work copy is reported as a duplicate");
        let duplicate_position = events
            .iter()
            .position(|event| event == duplicate)
            .expect("duplicate event position");
        let reports_position = events
            .iter()
            .position(|event| *event == ProgressEvent::WritingReports)
            .expect("reports event position");
        assert!(duplicate_position < reports_position);
        assert_eq!(
            duplicate.to_string(),
            concat!(
                "Duplicate: System / Game\n",
                "  Game.bin -> copy.bin [OK]\n",
                "  Game.cue -> copy.cue [OK]\n",
            )
        );
        assert!(filesystem.contains("/root/work/copy.bin"));
        assert!(filesystem.contains("/root/work/copy.cue"));
        assert_eq!(summary.promotions, 0);
        assert_eq!(summary.remaining_leftovers, 2);
        assert_eq!(
            summary.duplicate_details,
            vec![DuplicateDetail {
                system: "System".into(),
                game: "Game".into(),
                matches: vec![
                    LeftoverMatch {
                        expected_rom: "Game.bin".into(),
                        work_path: Some("copy.bin".into()),
                        status: LeftoverStatus::Ok,
                    },
                    LeftoverMatch {
                        expected_rom: "Game.cue".into(),
                        work_path: Some("copy.cue".into()),
                        status: LeftoverStatus::Ok,
                    },
                ],
            }]
        );
        assert!(
            format!("{}", summary.duplicate_details[0].colored())
                .contains("\x1b[96mDuplicate:\x1b[0m System / Game")
        );
        assert!(!format!("{summary}").contains("Duplicate:"));
    }

    #[test]
    fn duplicate_reports_do_not_reuse_the_same_physical_work_file() {
        let (filesystem, config) = fixture();
        filesystem.add_directory("/root/library/System");
        filesystem.add_file("/root/library/System/Alpha Shared.bin", b"shared".to_vec());
        filesystem.add_file("/root/library/System/Alpha.bin", b"alpha".to_vec());
        filesystem.add_file("/root/library/System/Beta Shared.bin", b"shared".to_vec());
        filesystem.add_file("/root/library/System/Beta.bin", b"beta".to_vec());
        filesystem.add_file("/root/work/shared.bin", b"shared".to_vec());
        filesystem.add_file("/root/work/alpha.bin", b"alpha".to_vec());
        filesystem.add_file("/root/work/beta.bin", b"beta".to_vec());
        let catalogs = [catalog(vec![
            GameSpec {
                name: "Alpha".into(),
                roms: vec![
                    rom("Alpha Shared.bin", b"shared"),
                    rom("Alpha.bin", b"alpha"),
                ],
            },
            GameSpec {
                name: "Beta".into(),
                roms: vec![rom("Beta Shared.bin", b"shared"), rom("Beta.bin", b"beta")],
            },
        ])];
        let mut cache = SqliteCache::in_memory().unwrap();

        let summary = execute(&filesystem, &mut cache, &config, &catalogs).unwrap();

        assert_eq!(summary.duplicate_details.len(), 1);
        assert_eq!(summary.duplicate_details[0].game, "Alpha");
        for filename in ["shared.bin", "alpha.bin", "beta.bin"] {
            assert!(filesystem.contains(Path::new("/root/work").join(filename)));
        }
    }

    #[test]
    fn fixed_queue_claims_supporting_files_for_the_first_winner() {
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
            ]
        );
        assert!(!format!("{summary}").contains("Incomplete:"));
    }

    #[test]
    fn priority_queue_processes_the_fewest_candidate_seed_first() {
        let (filesystem, config) = fixture();
        filesystem.add_file("/root/work/a-ambiguous.bin", b"shared".to_vec());
        filesystem.add_file("/root/work/z-unique.bin", b"unique".to_vec());
        let catalogs = [catalog(vec![
            GameSpec {
                name: "Alpha".into(),
                roms: vec![
                    rom("Alpha Shared.bin", b"shared"),
                    rom("Alpha Unique.bin", b"unique"),
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
        ])];
        let mut cache = SqliteCache::in_memory().unwrap();

        let summary = execute(&filesystem, &mut cache, &config, &catalogs).unwrap();

        assert_eq!(summary.leftover_details.len(), 1);
        let alpha = &summary.leftover_details[0];
        assert_eq!(alpha.game, "Alpha");
        assert_eq!(
            alpha
                .matches
                .iter()
                .filter(|rom| rom.status == LeftoverStatus::Ok)
                .count(),
            2
        );
        assert_eq!(
            alpha
                .matches
                .iter()
                .find(|rom| rom.expected_rom == "Alpha Shared.bin")
                .expect("the supporting ambiguous file is claimed")
                .work_path
                .as_deref(),
            Some("a-ambiguous.bin")
        );
    }

    #[test]
    fn priority_queue_uses_work_filename_to_break_candidate_count_ties() {
        let (filesystem, config) = fixture();
        filesystem.add_file("/root/work/a-seed.bin", b"zulu seed".to_vec());
        filesystem.add_file("/root/work/B-seed.bin", b"alpha seed".to_vec());
        let catalogs = [catalog(vec![
            GameSpec {
                name: "Alpha".into(),
                roms: vec![
                    rom("Alpha Seed.bin", b"alpha seed"),
                    rom("Alpha Missing.bin", b"alpha missing"),
                ],
            },
            GameSpec {
                name: "Zulu".into(),
                roms: vec![
                    rom("Zulu Seed.bin", b"zulu seed"),
                    rom("Zulu Missing.bin", b"zulu missing"),
                ],
            },
        ])];
        let mut cache = SqliteCache::in_memory().unwrap();

        let summary = execute(&filesystem, &mut cache, &config, &catalogs).unwrap();

        assert_eq!(
            summary
                .leftover_details
                .iter()
                .map(|detail| detail.game.as_str())
                .collect::<Vec<_>>(),
            vec!["Zulu", "Alpha"]
        );
    }

    #[test]
    fn filename_only_matches_do_not_create_content_queue_candidates() {
        let (filesystem, config) = fixture();
        filesystem.add_file("/root/work/Game.bin", b"wrong".to_vec());
        let catalogs = [catalog(vec![GameSpec {
            name: "Game".into(),
            roms: vec![rom("Game.bin", b"expected")],
        }])];
        let mut cache = SqliteCache::in_memory().unwrap();

        let summary = execute(&filesystem, &mut cache, &config, &catalogs).unwrap();

        assert!(summary.leftover_details.is_empty());
        assert_eq!(summary.unknown_files, 1);
        assert!(filesystem.contains("/root/work/Game.bin"));
    }

    #[test]
    fn displayed_mismatch_is_claimed_before_its_own_queue_item() {
        let (filesystem, config) = fixture();
        filesystem.add_file("/root/work/a-alpha-seed.bin", b"alpha seed".to_vec());
        filesystem.add_file("/root/work/Alpha Missing.bin", b"beta seed".to_vec());
        let catalogs = [catalog(vec![
            GameSpec {
                name: "Alpha".into(),
                roms: vec![
                    rom("Alpha Seed.bin", b"alpha seed"),
                    rom("Alpha Missing.bin", b"alpha expected"),
                ],
            },
            GameSpec {
                name: "Beta".into(),
                roms: vec![
                    rom("Beta Seed.bin", b"beta seed"),
                    rom("Beta Missing.bin", b"beta missing"),
                ],
            },
        ])];
        let mut cache = SqliteCache::in_memory().unwrap();

        let summary = execute(&filesystem, &mut cache, &config, &catalogs).unwrap();

        assert_eq!(summary.leftover_details.len(), 1);
        assert_eq!(summary.leftover_details[0].game, "Alpha");
        assert_eq!(
            summary.leftover_details[0]
                .matches
                .iter()
                .find(|rom| rom.expected_rom == "Alpha Missing.bin")
                .expect("exact-name mismatch is displayed")
                .status,
            LeftoverStatus::Mismatch
        );
    }

    #[test]
    fn complete_candidate_beats_an_alphabetically_earlier_incomplete_candidate() {
        let (filesystem, config) = fixture();
        filesystem.add_directory("/root/library/System");
        filesystem.add_file("/root/library/System/Source Extra.bin", b"extra".to_vec());
        filesystem.add_file("/root/work/seed.bin", b"shared".to_vec());
        let catalogs = [catalog(vec![
            GameSpec {
                name: "Source".into(),
                roms: vec![rom("Source Extra.bin", b"extra")],
            },
            GameSpec {
                name: "Alpha Incomplete".into(),
                roms: vec![
                    rom("Alpha Shared.bin", b"shared"),
                    rom("Alpha Missing.bin", b"missing"),
                ],
            },
            GameSpec {
                name: "Zulu Complete".into(),
                roms: vec![
                    rom("Zulu Shared.bin", b"shared"),
                    rom("Zulu Extra.bin", b"extra"),
                ],
            },
        ])];
        let mut cache = SqliteCache::in_memory().unwrap();

        let summary = execute(&filesystem, &mut cache, &config, &catalogs).unwrap();

        assert_eq!(summary.promotions, 1);
        assert_eq!(summary.library_copies, 1);
        assert_eq!(summary.leftover_details, Vec::new());
        assert!(filesystem.contains("/root/library/System/Zulu Shared.bin"));
        assert!(!filesystem.contains("/root/library/System/Alpha Shared.bin"));
    }

    #[test]
    fn incomplete_winner_score_includes_verified_library_content() {
        let (filesystem, config) = fixture();
        filesystem.add_directory("/root/library/System");
        filesystem.add_file("/root/library/System/Source Extra.bin", b"extra".to_vec());
        filesystem.add_file("/root/work/seed.bin", b"shared".to_vec());
        let catalogs = [catalog(vec![
            GameSpec {
                name: "Source".into(),
                roms: vec![rom("Source Extra.bin", b"extra")],
            },
            GameSpec {
                name: "Alpha".into(),
                roms: vec![
                    rom("Alpha Shared.bin", b"shared"),
                    rom("Alpha Missing.bin", b"alpha missing"),
                ],
            },
            GameSpec {
                name: "Zulu".into(),
                roms: vec![
                    rom("Zulu Shared.bin", b"shared"),
                    rom("Zulu Extra.bin", b"extra"),
                    rom("Zulu Missing.bin", b"zulu missing"),
                ],
            },
        ])];
        let mut cache = SqliteCache::in_memory().unwrap();

        let summary = execute(&filesystem, &mut cache, &config, &catalogs).unwrap();

        assert_eq!(summary.leftover_details.len(), 1);
        assert_eq!(summary.leftover_details[0].game, "Zulu");
        assert_eq!(
            summary.leftover_details[0]
                .matches
                .iter()
                .find(|rom| rom.expected_rom == "Zulu Extra.bin")
                .expect("verified library content contributes to the score")
                .status,
            LeftoverStatus::Library
        );
        assert!(format!("{}", summary.leftover_details[0]).contains("Zulu Extra.bin [LIBRARY]"));
        assert!(
            format!("{}", summary.leftover_details[0].colored())
                .contains("Zulu Extra.bin \x1b[33m[LIBRARY]\x1b[0m")
        );
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

        assert_eq!(summary.unknown_files, 1);
        let detail = summary
            .leftover_details
            .iter()
            .rev()
            .find(|detail| {
                detail
                    .matches
                    .iter()
                    .any(|rom| rom.expected_rom == "alpha.bin" && rom.status == LeftoverStatus::Ok)
            })
            .expect("content processing emits the detailed incomplete result");
        assert_eq!(
            detail,
            &LeftoverDetail {
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
            }
        );
        assert!(format!("{detail}").contains(concat!(
            "Incomplete: System / Game\n",
            "  alpha.bin -> download.bin [OK]\n",
            "  Exact.bin [OK]\n",
            "  Game.cue [MISMATCH]\n",
            "  Missing.bin [MISSING]\n",
            "  zBad.bin [MISMATCH]\n",
        )));
        let colored = format!("{}", detail.colored());
        assert!(colored.contains("\x1b[96mIncomplete:\x1b[0m System / Game"));
        assert!(colored.contains("zBad.bin \x1b[38;5;208m[MISMATCH]\x1b[0m"));
        assert!(colored.contains("Exact.bin \x1b[32m[OK]\x1b[0m"));
        assert!(colored.contains("Missing.bin \x1b[31m[MISSING]\x1b[0m"));
        assert!(!format!("{summary}").contains('\x1b'));
        assert!(!format!("{summary}").contains("Incomplete:"));
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
    fn cache_records_survive_a_logical_root_relocation() {
        let (filesystem, config) = fixture();
        filesystem.add_file("/root/work/unknown.bin", b"payload".to_vec());
        let mut cache = SqliteCache::in_memory().unwrap();

        let first = execute(&filesystem, &mut cache, &config, &[]).unwrap();
        filesystem
            .rename(Path::new("/root"), Path::new("/relocated"))
            .unwrap();
        let relocated = ResolvedConfig {
            root: PathBuf::from("/relocated"),
            library_path: PathBuf::from("/relocated/library"),
            work_path: PathBuf::from("/relocated/work"),
            dat_path: PathBuf::from("/relocated/dats"),
            database_path: PathBuf::from("/relocated/.romero.sqlite3"),
        };
        let second = execute(&filesystem, &mut cache, &relocated, &[]).unwrap();

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
