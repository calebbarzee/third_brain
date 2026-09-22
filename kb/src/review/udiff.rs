//! a small line-based diff producing unified hunks (3 lines of context), with no new dependency.
//! used to build the three diffs a conflict resolve packet needs (design.md §14); review packet
//! diffs come from `git diff` text directly instead, since those are real commit-to-commit diffs.
//!
//! the algorithm is a textbook lcs over lines (O(n*m) time and space), which is adequate here:
//! callers only ever diff one document's text, and document sizes are bounded by the layer word
//! budgets in manifest.toml (a few thousand words at most).

/// one line of a hunk body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLine {
    Context(String),
    Remove(String),
    Add(String),
}

/// one unified-diff hunk. `old_start`/`new_start` are 1-based, per the unified diff format; a
/// hunk with `old_len == 0` (pure insertion) or `new_len == 0` (pure deletion) uses the
/// insertion-point convention: the start line is the count of old (or new) lines consumed
/// before the hunk, which may be `0` at the very start of a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: usize,
    pub old_len: usize,
    pub new_start: usize,
    pub new_len: usize,
    pub lines: Vec<DiffLine>,
}

impl Hunk {
    /// the `@@ -a,b +c,d @@` header line, with no trailing newline.
    pub fn header(&self) -> String {
        format!("@@ -{},{} +{},{} @@", self.old_start, self.old_len, self.new_start, self.new_len)
    }
}

#[derive(Debug, Clone, Copy)]
enum Op<'a> {
    Same(&'a str),
    Remove(&'a str),
    Add(&'a str),
}

fn split_lines(s: &str) -> Vec<&str> {
    if s.is_empty() { Vec::new() } else { s.lines().collect() }
}

/// minimal edit script turning `old` into `new`, as a sequence of same/remove/add ops in order.
fn lcs_ops<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<Op<'a>> {
    let n = old.len();
    let m = new.len();
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if old[i] == new[j] { dp[i + 1][j + 1] + 1 } else { dp[i + 1][j].max(dp[i][j + 1]) };
        }
    }
    let mut ops = Vec::with_capacity(n + m);
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if old[i] == new[j] {
            ops.push(Op::Same(old[i]));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            ops.push(Op::Remove(old[i]));
            i += 1;
        } else {
            ops.push(Op::Add(new[j]));
            j += 1;
        }
    }
    while i < n {
        ops.push(Op::Remove(old[i]));
        i += 1;
    }
    while j < m {
        ops.push(Op::Add(new[j]));
        j += 1;
    }
    ops
}

/// group changed ops into hunks with `context` lines of surrounding same-lines, merging groups
/// whose gap is small enough that their context windows would overlap.
fn build_hunks(ops: &[Op], context: usize) -> Vec<Hunk> {
    let change_idxs: Vec<usize> = ops.iter().enumerate().filter(|(_, o)| !matches!(o, Op::Same(_))).map(|(i, _)| i).collect();
    if change_idxs.is_empty() {
        return Vec::new();
    }

    let mut groups: Vec<(usize, usize)> = Vec::new();
    let mut start = change_idxs[0];
    let mut end = change_idxs[0];
    for &idx in &change_idxs[1..] {
        if idx - end <= 2 * context {
            end = idx;
        } else {
            groups.push((start, end));
            start = idx;
            end = idx;
        }
    }
    groups.push((start, end));

    // prefix counts of old-consumed and new-consumed lines, so a hunk's start line reflects
    // exactly how many lines of each side preceded it, per the unified diff convention.
    let mut old_consumed = vec![0usize; ops.len() + 1];
    let mut new_consumed = vec![0usize; ops.len() + 1];
    for (idx, op) in ops.iter().enumerate() {
        old_consumed[idx + 1] = old_consumed[idx] + usize::from(!matches!(op, Op::Add(_)));
        new_consumed[idx + 1] = new_consumed[idx] + usize::from(!matches!(op, Op::Remove(_)));
    }

    let mut hunks = Vec::with_capacity(groups.len());
    for (start, end) in groups {
        let from = start.saturating_sub(context);
        let to = (end + context + 1).min(ops.len());
        let slice = &ops[from..to];

        let mut lines = Vec::with_capacity(slice.len());
        let (mut old_len, mut new_len) = (0usize, 0usize);
        for op in slice {
            match op {
                Op::Same(s) => {
                    lines.push(DiffLine::Context((*s).to_string()));
                    old_len += 1;
                    new_len += 1;
                }
                Op::Remove(s) => {
                    lines.push(DiffLine::Remove((*s).to_string()));
                    old_len += 1;
                }
                Op::Add(s) => {
                    lines.push(DiffLine::Add((*s).to_string()));
                    new_len += 1;
                }
            }
        }
        let old_start = if old_len > 0 { old_consumed[from] + 1 } else { old_consumed[from] };
        let new_start = if new_len > 0 { new_consumed[from] + 1 } else { new_consumed[from] };
        hunks.push(Hunk { old_start, old_len, new_start, new_len, lines });
    }
    hunks
}

/// unified-diff hunks between `old` and `new`, both split on `\n`, with 3 lines of context.
/// empty when the two texts are identical.
pub fn diff_lines(old: &str, new: &str) -> Vec<Hunk> {
    if old == new {
        return Vec::new();
    }
    let old_lines = split_lines(old);
    let new_lines = split_lines(new);
    let ops = lcs_ops(&old_lines, &new_lines);
    build_hunks(&ops, 3)
}

/// render a full unified diff (`--- `/`+++ ` labels, then each hunk) as text, matching the shape
/// (not byte-for-byte output) of `diff -u`. empty string when the two texts are identical.
pub fn render_unified(old_label: &str, new_label: &str, old: &str, new: &str) -> String {
    let hunks = diff_lines(old, new);
    if hunks.is_empty() {
        return String::new();
    }
    let mut out = format!("--- {old_label}\n+++ {new_label}\n");
    for h in &hunks {
        out.push_str(&h.header());
        out.push('\n');
        for l in &h.lines {
            match l {
                DiffLine::Context(s) => {
                    out.push(' ');
                    out.push_str(s);
                }
                DiffLine::Remove(s) => {
                    out.push('-');
                    out.push_str(s);
                }
                DiffLine::Add(s) => {
                    out.push('+');
                    out.push_str(s);
                }
            }
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_text_has_no_hunks() {
        assert!(diff_lines("a\nb\nc\n", "a\nb\nc\n").is_empty());
        assert_eq!(render_unified("old", "new", "a\n", "a\n"), "");
    }

    #[test]
    fn single_line_change_produces_one_hunk_with_context() {
        let old = "a\nb\nc\nd\ne\n";
        let new = "a\nb\nX\nd\ne\n";
        let hunks = diff_lines(old, new);
        assert_eq!(hunks.len(), 1);
        let h = &hunks[0];
        assert!(h.lines.contains(&DiffLine::Remove("c".to_string())));
        assert!(h.lines.contains(&DiffLine::Add("X".to_string())));
        // full file fits inside one 3-line-context window, so every line is present.
        assert_eq!(h.lines.len(), 6);
        assert_eq!(h.old_len, 5);
        assert_eq!(h.new_len, 5);
    }

    #[test]
    fn pure_addition_into_an_empty_file_uses_zero_length_old_side() {
        // with no old lines at all to pull in as context, the hunk is a pure insertion: the
        // unified-diff convention for that is `old_len == 0` and `old_start == 0`.
        let hunks = diff_lines("", "x\n");
        assert_eq!(hunks.len(), 1);
        let h = &hunks[0];
        assert_eq!(h.old_len, 0);
        assert_eq!(h.old_start, 0);
        assert_eq!(h.new_start, 1);
        assert_eq!(h.new_len, 1);
    }

    #[test]
    fn addition_with_trailing_context_counts_that_context_on_the_old_side_too() {
        // real diff tools fold the same trailing lines into both sides' hunk ranges; a pure
        // insertion only has `old_len == 0` when there is no context left to pull in (see the
        // test above).
        let old = "a\nb\n";
        let new = "x\na\nb\n";
        let hunks = diff_lines(old, new);
        assert_eq!(hunks.len(), 1);
        let h = &hunks[0];
        assert_eq!(h.old_start, 1);
        assert_eq!(h.old_len, 2);
        assert_eq!(h.new_start, 1);
        assert_eq!(h.new_len, 3);
        assert_eq!(h.lines[0], DiffLine::Add("x".to_string()));
    }

    #[test]
    fn two_far_apart_changes_produce_two_hunks() {
        let old_lines: Vec<String> = (0..40).map(|i| format!("line{i}")).collect();
        let mut new_lines = old_lines.clone();
        new_lines[2] = "changed-early".to_string();
        new_lines[35] = "changed-late".to_string();
        let old = old_lines.join("\n") + "\n";
        let new = new_lines.join("\n") + "\n";
        let hunks = diff_lines(&old, &new);
        assert_eq!(hunks.len(), 2, "changes far enough apart must not merge into one hunk");
    }

    #[test]
    fn render_unified_matches_diff_u_shape() {
        let old = "keep1\nkeep2\nremoved\nkeep3\nkeep4\n";
        let new = "keep1\nkeep2\nadded\nkeep3\nkeep4\n";
        let text = render_unified("a", "b", old, new);
        assert!(text.starts_with("--- a\n+++ b\n"));
        assert!(text.contains("@@ -"));
        assert!(!text.contains("@@ +")); // header is one line, not two separate markers
        assert!(text.lines().any(|l| l == "-removed"));
        assert!(text.lines().any(|l| l == "+added"));
        assert!(text.lines().any(|l| l == " keep1"));
    }
}
