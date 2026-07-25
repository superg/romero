use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::path::Path;

use crate::filesystem::EntryKind;
use crate::model::{GameSpec, RomSpec};
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

pub(crate) fn deterministic_assignment(
    sources: &[(String, String)],
    expected: &[&RomSpec],
) -> Option<BTreeMap<String, String>> {
    all_assignments(sources, expected).into_iter().next()
}

pub(crate) fn all_assignments(
    sources: &[(String, String)],
    expected: &[&RomSpec],
) -> Vec<BTreeMap<String, String>> {
    if sources.len() != expected.len() {
        return Vec::new();
    }

    let mut sources_by_hash = BTreeMap::<String, Vec<String>>::new();
    for (name, hash) in sources {
        sources_by_hash
            .entry(hash.clone())
            .or_default()
            .push(name.clone());
    }
    let mut expected_by_hash = BTreeMap::<String, Vec<String>>::new();
    for rom in expected {
        expected_by_hash
            .entry(rom.sha1.clone())
            .or_default()
            .push(rom.name.clone());
    }
    if sources_by_hash.keys().ne(expected_by_hash.keys())
        || sources_by_hash
            .iter()
            .any(|(hash, sources)| sources.len() != expected_by_hash[hash].len())
    {
        return Vec::new();
    }

    for sources in sources_by_hash.values_mut() {
        sources.sort_by(|left, right| ordering::text(left, right));
    }
    for expected in expected_by_hash.values_mut() {
        expected.sort_by(|left, right| ordering::text(left, right));
    }

    let groups: Vec<_> = sources_by_hash
        .into_iter()
        .map(|(hash, sources)| (sources, expected_by_hash.remove(&hash).unwrap_or_default()))
        .collect();
    let mut assignments = vec![BTreeMap::new()];
    for (sources, expected) in groups {
        let permutations = permutations(expected);
        let mut expanded = Vec::new();
        for assignment in assignments {
            for permutation in &permutations {
                let mut next = assignment.clone();
                for (source, target) in sources.iter().zip(permutation) {
                    next.insert(source.clone(), target.clone());
                }
                expanded.push(next);
            }
        }
        assignments = expanded;
    }
    assignments
}

fn permutations(mut values: Vec<String>) -> Vec<Vec<String>> {
    values.sort_by(|left, right| ordering::text(left, right));
    let mut result = Vec::new();
    permute(&mut values, 0, &mut result);
    result
}

fn permute(values: &mut [String], index: usize, result: &mut Vec<Vec<String>>) {
    if index == values.len() {
        result.push(values.to_vec());
        return;
    }
    let mut used = BTreeSet::new();
    for candidate in index..values.len() {
        if used.insert(values[candidate].clone()) {
            values.swap(index, candidate);
            permute(values, index + 1, result);
            values.swap(index, candidate);
        }
    }
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

    fn rom(name: &str, hash: &str) -> RomSpec {
        RomSpec {
            name: name.into(),
            size: 1,
            sha1: hash.into(),
        }
    }

    #[test]
    fn repeated_hash_assignments_are_deterministic_and_complete() {
        let first = rom("Alpha Target.bin", "1");
        let second = rom("zulu target.bin", "1");
        let sources = vec![
            ("alpha source.bin".to_owned(), "1".to_owned()),
            ("Zulu Source.bin".to_owned(), "1".to_owned()),
        ];
        let assignments = all_assignments(&sources, &[&first, &second]);
        assert_eq!(assignments.len(), 2);
        assert_eq!(assignments[0]["alpha source.bin"], "Alpha Target.bin");
        assert_eq!(assignments[0]["Zulu Source.bin"], "zulu target.bin");
    }

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
