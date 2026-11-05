// SPDX-License-Identifier: GPL-3.0-or-later
//! The status buffer: what Magit shows, and which line means what.
//!
//! The buffer is read-only text the plugin rewrites on every refresh,
//! with one major mode for its keys. Each rendered line maps back to a
//! section heading, a file, a hunk (and the line in it), a stash or a
//! commit, so point actions (`s`, `u`, `k`, `RET`) know what they act on.
//!
//! Refreshing runs every git it needs at once and renders when the last
//! one answers; a newer refresh supersedes an older one still running.

use crate::diff::{self, DIFF_ARGS, FileDiff};
use crate::git::{
    self, EntryKind, FileStatus, LOG_FORMAT, LogEntry, Output, Run, STASH_FORMAT, Sequence, Stash,
    Status,
};
use forge_plugin_sdk::{Buffer, Decoration, Position, Span, decorations, message};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

/// The parts of a status buffer, top to bottom.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Section {
    Head,
    Sequence,
    Untracked,
    Unstaged,
    Staged,
    Stashes,
    Unpushed,
    Unpulled,
    Recent,
}

impl Section {
    /// The file sections, whose rows have diffs.
    pub fn has_files(self) -> bool {
        matches!(self, Section::Untracked | Section::Unstaged | Section::Staged)
    }
}

/// What one rendered line stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusLine {
    /// Blank lines: point can sit here, nothing acts on it.
    Blank,
    /// A section's heading; TAB folds the section.
    Heading(Section),
    /// A line of the Head section (Head:, Merge:, Tag:, ...); RET shows
    /// its commit, if it names one.
    Info { hash: Option<String> },
    /// A file row.
    File { section: Section, path: String },
    /// A line of an expanded file's diff: hunk `index`, and which body
    /// line (None for the `@@` header).
    Hunk {
        section: Section,
        path: String,
        index: usize,
        line: Option<usize>,
    },
    Stash { name: String },
    Commit { section: Section, hash: String },
}

impl StatusLine {
    /// Section depth, for n/p (every level stops), M-n/M-p (same level)
    /// and ^ (a lower one). None: not a stop.
    pub fn level(&self) -> Option<u8> {
        match self {
            StatusLine::Blank => None,
            StatusLine::Heading(_) => Some(0),
            StatusLine::Info { .. }
            | StatusLine::File { .. }
            | StatusLine::Stash { .. }
            | StatusLine::Commit { .. } => Some(1),
            StatusLine::Hunk { line: None, .. } => Some(2),
            StatusLine::Hunk { line: Some(_), .. } => None,
        }
    }

    pub fn section(&self) -> Option<Section> {
        match self {
            StatusLine::Heading(s) => Some(*s),
            StatusLine::Info { .. } => Some(Section::Head),
            StatusLine::File { section, .. }
            | StatusLine::Hunk { section, .. }
            | StatusLine::Commit { section, .. } => Some(*section),
            StatusLine::Stash { .. } => Some(Section::Stashes),
            StatusLine::Blank => None,
        }
    }

    /// The file this line belongs to, if any.
    pub fn path(&self) -> Option<&str> {
        match self {
            StatusLine::File { path, .. } | StatusLine::Hunk { path, .. } => Some(path),
            _ => None,
        }
    }
}

/// Everything one status buffer shows.
#[derive(Debug, Default)]
pub struct RepoView {
    pub root: String,
    pub buffer: u32,
    /// Bumped by every refresh; an answer for an older one is dropped.
    pub generation: u64,
    pub loaded: bool,
    pub status: Status,
    pub sequence: Sequence,
    /// Short MERGE_HEAD, while merging.
    pub merge_head: Option<String>,
    pub head_commit: Option<LogEntry>,
    pub upstream_commit: Option<LogEntry>,
    pub tag: Option<(String, u32)>,
    pub stashes: Vec<Stash>,
    pub unpushed: Vec<LogEntry>,
    pub unpulled: Vec<LogEntry>,
    pub recent: Vec<LogEntry>,
    /// One entry per rendered line.
    pub lines: Vec<StatusLine>,
    /// Expanded files' diffs.
    pub expanded: HashMap<(Section, String), FileDiff>,
    /// Folded sections.
    pub collapsed: HashSet<Section>,
}

impl RepoView {
    pub fn new(root: &str, buffer: u32) -> Self {
        RepoView {
            root: root.to_string(),
            buffer,
            ..Default::default()
        }
    }

    /// The files a file section lists, in display order.
    pub fn files(&self, section: Section) -> Vec<&FileStatus> {
        match section {
            Section::Untracked => self.status.untracked().collect(),
            Section::Unstaged => self.status.unstaged(),
            Section::Staged => self.status.staged().collect(),
            _ => Vec::new(),
        }
    }

    pub fn file(&self, section: Section, path: &str) -> Option<&FileStatus> {
        self.files(section).into_iter().find(|f| f.path == path)
    }

    /// The operation in progress, if any.
    pub fn op(&self) -> Option<git::Op> {
        if self.merge_head.is_some() {
            return Some(git::Op::Merge);
        }
        self.sequence.op
    }
}

thread_local! {
    static VIEWS: RefCell<HashMap<u32, RepoView>> = RefCell::new(HashMap::new());
    /// Root -> status buffer.
    static ROOTS: RefCell<HashMap<String, u32>> = RefCell::new(HashMap::new());
}

pub fn with<R>(f: impl FnOnce(&HashMap<u32, RepoView>) -> R) -> R {
    VIEWS.with(|v| f(&v.borrow()))
}

pub fn with_mut<R>(f: impl FnOnce(&mut HashMap<u32, RepoView>) -> R) -> R {
    VIEWS.with(|v| f(&mut v.borrow_mut()))
}

/// Run `f` on the view in `buffer`, if there is one.
pub fn with_view<R>(buffer: u32, f: impl FnOnce(&RepoView) -> R) -> Option<R> {
    with(|views| views.get(&buffer).map(f))
}

pub fn is_status(buffer: u32) -> bool {
    with(|v| v.contains_key(&buffer))
}

pub fn root_of(buffer: u32) -> Option<String> {
    with_view(buffer, |v| v.root.clone())
}

/// The status buffer showing `root`, if one is open.
pub fn buffer_for(root: &str) -> Option<u32> {
    ROOTS.with(|r| r.borrow().get(root).copied()).filter(|b| is_status(*b))
}

/// All open status buffers: (buffer, root).
pub fn open_views() -> Vec<(u32, String)> {
    with(|views| views.iter().map(|(id, v)| (*id, v.root.clone())).collect())
}

/// Refresh every status buffer showing `path` (saved a file there).
pub fn refresh_for_path(path: &str) {
    for (buffer, root) in open_views() {
        if path == root || path.starts_with(&format!("{root}/")) {
            refresh(buffer);
        }
    }
}

/// Refresh the status buffer of `root`, if one is open.
pub fn refresh_root(root: &str) {
    if let Some(b) = buffer_for(root) {
        refresh(b);
    }
}

/// Forget a closed status buffer.
pub fn forget(buffer: u32) {
    with_mut(|views| {
        if let Some(view) = views.remove(&buffer) {
            ROOTS.with(|r| {
                let mut r = r.borrow_mut();
                if r.get(&view.root) == Some(&buffer) {
                    r.remove(&view.root);
                }
            });
        }
    });
}

/// The buffer name for `root`: its last component, unless another
/// repository already has that name (then its parent's too).
fn buffer_name(root: &str) -> String {
    let mut parts = root.rsplit('/').filter(|p| !p.is_empty());
    let last = parts.next().unwrap_or(root);
    let name = format!("*magit: {last}*");
    let taken = with(|views| {
        views.values().any(|v| {
            v.root != root
                && Buffer::from_id(v.buffer).is_some_and(|b| b.alive() && b.name() == name)
        })
    });
    match (taken, parts.next()) {
        (true, Some(parent)) => format!("*magit: {parent}/{last}*"),
        _ => name,
    }
}

/// Open (or switch to) the status buffer for `root`, then refresh it.
pub fn open(root: &str) {
    if let Some(id) = buffer_for(root) {
        if let Some(b) = Buffer::from_id(id).filter(|b| b.alive()) {
            b.show();
            refresh(id);
            return;
        }
        forget(id);
    }
    let name = buffer_name(root);
    // A buffer by that name that isn't a live status buffer (left over
    // from an unloaded plugin) is taken over; one that is gets <2>.
    let b = match Buffer::find(&name) {
        Some(b) if !is_status(b.id()) => b,
        _ => Buffer::create(&name),
    };
    let _ = b.set_major_mode("magit-status-mode");
    let _ = b.set_option("read-only", "true");
    b.show();
    let id = b.id();
    ROOTS.with(|r| r.borrow_mut().insert(root.to_string(), id));
    with_mut(|views| views.insert(id, RepoView::new(root, id)));
    refresh(id);
}

// --------------------------------------------------------------- refresh

/// The git that fetches one expanded file's diff.
fn diff_run(section: Section, f: Option<&FileStatus>, path: &str) -> Run {
    let mut args: Vec<String> = vec!["diff".into()];
    args.extend(DIFF_ARGS.iter().map(|s| s.to_string()));
    match section {
        Section::Untracked => {
            args.extend(["--no-index".into(), "--".into(), "/dev/null".into(), path.into()]);
        }
        Section::Staged => {
            args.extend(["--cached".into(), "-M".into(), "--".into()]);
            if let Some(from) = f.and_then(|f| f.from.clone()) {
                args.push(from);
            }
            args.push(path.into());
        }
        _ => {
            args.extend(["--".into(), path.into()]);
        }
    }
    Run::owned(args)
}

fn log_run(extra: &[&str]) -> Run {
    let mut args = vec!["log", LOG_FORMAT, "--date=short"];
    args.extend(extra);
    Run::new(&args)
}

/// Re-run git and re-render the status buffer.
pub fn refresh(buffer: u32) {
    refresh_then(buffer, || {});
}

/// Refresh, then call `then` (after the render). `then` is called once
/// whatever happens: also when the refresh failed, the buffer went, or
/// a newer refresh took over (which renders when it is done).
pub fn refresh_then(buffer: u32, then: impl FnOnce() + 'static) {
    let Some((root, generation, keys)) = with_mut(|views| {
        let v = views.get_mut(&buffer)?;
        v.generation += 1;
        let keys: Vec<(Section, String, Option<FileStatus>)> = v
            .expanded
            .keys()
            .map(|(s, p)| (*s, p.clone(), v.file(*s, p).cloned()))
            .collect();
        Some((v.root.clone(), v.generation, keys))
    }) else {
        then();
        return;
    };
    let mut runs = vec![
        Run::new(&[
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=all",
        ]),
        Run::new(&["status", "--untracked-files=no"]).c_locale(),
        Run::new(&["rev-parse", "--verify", "--quiet", "--short", "MERGE_HEAD"]),
        Run::new(&["stash", "list", STASH_FORMAT]),
        log_run(&["-1", "HEAD"]),
        log_run(&["-1", "@{upstream}"]),
        log_run(&["-n", "50", "@{upstream}..HEAD"]),
        log_run(&["-n", "50", "HEAD..@{upstream}"]),
        log_run(&["-n", "10", "HEAD"]),
        Run::new(&["describe", "--tags", "--long"]),
    ];
    let fixed = runs.len();
    for (section, path, f) in &keys {
        runs.push(diff_run(*section, f.as_ref(), path));
    }
    git::join(&root, runs, move |outs| {
        let applied = with_mut(|views| {
            let Some(view) = views.get_mut(&buffer) else {
                return Err(None);
            };
            if view.generation != generation {
                return Err(None);
            }
            if !outs[0].ok() {
                return Err(Some(outs[0].error()));
            }
            apply_refresh(view, &outs, fixed, &keys);
            Ok(())
        });
        match applied {
            Ok(()) => render(buffer),
            Err(Some(e)) => message(&format!("magit: status failed: {e}")),
            Err(None) => {}
        }
        then();
    });
}

/// Store what a refresh's gits said.
fn apply_refresh(
    view: &mut RepoView,
    outs: &[Output],
    fixed: usize,
    keys: &[(Section, String, Option<FileStatus>)],
) {
    let ok = |i: usize| outs.get(i).filter(|o| o.ok()).map(|o| o.stdout.as_str());
    view.status = git::parse_status(&outs[0].stdout);
    view.sequence = ok(1).map(git::parse_sequence).unwrap_or_default();
    view.merge_head = ok(2).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    view.stashes = ok(3).map(git::parse_stash_list).unwrap_or_default();
    let first = |i| ok(i).and_then(|s| git::parse_log(s).into_iter().next());
    view.head_commit = first(4);
    view.upstream_commit = first(5);
    view.unpushed = ok(6).map(git::parse_log).unwrap_or_default();
    view.unpulled = ok(7).map(git::parse_log).unwrap_or_default();
    view.recent = ok(8).map(git::parse_log).unwrap_or_default();
    view.tag = ok(9).and_then(git::parse_describe);
    for (i, (section, path, _)) in keys.iter().enumerate() {
        let key = (*section, path.clone());
        let still_listed = view.file(*section, path).is_some();
        // --no-index exits 1 when there are differences.
        let out = outs.get(fixed + i).filter(|o| o.ok() || *section == Section::Untracked);
        match out.map(|o| diff::parse_file_diff(&o.stdout)) {
            Some(d) if still_listed && (!d.hunks.is_empty() || d.note.is_some()) => {
                view.expanded.insert(key, d);
            }
            _ => {
                view.expanded.remove(&key);
            }
        }
    }
    view.loaded = true;
}

/// Show or hide the diff under the file at point; on a heading, fold
/// the section.
pub fn toggle(buffer: u32) {
    let Some(target) = point_target(buffer) else {
        return;
    };
    match target {
        StatusLine::Heading(section) => {
            with_mut(|views| {
                if let Some(v) = views.get_mut(&buffer)
                    && !v.collapsed.remove(&section)
                {
                    v.collapsed.insert(section);
                }
            });
            render(buffer);
        }
        StatusLine::File { section, path } | StatusLine::Hunk { section, path, .. } => {
            let key = (section, path.clone());
            let shown = with_view(buffer, |v| v.expanded.contains_key(&key)).unwrap_or(false);
            if shown {
                with_mut(|views| {
                    if let Some(v) = views.get_mut(&buffer) {
                        v.expanded.remove(&key);
                    }
                });
                // Point goes back to the file row it hid.
                render_at(buffer, Some(StatusLine::File { section, path }));
                return;
            }
            let Some((root, f)) =
                with_view(buffer, |v| (v.root.clone(), v.file(section, &path).cloned()))
            else {
                return;
            };
            git::exec(&root, diff_run(section, f.as_ref(), &path), move |out| {
                if !out.ok() && section != Section::Untracked {
                    message(&format!("magit: diff failed: {}", out.error()));
                    return;
                }
                let d = diff::parse_file_diff(&out.stdout);
                if d.hunks.is_empty() && d.note.is_none() {
                    message("No differences");
                    return;
                }
                with_mut(|views| {
                    if let Some(view) = views.get_mut(&buffer) {
                        view.expanded.insert(key, d);
                    }
                });
                render(buffer);
            });
        }
        _ => message("Nothing to show or hide here"),
    }
}

// ---------------------------------------------------------------- render

/// What a render produces: the text, what each line means, and faces
/// as (line, start column, end column, face).
pub struct Rendered {
    pub text: String,
    pub lines: Vec<StatusLine>,
    pub faces: Vec<(usize, usize, usize, &'static str)>,
}

struct Out {
    text: String,
    lines: Vec<StatusLine>,
    faces: Vec<(usize, usize, usize, &'static str)>,
}

impl Out {
    fn line(&mut self, s: &str, l: StatusLine) -> usize {
        self.text.push_str(s);
        self.text.push('\n');
        self.lines.push(l);
        self.lines.len() - 1
    }

    /// A line with faces on parts of it: (text, face) pieces.
    fn pieces(&mut self, pieces: &[(&str, Option<&'static str>)], l: StatusLine) {
        let row = self.lines.len();
        let mut col = 0;
        let mut s = String::new();
        for (text, face) in pieces {
            let n = text.chars().count();
            if let Some(f) = face
                && n > 0
            {
                self.faces.push((row, col, col + n, f));
            }
            col += n;
            s.push_str(text);
        }
        self.line(&s, l);
    }

    fn face_line(&mut self, s: &str, face: &'static str, l: StatusLine) {
        let n = s.chars().count();
        let row = self.line(s, l);
        if n > 0 {
            self.faces.push((row, 0, n, face));
        }
    }
}

/// A commit row: hash, refs, subject.
fn commit_pieces(out: &mut Out, e: &LogEntry, l: StatusLine) {
    let refs = if e.refs.is_empty() {
        String::new()
    } else {
        format!("({}) ", e.refs)
    };
    out.pieces(
        &[
            (&e.short, Some("comment")),
            (" ", None),
            (&refs, Some("function")),
            (&e.subject, None),
        ],
        l,
    );
}

pub fn render_view(view: &RepoView) -> Rendered {
    let mut out = Out {
        text: String::new(),
        lines: Vec::new(),
        faces: Vec::new(),
    };
    if !view.loaded {
        out.line("Loading...", StatusLine::Blank);
        return Rendered {
            text: out.text,
            lines: out.lines,
            faces: out.faces,
        };
    }
    let head = &view.status.head;
    let head_hash = view.head_commit.as_ref().map(|c| c.hash.clone());
    let branch = head
        .branch
        .clone()
        .unwrap_or_else(|| "(detached)".to_string());
    if head.unborn() {
        out.pieces(
            &[
                ("Head:     ", Some("keyword")),
                (&branch, Some("function")),
                (" (no commits yet)", Some("comment")),
            ],
            StatusLine::Info { hash: None },
        );
    } else {
        let (short, subject) = view
            .head_commit
            .as_ref()
            .map(|c| (c.short.clone(), c.subject.clone()))
            .unwrap_or_default();
        let name = if head.branch.is_some() { branch.clone() } else { short.clone() };
        out.pieces(
            &[
                ("Head:     ", Some("keyword")),
                (&name, Some("function")),
                (" ", None),
                (&subject, None),
            ],
            StatusLine::Info { hash: head_hash },
        );
    }
    if let Some(up) = &head.upstream {
        let mut ab = Vec::new();
        if head.ahead > 0 {
            ab.push(format!("ahead {}", head.ahead));
        }
        if head.behind > 0 {
            ab.push(format!("behind {}", head.behind));
        }
        let ab = if ab.is_empty() {
            String::new()
        } else {
            format!(" [{}]", ab.join(", "))
        };
        let (subject, hash) = match &view.upstream_commit {
            Some(c) => (c.subject.clone(), Some(c.hash.clone())),
            None => ("(gone)".to_string(), None),
        };
        out.pieces(
            &[
                ("Merge:    ", Some("keyword")),
                (up, Some("type")),
                (&ab, Some("comment")),
                (" ", None),
                (&subject, None),
            ],
            StatusLine::Info { hash },
        );
    }
    if let Some((tag, n)) = &view.tag {
        out.pieces(
            &[
                ("Tag:      ", Some("keyword")),
                (tag, Some("type")),
                (&format!(" ({n})"), Some("comment")),
            ],
            StatusLine::Info { hash: None },
        );
    }
    if let Some(op) = view.op() {
        let detail = match op {
            git::Op::Merge => view.merge_head.clone(),
            _ => view.sequence.detail.clone(),
        };
        let rebasing = view.sequence.branch.clone().unwrap_or_else(|| branch.clone());
        let what = match (op, &detail) {
            (git::Op::Rebase, Some(onto)) => format!("{rebasing} onto {onto}"),
            (_, Some(d)) => d.clone(),
            (_, None) => String::new(),
        };
        let mut label = format!("{:<10}", format!("{}:", op.name()));
        if !label.ends_with(' ') {
            label.push(' ');
        }
        out.pieces(
            &[(&label, Some("keyword")), (&what, Some("warning"))],
            StatusLine::Info {
                hash: detail.filter(|d| d.chars().all(|c| c.is_ascii_hexdigit())),
            },
        );
    }
    out.line("", StatusLine::Blank);

    // A rebase's sequence: what's left to do, then what's done.
    if view.op() == Some(git::Op::Rebase)
        && !(view.sequence.todo.is_empty() && view.sequence.done.is_empty())
    {
        heading(&mut out, view, Section::Sequence, "Rebase sequence", None);
        if !view.collapsed.contains(&Section::Sequence) {
            for t in view.sequence.todo.iter().rev() {
                todo_row(&mut out, t, "");
            }
            // The last one done is where git stopped (a conflict, an edit).
            for (i, t) in view.sequence.done.iter().enumerate().rev() {
                let stopped = i + 1 == view.sequence.done.len();
                todo_row(&mut out, t, if stopped { "stop" } else { "done" });
            }
        }
        out.line("", StatusLine::Blank);
    }

    for (section, title) in [
        (Section::Untracked, "Untracked files"),
        (Section::Unstaged, "Unstaged changes"),
        (Section::Staged, "Staged changes"),
    ] {
        render_files(&mut out, view, section, title);
    }

    if !view.stashes.is_empty() {
        heading(&mut out, view, Section::Stashes, "Stashes", Some(view.stashes.len()));
        if !view.collapsed.contains(&Section::Stashes) {
            for s in &view.stashes {
                out.pieces(
                    &[(&s.name, Some("comment")), (" ", None), (&s.message, None)],
                    StatusLine::Stash {
                        name: s.name.clone(),
                    },
                );
            }
        }
        out.line("", StatusLine::Blank);
    }

    let up = head.upstream.clone().unwrap_or_default();
    let lists: [(Section, String, &Vec<LogEntry>); 2] = [
        (Section::Unpushed, format!("Unmerged into {up}"), &view.unpushed),
        (Section::Unpulled, format!("Unpulled from {up}"), &view.unpulled),
    ];
    for (section, title, list) in lists {
        if head.upstream.is_some() && !list.is_empty() {
            commit_list(&mut out, view, section, &title, list);
        }
    }
    if view.unpushed.is_empty() && !view.recent.is_empty() {
        commit_list(&mut out, view, Section::Recent, "Recent commits", &view.recent);
    }
    Rendered {
        text: out.text,
        lines: out.lines,
        faces: out.faces,
    }
}

fn heading(out: &mut Out, view: &RepoView, section: Section, title: &str, count: Option<usize>) {
    let mut text = title.to_string();
    if let Some(n) = count {
        text.push_str(&format!(" ({n})"));
    }
    if view.collapsed.contains(&section) {
        text.push_str(" ...");
    }
    out.face_line(&text, "keyword", StatusLine::Heading(section));
}

/// A rebase todo line: `pick abc1234 Subject`, shown with `label`
/// ("done", "stop") in place of its verb when given.
fn todo_row(out: &mut Out, line: &str, label: &str) {
    let mut words = line.splitn(3, ' ');
    let verb = words.next().unwrap_or("");
    let hash = words.next().unwrap_or("").to_string();
    let rest = words.next().unwrap_or("");
    let done = !label.is_empty();
    let verb = if done { label } else { verb };
    let l = if hash.chars().all(|c| c.is_ascii_hexdigit()) && !hash.is_empty() {
        StatusLine::Commit {
            section: Section::Sequence,
            hash: hash.clone(),
        }
    } else {
        StatusLine::Blank
    };
    out.pieces(
        &[
            (&format!("{verb:<7}"), Some(if done { "comment" } else { "keyword" })),
            (&hash, Some("comment")),
            (" ", None),
            (rest, None),
        ],
        l,
    );
}

fn commit_list(out: &mut Out, view: &RepoView, section: Section, title: &str, list: &[LogEntry]) {
    let count = (section != Section::Recent).then_some(list.len());
    heading(out, view, section, title, count);
    if !view.collapsed.contains(&section) {
        for e in list {
            commit_pieces(
                out,
                e,
                StatusLine::Commit {
                    section,
                    hash: e.hash.clone(),
                },
            );
        }
    }
    out.line("", StatusLine::Blank);
}

/// One file section: the heading, a row per file, and expanded diffs.
/// Empty sections aren't shown.
fn render_files(out: &mut Out, view: &RepoView, section: Section, title: &str) {
    let files = view.files(section);
    if files.is_empty() {
        return;
    }
    heading(out, view, section, title, Some(files.len()));
    if view.collapsed.contains(&section) {
        out.line("", StatusLine::Blank);
        return;
    }
    for f in files {
        let file_line = StatusLine::File {
            section,
            path: f.path.clone(),
        };
        if section == Section::Untracked {
            out.line(&f.path, file_line.clone());
        } else {
            let label = format!("{:<11}", git::change_label(f, section == Section::Staged));
            let face = if f.kind == EntryKind::Unmerged { "builtin" } else { "comment" };
            let path = match &f.from {
                Some(from) => format!("{from} -> {}", f.path),
                None => f.path.clone(),
            };
            out.pieces(&[(&label, Some(face)), (&path, None)], file_line.clone());
        }
        let Some(d) = view.expanded.get(&(section, f.path.clone())) else {
            continue;
        };
        if let Some(note) = &d.note {
            out.face_line(&format!("  {note}"), "comment", file_line.clone());
        }
        for (i, hunk) in d.hunks.iter().enumerate() {
            let combined = hunk.combined();
            out.face_line(
                &hunk.header,
                "constant",
                StatusLine::Hunk {
                    section,
                    path: f.path.clone(),
                    index: i,
                    line: None,
                },
            );
            for (j, body) in hunk.body.iter().enumerate() {
                let l = StatusLine::Hunk {
                    section,
                    path: f.path.clone(),
                    index: i,
                    line: Some(j),
                };
                // A CRLF file's \r is part of the line, not something to show.
                let shown = body.strip_suffix('\r').unwrap_or(body);
                match diff::line_face(body, true, combined) {
                    Some(face) => out.face_line(shown, face, l),
                    None => {
                        out.line(shown, l);
                    }
                }
            }
        }
    }
    out.line("", StatusLine::Blank);
}

/// Render the view into its buffer, keeping point where it makes sense.
pub fn render(buffer: u32) {
    render_at(buffer, None);
}

/// Render; `prefer` puts point on that line if it is still there.
pub fn render_at(buffer: u32, prefer: Option<StatusLine>) {
    let Some(b) = Buffer::from_id(buffer).filter(|b| b.alive()) else {
        forget(buffer);
        return;
    };
    let row = b.point().line as usize;
    let Some((rendered, new_row)) = with_mut(|views| {
        let view = views.get_mut(&buffer)?;
        let anchor = Anchor::at(&view.lines, row);
        let rendered = render_view(view);
        let new_row = prefer
            .and_then(|p| rendered.lines.iter().position(|l| *l == p))
            .unwrap_or_else(|| anchor.restore(&rendered.lines));
        view.lines = rendered.lines.clone();
        Some((rendered, new_row))
    }) else {
        return;
    };
    b.replace(
        Span {
            start: Position { line: 0, column: 0 },
            end: Position {
                line: u32::MAX,
                column: u32::MAX,
            },
        },
        &rendered.text,
    );
    let decos: Vec<Decoration> = rendered
        .faces
        .iter()
        .map(|(line, a, z, face)| Decoration {
            span: Span {
                start: Position {
                    line: *line as u32,
                    column: *a as u32,
                },
                end: Position {
                    line: *line as u32,
                    column: *z as u32,
                },
            },
            face: face.to_string(),
            message: None,
        })
        .collect();
    let _ = decorations::set(&b, &decos);
    b.set_point(Position {
        line: new_row as u32,
        column: 0,
    });
}

// ------------------------------------------------------- point and motion

/// Where point was, to find the same place in a fresh render.
#[derive(Debug, Clone)]
pub struct Anchor {
    line: Option<StatusLine>,
    row: usize,
    /// The section, and which item of it (file, stash, commit) point was
    /// on or under.
    item: Option<(Section, usize)>,
}

impl Anchor {
    pub fn at(lines: &[StatusLine], row: usize) -> Anchor {
        let line = lines.get(row).cloned();
        let item = line.as_ref().and_then(|l| {
            let section = l.section()?;
            let start = lines[..=row]
                .iter()
                .rposition(|x| *x == StatusLine::Heading(section))
                .map(|i| i + 1)
                .unwrap_or(0);
            let n = lines[start..=row]
                .iter()
                .filter(|x| x.level() == Some(1) && x.section() == Some(section))
                .count();
            Some((section, n.saturating_sub(1)))
        });
        Anchor { line, row, item }
    }

    /// The row in `lines` to put point on.
    pub fn restore(&self, lines: &[StatusLine]) -> usize {
        let last = lines.len().saturating_sub(1);
        if let Some(l) = &self.line
            && *l != StatusLine::Blank
        {
            if let Some(i) = lines.iter().position(|x| x == l) {
                return i;
            }
            // A diff line that went: its hunk's header, else its file.
            if let StatusLine::Hunk {
                section,
                path,
                index,
                ..
            } = l
            {
                let header = StatusLine::Hunk {
                    section: *section,
                    path: path.clone(),
                    index: *index,
                    line: None,
                };
                if let Some(i) = lines.iter().position(|x| *x == header) {
                    return i;
                }
                // The hunk went (staged away): the next one of the file,
                // or the one before it.
                let others: Vec<usize> = lines
                    .iter()
                    .enumerate()
                    .filter(|(_, x)| {
                        matches!(x, StatusLine::Hunk { section: s, path: p, line: None, .. }
                            if s == section && p == path)
                    })
                    .map(|(i, _)| i)
                    .collect();
                if let Some(i) = others.last() {
                    return *i;
                }
            }
        }
        // The item that took its place in the same section (after
        // staging a file, the next one), or the last one left.
        if let Some((section, n)) = self.item {
            let items: Vec<usize> = lines
                .iter()
                .enumerate()
                .filter(|(_, x)| x.level() == Some(1) && x.section() == Some(section))
                .map(|(i, _)| i)
                .collect();
            if !items.is_empty() {
                return items[n.min(items.len() - 1)];
            }
            if let Some(i) = lines.iter().position(|x| *x == StatusLine::Heading(section)) {
                return i;
            }
        }
        self.row.min(last)
    }
}

/// The next (`by` 1) or previous (-1) stop after `row`: any section
/// (`same_level` false) or one at the same level, stopping at a parent.
pub fn next_stop(lines: &[StatusLine], row: usize, by: isize, same_level: bool) -> Option<usize> {
    let level = lines.get(row).and_then(StatusLine::level);
    let mut i = row as isize;
    loop {
        i += by;
        if i < 0 || i as usize >= lines.len() {
            return None;
        }
        let Some(l) = lines[i as usize].level() else {
            continue;
        };
        match (same_level, level) {
            (false, _) | (true, None) => return Some(i as usize),
            // Left the parent: no sibling that way.
            (true, Some(lv)) if l < lv => return None,
            (true, Some(lv)) if l == lv => return Some(i as usize),
            _ => continue,
        }
    }
}

/// The enclosing section's row (^).
pub fn parent(lines: &[StatusLine], row: usize) -> Option<usize> {
    let level = match lines.get(row)? {
        // A diff body line's parent is its hunk.
        StatusLine::Hunk { line: Some(_), .. } => 3,
        l => l.level()?,
    };
    (0..row)
        .rev()
        .find(|&i| lines[i].level().is_some_and(|l| l < level))
}

/// What point is on in `buffer`.
pub fn point_target(buffer: u32) -> Option<StatusLine> {
    let row = Buffer::from_id(buffer)?.point().line as usize;
    with_view(buffer, |v| v.lines.get(row).cloned()).flatten()
}

/// The lines the active region covers, (first row, last row). As in
/// Magit, selection is by whole lines: the lines of both ends count.
pub fn region_rows(buffer: u32) -> Option<(usize, usize)> {
    let b = Buffer::from_id(buffer)?;
    let r = b.region()?;
    // An empty region (C-SPC and no motion) selects nothing.
    if (r.start.line, r.start.column) == (r.end.line, r.end.column) {
        return None;
    }
    Some((r.start.line as usize, r.end.line as usize))
}

/// Move point in a status buffer.
pub fn move_point(buffer: u32, f: impl FnOnce(&[StatusLine], usize) -> Option<usize>) -> bool {
    let Some(b) = Buffer::from_id(buffer) else {
        return false;
    };
    let row = b.point().line as usize;
    let to = with_view(buffer, |v| f(&v.lines, row)).flatten();
    match to {
        Some(i) => {
            b.set_point(Position {
                line: i as u32,
                column: 0,
            });
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> RepoView {
        let status = git::parse_status(
            &[
                "# branch.oid abc",
                "# branch.head main",
                "# branch.upstream origin/main",
                "# branch.ab +1 -0",
                "1 .M N... 100644 100644 100644 a b one.rs",
                "1 .M N... 100644 100644 100644 a b two.rs",
                "1 M. N... 100644 100644 100644 a b three.rs",
                "? new.rs",
                "",
            ]
            .join("\0"),
        );
        let mut v = RepoView::new("/r", 1);
        v.loaded = true;
        v.status = status;
        v.head_commit = Some(LogEntry {
            hash: "abcdef".into(),
            short: "abc".into(),
            author: "A".into(),
            date: "d".into(),
            refs: String::new(),
            subject: "Head subject".into(),
        });
        v.unpushed = vec![v.head_commit.clone().unwrap()];
        v
    }

    fn rows(v: &RepoView) -> (Vec<String>, Vec<StatusLine>) {
        let r = render_view(v);
        (r.text.lines().map(str::to_string).collect(), r.lines)
    }

    #[test]
    fn renders_magit_sections_in_order_and_skips_empty_ones() {
        let v = view();
        let (text, lines) = rows(&v);
        assert_eq!(text[0], "Head:     main Head subject");
        assert!(text[1].starts_with("Merge:    origin/main [ahead 1]"), "{}", text[1]);
        let pos = |s: &str| text.iter().position(|l| l.starts_with(s)).unwrap();
        assert!(pos("Untracked files (1)") < pos("Unstaged changes (2)"));
        assert!(pos("Unstaged changes (2)") < pos("Staged changes (1)"));
        assert!(pos("Staged changes") < pos("Unmerged into origin/main (1)"));
        assert!(!text.iter().any(|l| l.starts_with("Stashes")));
        assert!(!text.iter().any(|l| l.starts_with("Recent")), "unpushed shown instead");
        assert_eq!(text[pos("Unstaged") + 1], "modified   one.rs");
        assert_eq!(text.len(), lines.len());
    }

    #[test]
    fn expanded_diffs_and_folded_sections() {
        let mut v = view();
        v.expanded.insert(
            (Section::Unstaged, "one.rs".into()),
            diff::parse_file_diff("--- a/one.rs\n+++ b/one.rs\n@@ -1 +1 @@\n-a\r\n+b\r\n"),
        );
        let (text, lines) = rows(&v);
        let file = text.iter().position(|l| l == "modified   one.rs").unwrap();
        assert_eq!(text[file + 1], "@@ -1 +1 @@");
        assert_eq!(text[file + 2], "-a", "the \\r isn't shown");
        assert_eq!(
            lines[file + 3],
            StatusLine::Hunk {
                section: Section::Unstaged,
                path: "one.rs".into(),
                index: 0,
                line: Some(1)
            }
        );
        v.collapsed.insert(Section::Unstaged);
        let (text, _) = rows(&v);
        assert!(text.iter().any(|l| l == "Unstaged changes (2) ..."));
        assert!(!text.iter().any(|l| l.contains("one.rs")));
    }

    #[test]
    fn point_moves_to_the_next_file_when_its_file_is_staged() {
        let mut v = view();
        let (_, before) = rows(&v);
        let row = before
            .iter()
            .position(|l| *l == StatusLine::File { section: Section::Unstaged, path: "one.rs".into() })
            .unwrap();
        let anchor = Anchor::at(&before, row);
        // one.rs gets staged: it leaves Unstaged.
        v.status.files.retain(|f| f.path != "one.rs");
        v.status.files.push(FileStatus {
            path: "one.rs".into(),
            staged: 'M',
            unstaged: ' ',
            from: None,
            kind: EntryKind::Changed,
        });
        let (_, after) = rows(&v);
        let to = anchor.restore(&after);
        assert_eq!(
            after[to],
            StatusLine::File { section: Section::Unstaged, path: "two.rs".into() },
            "s s s works down the list"
        );
    }

    #[test]
    fn point_stays_on_a_file_that_is_still_there() {
        let v = view();
        let (_, lines) = rows(&v);
        let row = lines
            .iter()
            .position(|l| *l == StatusLine::File { section: Section::Unstaged, path: "two.rs".into() })
            .unwrap();
        assert_eq!(Anchor::at(&lines, row).restore(&lines), row);
    }

    #[test]
    fn navigation_moves_by_section_and_hunk_not_by_line() {
        let mut v = view();
        v.expanded.insert(
            (Section::Unstaged, "one.rs".into()),
            diff::parse_file_diff("@@ -1 +1 @@\n-a\n+b\n@@ -5 +5 @@\n-c\n+d\n"),
        );
        let (text, lines) = rows(&v);
        let file = text.iter().position(|l| l == "modified   one.rs").unwrap();
        let h1 = next_stop(&lines, file, 1, false).unwrap();
        assert_eq!(text[h1], "@@ -1 +1 @@");
        let h2 = next_stop(&lines, h1, 1, false).unwrap();
        assert_eq!(text[h2], "@@ -5 +5 @@", "skips the hunk's lines");
        assert_eq!(next_stop(&lines, h2, -1, false), Some(h1));
        // Siblings: file to file, past the diff.
        let sib = next_stop(&lines, file, 1, true).unwrap();
        assert_eq!(text[sib], "modified   two.rs");
        assert_eq!(parent(&lines, h2 + 1), Some(h2));
        assert_eq!(parent(&lines, h2), Some(file));
        let heading = parent(&lines, file).unwrap();
        assert!(text[heading].starts_with("Unstaged changes"));
    }

    #[test]
    fn hunk_point_survives_staging_the_hunk() {
        let mut v = view();
        let key = (Section::Unstaged, "one.rs".to_string());
        v.expanded.insert(key.clone(), diff::parse_file_diff("@@ -1 +1 @@\n-a\n+b\n@@ -5 +5 @@\n-c\n+d\n"));
        let (_, before) = rows(&v);
        let row = before
            .iter()
            .position(|l| matches!(l, StatusLine::Hunk { index: 1, line: None, .. }))
            .unwrap();
        let anchor = Anchor::at(&before, row);
        // The second hunk got staged: one hunk left.
        v.expanded.insert(key, diff::parse_file_diff("@@ -1 +1 @@\n-a\n+b\n"));
        let (text, after) = rows(&v);
        let to = anchor.restore(&after);
        assert_eq!(text[to], "@@ -1 +1 @@");
    }

    #[test]
    fn rebase_sequence_is_listed() {
        let mut v = view();
        v.sequence = git::parse_sequence(
            "interactive rebase in progress; onto 1a2b3c4\nLast command done (1 command done):\n   pick 1111111 First\nNext command to do (1 remaining command):\n   pick 2222222 Second\n",
        );
        let (text, lines) = rows(&v);
        assert!(text.iter().any(|l| l.starts_with("Rebasing: main onto 1a2b3c4")), "{text:?}");
        let h = text.iter().position(|l| l == "Rebase sequence").unwrap();
        assert!(text[h + 1].starts_with("pick   2222222 Second"));
        assert!(text[h + 2].starts_with("stop   1111111 First"), "where git stopped");
        assert_eq!(
            lines[h + 1],
            StatusLine::Commit { section: Section::Sequence, hash: "2222222".into() }
        );
    }
}
