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

#[cfg(test)]
mod tests {
    use super::*;

    fn rom(name: &str, size: u64, sha1: &str) -> RomSpec {
        RomSpec {
            name: name.into(),
            size,
            sha1: sha1.into(),
        }
    }

    #[test]
    fn identifies_cues_and_iterates_non_cue_roms() {
        let game = GameSpec {
            name: "Game".into(),
            roms: vec![
                rom("Game.CUE", 1, "cue"),
                rom("Game.bin", 2, "bin"),
                rom("cue", 3, "plain"),
            ],
        };

        assert_eq!(game.cue().map(|rom| rom.name.as_str()), Some("Game.CUE"));
        assert_eq!(
            game.non_cue_roms()
                .map(|rom| rom.name.as_str())
                .collect::<Vec<_>>(),
            ["Game.bin", "cue"]
        );
    }

    #[test]
    fn content_multiset_sorts_hashes_and_preserves_duplicates() {
        let game = GameSpec {
            name: "Game".into(),
            roms: vec![
                rom("third.bin", 1, "b"),
                rom("first.bin", 1, "a"),
                rom("second.bin", 1, "a"),
            ],
        };

        assert_eq!(game.content_multiset(), ["a", "a", "b"]);
    }

    #[test]
    fn semantic_map_normalizes_rom_order_without_changing_games() {
        let catalog = DatCatalog {
            name: "System".into(),
            date: DatDate([2026, 1, 1, 0, 0, 0]),
            games: vec![GameSpec {
                name: "Game".into(),
                roms: vec![
                    rom("z.bin", 2, "b"),
                    rom("Alpha.bin", 1, "a"),
                    rom("alpha.bin", 3, "c"),
                ],
            }],
            source: "memory.dat".into(),
        };

        let map = catalog.semantic_map();
        let names: Vec<_> = map["Game"].iter().map(|rom| rom.name.as_str()).collect();

        assert_eq!(names, ["Alpha.bin", "alpha.bin", "z.bin"]);
        assert_eq!(catalog.games[0].roms[0].name, "z.bin");
    }
}
