//! A line-level view of dotenv files, for the jobs `dotenvy` does not do: reading `# secret`
//! markers in `.env.example`, locating entries by line for leak reports, and rewriting one key
//! in place while keeping comments and order. Values are *loaded* with `dotenvy`.

/// One `KEY=value` entry. Lines are 0-based; `last_line` differs from `first_line` for a quoted
/// value spanning several lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub key: String,
    /// Whether the value is non-empty (quotes removed). The value itself is not kept.
    pub has_value: bool,
    /// Text of a trailing `# comment`, without the `#`.
    pub comment: Option<String>,
    pub first_line: usize,
    pub last_line: usize,
}

fn lines(text: &str) -> Vec<&str> {
    text.split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect()
}

/// Every well-formed entry in `text`. Malformed lines are skipped.
pub(crate) fn entries(text: &str) -> Vec<Entry> {
    let lines = lines(text);
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        match entry_at(&lines, i) {
            Some(e) => {
                i = e.last_line + 1;
                out.push(e);
            }
            None => i += 1,
        }
    }
    out
}

fn entry_at(lines: &[&str], i: usize) -> Option<Entry> {
    let t = lines[i].trim_start();
    if t.is_empty() || t.starts_with('#') {
        return None;
    }
    let t = t
        .strip_prefix("export")
        .filter(|r| r.starts_with([' ', '\t']))
        .map_or(t, str::trim_start);
    let key_len = t
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.'))
        .unwrap_or(t.len());
    let key = &t[..key_len];
    if key.is_empty() || key.starts_with(|c: char| c.is_ascii_digit() || c == '.') {
        return None;
    }
    let rest = t[key_len..].trim_start().strip_prefix('=')?.trim_start();

    let (has_value, after, last_line) = match rest.chars().next() {
        Some(q @ ('\'' | '"')) => {
            // Find the closing quote, possibly on a later line.
            let mut buf = rest[1..].to_string();
            let mut line = i;
            loop {
                if let Some(end) = closing_quote(&buf, q) {
                    let has_value = end > 0;
                    let after = buf[end + 1..].to_string();
                    break (has_value, after, line);
                }
                line += 1;
                if line >= lines.len() {
                    return None;
                }
                buf.push('\n');
                buf.push_str(lines[line]);
            }
        }
        _ => {
            let end = comment_start(rest).unwrap_or(rest.len());
            let value = rest[..end].trim_end();
            (!value.is_empty(), rest[end..].to_string(), i)
        }
    };
    let after = after.trim_start();
    let comment = after.strip_prefix('#').map(|c| c.trim().to_string());
    Some(Entry {
        key: key.to_string(),
        has_value,
        comment,
        first_line: i,
        last_line,
    })
}

/// Byte index of the closing `q` in `s` (which starts just after the opening quote).
fn closing_quote(s: &str, q: char) -> Option<usize> {
    let mut escaped = false;
    for (idx, c) in s.char_indices() {
        if escaped {
            escaped = false;
        } else if c == '\\' && q == '"' {
            escaped = true;
        } else if c == q {
            return Some(idx);
        }
    }
    None
}

/// Where an unquoted value's inline comment starts: a `#` at the start or after whitespace.
fn comment_start(s: &str) -> Option<usize> {
    let mut prev_ws = true;
    for (idx, c) in s.char_indices() {
        if c == '#' && prev_ws {
            return Some(idx);
        }
        prev_ws = c == ' ' || c == '\t';
    }
    None
}

/// `value` as a double-quoted dotenv value that `dotenvy` reads back unchanged: no
/// `${VAR}` expansion, newlines as `\n`.
pub(crate) fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '$' => out.push_str("\\$"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `text` with every entry for `key` replaced by one `KEY="value"` line at the first entry's
/// position (appended when absent). Comments, blank lines and other entries are kept.
pub(crate) fn upsert(text: &str, key: &str, value: &str) -> String {
    let src = lines(text);
    let found: Vec<Entry> = entries(text).into_iter().filter(|e| e.key == key).collect();
    let new_line = format!("{key}={}", quote(value));
    let mut out: Vec<String> = Vec::with_capacity(src.len() + 1);
    let mut placed = false;
    let mut i = 0;
    while i < src.len() {
        if let Some(e) = found.iter().find(|e| e.first_line == i) {
            if !placed {
                out.push(new_line.clone());
                placed = true;
            }
            i = e.last_line + 1;
        } else {
            out.push(src[i].to_string());
            i += 1;
        }
    }
    // `split('\n')` yields a trailing "" for text ending in a newline; keep appends before it.
    if !placed {
        if out.last().is_some_and(String::is_empty) {
            out.pop();
        }
        out.push(new_line);
    }
    if !out.last().is_some_and(String::is_empty) {
        out.push(String::new());
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(text: &str) -> Vec<(String, bool, Option<String>, usize)> {
        entries(text)
            .into_iter()
            .map(|e| (e.key, e.has_value, e.comment, e.first_line + 1))
            .collect()
    }

    #[test]
    fn scans_entries_and_comments() {
        let text = "# header\nA=1\nexport B = two # note\nC=\nD= # secret\nE=\"\" # secret: token\nF='x' #secret\n  G=a#b\nbad line\n";
        assert_eq!(
            keys(text),
            vec![
                ("A".into(), true, None, 2),
                ("B".into(), true, Some("note".into()), 3),
                ("C".into(), false, None, 4),
                ("D".into(), false, Some("secret".into()), 5),
                ("E".into(), false, Some("secret: token".into()), 6),
                ("F".into(), true, Some("secret".into()), 7),
                ("G".into(), true, None, 8),
            ]
        );
    }

    #[test]
    fn multi_line_quoted_values() {
        let text = "A='line1\nline2' # secret\nB=\"x\\\"\ny\"\nC=3\r\n";
        let e = entries(text);
        assert_eq!(e.len(), 3);
        assert_eq!(
            (e[0].first_line, e[0].last_line, e[0].comment.as_deref()),
            (0, 1, Some("secret"))
        );
        assert_eq!((e[1].first_line, e[1].last_line), (2, 3));
        assert_eq!(e[2].key, "C");
        // An unterminated quote is malformed: skipped, later lines still scanned.
        assert_eq!(keys("A='open\nB=2\n").len(), 1);
    }

    #[test]
    fn upsert_replaces_in_place_and_appends() {
        let text = "# keep\nA=1\nB='multi\nline'\nC=3\nB=dup\n";
        assert_eq!(upsert(text, "B", "new"), "# keep\nA=1\nB=\"new\"\nC=3\n");
        assert_eq!(upsert(text, "D", "4"), format!("{text}D=\"4\"\n"));
        assert_eq!(upsert("", "A", "1"), "A=\"1\"\n");
        assert_eq!(upsert("X=1", "A", "1"), "X=1\nA=\"1\"\n");
    }

    #[test]
    fn quoted_values_read_back_exactly_through_dotenvy() {
        let values = [
            "plain",
            "with spaces # and hash",
            "quote ' and \" and $HOME and ${X} and \\ backslash",
            "multi\nline\nvalue",
            "ends with backslash\\",
            "  padded  ",
            "",
            "tab\there",
        ];
        let mut text = String::new();
        for (i, v) in values.iter().enumerate() {
            text = upsert(&text, &format!("K{i}"), v);
        }
        let parsed: Vec<(String, String)> = dotenvy::from_read_iter(text.as_bytes())
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(parsed.len(), values.len());
        for (i, v) in values.iter().enumerate() {
            assert_eq!(parsed[i], (format!("K{i}"), v.to_string()), "{text}");
        }
    }
}
