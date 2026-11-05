// SPDX-License-Identifier: GPL-3.0-or-later
//! Read-only view buffers: logs, commits, stashes, diffs, blame, the
//! process log and the key help. Each line may point at a commit (RET
//! shows it) or a file line (RET visits it).

use crate::diff::{self, DIFF_ARGS};
use crate::git::{self, LOG_FORMAT, LogEntry, Run};
use forge_plugin_sdk::{Buffer, Decoration, Position, Span, decorations, message};
use std::cell::RefCell;
use std::collections::HashMap;

/// What RET on a view line does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Commit(String),
    File(String, usize),
}

/// What kind of view a buffer is, so `g` knows how to redo it.
#[derive(Debug, Clone)]
pub enum Source {
    /// Revisions, options, paths.
    Log(Vec<String>, Vec<String>, Vec<String>),
    Show(String),
    Stash(String),
    Diff(String, Vec<String>),
    Blame(String),
    Process,
    Text,
}

#[derive(Debug, Clone)]
pub struct View {
    pub root: String,
    pub source: Source,
    pub targets: Vec<Option<Target>>,
}

thread_local! {
    static VIEWS: RefCell<HashMap<u32, View>> = RefCell::new(HashMap::new());
}

pub fn view(buffer: u32) -> Option<View> {
    VIEWS.with(|v| v.borrow().get(&buffer).cloned())
}

pub fn forget(buffer: u32) {
    VIEWS.with(|v| v.borrow_mut().remove(&buffer));
}

/// The repository a view buffer belongs to.
pub fn root_of(buffer: u32) -> Option<String> {
    view(buffer).map(|v| v.root)
}

/// What the line at `row` of a view points at.
pub fn target(buffer: u32, row: usize) -> Option<Target> {
    view(buffer)?.targets.get(row).cloned().flatten()
}

/// The commit at `row`, if the line names one.
pub fn commit_at(buffer: u32, row: usize) -> Option<String> {
    match target(buffer, row)? {
        Target::Commit(h) => Some(h),
        Target::File(..) => None,
    }
}

/// Fill `name` with `text` in `mode`, read-only, and show it.
fn show_buffer(
    name: &str,
    mode: &str,
    text: &str,
    faces: &[(usize, usize, usize, &'static str)],
    view: View,
    keep_point: bool,
) {
    let b = match Buffer::find(name) {
        Some(b) => b,
        None => Buffer::create(name),
    };
    let row = if keep_point { b.point().line } else { 0 };
    let _ = b.set_major_mode(mode);
    let _ = b.set_option("read-only", "true");
    b.replace(
        Span {
            start: Position { line: 0, column: 0 },
            end: Position {
                line: u32::MAX,
                column: u32::MAX,
            },
        },
        text,
    );
    let decos: Vec<Decoration> = faces
        .iter()
        .map(|(l, a, z, f)| Decoration {
            span: Span {
                start: Position {
                    line: *l as u32,
                    column: *a as u32,
                },
                end: Position {
                    line: *l as u32,
                    column: *z as u32,
                },
            },
            face: f.to_string(),
            message: None,
        })
        .collect();
    let _ = decorations::set(&b, &decos);
    b.set_point(Position { line: row, column: 0 });
    VIEWS.with(|v| v.borrow_mut().insert(b.id(), view));
    b.show();
}

fn whole_line_faces(text: &str, faces: Vec<(usize, &'static str)>) -> Vec<(usize, usize, usize, &'static str)> {
    let lines: Vec<&str> = text.split('\n').collect();
    faces
        .into_iter()
        .map(|(i, f)| (i, 0, lines.get(i).map(|l| l.chars().count()).unwrap_or(0), f))
        .collect()
}

fn short(root: &str) -> &str {
    root.rsplit('/').find(|p| !p.is_empty()).unwrap_or(root)
}

// -------------------------------------------------------------------- log

/// Render a log: a title, then one line per commit.
/// A face on part of a line: (line, start column, end column, face).
pub type Face = (usize, usize, usize, &'static str);

pub fn render_log(title: &str, entries: &[LogEntry]) -> (String, Vec<Option<Target>>, Vec<Face>) {
    let mut text = format!("{title}\n\n");
    let mut targets = vec![None, None];
    let mut faces = vec![(0, 0, title.chars().count(), "keyword")];
    for (i, e) in entries.iter().enumerate() {
        let row = i + 2;
        let refs = if e.refs.is_empty() {
            String::new()
        } else {
            format!("({}) ", e.refs)
        };
        let mut col = 0;
        let mut piece = |s: &str, face: Option<&'static str>, text: &mut String| {
            let n = s.chars().count();
            if let Some(f) = face
                && n > 0
            {
                faces.push((row, col, col + n, f));
            }
            col += n;
            text.push_str(s);
        };
        piece(&e.short, Some("comment"), &mut text);
        piece(" ", None, &mut text);
        piece(&refs, Some("function"), &mut text);
        piece(&e.subject, None, &mut text);
        piece(&format!("  {} {}", e.author, e.date), Some("comment"), &mut text);
        text.push('\n');
        targets.push(Some(Target::Commit(e.hash.clone())));
    }
    if entries.is_empty() {
        text.push_str("(no commits)\n");
        targets.push(None);
    }
    (text, targets, faces)
}

/// Show the log of `revs` (empty: HEAD) with extra `args` (-n 256,
/// --no-merges, ...), limited to `paths` if any.
pub fn show_log(root: &str, revs: &[String], args: &[String], paths: &[String], keep_point: bool) {
    let mut argv: Vec<String> = vec!["log".into(), LOG_FORMAT.into(), "--date=short".into()];
    argv.extend(args.iter().cloned());
    argv.extend(revs.iter().cloned());
    argv.push("--".into());
    argv.extend(paths.iter().cloned());
    let mut what = if revs.is_empty() {
        "HEAD".to_string()
    } else {
        revs.join(" ")
    };
    if !paths.is_empty() {
        what.push_str(&format!(" touching {}", paths.join(" ")));
    }
    let root_c = root.to_string();
    let source = Source::Log(revs.to_vec(), args.to_vec(), paths.to_vec());
    git::exec(root, Run::owned(argv), move |out| {
        if !out.ok() {
            message(&format!("magit: log failed: {}", out.error()));
            return;
        }
        let entries = git::parse_log(&out.stdout);
        let title = format!("Commits in {what}");
        let (text, targets, faces) = render_log(&title, &entries);
        show_buffer(
            &format!("*magit-log: {}*", short(&root_c)),
            "magit-log-mode",
            &text,
            &faces,
            View {
                root: root_c.clone(),
                source: source.clone(),
                targets,
            },
            keep_point,
        );
    });
}

/// The reflog: where HEAD has been. RET shows a commit.
pub fn show_reflog(root: &str, entries: &[LogEntry]) {
    let (text, targets, faces) = render_log("Reflog of HEAD", entries);
    show_buffer(
        &format!("*magit-reflog: {}*", short(root)),
        "magit-log-mode",
        &text,
        &faces,
        View {
            root: root.to_string(),
            source: Source::Text,
            targets,
        },
        false,
    );
}

// ---------------------------------------------------------- diffs, shows

/// A view whose text is a diff (or a commit with one).
fn show_diff_text(root: &str, name: &str, text: &str, source: Source, keep_point: bool) {
    let mut targets: Vec<Option<Target>> = diff::diff_targets(text)
        .into_iter()
        .map(|t| t.map(|(p, l)| Target::File(p, l)))
        .collect();
    // "commit <hash>" lines show the commit.
    for (i, line) in text.split('\n').enumerate() {
        if let Some(h) = line.strip_prefix("commit ") {
            let h = h.split_whitespace().next().unwrap_or("").to_string();
            if let Some(t) = targets.get_mut(i) {
                *t = Some(Target::Commit(h));
            }
        }
    }
    let faces = whole_line_faces(text, diff::diff_faces(text));
    show_buffer(
        name,
        "magit-diff-mode",
        text,
        &faces,
        View {
            root: root.to_string(),
            source,
            targets,
        },
        keep_point,
    );
}

/// Show a commit (or anything `git show` takes).
pub fn show_commit(root: &str, rev: &str) {
    show_commit_at(root, rev, false);
}

fn show_commit_at(root: &str, rev: &str, keep_point: bool) {
    let mut args = vec!["show", "--format=fuller", "--stat", "--patch"];
    args.extend(DIFF_ARGS);
    args.push(rev);
    let (root_c, rev_c) = (root.to_string(), rev.to_string());
    git::exec(root, Run::new(&args), move |out| match out.result() {
        Ok(text) => show_diff_text(
            &root_c,
            &format!("*magit-revision: {}*", short_rev(&rev_c)),
            &text,
            Source::Show(rev_c.clone()),
            keep_point,
        ),
        Err(e) => message(&format!("magit: show failed: {e}")),
    });
}

fn short_rev(rev: &str) -> &str {
    if rev.len() > 12 && rev.chars().all(|c| c.is_ascii_hexdigit()) {
        &rev[..12]
    } else {
        rev
    }
}

/// Show a stash's changes.
pub fn show_stash(root: &str, name: &str) {
    show_stash_at(root, name, false);
}

fn show_stash_at(root: &str, name: &str, keep_point: bool) {
    let mut args = vec!["stash", "show", "--stat", "--patch"];
    args.extend(DIFF_ARGS);
    args.push(name);
    let (root_c, name_c) = (root.to_string(), name.to_string());
    git::exec(root, Run::new(&args), move |out| match out.result() {
        Ok(text) if text.trim().is_empty() => {
            message("The stash has no tracked changes (untracked files only?)")
        }
        Ok(text) => show_diff_text(
            &root_c,
            &format!("*magit-stash: {name_c}*"),
            &text,
            Source::Stash(name_c.clone()),
            keep_point,
        ),
        Err(e) => message(&format!("magit: stash show failed: {e}")),
    });
}

/// Show `git diff <args>` (args after the DIFF_ARGS).
pub fn show_diff(root: &str, title: &str, args: &[String]) {
    show_diff_at(root, title, args, false);
}

fn show_diff_at(root: &str, title: &str, args: &[String], keep_point: bool) {
    let mut argv: Vec<String> = vec!["diff".into()];
    argv.extend(DIFF_ARGS.iter().map(|s| s.to_string()));
    argv.extend(args.iter().cloned());
    let (root_c, title_c, args_c) = (root.to_string(), title.to_string(), args.to_vec());
    git::exec(root, Run::owned(argv), move |out| match out.result() {
        Ok(text) if text.trim().is_empty() => message(&format!("No differences ({title_c})")),
        Ok(text) => show_diff_text(
            &root_c,
            &format!("*magit-diff: {}*", short(&root_c)),
            &format!("{title_c}\n\n{text}"),
            Source::Diff(title_c.clone(), args_c.clone()),
            keep_point,
        ),
        Err(e) => message(&format!("magit: diff failed: {e}")),
    });
}

// ------------------------------------------------------------------ blame

/// Parse `git blame` output: each line's commit (None for uncommitted
/// lines).
pub fn blame_targets(text: &str) -> Vec<Option<Target>> {
    text.split('\n')
        .map(|l| {
            let hash = l.split_whitespace().next()?.trim_start_matches('^');
            (hash.chars().all(|c| c.is_ascii_hexdigit()) && !hash.chars().all(|c| c == '0'))
                .then(|| Target::Commit(hash.to_string()))
        })
        .collect()
}

/// Blame `path` (relative to `root`), with point on `line` (1-based).
pub fn show_blame(root: &str, path: &str, line: usize) {
    let (root_c, path_c) = (root.to_string(), path.to_string());
    git::exec(
        root,
        Run::new(&["blame", "--date=short", "--", path]),
        move |out| match out.result() {
            Ok(text) => {
                let targets = blame_targets(&text);
                let faces: Vec<(usize, usize, usize, &'static str)> = text
                    .split('\n')
                    .enumerate()
                    .filter_map(|(i, l)| {
                        let end = l.find(')')? + 1;
                        Some((i, 0, l[..end].chars().count(), "comment"))
                    })
                    .collect();
                let name = format!("*magit-blame: {path_c}*");
                show_buffer(
                    &name,
                    "magit-view-mode",
                    &text,
                    &faces,
                    View {
                        root: root_c.clone(),
                        source: Source::Blame(path_c.clone()),
                        targets,
                    },
                    false,
                );
                if let Some(b) = Buffer::find(&name) {
                    b.set_point(Position {
                        line: line.saturating_sub(1) as u32,
                        column: 0,
                    });
                }
            }
            Err(e) => message(&format!("magit: blame failed: {e}")),
        },
    );
}

// ------------------------------------------------------------- the rest

/// The process log (`$`): every git that changed something, and what it said.
pub fn show_process_log(root: &str) {
    let mut text = format!("Git commands run in {root}\n\n");
    let mut faces = vec![(0, 0, text.trim_end().chars().count(), "keyword")];
    for r in git::process_log(root) {
        let row = text.matches('\n').count();
        let cmd = format!("$ git {}", r.args.join(" "));
        faces.push((row, 0, cmd.chars().count(), "function"));
        text.push_str(&cmd);
        text.push('\n');
        match &r.output {
            None => text.push_str("  (running)\n"),
            Some(o) => {
                for l in o.stdout.lines().chain(o.stderr.lines()) {
                    text.push_str("  ");
                    text.push_str(l);
                    text.push('\n');
                }
                if !o.ok() {
                    let row = text.matches('\n').count();
                    let s = format!("  [exit {}]", o.code.map(|c| c.to_string()).unwrap_or("signal".into()));
                    faces.push((row, 0, s.chars().count(), "builtin"));
                    text.push_str(&s);
                    text.push('\n');
                }
            }
        }
        text.push('\n');
    }
    let n = text.matches('\n').count();
    show_buffer(
        &format!("*magit-process: {}*", short(root)),
        "magit-view-mode",
        &text,
        &faces,
        View {
            root: root.to_string(),
            source: Source::Process,
            targets: vec![None; n + 1],
        },
        false,
    );
    if let Some(b) = Buffer::find(&format!("*magit-process: {}*", short(root))) {
        b.set_point(Position {
            line: n.saturating_sub(1) as u32,
            column: 0,
        });
    }
}

/// The refs list (`y`): RET on a ref shows its commit.
pub fn show_refs_text(root: &str, text: &str) {
    let mut targets = Vec::new();
    let mut faces = Vec::new();
    for (i, line) in text.split('\n').enumerate() {
        let words: Vec<&str> = line.split_whitespace().collect();
        let hash = match words.first() {
            Some(&"*") => words.get(1),
            _ if line.starts_with("  ") => words.first(),
            _ => None,
        };
        match hash {
            Some(h) => {
                targets.push(Some(Target::Commit(h.to_string())));
                if let Some(at) = line.find(h) {
                    let a = line[..at].chars().count();
                    faces.push((i, a, a + h.chars().count(), "comment"));
                }
            }
            None => {
                targets.push(None);
                if !line.is_empty() {
                    faces.push((i, 0, line.chars().count(), "keyword"));
                }
            }
        }
    }
    show_buffer(
        &format!("*magit-refs: {}*", short(root)),
        "magit-log-mode",
        text,
        &faces,
        View {
            root: root.to_string(),
            source: Source::Text,
            targets,
        },
        false,
    );
}

/// Show plain text (help, lists) in a view buffer.
pub fn show_text(root: &str, name: &str, text: &str) {
    let first = text.lines().next().unwrap_or("").chars().count();
    show_buffer(
        name,
        "magit-view-mode",
        text,
        &[(0, 0, first, "keyword")],
        View {
            root: root.to_string(),
            source: Source::Text,
            targets: Vec::new(),
        },
        false,
    );
}

/// `g` in a view: run it again, keeping point.
pub fn refresh(buffer: u32) -> bool {
    let Some(v) = view(buffer) else {
        return false;
    };
    match &v.source {
        Source::Log(revs, args, paths) => show_log(&v.root, revs, args, paths, true),
        Source::Show(rev) => show_commit_at(&v.root, rev, true),
        Source::Stash(name) => show_stash_at(&v.root, name, true),
        Source::Diff(title, args) => show_diff_at(&v.root, title, args, true),
        Source::Blame(path) => {
            let line = Buffer::from_id(buffer).map(|b| b.point().line as usize + 1).unwrap_or(1);
            show_blame(&v.root, path, line)
        }
        Source::Process => show_process_log(&v.root),
        Source::Text => {}
    }
    true
}

/// n/p in a view: the next line that points somewhere new (a commit, a
/// hunk, a file).
pub fn next_stop(buffer: u32, by: isize) -> bool {
    let Some(b) = Buffer::from_id(buffer) else {
        return false;
    };
    let Some(v) = view(buffer) else { return false };
    let row = b.point().line as isize;
    let lines: Vec<String> = (0..b.line_count()).map(|i| b.line(i)).collect();
    let stop = |i: usize| -> bool {
        let line = lines.get(i).map(String::as_str).unwrap_or("");
        match &v.source {
            Source::Log(..) | Source::Blame(_) | Source::Text => {
                matches!(v.targets.get(i), Some(Some(Target::Commit(_))))
                    && (i == 0 || v.targets.get(i - 1) != v.targets.get(i))
            }
            _ => line.starts_with("@@") || line.starts_with("diff --git") || line.starts_with("commit "),
        }
    };
    let mut i = row + by;
    while i >= 0 && (i as usize) < lines.len() {
        if stop(i as usize) {
            b.set_point(Position {
                line: i as u32,
                column: 0,
            });
            return true;
        }
        i += by;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_lines_map_to_commits_past_the_title() {
        let e = LogEntry {
            hash: "h1".into(),
            short: "h".into(),
            author: "A".into(),
            date: "2026-01-01".into(),
            refs: "HEAD -> main".into(),
            subject: "S".into(),
        };
        let (text, targets, faces) = render_log("Commits in HEAD", &[e.clone(), e]);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[2], "h (HEAD -> main) S  A 2026-01-01");
        assert_eq!(targets[0], None, "the title shows nothing");
        assert_eq!(targets[2], Some(Target::Commit("h1".into())), "RET on the first commit");
        assert!(faces.contains(&(2, 0, 1, "comment")));
    }

    #[test]
    fn blame_lines_name_their_commits() {
        let t = blame_targets("^abc123 (Ada 2026-01-01 1) one\n00000000 (Not Committed Yet 2026-01-02 2) two\n");
        assert_eq!(t[0], Some(Target::Commit("abc123".into())));
        assert_eq!(t[1], None);
    }
}
