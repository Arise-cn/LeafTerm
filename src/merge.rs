//! Byte-preserving parsing of Git's merge, diff3, and zdiff3 conflict markers.

use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    Current,
    Incoming,
    Both,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    /// Entire conflict, including both marker lines and their line endings.
    pub range: Range<usize>,
    pub current: Range<usize>,
    pub incoming: Range<usize>,
    pub base: Option<Range<usize>>,
    pub current_label: String,
    pub incoming_label: String,
    /// Zero-based line index of the opening marker.
    pub start_line: usize,
}

impl Conflict {
    /// Use the same text revision that was passed to `parse`.
    pub fn replacement(&self, text: &str, choice: Choice) -> String {
        match choice {
            Choice::Current => text[self.current.clone()].to_owned(),
            Choice::Incoming => text[self.incoming.clone()].to_owned(),
            Choice::Both => {
                let mut replacement = text[self.current.clone()].to_owned();
                replacement.push_str(&text[self.incoming.clone()]);
                replacement
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Section {
    Current,
    Base(usize),
    Incoming(usize),
}

struct Pending {
    marker_len: usize,
    conflict: Conflict,
    section: Section,
}

/// Markers must occupy whole lines, starting in column zero. A label must be
/// separated from the marker by whitespace; operators or quoted strings are text.
pub fn parse(text: &str) -> Result<Vec<Conflict>, String> {
    let mut conflicts = Vec::new();
    let mut pending: Option<Pending> = None;
    let mut offset = 0;
    for (line_index, line) in text.split_inclusive('\n').enumerate() {
        let start = offset;
        offset += line.len();
        let body = line.strip_suffix('\n').unwrap_or(line);
        let body = body.strip_suffix('\r').unwrap_or(body);
        let Some((kind, marker_len, label)) = marker(body) else {
            continue;
        };
        let invalid = || format!("Invalid conflict marker on line {}.", line_index + 1);
        if kind == b'<' {
            if pending.is_some() {
                return Err(invalid());
            }
            pending = Some(Pending {
                marker_len,
                conflict: Conflict {
                    range: start..offset,
                    current: offset..offset,
                    incoming: offset..offset,
                    base: None,
                    current_label: label.to_owned(),
                    incoming_label: String::new(),
                    start_line: line_index,
                },
                section: Section::Current,
            });
            continue;
        }
        let active = pending.as_mut().ok_or_else(invalid)?;
        if marker_len != active.marker_len {
            return Err(invalid());
        }
        match (kind, active.section) {
            (b'|', Section::Current) => {
                active.conflict.current.end = start;
                active.section = Section::Base(offset);
            }
            (b'=', Section::Current) => {
                active.conflict.current.end = start;
                active.section = Section::Incoming(offset);
            }
            (b'=', Section::Base(base_start)) => {
                active.conflict.base = Some(base_start..start);
                active.section = Section::Incoming(offset);
            }
            (b'>', Section::Incoming(incoming_start)) => {
                active.conflict.incoming = incoming_start..start;
                active.conflict.incoming_label = label.to_owned();
                active.conflict.range.end = offset;
                conflicts.push(pending.take().unwrap().conflict);
            }
            _ => return Err(invalid()),
        }
    }
    if let Some(active) = pending {
        return Err(format!(
            "Unfinished conflict starting on line {}.",
            active.conflict.start_line + 1
        ));
    }
    Ok(conflicts)
}

fn marker(line: &str) -> Option<(u8, usize, &str)> {
    let kind = *line.as_bytes().first()?;
    if !matches!(kind, b'<' | b'|' | b'=' | b'>') {
        return None;
    }
    let len = line.bytes().take_while(|&byte| byte == kind).count();
    if len < 7 {
        return None;
    }
    let rest = &line[len..];
    if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
        return None;
    }
    let label = rest.trim();
    if kind == b'=' && !label.is_empty() {
        return None;
    }
    Some((kind, len, label))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiple_conflicts_keep_unicode_ranges_and_surrounding_text() {
        let text = "前言\n<<<<<<< HEAD\n当前\n=======\n传入\n>>>>>>> topic\n中间\n<<<<<<< ours\n甲\n=======\n乙\n>>>>>>> theirs\n结束";
        let conflicts = parse(text).unwrap();
        assert_eq!(conflicts.len(), 2);
        assert_eq!(conflicts[0].start_line, 1);
        assert_eq!(conflicts[1].start_line, 7);
        assert_eq!(conflicts[0].current_label, "HEAD");
        assert_eq!(conflicts[0].incoming_label, "topic");
        assert_eq!(conflicts[0].replacement(text, Choice::Current), "当前\n");
        assert_eq!(conflicts[0].replacement(text, Choice::Incoming), "传入\n");
        assert_eq!(conflicts[0].replacement(text, Choice::Both), "当前\n传入\n");
        let mut resolved = text.to_owned();
        for conflict in conflicts.iter().rev() {
            resolved.replace_range(
                conflict.range.clone(),
                &conflict.replacement(text, Choice::Both),
            );
        }
        assert_eq!(resolved, "前言\n当前\n传入\n中间\n甲\n乙\n结束");
        assert!(parse(&resolved).unwrap().is_empty());
    }

    #[test]
    fn empty_sides_and_closing_marker_without_final_newline_are_valid() {
        for (text, current, incoming) in [
            ("<<<<<<<\n=======\nnew\n>>>>>>>", "", "new\n"),
            ("<<<<<<<\nold\n=======\n>>>>>>>\n", "old\n", ""),
            ("<<<<<<<\n=======\n>>>>>>>\n", "", ""),
        ] {
            let conflict = parse(text).unwrap().pop().unwrap();
            assert_eq!(conflict.range, 0..text.len());
            assert_eq!(conflict.replacement(text, Choice::Current), current);
            assert_eq!(conflict.replacement(text, Choice::Incoming), incoming);
            assert_eq!(
                conflict.replacement(text, Choice::Both),
                format!("{current}{incoming}")
            );
        }
    }

    #[test]
    fn diff3_custom_marker_lengths_preserve_crlf_and_exclude_the_base() {
        let text = "shared\r\n<<<<<<<<< ours\r\n当前\r\n||||||||| base\r\n旧版\r\n=========\r\n传入\r\n>>>>>>>>> theirs\r\nshared end\r\n";
        let conflict = parse(text).unwrap().pop().unwrap();
        assert_eq!(&text[conflict.base.clone().unwrap()], "旧版\r\n");
        assert_eq!(conflict.replacement(text, Choice::Both), "当前\r\n传入\r\n");
        let mut resolved = text.to_owned();
        resolved.replace_range(
            conflict.range.clone(),
            &conflict.replacement(text, Choice::Incoming),
        );
        assert_eq!(resolved, "shared\r\n传入\r\nshared end\r\n");
        // zdiff3 uses the same markers, moving shared context outside the block.
        assert_eq!(conflict.start_line, 1);
    }

    #[test]
    fn malformed_nested_or_unfinished_markers_are_rejected() {
        for text in [
            "<<<<<<< ours\ncurrent\n",
            "=======\n",
            ">>>>>>> theirs\n",
            "||||||| base\n",
            "<<<<<<< ours\n>>>>>>>> theirs\n",
            "<<<<<<< ours\n========\n>>>>>>> theirs\n",
            "<<<<<<< ours\n=======\n>>>>>>>> theirs\n",
            "<<<<<<< ours\n|||||||| base\n=======\n>>>>>>> theirs\n",
            "<<<<<<< ours\n<<<<<<< nested\n=======\n>>>>>>> theirs\n",
            "<<<<<<< ours\n=======\n=======\n>>>>>>> theirs\n",
            "<<<<<<< ours\n||||||| base\n>>>>>>> theirs\n",
            "<<<<<<< ours\n=======\n||||||| base\n>>>>>>> theirs\n",
        ] {
            assert!(parse(text).is_err(), "accepted {text:?}");
        }
    }

    #[test]
    fn ordinary_strings_operators_and_short_runs_are_not_markers() {
        let text = "let text = \"<<<<<<< HEAD\";\n  <<<<<<< indented\n<<<<<<<operator\n>>>>>>>value\n|||||||value\n=======value\n<<<<<< short\n======\n>>>>>>\n";
        assert!(parse(text).unwrap().is_empty());
    }
}
