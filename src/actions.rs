// SPDX-License-Identifier: GPL-3.0-or-later
//! What the keys do: staging, unstaging, discarding and reversing what
//! point (or the region) is on, visiting it, and the helpers every
//! command uses to run git, refresh, and ask for branches and commits.

use crate::diff;
use crate::git::{self, EntryKind, FileStatus, Run};
use crate::popup::{self, Ctx};
use crate::status::{self, Section, StatusLine};
use crate::views;
use forge_plugin_sdk::{Buffer, Position, completing_read, message, read_string, run_command, y_or_n};
use std::cell::RefCell;
use std::collections::HashSet;

thread_local! {
    /// Repositories with an index/worktree change running: another waits
    /// for it (and its refresh), so it never acts on a stale diff.
    static BUSY: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

/// The line worth showing of a failure: git's own error or conflict
/// line if it gave one, else its first line.
pub fn first_error(out: &git::Output) -> String {
    let e = out.error();
    let lines: Vec<&str> = e.lines().filter(|l| !l.trim().is_empty()).collect();
    let pick = lines
        .iter()
        .find(|l| l.starts_with("fatal:") || l.starts_with("error:") || l.starts_with("CONFLICT"))
        .or(lines.first())
        .copied()
        .unwrap_or("")
        .to_string();
    if lines.len() > 1 {
        format!("{pick} ($ shows all)")
    } else {
        pick
    }
}

/// Run a git that changes the index or worktree (stage, unstage,
/// discard), logged; refresh after. One at a time per repository.
pub fn mutate(root: &str, run: Run, what: &str) {
    let root = root.to_string();
    let busy = BUSY.with(|b| !b.borrow_mut().insert(root.clone()));
    if busy {
        message("Git is still busy with the last change; try again in a moment");
        return;
    }
    let what = what.to_string();
    git::exec(&root.clone(), run.logged(), move |out| {
        if !out.ok() {
            message(&format!("magit: {what} failed: {}", first_error(&out)));
        }
        done(&root);
    });
}

/// Several changes in a row, stopping at the first failure.
pub fn mutate_all(root: &str, runs: Vec<Run>, what: &str) {
    let root = root.to_string();
    let busy = BUSY.with(|b| !b.borrow_mut().insert(root.clone()));
    if busy {
        message("Git is still busy with the last change; try again in a moment");
        return;
    }
    chain(root, runs, what.to_string());
}

fn chain(root: String, mut runs: Vec<Run>, what: String) {
    if runs.is_empty() {
        done(&root);
        return;
    }
    let run = runs.remove(0);
    let r = root.clone();
    git::exec(&root, run.logged(), move |out| {
        if out.ok() {
            chain(r, runs, what);
        } else {
            message(&format!("magit: {what} failed: {}", first_error(&out)));
            done(&r);
        }
    });
}

/// A change finished: refresh, then let the next one in.
fn done(root: &str) {
    let r = root.to_string();
    let release = move || {
        BUSY.with(|b| b.borrow_mut().remove(&r));
    };
    match status::buffer_for(root) {
        Some(b) => status::refresh_then(b, release),
        None => release(),
    }
}

/// Run a logged git that isn't a local index change (commit, push,
/// checkout, ...), then refresh. `quiet` keeps success silent; otherwise
/// git's own last word is shown.
pub fn run_and_refresh(root: &str, args: Vec<String>, what: &str, quiet: bool) {
    run_then(root, args, what, quiet, |_| {});
}

/// Like run_and_refresh, then `then` with whether it worked.
pub fn run_then(root: &str, args: Vec<String>, what: &str, quiet: bool, then: impl FnOnce(bool) + 'static) {
    let (r, what) = (root.to_string(), what.to_string());
    if !quiet {
        message(&format!("Running git {}...", args.first().map(String::as_str).unwrap_or("")));
    }
    git::exec(root, Run::owned(args).logged(), move |out| {
        if out.ok() {
            if !quiet {
                // Git's last word: on stderr for most commands (push,
                // rebase, checkout), else on stdout.
                let last = |s: &str| {
                    s.replace('\r', "\n")
                        .lines()
                        .map(str::trim)
                        .rfind(|l| !l.is_empty() && !l.starts_with("hint:"))
                        .map(str::to_string)
                };
                // A new commit's "[main abc1234] Subject" says it best.
                let commit_line = out
                    .stdout
                    .lines()
                    .map(str::trim)
                    .find(|l| l.starts_with('[') && l.contains(']'))
                    .map(str::to_string);
                let said = commit_line
                    .or_else(|| last(&out.stderr))
                    .or_else(|| last(&out.stdout))
                    .unwrap_or_default();
                message(&if said.is_empty() {
                    format!("git {what}: done")
                } else {
                    said
                });
            }
        } else {
            message(&format!("magit: {what} failed: {}", first_error(&out)));
        }
        status::refresh_root(&r);
        // Views of this repository may show stale commits now; the user
        // refreshes them with g.
        then(out.ok());
    });
}

/// `["a", "b"]` as owned args.
pub fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

/// Paths as NUL-separated pathspecs on stdin (`--pathspec-from-file=-
/// --pathspec-file-nul`): no argument-length limit, any file name.
fn pathspec_run(mut args: Vec<String>, paths: &[String]) -> Run {
    args.extend(["--pathspec-from-file=-".into(), "--pathspec-file-nul".into()]);
    let mut input = String::new();
    for p in paths {
        input.push_str(":(literal)");
        input.push_str(p);
        input.push('\0');
    }
    Run::owned(args).input(&input)
}

// ------------------------------------------------------------- prompting

/// `prompt` with "(default X)" before its colon.
pub fn with_default(prompt: &str, default: Option<&str>) -> String {
    match default {
        Some(d) => {
            let base = prompt.trim_end().trim_end_matches(':');
            format!("{base} (default {d}): ")
        }
        None => prompt.to_string(),
    }
}

/// Complete over `candidates`; an empty answer takes `default`. With
/// `strict`, only a candidate is accepted.
pub fn choose(
    prompt: &str,
    candidates: Vec<String>,
    default: Option<String>,
    strict: bool,
    then: impl FnOnce(String) + 'static,
) {
    let shown = with_default(prompt, default.as_deref());
    let cands = candidates.clone();
    completing_read(&shown, &candidates, false, move |answer| {
        let Some(answer) = answer else { return };
        let answer = answer.trim().to_string();
        let pick = if answer.is_empty() { default } else { Some(answer) };
        let Some(pick) = pick else { return };
        if strict && !cands.contains(&pick) {
            message(&format!("No such choice: {pick}"));
            return;
        }
        then(pick);
    });
}

/// Branch names (local first, the current one first of all), then remote
/// ones. `skip_current` leaves the current branch out (checkout, merge,
/// delete); the first name left is the default. With `strict`, only an
/// existing branch is taken; otherwise any revision can be typed.
pub fn read_branch(
    root: &str,
    prompt: &str,
    remote: bool,
    strict: bool,
    skip_current: bool,
    then: impl FnOnce(String) + 'static,
) {
    let prompt = prompt.to_string();
    let mut a = vec!["for-each-ref", git::REF_FORMAT, "refs/heads"];
    if remote {
        a.push("refs/remotes");
    }
    git::run(root, &a, move |result| {
        let mut branches = git::parse_refs(&result.unwrap_or_default());
        branches.sort_by_key(|b| (!b.current, b.remote));
        let names: Vec<String> = branches
            .into_iter()
            .filter(|b| !(skip_current && b.current))
            .map(|b| b.name)
            .collect();
        let default = names.first().cloned();
        choose(&prompt, names, default, strict, then);
    });
}

/// A remote's name (default: origin, or the only one).
pub fn read_remote(root: &str, prompt: &str, then: impl FnOnce(String) + 'static) {
    let prompt = prompt.to_string();
    git::run(root, &["remote"], move |result| {
        let remotes: Vec<String> = result
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        if remotes.is_empty() {
            message("This repository has no remotes (M a adds one)");
            return;
        }
        let default = remotes
            .iter()
            .find(|r| *r == "origin")
            .or(remotes.first())
            .cloned();
        choose(&prompt, remotes, default, true, then);
    });
}

/// A commit: the one at point, or one named (branches complete).
pub fn read_commit(ctx: &Ctx, prompt: &str, then: impl FnOnce(String) + 'static) {
    if let Some(c) = ctx.commit.clone().filter(|c| !c.starts_with("stash@")) {
        then(c);
        return;
    }
    read_branch(&ctx.root, prompt, true, false, false, then);
}

/// A string, not empty.
pub fn read_nonempty(prompt: &str, initial: &str, then: impl FnOnce(String) + 'static) {
    read_string(prompt, initial, move |s| {
        if let Some(s) = s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
            then(s);
        }
    });
}

// ------------------------------------------------- what the keys act on

/// What a status command acts on: point's line, or the files (or lines
/// of one hunk) the region covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    Heading(Section),
    /// Files of one section.
    Files(Section, Vec<String>),
    /// A hunk, or some lines of it (body indices, inclusive).
    Hunk {
        section: Section,
        path: String,
        index: usize,
        lines: Option<(usize, usize)>,
    },
    Other(StatusLine),
}

/// Work out the selection from the rendered lines, point's row and the
/// region's rows.
pub fn selection(lines: &[StatusLine], row: usize, region: Option<(usize, usize)>) -> Option<Selection> {
    let at = lines.get(row)?.clone();
    if let Some((a, z)) = region {
        let picked = &lines[a.min(lines.len() - 1)..=z.min(lines.len() - 1)];
        // Lines of one hunk: just those lines.
        let mut hunk: Option<(Section, String, usize)> = None;
        let mut body = Vec::new();
        let mut one_hunk = true;
        for l in picked {
            match l {
                StatusLine::Hunk {
                    section,
                    path,
                    index,
                    line,
                } => {
                    let key = (*section, path.clone(), *index);
                    match &hunk {
                        Some(h) if *h != key => one_hunk = false,
                        _ => hunk = Some(key),
                    }
                    if let Some(i) = line {
                        body.push(*i);
                    }
                }
                _ => one_hunk = false,
            }
        }
        if one_hunk
            && let Some((section, path, index)) = hunk
            && !body.is_empty()
        {
            return Some(Selection::Hunk {
                section,
                path,
                index,
                lines: Some((*body.iter().min().unwrap(), *body.iter().max().unwrap())),
            });
        }
        // Files: every file of point's section the region touches.
        if let Some(section) = at.section().filter(|s| s.has_files()) {
            let mut paths: Vec<String> = Vec::new();
            for l in picked {
                if l.section() == Some(section)
                    && let Some(p) = l.path()
                    && !paths.iter().any(|x| x == p)
                {
                    paths.push(p.to_string());
                }
            }
            if !paths.is_empty() {
                return Some(Selection::Files(section, paths));
            }
        }
    }
    Some(match at {
        StatusLine::Heading(s) => Selection::Heading(s),
        StatusLine::File { section, path } => Selection::Files(section, vec![path]),
        StatusLine::Hunk {
            section,
            path,
            index,
            ..
        } => Selection::Hunk {
            section,
            path,
            index,
            lines: None,
        },
        other => Selection::Other(other),
    })
}

/// The selection in the current status buffer.
fn current_selection() -> Option<(u32, String, Selection)> {
    let b = Buffer::current();
    let id = b.id();
    let root = status::root_of(id)?;
    let row = b.point().line as usize;
    let region = status::region_rows(id);
    let sel = status::with_view(id, |v| selection(&v.lines, row, region)).flatten()?;
    Some((id, root, sel))
}

/// The patch for a hunk selection, from the diff on screen.
fn hunk_patch(buffer: u32, section: Section, path: &str, index: usize, lines: Option<(usize, usize)>, reverse: bool) -> Result<String, &'static str> {
    status::with_view(buffer, |v| {
        let d = v.expanded.get(&(section, path.to_string())).ok_or("Hunk moved; refresh with g")?;
        let h = d.hunks.get(index).ok_or("Hunk moved; refresh with g")?;
        if h.combined() {
            return Err("That's a conflict: edit the file, then stage it with s on its row");
        }
        match lines {
            None => Ok(diff::hunk_patch(&d.header, h)),
            Some((a, z)) => diff::partial_patch(&d.header, h, a, z, reverse).ok_or("No changed lines selected"),
        }
    })
    .unwrap_or(Err("Not in a status buffer"))
}

fn apply_run(patch: &str, cached: bool, reverse: bool, check: bool) -> Run {
    let mut a = vec!["apply".to_string()];
    if cached {
        a.push("--cached".into());
    }
    if reverse {
        a.push("--reverse".into());
    }
    if check {
        a.push("--check".into());
    }
    if diff::zero_context(patch) {
        a.push("--unidiff-zero".into());
    }
    a.push("-".into());
    Run::owned(a).input(patch)
}

/// The files of `section` (the heading's whole list).
fn section_files(buffer: u32, section: Section) -> Vec<FileStatus> {
    status::with_view(buffer, |v| v.files(section).into_iter().cloned().collect()).unwrap_or_default()
}

fn file_entries(buffer: u32, section: Section, paths: &[String]) -> Vec<FileStatus> {
    status::with_view(buffer, |v| {
        paths.iter().filter_map(|p| v.file(section, p).cloned()).collect()
    })
    .unwrap_or_default()
}

fn unborn(buffer: u32) -> bool {
    status::with_view(buffer, |v| v.status.head.unborn()).unwrap_or(false)
}

fn describe(paths: &[String]) -> String {
    match paths {
        [one] => one.clone(),
        many => format!("{} files", many.len()),
    }
}

// ------------------------------------------------------------------ stage

pub fn stage() {
    let Some((id, root, sel)) = current_selection() else {
        message("Not in a Magit status buffer");
        return;
    };
    match sel {
        Selection::Heading(Section::Untracked) => {
            let paths: Vec<String> = section_files(id, Section::Untracked).into_iter().map(|f| f.path).collect();
            mutate(&root, pathspec_run(args(&["add", "--"]), &paths), "stage");
        }
        Selection::Heading(Section::Unstaged) => mutate(&root, Run::new(&["add", "-u"]), "stage"),
        Selection::Files(Section::Untracked | Section::Unstaged, paths) => {
            mutate(&root, pathspec_run(args(&["add", "-A"]), &paths), "stage")
        }
        Selection::Hunk {
            section: Section::Untracked,
            path,
            lines: None,
            ..
        } => mutate(&root, pathspec_run(args(&["add"]), &[path]), "stage"),
        Selection::Hunk {
            section: section @ (Section::Unstaged | Section::Untracked),
            path,
            index,
            lines,
        } => match hunk_patch(id, section, &path, index, lines, false) {
            Ok(p) => mutate(&root, apply_run(&p, true, false, false), "stage"),
            Err(e) => message(e),
        },
        Selection::Heading(Section::Staged)
        | Selection::Files(Section::Staged, _)
        | Selection::Hunk {
            section: Section::Staged,
            ..
        } => message("Already staged"),
        _ => message("Nothing to stage here"),
    }
}

pub fn stage_all() {
    let Some((_, root, _)) = current_selection() else {
        message("Not in a Magit status buffer");
        return;
    };
    // Magit's S: everything tracked; untracked files stay (stage them
    // with s on their section).
    mutate(&root, Run::new(&["add", "-u"]), "stage all");
}

// ---------------------------------------------------------------- unstage

pub fn unstage() {
    let Some((id, root, sel)) = current_selection() else {
        message("Not in a Magit status buffer");
        return;
    };
    match sel {
        Selection::Heading(Section::Staged) => unstage_files(id, &root, None),
        Selection::Files(Section::Staged, paths) => unstage_files(id, &root, Some(paths)),
        Selection::Hunk {
            section: Section::Staged,
            path,
            index,
            lines,
        } => match hunk_patch(id, Section::Staged, &path, index, lines, true) {
            Ok(p) => mutate(&root, apply_run(&p, true, true, false), "unstage"),
            Err(e) => message(e),
        },
        Selection::Heading(Section::Untracked | Section::Unstaged)
        | Selection::Files(..)
        | Selection::Hunk { .. } => message("Already unstaged"),
        _ => message("Nothing to unstage here"),
    }
}

/// Unstage files (None: all). Before the first commit there is no HEAD
/// to restore from: the files leave the index instead.
fn unstage_files(id: u32, root: &str, paths: Option<Vec<String>>) {
    let entries = match &paths {
        Some(p) => file_entries(id, Section::Staged, p),
        None => section_files(id, Section::Staged),
    };
    let mut all: Vec<String> = Vec::new();
    for f in &entries {
        all.push(f.path.clone());
        if let Some(from) = &f.from {
            all.push(from.clone());
        }
    }
    if all.is_empty() {
        return;
    }
    if unborn(id) {
        mutate(root, pathspec_run(args(&["rm", "--cached", "-r", "-q"]), &all), "unstage");
    } else if paths.is_none() {
        mutate(root, Run::new(&["reset", "-q", "--"]), "unstage all");
    } else {
        mutate(root, pathspec_run(args(&["restore", "--staged"]), &all), "unstage");
    }
}

pub fn unstage_all() {
    let Some((id, root, _)) = current_selection() else {
        message("Not in a Magit status buffer");
        return;
    };
    unstage_files(id, &root, None);
}

// ---------------------------------------------------------------- discard

pub fn discard() {
    let Some((id, root, sel)) = current_selection() else {
        message("Not in a Magit status buffer");
        return;
    };
    match sel {
        Selection::Heading(Section::Untracked) => {
            let paths: Vec<String> = section_files(id, Section::Untracked).into_iter().map(|f| f.path).collect();
            discard_untracked(&root, paths);
        }
        Selection::Files(Section::Untracked, paths) => discard_untracked(&root, paths),
        Selection::Hunk {
            section: Section::Untracked,
            path,
            ..
        } => discard_untracked(&root, vec![path]),
        Selection::Heading(Section::Unstaged) => {
            let entries = section_files(id, Section::Unstaged);
            discard_unstaged(&root, entries);
        }
        Selection::Files(Section::Unstaged, paths) => {
            discard_unstaged(&root, file_entries(id, Section::Unstaged, &paths))
        }
        Selection::Heading(Section::Staged) => discard_staged(&root, section_files(id, Section::Staged), unborn(id)),
        Selection::Files(Section::Staged, paths) => {
            discard_staged(&root, file_entries(id, Section::Staged, &paths), unborn(id))
        }
        Selection::Hunk {
            section,
            path,
            index,
            lines,
        } => {
            let patch = match hunk_patch(id, section, &path, index, lines, true) {
                Ok(p) => p,
                Err(e) => {
                    message(e);
                    return;
                }
            };
            let staged = section == Section::Staged;
            let what = if lines.is_some() { "these lines" } else { "this hunk" };
            let root = root.clone();
            y_or_n(&format!("Discard {what} of {path}? "), move |yes| {
                if !yes {
                    return;
                }
                if staged {
                    // Out of the index and the worktree; check the worktree
                    // first, so nothing is half done.
                    mutate_all(
                        &root,
                        vec![
                            apply_run(&patch, false, true, true),
                            apply_run(&patch, true, true, false),
                            apply_run(&patch, false, true, false),
                        ],
                        "discard",
                    );
                } else {
                    mutate(&root, apply_run(&patch, false, true, false), "discard");
                }
            });
        }
        Selection::Other(StatusLine::Stash { name }) => {
            let root = root.clone();
            y_or_n(&format!("Drop {name}? "), move |yes| {
                if yes {
                    run_and_refresh(&root, args(&["stash", "drop", &name]), "stash drop", false);
                }
            });
        }
        _ => message("Nothing to discard here"),
    }
}

fn discard_untracked(root: &str, paths: Vec<String>) {
    if paths.is_empty() {
        return;
    }
    let root = root.to_string();
    y_or_n(&format!("Delete {}? ", describe(&paths)), move |yes| {
        if yes {
            let mut a = args(&["clean", "-f", "-q", "--"]);
            a.extend(paths);
            mutate(&root, Run::owned(a), "delete");
        }
    });
}

fn discard_unstaged(root: &str, entries: Vec<FileStatus>) {
    let (conflicts, plain): (Vec<FileStatus>, Vec<FileStatus>) =
        entries.into_iter().partition(|f| f.kind == EntryKind::Unmerged);
    let root = root.to_string();
    if let [conflict] = conflicts.as_slice()
        && plain.is_empty()
    {
        let path = conflict.path.clone();
        let choices = vec!["ours".to_string(), "theirs".to_string()];
        choose(&format!("Resolve {path} using: "), choices, None, true, move |side| {
            mutate_all(
                &root,
                vec![
                    Run::owned(args(&["checkout", &format!("--{side}"), "--", &path])),
                    Run::owned(args(&["add", "--", &path])),
                ],
                "resolve",
            );
        });
        return;
    }
    if !conflicts.is_empty() {
        message("Conflicted files are resolved one at a time (k on the file)");
    }
    let paths: Vec<String> = plain.into_iter().map(|f| f.path).collect();
    if paths.is_empty() {
        return;
    }
    y_or_n(&format!("Discard unstaged changes to {}? ", describe(&paths)), move |yes| {
        if yes {
            mutate(&root, pathspec_run(args(&["restore", "--worktree"]), &paths), "discard");
        }
    });
}

fn discard_staged(root: &str, entries: Vec<FileStatus>, unborn: bool) {
    if entries.is_empty() {
        return;
    }
    let (added, changed): (Vec<FileStatus>, Vec<FileStatus>) =
        entries.into_iter().partition(|f| unborn || f.staged == 'A');
    let mut paths: Vec<String> = added.iter().chain(changed.iter()).map(|f| f.path.clone()).collect();
    paths.dedup();
    let root = root.to_string();
    y_or_n(
        &format!("Discard staged and unstaged changes to {}? ", describe(&paths)),
        move |yes| {
            if !yes {
                return;
            }
            let mut runs = Vec::new();
            if !added.is_empty() {
                let p: Vec<String> = added.iter().map(|f| f.path.clone()).collect();
                runs.push(pathspec_run(args(&["rm", "-f", "-q", "-r"]), &p));
            }
            if !changed.is_empty() {
                let mut p: Vec<String> = Vec::new();
                for f in &changed {
                    p.push(f.path.clone());
                    if let Some(from) = &f.from {
                        p.push(from.clone());
                    }
                }
                runs.push(pathspec_run(
                    args(&["restore", "--source=HEAD", "--staged", "--worktree"]),
                    &p,
                ));
            }
            mutate_all(&root, runs, "discard");
        },
    );
}

/// `v`: take a staged change back out of the worktree, leaving the index.
pub fn reverse() {
    let Some((id, root, sel)) = current_selection() else {
        message("Not in a Magit status buffer");
        return;
    };
    match sel {
        Selection::Hunk {
            section: Section::Staged,
            path,
            index,
            lines,
        } => match hunk_patch(id, Section::Staged, &path, index, lines, true) {
            Ok(p) => {
                let root = root.clone();
                y_or_n(&format!("Reverse this change to {path} in the worktree? "), move |yes| {
                    if yes {
                        mutate(&root, apply_run(&p, false, true, false), "reverse");
                    }
                })
            }
            Err(e) => message(e),
        },
        Selection::Files(Section::Staged, paths) => {
            let root = root.clone();
            y_or_n(&format!("Reverse the staged changes to {} in the worktree? ", describe(&paths)), move |yes| {
                if !yes {
                    return;
                }
                let mut a: Vec<String> = vec!["diff".into(), "--cached".into()];
                a.extend(diff::DIFF_ARGS.iter().map(|s| s.to_string()));
                a.push("--".into());
                a.extend(paths);
                let r = root.clone();
                git::exec(&root, Run::owned(a), move |out| match out.result() {
                    Ok(patch) if !patch.is_empty() => mutate(&r, apply_run(&patch, false, true, false), "reverse"),
                    Ok(_) => message("Nothing to reverse"),
                    Err(e) => message(&format!("magit: diff failed: {e}")),
                });
            });
        }
        _ => message("Reverse works on staged changes (v on a staged file or hunk)"),
    }
}

// ------------------------------------------------------------------ visit

/// Open `path` (relative to `root`) at `line`.
pub fn visit_file(root: &str, path: &str, line: usize) {
    let full = format!("{}/{path}", root.trim_end_matches('/'));
    match Buffer::visit(&full) {
        Ok(b) => {
            b.show();
            b.set_point(Position {
                line: line.saturating_sub(1) as u32,
                column: 0,
            });
        }
        Err(e) => message(&format!("magit: cannot visit {path}: {e}")),
    }
}

/// RET.
pub fn visit() {
    let b = Buffer::current();
    let id = b.id();
    let row = b.point().line as usize;
    if let Some(root) = status::root_of(id) {
        let Some(t) = status::point_target(id) else { return };
        match t {
            StatusLine::Heading(_) => status::toggle(id),
            StatusLine::File { section, path } => {
                let deleted = status::with_view(id, |v| {
                    v.file(section, &path).is_some_and(|f| {
                        (section == Section::Staged && f.staged == 'D') || (section == Section::Unstaged && f.unstaged == 'D')
                    })
                })
                .unwrap_or(false);
                if deleted {
                    message(&format!("{path} is deleted"));
                } else {
                    visit_file(&root, &path, 1);
                }
            }
            StatusLine::Hunk {
                section,
                path,
                index,
                line,
            } => {
                let at = status::with_view(id, |v| {
                    v.expanded
                        .get(&(section, path.clone()))
                        .and_then(|d| d.hunks.get(index))
                        .map(|h| h.file_line(line))
                })
                .flatten()
                .unwrap_or(1);
                visit_file(&root, &path, at);
            }
            StatusLine::Stash { name } => views::show_stash(&root, &name),
            StatusLine::Commit { hash, .. } => views::show_commit(&root, &hash),
            StatusLine::Info { hash: Some(h) } => views::show_commit(&root, &h),
            _ => message("Nothing to visit here"),
        }
        return;
    }
    if let Some(v) = views::view(id) {
        match views::target(id, row) {
            Some(views::Target::Commit(h)) => views::show_commit(&v.root, &h),
            Some(views::Target::File(p, l)) => visit_file(&v.root, &p, l),
            None => message("Nothing to visit here"),
        }
        return;
    }
    message("Not in a Magit buffer");
}

// ---------------------------------------------------------------- ignore

/// Append `rule` to the ignore file at `path` (relative to root, or
/// absolute): visit it, add the line, save. A missing file is created
/// with git's help (blob, index, checkout-index), then left untracked.
pub fn add_ignore_rule(root: &str, path: &str, rule: &str) {
    let full = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("{}/{path}", root.trim_end_matches('/'))
    };
    match Buffer::visit(&full) {
        Ok(b) => {
            let text = b.text();
            let mut add = String::new();
            if !text.is_empty() && !text.ends_with('\n') {
                add.push('\n');
            }
            add.push_str(rule);
            add.push('\n');
            let end = Position {
                line: b.line_count(),
                column: 0,
            };
            b.replace(forge_plugin_sdk::Span { start: end, end }, &add);
            b.show();
            let _ = run_command("save-buffer");
            message(&format!("Added {rule} to {path}"));
        }
        Err(_) if !path.starts_with('/') && !path.contains("/.git/") => {
            // No such file yet: have git write it.
            let (r, p, content) = (root.to_string(), path.to_string(), format!("{rule}\n"));
            git::exec(
                root,
                Run::new(&["hash-object", "-w", "--stdin"]).input(&content),
                move |out| {
                    let Ok(blob) = out.result() else {
                        message("magit: could not create the ignore file");
                        return;
                    };
                    let info = format!("100644,{},{p}", blob.trim());
                    mutate_all(
                        &r,
                        vec![
                            Run::owned(args(&["update-index", "--add", "--cacheinfo", &info])),
                            Run::owned(args(&["checkout-index", "-f", "--", &p])),
                            Run::owned(args(&["rm", "--cached", "-q", "--", &p])),
                        ],
                        "write the ignore file",
                    );
                    message(&format!("Created {p} with {}", content.trim()));
                },
            );
        }
        Err(e) => message(&format!("magit: cannot open {path}: {e}")),
    }
}

/// `i t` / `i p`: ignore the file at point (or a typed pattern).
pub fn ignore(ctx: Ctx, private: bool) {
    let initial = ctx.file.as_ref().map(|f| format!("/{f}")).unwrap_or_default();
    let root = ctx.root.clone();
    read_nonempty("Ignore (pattern): ", &initial, move |rule| {
        if private {
            let r = root.clone();
            git::run(&root, &["rev-parse", "--git-path", "info/exclude"], move |result| match result {
                Ok(p) => add_ignore_rule(&r, p.trim(), &rule),
                Err(e) => message(&format!("magit: {e}")),
            });
        } else {
            add_ignore_rule(&root, ".gitignore", &rule);
        }
    });
}

// ----------------------------------------------------------- refs, run

/// `y`: branches and tags, RET shows one.
pub fn show_refs(ctx: Ctx) {
    let root = ctx.root.clone();
    git::run(
        &ctx.root,
        &[
            "for-each-ref",
            "--format=%(refname)%00%(refname:short)%00%(objectname:short)%00%(HEAD)%00%(upstream:short)%00%(subject)",
            "refs/heads",
            "refs/remotes",
            "refs/tags",
        ],
        move |result| match result {
            Ok(out) => {
                let mut text = String::from("References\n");
                let mut last_kind = "";
                for line in out.lines() {
                    let f: Vec<&str> = line.split('\0').collect();
                    if f.len() < 6 || f[0].ends_with("/HEAD") {
                        continue;
                    }
                    let kind = if f[0].starts_with("refs/heads/") {
                        "Branches"
                    } else if f[0].starts_with("refs/remotes/") {
                        "Remote branches"
                    } else {
                        "Tags"
                    };
                    if kind != last_kind {
                        text.push_str(&format!("\n{kind}\n"));
                        last_kind = kind;
                    }
                    let up = if f[4].is_empty() { String::new() } else { format!(" [{}]", f[4]) };
                    text.push_str(&format!("{} {} {}{} {}\n", if f[3] == "*" { "*" } else { " " }, f[2], f[1], up, f[5]));
                }
                views::show_refs_text(&root, &text);
            }
            Err(e) => message(&format!("magit: {e}")),
        },
    );
}

/// `!`: run any git command in the repository, then show what it said.
pub fn run_git(ctx: Ctx) {
    let root = ctx.root.clone();
    read_nonempty("Run git (in the repository): ", "", move |line| {
        let argv = split_args(&line);
        let argv: Vec<String> = match argv.first().map(String::as_str) {
            Some("git") => argv[1..].to_vec(),
            _ => argv,
        };
        let r = root.clone();
        git::exec(&root, Run::owned(argv).logged(), move |out| {
            if !out.ok() {
                message(&format!("magit: git failed: {}", first_error(&out)));
            }
            views::show_process_log(&r);
            status::refresh_root(&r);
        });
    });
}

/// Split a command line on blanks, honouring '...' and "..." quotes.
pub fn split_args(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut any = false;
    for c in line.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                any = true;
            }
            (None, c) if c.is_whitespace() => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}

// ------------------------------------------------------------- blame

/// Blame the file in context: the file at point, or the file being
/// visited (at its line).
pub fn blame(ctx: Ctx) {
    match &ctx.file {
        Some(f) => views::show_blame(&ctx.root, f, ctx.line),
        None => message("No file here to blame"),
    }
}

/// `x`: reset HEAD (mixed) to the commit at point.
pub fn reset_quickly(ctx: Ctx) {
    let Some(c) = ctx.commit.clone().filter(|c| !c.starts_with("stash@")) else {
        popup_reset(ctx);
        return;
    };
    let root = ctx.root.clone();
    y_or_n(&format!("Reset HEAD to {} (keeping changes in the worktree)? ", &c[..c.len().min(12)]), move |yes| {
        if yes {
            run_and_refresh(&root, args(&["reset", "--mixed", &c]), "reset", false);
        }
    });
}

fn popup_reset(_ctx: Ctx) {
    if let Some(p) = popup::all().into_iter().find(|p| p.id == "reset") {
        popup::open(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines() -> Vec<StatusLine> {
        let f = |p: &str| StatusLine::File {
            section: Section::Unstaged,
            path: p.into(),
        };
        let h = |line: Option<usize>| StatusLine::Hunk {
            section: Section::Unstaged,
            path: "a".into(),
            index: 0,
            line,
        };
        vec![
            StatusLine::Heading(Section::Unstaged),
            f("a"),
            h(None),
            h(Some(0)),
            h(Some(1)),
            h(Some(2)),
            f("b"),
            f("c"),
            StatusLine::Blank,
        ]
    }

    #[test]
    fn point_alone_selects_its_line() {
        let l = lines();
        assert_eq!(selection(&l, 1, None), Some(Selection::Files(Section::Unstaged, vec!["a".into()])));
        assert_eq!(selection(&l, 0, None), Some(Selection::Heading(Section::Unstaged)));
        assert_eq!(
            selection(&l, 4, None),
            Some(Selection::Hunk { section: Section::Unstaged, path: "a".into(), index: 0, lines: None })
        );
    }

    #[test]
    fn a_region_in_one_hunk_selects_lines() {
        let l = lines();
        assert_eq!(
            selection(&l, 5, Some((4, 5))),
            Some(Selection::Hunk { section: Section::Unstaged, path: "a".into(), index: 0, lines: Some((1, 2)) })
        );
    }

    #[test]
    fn a_region_over_files_selects_them() {
        let l = lines();
        assert_eq!(
            selection(&l, 7, Some((6, 7))),
            Some(Selection::Files(Section::Unstaged, vec!["b".into(), "c".into()]))
        );
        assert_eq!(
            selection(&l, 6, Some((3, 6))),
            Some(Selection::Files(Section::Unstaged, vec!["a".into(), "b".into()]))
        );
    }

    #[test]
    fn split_args_honours_quotes() {
        assert_eq!(split_args(r#"commit -m "two words" --x ''"#), ["commit", "-m", "two words", "--x", ""]);
        assert_eq!(split_args("  log   -1 "), ["log", "-1"]);
    }
}
