use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufReader, Cursor, Read};
use std::path::{Path, PathBuf};

use quick_xml::encoding::Decoder;
use quick_xml::escape::{resolve_xml_entity, unescape};
use quick_xml::events::{BytesCData, BytesRef, BytesStart, Event};
use quick_xml::{Reader, XmlVersion};
use zip::ZipArchive;

use crate::error::{Result, RomeroError};
use crate::filesystem::{EntryKind, FileSystem};
use crate::model::{DatCatalog, DatDate, GameSpec, RomSpec};
use crate::ordering;

pub(crate) fn load_selected_dats<F: FileSystem>(
    filesystem: &F,
    dat_path: &Path,
) -> Result<Vec<DatCatalog>> {
    match filesystem.metadata(dat_path)? {
        Some(metadata) if metadata.kind == EntryKind::Symlink => {
            return Err(RomeroError::Dat(format!(
                "DAT path is a symlink: {}",
                dat_path.display()
            )));
        }
        Some(metadata) if metadata.kind != EntryKind::Directory => {
            return Err(RomeroError::Dat(format!(
                "DAT path is not a directory: {}",
                dat_path.display()
            )));
        }
        Some(_) => {}
        None => return Ok(Vec::new()),
    }

    let mut paths = Vec::new();
    for entry in filesystem.read_directory(dat_path)? {
        if entry.kind == EntryKind::Symlink {
            return Err(RomeroError::Dat(format!(
                "DAT directory contains a symlink: {}",
                entry.path.display()
            )));
        }
        if entry.kind == EntryKind::File
            && (has_extension(&entry.path, "dat") || has_extension(&entry.path, "zip"))
        {
            paths.push(entry.path);
        }
    }
    paths.sort_by(|left, right| ordering::path(left, right));

    let mut catalogs = Vec::new();
    for path in paths {
        if has_extension(&path, "dat") {
            let file = filesystem.open_reader(&path)?;
            catalogs.push(parse_dat(
                BufReader::new(file),
                path.to_string_lossy().into_owned(),
            )?);
        } else {
            load_zip_dats(filesystem, &path, &mut catalogs)?;
        }
    }

    select_catalogs(catalogs)
}

fn load_zip_dats<F: FileSystem>(
    filesystem: &F,
    path: &Path,
    catalogs: &mut Vec<DatCatalog>,
) -> Result<()> {
    let bytes = filesystem.read(path)?;
    let mut archive = ZipArchive::new(Cursor::new(bytes)).map_err(|error| {
        RomeroError::Dat(format!(
            "cannot read DAT archive {}: {error}",
            path.display()
        ))
    })?;

    let mut entries = Vec::new();
    for index in 0..archive.len() {
        let file = archive.by_index(index).map_err(|error| {
            RomeroError::Dat(format!(
                "cannot inspect DAT archive {}: {error}",
                path.display()
            ))
        })?;
        if !file.is_dir() && extension_of_name(file.name(), "dat") {
            entries.push((file.name().to_owned(), index));
        }
    }
    entries.sort_by(|left, right| ordering::text(&left.0, &right.0));

    for (name, index) in entries {
        let mut file = archive.by_index(index).map_err(|error| {
            RomeroError::Dat(format!(
                "cannot open {name} in DAT archive {}: {error}",
                path.display()
            ))
        })?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(|error| {
            RomeroError::io(
                format!("cannot read {name} in DAT archive {}", path.display()),
                error,
            )
        })?;
        catalogs.push(parse_dat(
            Cursor::new(bytes),
            format!("{}:{name}", path.display()),
        )?);
    }
    Ok(())
}

pub(crate) fn parse_dat(reader: impl std::io::BufRead, source: String) -> Result<DatCatalog> {
    let mut reader = Reader::from_reader(reader);
    reader.config_mut().trim_text(false);

    let mut stack: Vec<Vec<u8>> = Vec::new();
    let mut header_name = String::new();
    let mut header_date = String::new();
    let mut games = Vec::new();
    let mut current_game: Option<GameSpec> = None;
    let mut buffer = Vec::new();

    loop {
        match reader
            .read_event_into(&mut buffer)
            .map_err(|error| RomeroError::Dat(format!("invalid DAT XML in {source}: {error}")))?
        {
            Event::Start(element) => {
                let name = element.name().as_ref().to_vec();
                stack.push(name);
                if path_is(&stack, &[b"datafile", b"game"]) {
                    let game_name = required_attribute(
                        &element,
                        b"name",
                        reader.decoder(),
                        "game name",
                        &source,
                    )?;
                    current_game = Some(GameSpec {
                        name: game_name,
                        roms: Vec::new(),
                    });
                } else if path_is(&stack, &[b"datafile", b"game", b"rom"]) {
                    push_rom(&element, reader.decoder(), &source, &mut current_game)?;
                }
            }
            Event::Empty(element) => {
                let name = element.name().as_ref().to_vec();
                stack.push(name);
                if path_is(&stack, &[b"datafile", b"game", b"rom"]) {
                    push_rom(&element, reader.decoder(), &source, &mut current_game)?;
                }
                stack.pop();
            }
            Event::Text(text) => {
                if let Some(target) = header_text_target(&stack, &mut header_name, &mut header_date)
                {
                    target.push_str(&decode_text(&text, &source)?);
                }
            }
            Event::CData(text) => {
                if let Some(target) = header_text_target(&stack, &mut header_name, &mut header_date)
                {
                    target.push_str(&decode_cdata(&text, &source)?);
                }
            }
            Event::GeneralRef(reference) => {
                if let Some(target) = header_text_target(&stack, &mut header_name, &mut header_date)
                {
                    target.push_str(&decode_reference(&reference, &source)?);
                }
            }
            Event::End(_) => {
                if path_is(&stack, &[b"datafile", b"game"]) {
                    let game = current_game.take().ok_or_else(|| {
                        RomeroError::Dat(format!("game without a name in {source}"))
                    })?;
                    games.push(game);
                }
                stack.pop();
            }
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();
    }

    let name = header_name.trim().to_owned();
    if name.is_empty() {
        return Err(RomeroError::Dat(format!("missing header name in {source}")));
    }
    sanitize_header_name(&name, &source)?;
    let date_text = header_date.trim().to_owned();
    if date_text.is_empty() {
        return Err(RomeroError::Dat(format!("missing header date in {source}")));
    }
    let date = parse_date(&date_text).ok_or_else(|| {
        RomeroError::Dat(format!("invalid header date {date_text:?} in {source}"))
    })?;

    let catalog = DatCatalog {
        name,
        date,
        games,
        source,
    };
    validate_catalog(&catalog)?;
    Ok(catalog)
}

fn push_rom(
    element: &BytesStart<'_>,
    decoder: Decoder,
    source: &str,
    game: &mut Option<GameSpec>,
) -> Result<()> {
    let game = game
        .as_mut()
        .ok_or_else(|| RomeroError::Dat(format!("ROM outside a game in {source}")))?;
    let name = required_attribute(element, b"name", decoder, "ROM name", source)?;
    let size = required_attribute(element, b"size", decoder, "ROM size", source)?
        .parse::<u64>()
        .map_err(|_| RomeroError::Dat(format!("invalid ROM size for {name:?} in {source}")))?;
    let sha1 =
        required_attribute(element, b"sha1", decoder, "ROM SHA-1", source)?.to_ascii_lowercase();
    if sha1.len() != 40 || !sha1.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(RomeroError::Dat(format!(
            "invalid ROM SHA-1 for {name:?} in {source}"
        )));
    }
    validate_target_name(&name, "ROM name", source)?;
    game.roms.push(RomSpec { name, size, sha1 });
    Ok(())
}

fn required_attribute(
    element: &BytesStart<'_>,
    key: &[u8],
    decoder: Decoder,
    label: &str,
    source: &str,
) -> Result<String> {
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|error| {
            RomeroError::Dat(format!("invalid XML attribute in {source}: {error}"))
        })?;
        if attribute.key.as_ref() == key {
            let value = attribute
                .decoded_and_normalized_value(XmlVersion::Implicit1_0, decoder)
                .map_err(|error| RomeroError::Dat(format!("invalid {label} in {source}: {error}")))?
                .into_owned();
            if value.is_empty() {
                break;
            }
            return Ok(value);
        }
    }
    Err(RomeroError::Dat(format!("missing {label} in {source}")))
}

fn decode_text(text: &quick_xml::events::BytesText<'_>, source: &str) -> Result<String> {
    let decoded = text
        .xml10_content()
        .map_err(|error| RomeroError::Dat(format!("invalid XML text in {source}: {error}")))?;
    unescape(&decoded)
        .map(|text| text.into_owned())
        .map_err(|error| RomeroError::Dat(format!("invalid XML escape in {source}: {error}")))
}

fn decode_cdata(text: &BytesCData<'_>, source: &str) -> Result<String> {
    text.xml10_content()
        .map(|text| text.into_owned())
        .map_err(|error| RomeroError::Dat(format!("invalid XML text in {source}: {error}")))
}

fn decode_reference(reference: &BytesRef<'_>, source: &str) -> Result<String> {
    if let Some(character) = reference
        .resolve_char_ref()
        .map_err(|error| RomeroError::Dat(format!("invalid XML reference in {source}: {error}")))?
    {
        return Ok(character.to_string());
    }
    let name = reference
        .xml10_content()
        .map_err(|error| RomeroError::Dat(format!("invalid XML reference in {source}: {error}")))?;
    resolve_xml_entity(&name)
        .map(str::to_owned)
        .ok_or_else(|| RomeroError::Dat(format!("unknown XML entity &{name}; in {source}")))
}

fn header_text_target<'a>(
    stack: &[Vec<u8>],
    header_name: &'a mut String,
    header_date: &'a mut String,
) -> Option<&'a mut String> {
    if path_is(stack, &[b"datafile", b"header", b"name"]) {
        Some(header_name)
    } else if path_is(stack, &[b"datafile", b"header", b"date"]) {
        Some(header_date)
    } else {
        None
    }
}

fn path_is(stack: &[Vec<u8>], expected: &[&[u8]]) -> bool {
    stack.len() == expected.len()
        && stack
            .iter()
            .zip(expected)
            .all(|(actual, expected)| actual.as_slice() == *expected)
}

fn parse_date(value: &str) -> Option<DatDate> {
    if value.len() != 19 {
        return None;
    }
    let bytes = value.as_bytes();
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b' '
        || bytes[13] != b'-'
        || bytes[16] != b'-'
    {
        return None;
    }
    let parts = [
        value[0..4].parse().ok()?,
        value[5..7].parse().ok()?,
        value[8..10].parse().ok()?,
        value[11..13].parse().ok()?,
        value[14..16].parse().ok()?,
        value[17..19].parse().ok()?,
    ];
    if !(1..=12).contains(&parts[1])
        || parts[2] == 0
        || parts[2] > days_in_month(parts[0], parts[1])
        || parts[3] > 23
        || parts[4] > 59
        || parts[5] > 59
    {
        return None;
    }
    Some(DatDate(parts))
}

fn days_in_month(year: u16, month: u16) -> u16 {
    match month {
        4 | 6 | 9 | 11 => 30,
        2 if year.is_multiple_of(400) || (year.is_multiple_of(4) && !year.is_multiple_of(100)) => {
            29
        }
        2 => 28,
        _ => 31,
    }
}

fn validate_catalog(catalog: &DatCatalog) -> Result<()> {
    let mut game_names = BTreeSet::new();
    let mut target_names = BTreeMap::<String, String>::new();
    let mut content_sets = BTreeMap::<Vec<String>, String>::new();

    for game in &catalog.games {
        if game.name.is_empty() {
            return Err(RomeroError::Dat(format!(
                "empty game name in {}",
                catalog.source
            )));
        }
        if !game_names.insert(game.name.clone()) {
            return Err(RomeroError::Dat(format!(
                "duplicate game name {:?} in {}",
                game.name, catalog.source
            )));
        }
        if game.roms.is_empty() {
            return Err(RomeroError::Dat(format!(
                "game {:?} has no ROMs in {}",
                game.name, catalog.source
            )));
        }
        if game.roms.iter().filter(|rom| rom.is_cue()).count() > 1 {
            return Err(RomeroError::Dat(format!(
                "game {:?} has more than one CUE in {}",
                game.name, catalog.source
            )));
        }

        for rom in &game.roms {
            let folded = rom.name.to_lowercase();
            if let Some(previous) = target_names.insert(folded, rom.name.clone()) {
                return Err(RomeroError::Dat(format!(
                    "case-insensitive ROM filename collision between {previous:?} and {:?} in {}",
                    rom.name, catalog.source
                )));
            }
        }

        let content = game.content_multiset();
        if let Some(previous) = content_sets.insert(content, game.name.clone()) {
            return Err(RomeroError::Dat(format!(
                "games {previous:?} and {:?} have identical ROM content multisets in {}",
                game.name, catalog.source
            )));
        }
    }
    Ok(())
}

fn select_catalogs(catalogs: Vec<DatCatalog>) -> Result<Vec<DatCatalog>> {
    let mut grouped = BTreeMap::<String, Vec<DatCatalog>>::new();
    for catalog in catalogs {
        grouped
            .entry(catalog.name.clone())
            .or_default()
            .push(catalog);
    }

    let mut selected = Vec::new();
    for (name, mut candidates) in grouped {
        candidates.sort_by(|left, right| {
            right
                .date
                .cmp(&left.date)
                .then_with(|| ordering::text(&left.source, &right.source))
        });
        let newest_date = candidates[0].date;
        let newest: Vec<_> = candidates
            .into_iter()
            .take_while(|catalog| catalog.date == newest_date)
            .collect();
        let semantic = newest[0].semantic_map();
        if let Some(conflict) = newest
            .iter()
            .skip(1)
            .find(|catalog| catalog.semantic_map() != semantic)
        {
            return Err(RomeroError::Dat(format!(
                "conflicting newest DATs for {name:?}: {} and {}",
                newest[0].source, conflict.source
            )));
        }
        selected.push(newest.into_iter().next().expect("newest DAT exists"));
    }

    let mut managed_names = BTreeMap::<String, (String, String)>::new();
    for catalog in &mut selected {
        let original = catalog.name.clone();
        let managed = sanitize_header_name(&original, &catalog.source)?;
        if let Some((previous_original, previous_managed)) =
            managed_names.insert(managed.to_lowercase(), (original.clone(), managed.clone()))
        {
            return Err(RomeroError::Dat(format!(
                "DAT header names {previous_original:?} and {original:?} resolve to colliding system directories {previous_managed:?} and {managed:?}"
            )));
        }
        catalog.name = managed;
    }

    let mut content_sets = BTreeMap::<Vec<String>, (String, String)>::new();
    for catalog in &selected {
        for game in &catalog.games {
            if let Some((previous_system, previous_game)) = content_sets.insert(
                game.content_multiset(),
                (catalog.name.clone(), game.name.clone()),
            ) {
                return Err(RomeroError::Dat(format!(
                    "games {previous_system:?} / {previous_game:?} and {:?} / {:?} have identical ROM content multisets",
                    catalog.name, game.name
                )));
            }
        }
    }

    selected.sort_by(|left, right| ordering::text(&left.name, &right.name));
    Ok(selected)
}

fn validate_target_name(name: &str, label: &str, source: &str) -> Result<()> {
    let invalid = name.is_empty()
        || name == "."
        || name == ".."
        || name.ends_with([' ', '.'])
        || name.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
                )
        });
    if invalid {
        return Err(RomeroError::Dat(format!(
            "unsafe {label} {name:?} in {source}"
        )));
    }

    if is_reserved_windows_name(name) {
        return Err(RomeroError::Dat(format!(
            "reserved {label} {name:?} in {source}"
        )));
    }
    Ok(())
}

fn sanitize_header_name(name: &str, source: &str) -> Result<String> {
    let mut sanitized = String::with_capacity(name.len());
    for character in name.chars() {
        if character.is_control()
            || matches!(
                character,
                '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
            )
        {
            sanitized.push('_');
        } else {
            sanitized.push(character);
        }
    }
    while sanitized.ends_with([' ', '.']) {
        sanitized.pop();
    }
    if sanitized.is_empty() {
        return Err(RomeroError::Dat(format!(
            "DAT header name {name:?} becomes empty after sanitization in {source}"
        )));
    }
    if is_reserved_windows_name(&sanitized) {
        sanitized.insert(0, '_');
    }
    Ok(sanitized)
}

fn is_reserved_windows_name(name: &str) -> bool {
    let stem = name
        .split('.')
        .next()
        .unwrap_or(name)
        .trim_end()
        .to_ascii_uppercase();
    matches!(
        stem.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

fn has_extension(path: &Path, extension: &str) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case(extension))
}

fn extension_of_name(name: &str, extension: &str) -> bool {
    PathBuf::from(name)
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case(extension))
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    use crate::filesystem::MemoryFileSystem;

    use super::*;

    fn sample(date: &str, games: &str) -> String {
        format!(
            r#"<?xml version="1.0"?>
<datafile>
  <header><name>Sony - PlayStation</name><date>{date}</date></header>
  {games}
</datafile>"#
        )
    }

    #[test]
    fn parses_catalog_from_memory() {
        let xml = sample(
            "2026-07-24 13-57-31",
            r#"<game name="Demo">
                <rom name="Demo.cue" size="10" sha1="1111111111111111111111111111111111111111"/>
                <rom name="Demo.bin" size="20" sha1="2222222222222222222222222222222222222222"/>
               </game>"#,
        );
        let dat = parse_dat(Cursor::new(xml), "memory.dat".into()).unwrap();
        assert_eq!(dat.name, "Sony - PlayStation");
        assert_eq!(dat.games[0].roms[1].name, "Demo.bin");
        assert_eq!(dat.games[0].roms[1].size, 20);
    }

    #[test]
    fn decodes_and_preserves_escaped_header_name_text() {
        let xml = sample("2026-07-24 13-57-31", "")
            .replace("Sony - PlayStation", "Sega - Mega CD &amp; Sega CD");

        let dat = parse_dat(Cursor::new(xml), "escaped.dat".into()).unwrap();

        assert_eq!(dat.name, "Sega - Mega CD & Sega CD");
    }

    #[test]
    fn rejects_invalid_date_hash_and_unsafe_name() {
        let invalid_date = sample("2026/07/24", "");
        assert!(parse_dat(Cursor::new(invalid_date), "date.dat".into()).is_err());

        let invalid_hash = sample(
            "2026-07-24 13-57-31",
            r#"<game name="Demo"><rom name="Demo.bin" size="20" sha1="bad"/></game>"#,
        );
        assert!(parse_dat(Cursor::new(invalid_hash), "hash.dat".into()).is_err());

        let unsafe_name = sample(
            "2026-07-24 13-57-31",
            r#"<game name="Demo"><rom name="../Demo.bin" size="20" sha1="2222222222222222222222222222222222222222"/></game>"#,
        );
        assert!(parse_dat(Cursor::new(unsafe_name), "name.dat".into()).is_err());

        let impossible_date = sample("2026-02-29 00-00-00", "");
        assert!(parse_dat(Cursor::new(impossible_date), "calendar.dat".into()).is_err());
    }

    #[test]
    fn rejects_case_collisions_and_duplicate_content_sets() {
        let collision = sample(
            "2026-07-24 13-57-31",
            r#"<game name="One"><rom name="Demo.bin" size="1" sha1="1111111111111111111111111111111111111111"/></game>
               <game name="Two"><rom name="demo.BIN" size="2" sha1="2222222222222222222222222222222222222222"/></game>"#,
        );
        assert!(parse_dat(Cursor::new(collision), "collision.dat".into()).is_err());

        let duplicate = sample(
            "2026-07-24 13-57-31",
            r#"<game name="One"><rom name="One.bin" size="1" sha1="1111111111111111111111111111111111111111"/></game>
               <game name="Two"><rom name="Two.bin" size="1" sha1="1111111111111111111111111111111111111111"/></game>"#,
        );
        assert!(parse_dat(Cursor::new(duplicate), "duplicate.dat".into()).is_err());
    }

    #[test]
    fn newest_equal_catalogs_deduplicate_and_conflicts_fail() {
        let older = parse_dat(
            Cursor::new(sample(
                "2026-01-01 00-00-00",
                r#"<game name="Old"><rom name="Old.bin" size="1" sha1="1111111111111111111111111111111111111111"/></game>"#,
            )),
            "older.dat".into(),
        )
        .unwrap();
        let newest_xml = sample(
            "2026-02-01 00-00-00",
            r#"<game name="New"><rom name="New.bin" size="1" sha1="2222222222222222222222222222222222222222"/></game>"#,
        );
        let newest = parse_dat(Cursor::new(&newest_xml), "newest.dat".into()).unwrap();
        let duplicate = parse_dat(Cursor::new(&newest_xml), "duplicate.dat".into()).unwrap();
        let selected = select_catalogs(vec![older, newest, duplicate]).unwrap();
        assert_eq!(selected[0].games[0].name, "New");

        let conflict = parse_dat(
            Cursor::new(sample(
                "2026-02-01 00-00-00",
                r#"<game name="Other"><rom name="Other.bin" size="1" sha1="3333333333333333333333333333333333333333"/></game>"#,
            )),
            "conflict.dat".into(),
        )
        .unwrap();
        let newest = parse_dat(Cursor::new(&newest_xml), "newest.dat".into()).unwrap();
        assert!(select_catalogs(vec![newest, conflict]).is_err());
    }

    #[test]
    fn sanitizes_header_names_and_rejects_managed_name_collisions() {
        let dotted_xml = sample("2026-01-01 00-00-00", "")
            .replace("Sony - PlayStation", "Hasbro - VideoNow Jr.");
        let dotted = parse_dat(Cursor::new(dotted_xml), "dotted.dat".into()).unwrap();
        assert_eq!(dotted.name, "Hasbro - VideoNow Jr.");

        let selected = select_catalogs(vec![dotted]).unwrap();
        assert_eq!(selected[0].name, "Hasbro - VideoNow Jr");

        assert_eq!(
            sanitize_header_name(r#"Arcade: "Test"?"#, "memory.dat").unwrap(),
            "Arcade_ _Test__"
        );
        assert_eq!(sanitize_header_name("CON", "memory.dat").unwrap(), "_CON");

        let plain_xml = sample("2026-01-01 00-00-00", "").replace("Sony - PlayStation", "System");
        let dotted_xml = sample("2026-01-01 00-00-00", "").replace("Sony - PlayStation", "System.");
        let plain = parse_dat(Cursor::new(plain_xml), "plain.dat".into()).unwrap();
        let dotted = parse_dat(Cursor::new(dotted_xml), "dotted.dat".into()).unwrap();

        assert!(select_catalogs(vec![plain, dotted]).is_err());
    }

    #[test]
    fn duplicate_content_across_selected_systems_is_rejected() {
        let first = parse_dat(
            Cursor::new(sample(
                "2026-01-01 00-00-00",
                r#"<game name="First"><rom name="First.bin" size="1" sha1="1111111111111111111111111111111111111111"/></game>"#,
            )),
            "first.dat".into(),
        )
        .unwrap();
        let second_xml = sample(
            "2026-01-01 00-00-00",
            r#"<game name="Second"><rom name="Second.bin" size="1" sha1="1111111111111111111111111111111111111111"/></game>"#,
        )
        .replace("Sony - PlayStation", "Other System");
        let second = parse_dat(Cursor::new(second_xml), "second.dat".into()).unwrap();

        assert!(select_catalogs(vec![first, second]).is_err());
    }

    #[test]
    fn discovers_direct_dats_from_memory_and_ignores_other_entries() {
        let filesystem = MemoryFileSystem::new(Path::new("/root"));
        filesystem.add_directory("/root/dats");
        filesystem.add_file(
            "/root/dats/catalog.DAT",
            sample("2026-07-24 13-57-31", "").into_bytes(),
        );
        filesystem.add_file("/root/dats/readme.txt", b"not a DAT".to_vec());
        filesystem.add_directory("/root/dats/nested.dat");
        filesystem.add_other("/root/dats/device.zip");

        let catalogs = load_selected_dats(&filesystem, Path::new("/root/dats")).unwrap();

        assert_eq!(catalogs.len(), 1);
        assert_eq!(catalogs[0].name, "Sony - PlayStation");
        assert_eq!(catalogs[0].source, "/root/dats/catalog.DAT");
    }

    #[test]
    fn missing_dat_directory_is_an_empty_catalog_set_in_memory() {
        let filesystem = MemoryFileSystem::new(Path::new("/root"));

        assert!(
            load_selected_dats(&filesystem, Path::new("/root/dats"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn rejects_invalid_dat_directory_kinds_and_symlinked_entries_in_memory() {
        let file_path = MemoryFileSystem::new(Path::new("/root"));
        file_path.add_file("/root/dats", Vec::new());
        assert!(
            load_selected_dats(&file_path, Path::new("/root/dats"))
                .unwrap_err()
                .to_string()
                .contains("not a directory")
        );

        let symlink_path = MemoryFileSystem::new(Path::new("/root"));
        symlink_path.add_symlink("/root/dats");
        assert!(
            load_selected_dats(&symlink_path, Path::new("/root/dats"))
                .unwrap_err()
                .to_string()
                .contains("DAT path is a symlink")
        );

        let symlink_entry = MemoryFileSystem::new(Path::new("/root"));
        symlink_entry.add_directory("/root/dats");
        symlink_entry.add_symlink("/root/dats/catalog.dat");
        assert!(
            load_selected_dats(&symlink_entry, Path::new("/root/dats"))
                .unwrap_err()
                .to_string()
                .contains("contains a symlink")
        );
    }

    #[test]
    fn loads_nested_zip_dat_from_memory_and_sanitizes_its_name() {
        let xml = sample("2026-07-24 13-57-31", "")
            .replace("Sony - PlayStation", "Hasbro - VideoNow Jr.");
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file("ignored.txt", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"ignored").unwrap();
        writer
            .start_file(
                "nested/catalog.dat",
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated),
            )
            .unwrap();
        writer.write_all(xml.as_bytes()).unwrap();
        let archive = writer.finish().unwrap().into_inner();

        let filesystem = MemoryFileSystem::new(Path::new("/root"));
        filesystem.add_directory("/root/dats");
        filesystem.add_file("/root/dats/catalogs.zip", archive);

        let catalogs = load_selected_dats(&filesystem, Path::new("/root/dats")).unwrap();

        assert_eq!(catalogs.len(), 1);
        assert_eq!(catalogs[0].name, "Hasbro - VideoNow Jr");
        assert_eq!(
            catalogs[0].source,
            "/root/dats/catalogs.zip:nested/catalog.dat"
        );
    }

    #[test]
    fn rejects_invalid_zip_bytes_from_memory() {
        let filesystem = MemoryFileSystem::new(Path::new("/root"));
        filesystem.add_directory("/root/dats");
        filesystem.add_file("/root/dats/catalog.zip", b"not a zip".to_vec());

        assert!(
            load_selected_dats(&filesystem, Path::new("/root/dats"))
                .unwrap_err()
                .to_string()
                .contains("cannot read DAT archive")
        );
    }
}
