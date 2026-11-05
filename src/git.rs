// SPDX-License-Identifier: GPL-3.0-or-later
//! Running git and parsing what it says.
//!
//! Everything goes through `git` on PATH (named in the manifest's
//! `processes` permission), in the repository's root. Every run gets an
//! environment that keeps git from waiting on a terminal that isn't
//! there: no editor (`GIT_EDITOR=true` keeps the message git proposes),
//! no credential prompts, no optional index locks. Stdin is closed at
//! once unless a patch or message is fed to it, so git never waits.
//!
//! Commands that change something are "logged": their output goes to the
//! repository's process log (`$`), the way Magit's process buffer works.

use forge_plugin_sdk::spawn_with;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// The environment every git gets.
const ENV: &[(&str, &str)] = &[
    // Sequencer commands (rebase --continue, revert, cherry-pick) keep the
    // message they propose instead of opening an editor nobody can see.
    ("GIT_EDITOR", "true"),
    ("GIT_SEQUENCE_EDITOR", "true"),
    // A password prompt would take over the terminal (or hang the app).
    ("GIT_TERMINAL_PROMPT", "0"),
    // Reads (status, diff) don't take the index lock, so a refresh never
    // makes a command of the user's fail with "index.lock exists".
    ("GIT_OPTIONAL_LOCKS", "0"),
];

/// Options every git gets, ahead of the subcommand.
const GLOBAL_ARGS: &[&str] = &["--no-pager", "-c", "color.ui=false", "-c", "core.quotePath=false"];

/// What one git run produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    /// None: killed by a signal, or it never started.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    /// Why it failed, in git's words.
    pub fn error(&self) -> String {
        // Progress lines ("Rebasing (1/1)") end in \r, not \n.
        let stderr = self.stderr.replace('\r', "\n");
        let stdout = self.stdout.replace('\r', "\n");
        let msg = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        if msg.is_empty() {
            match self.code {
                Some(c) => format!("git exited with code {c}"),
                None => "git was killed".to_string(),
            }
        } else {
            // The interesting part is usually the last "fatal:"/"error:"
            // line; keep everything but drop hint: lines.
            msg.lines()
                .filter(|l| !l.starts_with("hint:") && !l.trim().is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        }
    }

    /// Ok(stdout) or Err(why).
    pub fn result(self) -> Result<String, String> {
        if self.ok() {
            Ok(self.stdout)
        } else {
            Err(self.error())
        }
    }
}

/// How to run one git.
#[derive(Debug, Clone, Default)]
pub struct Run {
    pub args: Vec<String>,
    /// Fed to stdin, then EOF.
    pub input: Option<String>,
    /// Record it in the process log (`$`).
    pub logged: bool,
    /// `LC_ALL=C`, for the one output we read as prose (`git status`'s
    /// sequencer lines).
    pub c_locale: bool,
}

impl Run {
    pub fn new(args: &[&str]) -> Self {
        Run {
            args: args.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    pub fn owned(args: Vec<String>) -> Self {
        Run {
            args,
            ..Default::default()
        }
    }

    pub fn logged(mut self) -> Self {
        self.logged = true;
        self
    }

    pub fn input(mut self, input: &str) -> Self {
        self.input = Some(input.to_string());
        self
    }

    pub fn c_locale(mut self) -> Self {
        self.c_locale = true;
        self
    }
}

/// One entry of a repository's process log.
#[derive(Debug, Clone)]
pub struct Record {
    pub args: Vec<String>,
    pub output: Option<Output>,
}

/// Logged runs per repository, newest last.
const LOG_LIMIT: usize = 100;

thread_local! {
    static PROCESS_LOG: RefCell<HashMap<String, Vec<Record>>> = RefCell::new(HashMap::new());
}

/// The process log of `root`.
pub fn process_log(root: &str) -> Vec<Record> {
    PROCESS_LOG.with(|l| l.borrow().get(root).cloned().unwrap_or_default())
}

fn log_start(root: &str, args: &[String]) -> usize {
    PROCESS_LOG.with(|l| {
        let mut l = l.borrow_mut();
        let entries = l.entry(root.to_string()).or_default();
        if entries.len() >= LOG_LIMIT {
            entries.remove(0);
        }
        entries.push(Record {
            args: args.to_vec(),
            output: None,
        });
        entries.len() - 1
    })
}

fn log_finish(root: &str, args: &[String], out: &Output) {
    PROCESS_LOG.with(|l| {
        let mut l = l.borrow_mut();
        if let Some(r) = l
            .get_mut(root)
            .and_then(|e| e.iter_mut().rev().find(|r| r.output.is_none() && r.args == args))
        {
            r.output = Some(out.clone());
        }
    })
}

/// Run git in `root`; `then` gets everything it said.
pub fn exec(root: &str, run: Run, then: impl FnOnce(Output) + 'static) {
    let mut argv: Vec<String> = GLOBAL_ARGS.iter().map(|s| s.to_string()).collect();
    argv.extend(run.args.iter().cloned());
    let mut env: Vec<(&str, &str)> = ENV.to_vec();
    if run.c_locale {
        env.push(("LC_ALL", "C"));
        env.push(("LANGUAGE", "C"));
    }
    if run.logged {
        log_start(root, &run.args);
    }
    let out = Rc::new(RefCell::new(Vec::<u8>::new()));
    // FnOnce, but needed on two paths (a spawn failure answers at once):
    // taken exactly once, wherever the answer comes from.
    type Answer = Rc<RefCell<Option<Box<dyn FnOnce(Output)>>>>;
    let then: Answer =
        Rc::new(RefCell::new(Some(Box::new(then))));
    let (o, t) = (out.clone(), then.clone());
    let (root_c, args_c, logged) = (root.to_string(), run.args.clone(), run.logged);
    let result = spawn_with(
        "git",
        &argv,
        Some(root),
        &env,
        move |data| o.borrow_mut().extend_from_slice(data),
        move |code, stderr| {
            let stdout = String::from_utf8_lossy(&std::mem::take(&mut *out.borrow_mut())).into_owned();
            let output = Output {
                code,
                stdout,
                stderr,
            };
            if logged {
                log_finish(&root_c, &args_c, &output);
            }
            if let Some(then) = t.borrow_mut().take() {
                then(output);
            }
        },
    );
    match result {
        Ok(p) => {
            if let Some(input) = &run.input {
                let _ = p.write(input.as_bytes());
            }
            p.close_input();
        }
        Err(e) => {
            let output = Output {
                code: None,
                stdout: String::new(),
                stderr: e,
            };
            if run.logged {
                log_finish(root, &run.args, &output);
            }
            if let Some(then) = then.borrow_mut().take() {
                then(output);
            }
        }
    }
}

/// Run git in `root` and collect stdout; stderr becomes the error.
pub fn run(root: &str, args: &[&str], then: impl FnOnce(Result<String, String>) + 'static) {
    exec(root, Run::new(args), move |o| then(o.result()));
}

/// Run several gits at once; `then` gets their outputs in order.
pub fn join(root: &str, runs: Vec<Run>, then: impl FnOnce(Vec<Output>) + 'static) {
    let n = runs.len();
    if n == 0 {
        then(Vec::new());
        return;
    }
    type Join = (Vec<Option<Output>>, usize, Option<Box<dyn FnOnce(Vec<Output>)>>);
    let state: Rc<RefCell<Join>> = Rc::new(RefCell::new((vec![None; n], n, Some(Box::new(then)))));
    for (i, run) in runs.into_iter().enumerate() {
        let state = state.clone();
        exec(root, run, move |out| {
            let done = {
                let mut s = state.borrow_mut();
                s.0[i] = Some(out);
                s.1 -= 1;
                if s.1 == 0 {
                    let outs = std::mem::take(&mut s.0);
                    s.2.take().map(|f| (f, outs))
                } else {
                    None
                }
            };
            if let Some((f, outs)) = done {
                f(outs.into_iter().map(Option::unwrap_or_default).collect());
            }
        });
    }
}

// ----------------------------------------------------------------- status

/// One changed path, from `git status --porcelain=v2 -z`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStatus {
    pub path: String,
    /// The index (X) letter, `' '` when unchanged there.
    pub staged: char,
    /// The worktree (Y) letter, `' '` when unchanged there.
    pub unstaged: char,
    /// Where it was renamed or copied from, if anywhere.
    pub from: Option<String>,
    pub kind: EntryKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Changed,
    Unmerged,
    Untracked,
}

/// What `## branch.*` says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Head {
    /// None when HEAD is detached.
    pub branch: Option<String>,
    /// The commit, None before the first one.
    pub oid: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
}

impl Head {
    /// No commits yet.
    pub fn unborn(&self) -> bool {
        self.oid.is_none()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub head: Head,
    pub files: Vec<FileStatus>,
}

impl Status {
    pub fn untracked(&self) -> impl Iterator<Item = &FileStatus> {
        self.files.iter().filter(|f| f.kind == EntryKind::Untracked)
    }

    /// Unstaged changes, conflicts first among them (as Magit lists them).
    pub fn unstaged(&self) -> Vec<&FileStatus> {
        let mut v: Vec<&FileStatus> = self
            .files
            .iter()
            .filter(|f| f.kind == EntryKind::Unmerged)
            .collect();
        v.extend(
            self.files
                .iter()
                .filter(|f| f.kind == EntryKind::Changed && f.unstaged != ' '),
        );
        v
    }

    pub fn staged(&self) -> impl Iterator<Item = &FileStatus> {
        self.files
            .iter()
            .filter(|f| f.kind == EntryKind::Changed && f.staged != ' ')
    }
}

fn letter(c: char) -> char {
    if c == '.' { ' ' } else { c }
}

/// Parse `git status --porcelain=v2 --branch -z`. Records are NUL
/// separated; a rename (`2`) is followed by a record with its old path.
pub fn parse_status(out: &str) -> Status {
    let mut status = Status::default();
    let mut records = out.split('\0').filter(|r| !r.is_empty());
    while let Some(rec) = records.next() {
        if let Some(header) = rec.strip_prefix("# ") {
            let (key, value) = header.split_once(' ').unwrap_or((header, ""));
            match key {
                "branch.oid" => {
                    status.head.oid = (value != "(initial)").then(|| value.to_string())
                }
                "branch.head" => {
                    status.head.branch = (value != "(detached)").then(|| value.to_string())
                }
                "branch.upstream" => status.head.upstream = Some(value.to_string()),
                "branch.ab" => {
                    for part in value.split(' ') {
                        if let Some(n) = part.strip_prefix('+') {
                            status.head.ahead = n.parse().unwrap_or(0);
                        } else if let Some(n) = part.strip_prefix('-') {
                            status.head.behind = n.parse().unwrap_or(0);
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        let kind = rec.chars().next().unwrap_or(' ');
        // Fields before the path: the path is last and may hold spaces.
        let fields = match kind {
            '1' => 9,
            '2' => 10,
            'u' => 11,
            '?' => {
                status.files.push(FileStatus {
                    path: rec[2..].to_string(),
                    staged: '?',
                    unstaged: '?',
                    from: None,
                    kind: EntryKind::Untracked,
                });
                continue;
            }
            _ => continue, // '!' ignored, or something newer
        };
        let parts: Vec<&str> = rec.splitn(fields, ' ').collect();
        if parts.len() < fields {
            continue;
        }
        let mut xy = parts[1].chars();
        let (x, y) = (
            letter(xy.next().unwrap_or('.')),
            letter(xy.next().unwrap_or('.')),
        );
        let path = parts[fields - 1].to_string();
        let from = if kind == '2' {
            records.next().map(str::to_string)
        } else {
            None
        };
        status.files.push(FileStatus {
            path,
            staged: x,
            unstaged: y,
            from,
            kind: if kind == 'u' {
                EntryKind::Unmerged
            } else {
                EntryKind::Changed
            },
        });
    }
    status
}

/// What a file row says, Magit's words for a status letter.
pub fn change_label(f: &FileStatus, staged: bool) -> &'static str {
    if f.kind == EntryKind::Unmerged {
        return match (f.staged, f.unstaged) {
            ('D', 'D') => "both deleted",
            ('A', 'U') => "added by us",
            ('U', 'D') => "deleted by them",
            ('U', 'A') => "added by them",
            ('D', 'U') => "deleted by us",
            ('A', 'A') => "both added",
            _ => "unmerged",
        };
    }
    let c = if staged { f.staged } else { f.unstaged };
    match c {
        'M' => "modified",
        'A' => "new file",
        'D' => "deleted",
        'R' => "renamed",
        'C' => "copied",
        'T' => "typechange",
        _ => "changed",
    }
}

// ------------------------------------------------------- sequencer state

/// An operation in progress, read from `git status` (C locale).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sequence {
    pub op: Option<Op>,
    /// What `onto`/the commit being picked says.
    pub detail: Option<String>,
    /// The branch being rebased.
    pub branch: Option<String>,
    /// Rebase todo lines: done (newest last), then still to do.
    pub done: Vec<String>,
    pub todo: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Rebase,
    Merge,
    CherryPick,
    Revert,
    Bisect,
    Am,
}

impl Op {
    pub fn name(self) -> &'static str {
        match self {
            Op::Rebase => "Rebasing",
            Op::Merge => "Merging",
            Op::CherryPick => "Cherry-picking",
            Op::Revert => "Reverting",
            Op::Bisect => "Bisecting",
            Op::Am => "Applying patches",
        }
    }
}

/// Parse plain `git status` (LC_ALL=C) for the operation in progress.
pub fn parse_sequence(out: &str) -> Sequence {
    let mut seq = Sequence::default();
    #[derive(PartialEq)]
    enum List {
        None,
        Done,
        Todo,
    }
    let mut list = List::None;
    for line in out.lines() {
        let t = line.trim();
        if line.starts_with("   ") || line.starts_with('\t') {
            match list {
                List::Done if !t.starts_with('(') => seq.done.push(t.to_string()),
                List::Todo if !t.starts_with('(') => seq.todo.push(t.to_string()),
                _ => {}
            }
            continue;
        }
        list = List::None;
        if t.starts_with("Last command done") || t.starts_with("Last commands done") {
            list = List::Done;
        } else if t.starts_with("Next command to do") || t.starts_with("Next commands to do") {
            list = List::Todo;
        } else if t.contains("rebase in progress") {
            seq.op = Some(Op::Rebase);
            seq.detail = t.split_once("onto ").map(|(_, o)| o.trim().to_string());
        } else if t.starts_with("You are currently rebasing")
            || t.starts_with("You are currently editing")
            || t.starts_with("You are currently splitting")
        {
            seq.op.get_or_insert(Op::Rebase);
            // "... rebasing branch 'feature' on '4b313eb'."
            seq.branch = t
                .split_once("branch '")
                .and_then(|(_, rest)| rest.split_once('\''))
                .map(|(b, _)| b.to_string());
        } else if t.starts_with("You are currently cherry-picking commit") {
            seq.op = Some(Op::CherryPick);
            seq.detail = commit_word(t);
        } else if t.starts_with("You are currently reverting commit") {
            seq.op = Some(Op::Revert);
            seq.detail = commit_word(t);
        } else if t.starts_with("Cherry-pick currently in progress") {
            seq.op.get_or_insert(Op::CherryPick);
        } else if t.starts_with("Revert currently in progress") {
            seq.op.get_or_insert(Op::Revert);
        } else if t.starts_with("You are currently bisecting") {
            seq.op = Some(Op::Bisect);
        } else if t.starts_with("You are in the middle of an am session") {
            seq.op = Some(Op::Am);
        } else if (t.starts_with("All conflicts fixed but you are still merging")
            || t.starts_with("You have unmerged paths"))
            && seq.op.is_none()
        {
            // Settled for certain by MERGE_HEAD (see status.rs); this
            // covers a merge with conflicts left.
            seq.op = Some(Op::Merge);
        }
    }
    seq
}

fn commit_word(line: &str) -> Option<String> {
    line.split_whitespace()
        .last()
        .map(|w| w.trim_end_matches('.').to_string())
}

// -------------------------------------------------------------------- log

/// The `--format` every commit list uses: NUL between fields, one commit
/// per line (`%s` is the single-line subject).
pub const LOG_FORMAT: &str = "--format=%H%x00%h%x00%an%x00%ad%x00%D%x00%s";

/// One commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub hash: String,
    pub short: String,
    pub author: String,
    pub date: String,
    /// `HEAD -> main, origin/main, tag: v1`.
    pub refs: String,
    pub subject: String,
}

/// Parse [`LOG_FORMAT`] output (with `--date=short`).
pub fn parse_log(out: &str) -> Vec<LogEntry> {
    out.lines()
        .filter_map(|commit| {
            let mut f = commit.split('\0');
            Some(LogEntry {
                hash: f.next()?.to_string(),
                short: f.next()?.to_string(),
                author: f.next()?.to_string(),
                date: f.next()?.to_string(),
                refs: f.next()?.to_string(),
                subject: f.next().unwrap_or("").to_string(),
            })
        })
        .filter(|e| !e.hash.is_empty())
        .collect()
}

// ---------------------------------------------------------------- branches

/// The `for-each-ref` format [`parse_refs`] reads.
pub const REF_FORMAT: &str =
    "--format=%(refname)%00%(refname:short)%00%(HEAD)%00%(upstream:short)%00%(upstream:track)";

/// One branch, local or remote-tracking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branch {
    /// `main`, or `origin/main` for a remote one.
    pub name: String,
    pub remote: bool,
    pub current: bool,
    pub upstream: Option<String>,
    /// `[ahead 1, behind 2]`, `[gone]`, or empty.
    pub track: String,
}

/// Parse `git for-each-ref REF_FORMAT refs/heads refs/remotes`.
/// `origin/HEAD` (a pointer, not a branch) is left out.
pub fn parse_refs(out: &str) -> Vec<Branch> {
    out.lines()
        .filter_map(|line| {
            let mut f = line.split('\0');
            let full = f.next()?;
            let short = f.next()?.to_string();
            let head = f.next().unwrap_or("");
            let upstream = f.next().unwrap_or("");
            let track = f.next().unwrap_or("").to_string();
            let remote = full.starts_with("refs/remotes/");
            if remote && full.ends_with("/HEAD") {
                return None;
            }
            Some(Branch {
                name: short,
                remote,
                current: head == "*",
                upstream: (!upstream.is_empty()).then(|| upstream.to_string()),
                track,
            })
        })
        .collect()
}

// ------------------------------------------------------------------ stash

/// One stash, from `git stash list --format=%gd%x00%gs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stash {
    /// `stash@{0}`.
    pub name: String,
    pub message: String,
}

pub const STASH_FORMAT: &str = "--format=%gd%x00%gs";

pub fn parse_stash_list(out: &str) -> Vec<Stash> {
    out.lines()
        .filter_map(|line| {
            let (name, message) = line.split_once('\0')?;
            name.starts_with("stash@{").then(|| Stash {
                name: name.to_string(),
                message: message.to_string(),
            })
        })
        .collect()
}

// -------------------------------------------------------------- describe

/// `git describe --tags --long`: `v1.2-5-gabc1234` -> ("v1.2", 5).
pub fn parse_describe(out: &str) -> Option<(String, u32)> {
    let s = out.trim();
    let (rest, _hash) = s.rsplit_once('-')?;
    let (tag, n) = rest.rsplit_once('-')?;
    Some((tag.to_string(), n.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_v2_parses_headers_files_renames_conflicts_and_untracked() {
        let out = [
            "# branch.oid 1234567890abcdef",
            "# branch.head main",
            "# branch.upstream origin/main",
            "# branch.ab +2 -1",
            "1 M. N... 100644 100644 100644 aaa bbb staged.rs",
            "1 .M N... 100644 100644 100644 aaa bbb with space.rs",
            "2 R. N... 100644 100644 100644 aaa bbb R100 new.rs",
            "old.rs",
            "u UU N... 100644 100644 100644 100644 aaa bbb ccc both.rs",
            "? fresh.rs",
            "! target/",
            "",
        ]
        .join("\0");
        let s = parse_status(&out);
        assert_eq!(
            s.head,
            Head {
                branch: Some("main".into()),
                oid: Some("1234567890abcdef".into()),
                upstream: Some("origin/main".into()),
                ahead: 2,
                behind: 1,
            }
        );
        assert_eq!(s.files.len(), 5);
        assert_eq!(s.files[0].staged, 'M');
        assert_eq!(s.files[0].unstaged, ' ');
        assert_eq!(s.files[1].path, "with space.rs");
        assert_eq!(s.files[2].from.as_deref(), Some("old.rs"));
        assert_eq!(s.files[2].path, "new.rs");
        assert_eq!(s.files[3].kind, EntryKind::Unmerged);
        assert_eq!(change_label(&s.files[3], false), "unmerged");
        assert_eq!(s.files[4].kind, EntryKind::Untracked);
        assert_eq!(s.staged().count(), 2);
        let unstaged: Vec<_> = s.unstaged().iter().map(|f| f.path.clone()).collect();
        assert_eq!(unstaged, ["both.rs", "with space.rs"], "conflicts first");
        assert_eq!(s.untracked().count(), 1);
    }

    #[test]
    fn status_v2_unborn_and_detached() {
        let s = parse_status("# branch.oid (initial)\0# branch.head main\0");
        assert!(s.head.unborn());
        assert_eq!(s.head.branch.as_deref(), Some("main"));
        let s = parse_status("# branch.oid abc\0# branch.head (detached)\0");
        assert_eq!(s.head.branch, None);
        assert!(!s.head.unborn());
    }

    #[test]
    fn sequence_reads_rebase_with_todo() {
        let out = "interactive rebase in progress; onto 1a2b3c4
Last commands done (2 commands done):
   pick 1111111 First
   pick 2222222 Second
Next command to do (1 remaining command):
   pick 3333333 Third
  (use \"git rebase --edit-todo\" to view and edit)
You are currently rebasing branch 'main' on '1a2b3c4'.
";
        let s = parse_sequence(out);
        assert_eq!(s.op, Some(Op::Rebase));
        assert_eq!(s.detail.as_deref(), Some("1a2b3c4"));
        assert_eq!(s.done, ["pick 1111111 First", "pick 2222222 Second"]);
        assert_eq!(s.todo, ["pick 3333333 Third"]);
        assert_eq!(s.branch.as_deref(), Some("main"));
    }

    #[test]
    fn sequence_reads_cherry_pick_revert_and_merge() {
        let s = parse_sequence("You are currently cherry-picking commit abc1234.\n");
        assert_eq!(s.op, Some(Op::CherryPick));
        assert_eq!(s.detail.as_deref(), Some("abc1234"));
        let s = parse_sequence("You are currently reverting commit def5678.\n");
        assert_eq!(s.op, Some(Op::Revert));
        let s = parse_sequence("On branch main\nYou have unmerged paths.\n");
        assert_eq!(s.op, Some(Op::Merge));
        assert_eq!(parse_sequence("On branch main\nnothing to commit\n").op, None);
    }

    #[test]
    fn log_parses_fields_with_refs() {
        let line = ["abc123", "abc", "Ada", "2026-01-01", "HEAD -> main, tag: v1", "Subject"].join("\0");
        let log = parse_log(&format!("{line}\n"));
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].refs, "HEAD -> main, tag: v1");
        assert_eq!(log[0].subject, "Subject");
        assert!(parse_log("").is_empty());
    }

    #[test]
    fn refs_parse_local_remote_and_skip_remote_head() {
        let out = [
            "refs/heads/main\0main\0*\0origin/main\0[ahead 1]",
            "refs/heads/a-much-longer-name\0a-much-longer-name\0 \0\0",
            "refs/remotes/origin/HEAD\0origin\0 \0\0",
            "refs/remotes/origin/main\0origin/main\0 \0\0",
        ]
        .join("\n");
        let b = parse_refs(&out);
        assert_eq!(b.len(), 3);
        assert!(b[0].current);
        assert_eq!(b[0].upstream.as_deref(), Some("origin/main"));
        assert_eq!(b[0].track, "[ahead 1]");
        assert_eq!(b[1].name, "a-much-longer-name");
        assert!(!b[1].current);
        assert!(b[2].remote);
    }

    #[test]
    fn stash_list_and_describe() {
        let s = parse_stash_list("stash@{0}\0WIP on main: abc Subject\nstash@{1}\0On side: x\n");
        assert_eq!(s.len(), 2);
        assert_eq!(s[1].message, "On side: x");
        assert_eq!(parse_describe("v1.2-5-gabc1234\n"), Some(("v1.2".into(), 5)));
        assert_eq!(parse_describe("release-1-0-3-gabc"), Some(("release-1-0".into(), 3)));
        assert_eq!(parse_describe(""), None);
    }

    #[test]
    fn output_error_prefers_stderr_and_drops_hints() {
        let o = Output {
            code: Some(1),
            stdout: "out".into(),
            stderr: "error: nope\nhint: try this\n".into(),
        };
        assert_eq!(o.error(), "error: nope");
        let o = Output {
            code: Some(2),
            ..Default::default()
        };
        assert_eq!(o.error(), "git exited with code 2");
    }
}
