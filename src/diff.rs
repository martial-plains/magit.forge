// SPDX-License-Identifier: GPL-3.0-or-later
//! Diffs: splitting them into hunks, turning a hunk (or some lines of
//! one) back into a patch `git apply` takes, finding where a diff line
//! is in the file, and the faces that colour a diff.
//!
//! Lines are split on `\n` only, so a CRLF file's `\r` stays part of the
//! line and its hunks apply exactly as diff printed them.

/// Options for every `git diff` whose output is applied or shown: plain
/// a/ b/ prefixes whatever `diff.noprefix` or `diff.mnemonicPrefix` say,
/// and no external diff or textconv (whose output isn't a patch).
pub const DIFF_ARGS: &[&str] = &[
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--src-prefix=a/",
    "--dst-prefix=b/",
];

/// One hunk: its `@@` header and body lines (` `/`+`/`-`, and `\ No
/// newline` markers, which `git apply` needs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub header: String,
    pub body: Vec<String>,
}

impl Hunk {
    /// A combined diff's hunk (`@@@`, a merge conflict): shown, not applied.
    pub fn combined(&self) -> bool {
        self.header.starts_with("@@@")
    }

    /// Where the new side starts (the `+c` of `@@ -a,b +c,d @@`).
    pub fn new_start(&self) -> usize {
        let marker = if self.combined() { "@@@" } else { "@@" };
        self.header
            .trim_start_matches(marker)
            .split_whitespace()
            .rfind(|w| w.starts_with('+'))
            .and_then(|w| w[1..].split(',').next())
            .and_then(|n| n.parse().ok())
            .unwrap_or(1)
    }

    /// The file line (1-based) that body line `i` is at, or the next
    /// one for a removed line. None for a header.
    pub fn file_line(&self, i: Option<usize>) -> usize {
        let start = self.new_start();
        let Some(i) = i else { return start };
        let cols = if self.combined() { 2 } else { 1 };
        let added = self.body[..i.min(self.body.len())]
            .iter()
            .filter(|l| {
                let prefix: String = l.chars().take(cols).collect();
                !l.starts_with('\\') && !prefix.contains('-')
            })
            .count();
        start + added
    }
}

/// A file's diff: the lines before its first hunk, and the hunks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileDiff {
    pub header: Vec<String>,
    pub hunks: Vec<Hunk>,
    /// Binary, mode-only, or otherwise hunkless: what git said instead.
    pub note: Option<String>,
}

fn split_lines(s: &str) -> impl Iterator<Item = &str> {
    let s = s.strip_suffix('\n').unwrap_or(s);
    s.split('\n').filter(move |_| !s.is_empty())
}

/// Parse one file's diff (`git diff -- path`).
pub fn parse_file_diff(diff: &str) -> FileDiff {
    let mut d = FileDiff::default();
    for line in split_lines(diff) {
        if line.starts_with("@@") {
            d.hunks.push(Hunk {
                header: line.to_string(),
                body: Vec::new(),
            });
        } else if let Some(h) = d.hunks.last_mut() {
            h.body.push(line.to_string());
        } else {
            d.header.push(line.to_string());
        }
    }
    if d.hunks.is_empty() {
        d.note = d
            .header
            .iter()
            .find(|l| {
                l.starts_with("Binary files")
                    || l.starts_with("old mode")
                    || l.starts_with("new mode")
                    || l.starts_with("Submodule")
                    || l.starts_with("similarity index")
            })
            .cloned();
    }
    d
}

/// The header lines `git apply` needs ahead of a hunk: everything
/// before the first hunk, except `index` lines can stay (they only help).
fn patch_head(header: &[String]) -> String {
    let mut out = String::new();
    for l in header {
        out.push_str(l);
        out.push('\n');
    }
    out
}

/// A whole hunk as a patch applying on its own.
pub fn hunk_patch(header: &[String], hunk: &Hunk) -> String {
    let mut patch = patch_head(header);
    patch.push_str(&hunk.header);
    patch.push('\n');
    for line in &hunk.body {
        patch.push_str(line);
        patch.push('\n');
    }
    patch
}

/// Does the hunk have no context (diff.context=0)? Then `git apply`
/// needs `--unidiff-zero`.
pub fn zero_context(patch: &str) -> bool {
    let mut in_hunk = false;
    for l in patch.split('\n') {
        if l.starts_with("@@") {
            in_hunk = true;
        } else if in_hunk && l.starts_with(' ') {
            return false;
        }
    }
    true
}

/// Only the body lines `from..=to` of a hunk as a patch: the rest of
/// the hunk's changes are left out, so they stay where they are.
///
/// Applied forward (stage), a left-out removal is still in the target,
/// so it becomes context, and a left-out addition is dropped. Applied in
/// reverse (unstage, discard), the target has the new side: a left-out
/// addition becomes context and a left-out removal is dropped.
/// None when the lines hold no change.
pub fn partial_patch(
    header: &[String],
    hunk: &Hunk,
    from: usize,
    to: usize,
    reverse: bool,
) -> Option<String> {
    let mut body: Vec<String> = Vec::new();
    let mut changed = false;
    // Whether the line before a "\ No newline" marker was kept.
    let mut kept_last = true;
    for (i, line) in hunk.body.iter().enumerate() {
        let selected = (from..=to).contains(&i);
        let (keep, text) = match line.chars().next() {
            Some('\\') => (kept_last, line.clone()),
            Some('+') if selected => {
                changed = true;
                (true, line.clone())
            }
            Some('-') if selected => {
                changed = true;
                (true, line.clone())
            }
            Some('+') if reverse => (true, format!(" {}", &line[1..])),
            Some('-') if !reverse => (true, format!(" {}", &line[1..])),
            Some('+') | Some('-') => (false, String::new()),
            _ => (true, line.clone()),
        };
        if !line.starts_with('\\') {
            kept_last = keep;
        }
        if keep {
            body.push(text);
        }
    }
    if !changed {
        return None;
    }
    let old = body
        .iter()
        .filter(|l| l.starts_with(' ') || l.starts_with('-'))
        .count();
    let new = body
        .iter()
        .filter(|l| l.starts_with(' ') || l.starts_with('+'))
        .count();
    let (old_start, new_start) = hunk_starts(&hunk.header);
    let mut patch = patch_head(header);
    patch.push_str(&format!(
        "@@ -{},{} +{},{} @@\n",
        if old == 0 { old_start.saturating_sub(1) } else { old_start },
        old,
        if new == 0 { new_start.saturating_sub(1) } else { new_start },
        new
    ));
    for l in body {
        patch.push_str(&l);
        patch.push('\n');
    }
    Some(patch)
}

/// The `-a` and `+c` of a hunk header.
fn hunk_starts(header: &str) -> (usize, usize) {
    let mut old = 1;
    let mut new = 1;
    for w in header.trim_start_matches('@').split_whitespace() {
        let n = |w: &str| w[1..].split(',').next().and_then(|n| n.parse().ok());
        if w.starts_with('-') {
            old = n(w).unwrap_or(1);
        } else if w.starts_with('+') {
            new = n(w).unwrap_or(1);
            break;
        }
    }
    (old, new)
}

// ------------------------------------------------------------ diff views

/// Where each line of a multi-file diff (git show, git diff) points:
/// the file and line RET visits. None for lines that point nowhere.
pub fn diff_targets(text: &str) -> Vec<Option<(String, usize)>> {
    let mut out = Vec::new();
    let mut path: Option<String> = None;
    let mut hunk: Option<Hunk> = None;
    let mut body_index = 0;
    for line in split_lines(text) {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            // Until +++ says otherwise: the b/ side.
            path = rest
                .rsplit_once(" b/")
                .map(|(_, p)| p.to_string());
            hunk = None;
            out.push(path.clone().map(|p| (p, 1)));
            continue;
        }
        if let Some(p) = line.strip_prefix("+++ ") {
            if let Some(p) = p.strip_prefix("b/") {
                path = Some(p.to_string());
            }
            out.push(path.clone().map(|p| (p, 1)));
            continue;
        }
        if line.starts_with("@@") {
            let h = Hunk {
                header: line.to_string(),
                body: Vec::new(),
            };
            out.push(path.clone().map(|p| (p, h.new_start())));
            hunk = Some(h);
            body_index = 0;
            continue;
        }
        match (&mut hunk, &path) {
            (Some(h), Some(p)) if body_line(line) => {
                h.body.push(line.to_string());
                out.push(Some((p.clone(), h.file_line(Some(body_index)))));
                body_index += 1;
            }
            _ => {
                hunk = None;
                out.push(None);
            }
        }
    }
    out
}

/// The face a diff line gets, if any. `combined`: in a `@@@` hunk,
/// where the first two columns are the markers.
pub fn line_face(line: &str, in_hunk: bool, combined: bool) -> Option<&'static str> {
    if line.starts_with("@@") {
        return Some("constant");
    }
    if !in_hunk {
        return if line.starts_with("diff --git")
            || line.starts_with("diff --cc")
            || line.starts_with("+++ ")
            || line.starts_with("--- ")
        {
            Some("keyword")
        } else if line.starts_with("commit ") {
            Some("type")
        } else {
            None
        };
    }
    if line.starts_with('\\') {
        return Some("comment");
    }
    let marks: String = line.chars().take(if combined { 2 } else { 1 }).collect();
    if combined
        && (line.starts_with("++<<<<<<<")
            || line.starts_with("++=======")
            || line.starts_with("++>>>>>>>")
            || line.starts_with("++|||||||"))
    {
        Some("warning")
    } else if marks.contains('+') {
        Some("string")
    } else if marks.contains('-') {
        Some("builtin")
    } else {
        None
    }
}

/// Is `line` part of a hunk's body?
fn body_line(line: &str) -> bool {
    line.starts_with(' ') || line.starts_with('+') || line.starts_with('-') || line.starts_with('\\')
}

/// Faces for a whole diff text: (line, face) pairs.
pub fn diff_faces(text: &str) -> Vec<(usize, &'static str)> {
    let mut out = Vec::new();
    let mut in_hunk = false;
    let mut combined = false;
    for (i, line) in split_lines(text).enumerate() {
        if line.starts_with("@@") {
            in_hunk = true;
            combined = line.starts_with("@@@");
        } else if in_hunk && !body_line(line) {
            in_hunk = false;
        }
        if let Some(f) = line_face(line, in_hunk, combined) {
            out.push((i, f));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "diff --git a/f b/f\nindex 111..222 100644\n--- a/f\n+++ b/f\n@@ -1,4 +1,4 @@\n ctx\n-old\n+new\n+more\n ctx2\n@@ -9,1 +10,1 @@\n tail\n";

    #[test]
    fn file_diff_splits_header_and_hunks() {
        let d = parse_file_diff(DIFF);
        assert_eq!(d.header.len(), 4);
        assert_eq!(d.hunks.len(), 2);
        assert_eq!(d.hunks[0].body, [" ctx", "-old", "+new", "+more", " ctx2"]);
        let p = hunk_patch(&d.header, &d.hunks[1]);
        assert!(p.starts_with("diff --git a/f b/f\n"));
        assert!(p.ends_with("@@ -9,1 +10,1 @@\n tail\n"));
    }

    #[test]
    fn crlf_lines_keep_their_carriage_returns() {
        let d = parse_file_diff("--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n x\r\n-y\r\n+z\r\n");
        assert_eq!(d.hunks[0].body, [" x\r", "-y\r", "+z\r"]);
        assert!(hunk_patch(&d.header, &d.hunks[0]).contains("-y\r\n+z\r\n"));
    }

    #[test]
    fn hunkless_diffs_say_why() {
        let d = parse_file_diff("diff --git a/b b/b\nindex 1..2\nBinary files a/b and b/b differ\n");
        assert!(d.hunks.is_empty());
        assert_eq!(d.note.as_deref(), Some("Binary files a/b and b/b differ"));
    }

    #[test]
    fn file_lines_follow_the_new_side() {
        let d = parse_file_diff(DIFF);
        let h = &d.hunks[0];
        assert_eq!(h.new_start(), 1);
        assert_eq!(h.file_line(None), 1);
        assert_eq!(h.file_line(Some(0)), 1, "ctx");
        assert_eq!(h.file_line(Some(1)), 2, "a removed line points at what follows");
        assert_eq!(h.file_line(Some(2)), 2, "new");
        assert_eq!(h.file_line(Some(3)), 3, "more");
        assert_eq!(h.file_line(Some(4)), 4, "ctx2");
        assert_eq!(d.hunks[1].new_start(), 10);
    }

    #[test]
    fn partial_patch_stages_only_the_selected_lines() {
        let d = parse_file_diff(DIFF);
        // Stage just "+new" (body line 2).
        let p = partial_patch(&d.header, &d.hunks[0], 2, 2, false).unwrap();
        assert!(p.ends_with("@@ -1,3 +1,4 @@\n ctx\n old\n+new\n ctx2\n"), "{p}");
        // Unstage just "-old" (body line 1), reverse: additions become context.
        let p = partial_patch(&d.header, &d.hunks[0], 1, 1, true).unwrap();
        assert!(p.ends_with("@@ -1,5 +1,4 @@\n ctx\n-old\n new\n more\n ctx2\n"), "{p}");
        // Only context selected: nothing to do.
        assert_eq!(partial_patch(&d.header, &d.hunks[0], 0, 0, false), None);
    }

    #[test]
    fn partial_patch_applies_with_real_git() {
        // Exercise against git itself, when it's there.
        if std::process::Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("magit-partial-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str], input: Option<&str>| {
            use std::io::Write;
            let mut c = std::process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .current_dir(&dir)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            if let Some(i) = input {
                c.stdin.take().unwrap().write_all(i.as_bytes()).unwrap();
            }
            let o = c.wait_with_output().unwrap();
            assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
            String::from_utf8(o.stdout).unwrap()
        };
        git(&["init", "-q"], None);
        std::fs::write(dir.join("f"), "a\nb\nc\nd\r\n").unwrap();
        git(&["add", "f"], None);
        git(&["commit", "-qm", "one"], None);
        std::fs::write(dir.join("f"), "a\nB\nc\nD\r\nnew\n").unwrap();
        let mut args = vec!["diff"];
        args.extend(DIFF_ARGS);
        let d = parse_file_diff(&git(&args, None));
        assert_eq!(d.hunks.len(), 1);
        // Stage the first change only (lines "-b", "+B").
        let first_minus = d.hunks[0].body.iter().position(|l| l == "-b").unwrap();
        let p = partial_patch(&d.header, &d.hunks[0], first_minus, first_minus + 1, false).unwrap();
        git(&["apply", "--cached"], Some(&p));
        let staged = git(&["diff", "--cached", "--no-color"], None);
        assert!(staged.contains("-b\n+B\n"), "{staged}");
        assert!(!staged.contains("+new"), "{staged}");
        // Unstage it again, from the staged diff, line by line.
        let mut args = vec!["diff", "--cached"];
        args.extend(DIFF_ARGS);
        let s = parse_file_diff(&git(&args, None));
        let p = hunk_patch(&s.header, &s.hunks[0]);
        git(&["apply", "--cached", "--reverse"], Some(&p));
        assert_eq!(git(&["diff", "--cached"], None), "");
        let d = parse_file_diff(&git(&{
            let mut a = vec!["diff"];
            a.extend(DIFF_ARGS);
            a
        }, None));
        git(&["apply", "--cached"], Some(&hunk_patch(&d.header, &d.hunks[0])));
        assert_eq!(git(&["diff"], None), "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zero_context_is_detected() {
        assert!(zero_context("@@ -1 +1 @@\n-a\n+b\n"));
        assert!(!zero_context("@@ -1,2 +1,2 @@\n x\n-a\n+b\n"));
    }

    #[test]
    fn diff_targets_point_at_new_lines() {
        let text = "commit abc\nAuthor: A\n\n    msg\n\ndiff --git a/x.rs b/x.rs\nindex 1..2\n--- a/x.rs\n+++ b/x.rs\n@@ -3,2 +3,3 @@\n keep\n+add\n keep2\n";
        let t = diff_targets(text);
        assert_eq!(t[0], None);
        assert_eq!(t[5], Some(("x.rs".into(), 1)));
        assert_eq!(t[9], Some(("x.rs".into(), 3)));
        assert_eq!(t[10], Some(("x.rs".into(), 3)));
        assert_eq!(t[11], Some(("x.rs".into(), 4)));
        assert_eq!(t[12], Some(("x.rs".into(), 5)));
        assert_eq!(t.len(), 13);
    }

    #[test]
    fn faces_colour_diff_lines() {
        let f = diff_faces("diff --git a/x b/x\n@@ -1 +1 @@\n-a\n+b\n ctx\n - item\n");
        assert_eq!(f, [(0, "keyword"), (1, "constant"), (2, "builtin"), (3, "string")]);
        let f = diff_faces("@@@ -1 -1 +1 @@@\n++<<<<<<< HEAD\n +ours\n- old\n  ctx\n");
        assert_eq!(f, [(0, "constant"), (1, "warning"), (2, "string"), (3, "builtin")]);
    }

    #[test]
    fn combined_hunks_for_conflicts() {
        let d = parse_file_diff("diff --cc f\n@@@ -1,1 -1,1 +1,5 @@@\n++<<<<<<< HEAD\n +ours\n++=======\n+ theirs\n++>>>>>>> side\n");
        assert!(d.hunks[0].combined());
        assert_eq!(d.hunks[0].new_start(), 1);
        assert_eq!(d.hunks[0].file_line(Some(1)), 2);
    }
}
