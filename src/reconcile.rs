use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::Path;

use crate::filesystem::EntryKind;
use crate::model::GameSpec;
use crate::ordering;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HashedFile {
    pub relative_path: std::path::PathBuf,
    pub size: u64,
    pub modified_ns: i64,
    pub sha1: String,
}

pub(crate) fn game_is_complete(
    game: &GameSpec,
    files_by_name: &BTreeMap<String, HashedFile>,
) -> bool {
    game.roms.iter().all(|rom| {
        files_by_name
            .get(&rom.name)
            .is_some_and(|file| file.size == rom.size && file.sha1 == rom.sha1)
    })
}

pub(crate) fn collision_name(name: &OsStr, kind: EntryKind, counter: u32) -> OsString {
    debug_assert!(counter > 0);
    if kind == EntryKind::File {
        let path = Path::new(name);
        if let (Some(stem), Some(extension)) = (path.file_stem(), path.extension()) {
            let mut result = stem.to_os_string();
            result.push(format!(".{counter}."));
            result.push(extension);
            return result;
        }
    }
    let mut result = name.to_os_string();
    result.push(format!(".{counter}"));
    result
}

pub(crate) fn missing_report<'a>(games: impl Iterator<Item = &'a str>) -> Vec<u8> {
    let mut names: Vec<_> = games.collect();
    names.sort_by(|left, right| ordering::text(left, right));
    let mut report = names.join("\n");
    if !report.is_empty() {
        report.push('\n');
    }
    report.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_and_directory_collision_names_follow_policy() {
        assert_eq!(
            collision_name(OsStr::new("disc.bin"), EntryKind::File, 2),
            "disc.2.bin"
        );
        assert_eq!(
            collision_name(OsStr::new("directory.with.dot"), EntryKind::Directory, 1),
            "directory.with.dot.1"
        );
    }

    #[test]
    fn missing_report_is_alphabetical() {
        assert_eq!(
            missing_report(["Zulu", "alpha", "Beta"].into_iter()),
            b"alpha\nBeta\nZulu\n"
        );
        assert!(missing_report(std::iter::empty()).is_empty());
    }
}
