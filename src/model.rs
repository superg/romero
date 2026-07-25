use std::collections::BTreeMap;

use crate::ordering;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct RomSpec {
    pub name: String,
    pub size: u64,
    pub sha1: String,
}

impl RomSpec {
    pub fn is_cue(&self) -> bool {
        self.name
            .rsplit_once('.')
            .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("cue"))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GameSpec {
    pub name: String,
    pub roms: Vec<RomSpec>,
}

impl GameSpec {
    pub fn cue(&self) -> Option<&RomSpec> {
        self.roms.iter().find(|rom| rom.is_cue())
    }

    pub fn non_cue_roms(&self) -> impl Iterator<Item = &RomSpec> {
        self.roms.iter().filter(|rom| !rom.is_cue())
    }

    pub fn content_multiset(&self) -> Vec<String> {
        let mut hashes: Vec<_> = self.roms.iter().map(|rom| rom.sha1.clone()).collect();
        hashes.sort();
        hashes
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct DatDate(pub [u16; 6]);

#[derive(Clone, Debug)]
pub(crate) struct DatCatalog {
    pub name: String,
    pub date: DatDate,
    pub games: Vec<GameSpec>,
    pub source: String,
}

impl DatCatalog {
    pub fn semantic_map(&self) -> BTreeMap<String, Vec<RomSpec>> {
        self.games
            .iter()
            .map(|game| {
                let mut roms = game.roms.clone();
                roms.sort_by(|left, right| {
                    ordering::text(&left.name, &right.name)
                        .then_with(|| left.size.cmp(&right.size))
                        .then_with(|| left.sha1.cmp(&right.sha1))
                });
                (game.name.clone(), roms)
            })
            .collect()
    }
}
