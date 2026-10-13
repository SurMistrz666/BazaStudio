//! Changes the SQL editor makes to its text on a shortcut: comment lines,
//! change the letter case of the selection and find text. Positions are
//! counted in characters, as the cursor of the editor counts them.
use std::ops::Range;

/// The most places a search marks in the editor.
pub const MAX_MATCHES: usize = 500;

/// Put `-- ` in front of each line the selection touches, or take it away
/// when every line with text already starts with `--`. Returns the new text
/// and the selection over the changed lines.
pub fn toggle_comment(text: &str, selection: Range<usize>) -> (String, Range<usize>) {
    let chars: Vec<char> = text.chars().collect();
    let start = selection.start.min(chars.len());
    let mut end = selection.end.min(chars.len()).max(start);
    // A selection that ends where a line begins does not take that line.
    if end > start && chars[end - 1] == '\n' {
        end -= 1;
    }
    let first = chars[..start]
        .iter()
        .rposition(|c| *c == '\n')
        .map_or(0, |i| i + 1);
    let last = chars[end..]
        .iter()
        .position(|c| *c == '\n')
        .map_or(chars.len(), |i| end + i);
    let block: String = chars[first..last].iter().collect();
    let lines: Vec<&str> = block.split('\n').collect();
    let written = lines.iter().filter(|line| !line.trim().is_empty());
    let commented = written.clone().count() > 0
        && written
            .clone()
            .all(|line| line.trim_start().starts_with("--"));
    let changed = lines
        .iter()
        .map(|line| {
            if line.trim().is_empty() {
                (*line).to_owned()
            } else if commented {
                let indent = line.len() - line.trim_start().len();
                let rest = &line[indent + 2..];
                format!(
                    "{}{}",
                    &line[..indent],
                    rest.strip_prefix(' ').unwrap_or(rest)
                )
            } else {
                format!("-- {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut out: String = chars[..first].iter().collect();
    out.push_str(&changed);
    out.extend(&chars[last..]);
    let changed_end = first + changed.chars().count();
    (out, first..changed_end)
}

/// The selected text in capitals or in small letters. Returns the new text
/// and the selection over the changed part, which can grow (ß becomes SS).
pub fn change_case(text: &str, selection: Range<usize>, upper: bool) -> (String, Range<usize>) {
    let chars: Vec<char> = text.chars().collect();
    let start = selection.start.min(chars.len());
    let end = selection.end.min(chars.len()).max(start);
    let part: String = chars[start..end].iter().collect();
    let part = if upper {
        part.to_uppercase()
    } else {
        part.to_lowercase()
    };
    let mut out: String = chars[..start].iter().collect();
    out.push_str(&part);
    out.extend(&chars[end..]);
    let part_end = start + part.chars().count();
    (out, start..part_end)
}

/// A character as a search compares it: without letter case, one for one,
/// so positions in the text and in the search agree.
fn fold(c: char) -> char {
    let mut lower = c.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(single), None) => single,
        _ => c,
    }
}

/// Every place `needle` is found, without letter case, up to [`MAX_MATCHES`].
pub fn find_all(text: &str, needle: &str) -> Vec<Range<usize>> {
    let pattern: Vec<char> = needle.chars().map(fold).collect();
    if pattern.is_empty() {
        return Vec::new();
    }
    let hay: Vec<char> = text.chars().map(fold).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + pattern.len() <= hay.len() && out.len() < MAX_MATCHES {
        if hay[i..i + pattern.len()] == pattern[..] {
            out.push(i..i + pattern.len());
            i += pattern.len();
        } else {
            i += 1;
        }
    }
    out
}

/// The next place `needle` is found after `from`, or the previous one before
/// it, going round the text at its end.
pub fn find(text: &str, needle: &str, from: usize, backwards: bool) -> Option<Range<usize>> {
    let pattern: Vec<char> = needle.chars().map(fold).collect();
    let hay: Vec<char> = text.chars().map(fold).collect();
    if pattern.is_empty() || pattern.len() > hay.len() {
        return None;
    }
    let starts = hay.len() - pattern.len() + 1;
    let at = |i: &usize| hay[*i..*i + pattern.len()] == pattern[..];
    let found = if backwards {
        (0..starts.min(from))
            .rev()
            .find(at)
            .or_else(|| (0..starts).rev().find(at))
    } else {
        (from.min(starts)..starts)
            .find(at)
            .or_else(|| (0..starts).find(at))
    };
    found.map(|i| i..i + pattern.len())
}

/// Byte offset of a character position.
pub fn byte_of(text: &str, chars: usize) -> usize {
    text.char_indices()
        .nth(chars)
        .map_or(text.len(), |(i, _)| i)
}

/// Character positions of a byte range.
pub fn chars_of(text: &str, bytes: Range<usize>) -> Range<usize> {
    let count = |end: usize| text[..end.min(text.len())].chars().count();
    count(bytes.start)..count(bytes.end)
}

/// Number of the line (from 0) a character position is on.
pub fn line_of(text: &str, chars: usize) -> usize {
    text.chars().take(chars).filter(|c| *c == '\n').count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_commented_and_uncommented() {
        let text = "SELECT 1\n  FROM t\n\nWHERE x";
        // The cursor alone: its line only.
        let (once, selection) = toggle_comment(text, 3..3);
        assert_eq!(once, "-- SELECT 1\n  FROM t\n\nWHERE x");
        assert_eq!(selection, 0..11);
        // A selection over three lines keeps the empty one as it is.
        let (all, selection) = toggle_comment(text, 2..20);
        assert_eq!(all, "-- SELECT 1\n--   FROM t\n\n-- WHERE x");
        let (back, _) = toggle_comment(&all, selection);
        assert_eq!(back, text);
        // A selection that ends at the start of a line leaves that line.
        let (part, _) = toggle_comment(text, 0..9);
        assert_eq!(part, "-- SELECT 1\n  FROM t\n\nWHERE x");
        // Mixed lines get the mark; lines already commented keep theirs.
        let (mixed, _) = toggle_comment("-- a\nb", 0..6);
        assert_eq!(mixed, "-- -- a\n-- b");
        assert_eq!(toggle_comment("--x", 0..0).0, "x");
    }

    #[test]
    fn the_case_of_the_selection_changes() {
        assert_eq!(
            change_case("select name from łódź", 0..6, true),
            ("SELECT name from łódź".into(), 0..6)
        );
        assert_eq!(change_case("A STRASSE", 2..9, false).0, "A strasse");
        assert_eq!(change_case("maß", 0..3, true), ("MASS".into(), 0..4));
    }

    #[test]
    fn text_is_found_without_letter_case_and_around_the_end() {
        let text = "Select a FROM b; select c";
        assert_eq!(find_all(text, "select"), vec![0..6, 17..23]);
        assert_eq!(find(text, "SELECT", 1, false), Some(17..23));
        assert_eq!(find(text, "SELECT", 18, false), Some(0..6));
        assert_eq!(find(text, "select", 17, true), Some(0..6));
        assert_eq!(find(text, "select", 0, true), Some(17..23));
        assert_eq!(find(text, "", 0, false), None);
        assert_eq!(find(text, "nothing", 0, false), None);
    }

    #[test]
    fn positions_convert_between_bytes_and_characters() {
        let text = "zażółć\ngęślą";
        assert_eq!(byte_of(text, 3), 4);
        assert_eq!(byte_of(text, 100), text.len());
        assert_eq!(chars_of(text, 4..text.len()), 3..12);
        assert_eq!(line_of(text, 8), 1);
    }
}
