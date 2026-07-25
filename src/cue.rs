use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::ops::Range;

use crate::error::{Result, RomeroError};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CueDocument {
    source: String,
    references: BTreeMap<String, Vec<Range<usize>>>,
}

impl CueDocument {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let source = std::str::from_utf8(bytes)
            .map_err(|_| RomeroError::Operational("CUE is not valid UTF-8".into()))?
            .to_owned();
        let mut references = BTreeMap::<String, Vec<Range<usize>>>::new();
        let mut offset = 0;

        for line in source.split_inclusive('\n') {
            if let Some((name, span)) = parse_file_directive(line, offset)? {
                references.entry(name).or_default().push(span);
            }
            offset += line.len();
        }
        if offset < source.len() {
            let line = &source[offset..];
            if let Some((name, span)) = parse_file_directive(line, offset)? {
                references.entry(name).or_default().push(span);
            }
        }
        if references.is_empty() {
            return Err(RomeroError::Operational(
                "CUE does not contain a FILE directive".into(),
            ));
        }
        Ok(Self { source, references })
    }

    pub fn referenced_names(&self) -> impl Iterator<Item = &str> {
        self.references.keys().map(String::as_str)
    }

    pub fn rewrite(&self, replacements: &BTreeMap<String, String>) -> Result<Vec<u8>> {
        let mut edits = Vec::new();
        for (source, spans) in &self.references {
            let replacement = replacements.get(source).ok_or_else(|| {
                RomeroError::Operational(format!(
                    "CUE reference {source:?} has no DAT filename assignment"
                ))
            })?;
            for span in spans {
                edits.push((span.clone(), replacement.as_str()));
            }
        }
        edits.sort_by_key(|edit| Reverse(edit.0.start));

        let mut rewritten = self.source.clone();
        for (span, replacement) in edits {
            rewritten.replace_range(span, replacement);
        }
        Ok(rewritten.into_bytes())
    }
}

fn parse_file_directive(
    line: &str,
    absolute_offset: usize,
) -> Result<Option<(String, Range<usize>)>> {
    let bytes = line.as_bytes();
    let mut cursor = 0;
    while bytes
        .get(cursor)
        .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
    {
        cursor += 1;
    }
    if bytes.len() < cursor + 4 || !bytes[cursor..cursor + 4].eq_ignore_ascii_case(b"FILE") {
        return Ok(None);
    }
    cursor += 4;
    if !bytes
        .get(cursor)
        .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
    {
        return Ok(None);
    }
    while bytes
        .get(cursor)
        .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
    {
        cursor += 1;
    }

    let (start, end) = if bytes.get(cursor) == Some(&b'"') {
        cursor += 1;
        let start = cursor;
        let Some(relative_end) = bytes[cursor..].iter().position(|byte| *byte == b'"') else {
            return Err(RomeroError::Operational(
                "CUE contains an unterminated quoted FILE name".into(),
            ));
        };
        (start, cursor + relative_end)
    } else {
        let start = cursor;
        while bytes
            .get(cursor)
            .is_some_and(|byte| !matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
        {
            cursor += 1;
        }
        (start, cursor)
    };

    if start == end {
        return Err(RomeroError::Operational(
            "CUE contains an empty FILE name".into(),
        ));
    }
    let name = &line[start..end];
    if name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
    {
        return Err(RomeroError::Operational(format!(
            "CUE FILE reference is not a basename: {name:?}"
        )));
    }
    Ok(Some((
        name.to_owned(),
        absolute_offset + start..absolute_offset + end,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_only_filename_tokens_and_preserves_line_endings() {
        let source = b"FILE \"old.bin\" BINARY\r\n  TRACK 01 MODE2/2352\r\nFILE old.bin BINARY\r\n";
        let cue = CueDocument::parse(source).unwrap();
        let rewritten = cue
            .rewrite(&BTreeMap::from([(
                "old.bin".into(),
                "Final Name.bin".into(),
            )]))
            .unwrap();
        assert_eq!(
            rewritten,
            b"FILE \"Final Name.bin\" BINARY\r\n  TRACK 01 MODE2/2352\r\nFILE Final Name.bin BINARY\r\n"
        );
    }

    #[test]
    fn rejects_paths_and_non_utf8_content() {
        assert!(CueDocument::parse(b"FILE \"dir/file.bin\" BINARY\n").is_err());
        assert!(CueDocument::parse(&[0xff, 0xfe]).is_err());
    }

    #[test]
    fn requires_a_file_directive() {
        assert!(CueDocument::parse(b"TRACK 01 AUDIO\n").is_err());
    }
}
