//! Unified diffs of text, for `dot diff`.

/// One line of an edit script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Same(usize, usize),
    Del(usize),
    Ins(usize),
}

/// Beyond this many cells the middle of two texts is not searched for common lines: it is shown
/// as replaced.
const MAX_CELLS: usize = 4_000_000;

/// Lines with their line ending, so a missing final newline shows.
fn lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

/// The edit script turning `a` into `b`: common prefix and suffix, and a longest common
/// subsequence of the middle.
fn script(a: &[&str], b: &[&str]) -> Vec<Op> {
    let pre = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suf = a[pre..]
        .iter()
        .rev()
        .zip(b[pre..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (am, bm) = (&a[pre..a.len() - suf], &b[pre..b.len() - suf]);
    let mut ops: Vec<Op> = (0..pre).map(|i| Op::Same(i, i)).collect();
    let (n, m) = (am.len(), bm.len());
    if n.saturating_mul(m) > MAX_CELLS {
        ops.extend((0..n).map(|i| Op::Del(pre + i)));
        ops.extend((0..m).map(|j| Op::Ins(pre + j)));
    } else {
        // lcs[i][j]: the LCS length of am[i..] and bm[j..].
        let w = m + 1;
        let mut lcs = vec![0u32; (n + 1) * w];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                lcs[i * w + j] = if am[i] == bm[j] {
                    lcs[(i + 1) * w + j + 1] + 1
                } else {
                    lcs[(i + 1) * w + j].max(lcs[i * w + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n || j < m {
            if i < n && j < m && am[i] == bm[j] {
                ops.push(Op::Same(pre + i, pre + j));
                i += 1;
                j += 1;
            } else if i < n && (j == m || lcs[(i + 1) * w + j] >= lcs[i * w + j + 1]) {
                // Deletions first, as diff(1) shows them.
                ops.push(Op::Del(pre + i));
                i += 1;
            } else {
                ops.push(Op::Ins(pre + j));
                j += 1;
            }
        }
    }
    let (an, bn) = (a.len(), b.len());
    ops.extend((0..suf).map(|k| Op::Same(an - suf + k, bn - suf + k)));
    ops
}

fn push_line(out: &mut String, sign: char, line: &str) {
    out.push(sign);
    match line.strip_suffix('\n') {
        Some(l) => {
            out.push_str(l.strip_suffix('\r').unwrap_or(l));
            out.push('\n');
        }
        None => {
            out.push_str(line);
            out.push_str("\n\\ No newline at end of file\n");
        }
    }
}

/// A unified diff (`---` / `+++` headers, hunks with `context` lines around each change) from
/// `old` to `new`; empty when they are equal.
pub fn unified(old: &str, new: &str, old_label: &str, new_label: &str, context: usize) -> String {
    if old == new {
        return String::new();
    }
    let (a, b) = (lines(old), lines(new));
    let ops = script(&a, &b);
    let changed: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, o)| !matches!(o, Op::Same(..)))
        .map(|(k, _)| k)
        .collect();
    let mut out = format!("--- {old_label}\n+++ {new_label}\n");
    let mut k = 0;
    while k < changed.len() {
        // Group changes whose context would touch or overlap.
        let start = changed[k].saturating_sub(context);
        let mut end = changed[k];
        while k + 1 < changed.len() && changed[k + 1] <= end + 2 * context + 1 {
            k += 1;
            end = changed[k];
        }
        let end = (end + context + 1).min(ops.len());
        let hunk = &ops[start..end];
        let (mut a_start, mut b_start) = (None, None);
        let (mut a_len, mut b_len) = (0, 0);
        // Where each side's first line is, counting from where the hunk starts.
        let a_before = ops[..start]
            .iter()
            .filter(|o| !matches!(o, Op::Ins(_)))
            .count();
        let b_before = ops[..start]
            .iter()
            .filter(|o| !matches!(o, Op::Del(_)))
            .count();
        let mut body = String::new();
        for op in hunk {
            match *op {
                Op::Same(i, j) => {
                    a_start.get_or_insert(i);
                    b_start.get_or_insert(j);
                    a_len += 1;
                    b_len += 1;
                    push_line(&mut body, ' ', a[i]);
                }
                Op::Del(i) => {
                    a_start.get_or_insert(i);
                    a_len += 1;
                    push_line(&mut body, '-', a[i]);
                }
                Op::Ins(j) => {
                    b_start.get_or_insert(j);
                    b_len += 1;
                    push_line(&mut body, '+', b[j]);
                }
            }
        }
        // An empty side is reported as starting after the line before it (`-0,0` for a new
        // file), as diff(1) does.
        let range = |first: Option<usize>, before: usize, len: usize| match (first, len) {
            (_, 0) => format!("{before},0"),
            (Some(f), 1) => format!("{}", f + 1),
            (Some(f), n) => format!("{},{n}", f + 1),
            (None, n) => format!("{},{n}", before + 1),
        };
        out.push_str(&format!(
            "@@ -{} +{} @@\n",
            range(a_start, a_before, a_len),
            range(b_start, b_before, b_len)
        ));
        out.push_str(&body);
        k += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_texts_have_no_diff() {
        assert_eq!(unified("a\n", "a\n", "a", "b", 3), "");
    }

    #[test]
    fn a_changed_line_with_context() {
        let old = "1\n2\n3\n4\n5\n6\n7\n8\n9\n";
        let new = "1\n2\n3\n4\nfive\n6\n7\n8\n9\n";
        assert_eq!(
            unified(old, new, "~/.rc", "rc", 3),
            "--- ~/.rc\n+++ rc\n@@ -2,7 +2,7 @@\n 2\n 3\n 4\n-5\n+five\n 6\n 7\n 8\n"
        );
    }

    #[test]
    fn new_and_emptied_files() {
        assert_eq!(
            unified("", "a\nb\n", "old", "new", 3),
            "--- old\n+++ new\n@@ -0,0 +1,2 @@\n+a\n+b\n"
        );
        assert_eq!(
            unified("a\n", "", "old", "new", 3),
            "--- old\n+++ new\n@@ -1 +0,0 @@\n-a\n"
        );
    }

    #[test]
    fn distant_changes_make_separate_hunks_and_a_missing_newline_shows() {
        let old: String = (1..=20).map(|i| format!("{i}\n")).collect();
        let new: String = (1..=20)
            .map(|i| match i {
                2 => "two\n".to_string(),
                19 => "nineteen\n".to_string(),
                _ => format!("{i}\n"),
            })
            .collect::<String>()
            + "end";
        let d = unified(&old, &new, "a", "b", 1);
        assert_eq!(
            d,
            "--- a\n+++ b\n@@ -1,3 +1,3 @@\n 1\n-2\n+two\n 3\n@@ -18,3 +18,4 @@\n 18\n-19\n+nineteen\n 20\n+end\n\\ No newline at end of file\n"
        );
    }
}
