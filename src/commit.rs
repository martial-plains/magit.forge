// SPDX-License-Identifier: GPL-3.0-or-later
//! Writing a commit message: a buffer with the message on top and, below
//! a scissors line, what is being committed. `C-c C-c` commits with git's
//! own cleanup (comment lines go), `C-c C-k` gives up.
//!
//! The message reaches git on stdin (`-F -`), so it is never mangled as
//! an argument, and everything below the scissors line is cut off before
//! that, whatever `core.commentChar` says.

use crate::git::{self, Run};
use crate::popup::Ctx;
use crate::status;
use forge_plugin_sdk::{Buffer, Position, Span, message, y_or_n};
use std::cell::RefCell;
use std::collections::HashMap;

pub const SCISSORS: &str = "# ------------------------ >8 ------------------------";

/// A pending commit: where, and the arguments after `git commit`.
#[derive(Debug, Clone)]
struct Pending {
    root: String,
    args: Vec<String>,
}

thread_local! {
    static COMMITS: RefCell<HashMap<u32, Pending>> = RefCell::new(HashMap::new());
}

pub fn root_of(buffer: u32) -> Option<String> {
    COMMITS.with(|c| c.borrow().get(&buffer).map(|p| p.root.clone()))
}

pub fn forget(buffer: u32) {
    COMMITS.with(|c| c.borrow_mut().remove(&buffer));
}

/// What kind of commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Create,
    Amend,
    /// Amend only the message (`--only`: the staged changes stay staged).
    Reword,
}

/// The message part of a commit buffer: everything above the scissors.
pub fn message_of(text: &str) -> String {
    let mut out = Vec::new();
    for line in text.split('\n') {
        if line == SCISSORS {
            break;
        }
        out.push(line);
    }
    out.join("\n")
}

/// Is there anything but blank and comment lines in `msg`?
pub fn has_content(msg: &str) -> bool {
    msg.lines()
        .any(|l| !l.trim().is_empty() && !l.starts_with('#'))
}

/// The buffer text: the message, then the scissors and the summary.
pub fn buffer_text(message: &str, kind: Kind, branch: &str, summary: &str) -> String {
    let mut text = message.trim_end().to_string();
    text.push_str("\n\n");
    text.push_str(SCISSORS);
    text.push_str("\n# Do not modify or remove the line above; everything below it is ignored.\n");
    text.push_str("# C-c C-c commits, C-c C-k cancels.\n");
    let what = match kind {
        Kind::Create => "Committing",
        Kind::Amend => "Amending the last commit",
        Kind::Reword => "Rewording the last commit",
    };
    text.push_str(&format!("# {what} on {branch}.\n"));
    for l in summary.lines() {
        text.push_str("# ");
        text.push_str(l);
        text.push('\n');
    }
    text
}

/// Start a commit of `kind` from the popup context.
pub fn start(ctx: Ctx, kind: Kind) {
    let root = ctx.root.clone();
    // Is anything staged? Unstaged? (A fresh look: the status buffer may
    // be stale, or not open.)
    git::join(
        &root,
        vec![
            Run::new(&["diff", "--cached", "--quiet"]),
            Run::new(&["diff", "--quiet"]),
            Run::new(&["rev-parse", "--verify", "--quiet", "HEAD"]),
        ],
        move |outs| {
            let staged = outs[0].code == Some(1);
            let unstaged = outs[1].code == Some(1);
            let has_head = outs[2].ok();
            let mut ctx = ctx;
            if kind != Kind::Create && !has_head {
                message("Nothing to amend: there are no commits yet");
                return;
            }
            let all = ctx.has("--all") || ctx.has("--allow-empty");
            let merging = matches!(ctx.op, Some(git::Op::Merge | git::Op::CherryPick | git::Op::Revert));
            if kind == Kind::Create && !staged && !all && !merging {
                if unstaged {
                    y_or_n("Nothing staged. Commit all uncommitted changes? ", move |yes| {
                        if yes {
                            ctx.args.push("--all".to_string());
                            open_buffer(ctx, kind);
                        }
                    });
                } else {
                    message("Nothing to commit (stage with s, or use -e to allow an empty commit)");
                }
                return;
            }
            open_buffer(ctx, kind);
        },
    );
}

/// Read a file of git's (MERGE_MSG) through a buffer, then let the
/// buffer go again if it wasn't open before.
fn read_git_file(path: &str) -> Option<String> {
    let before: Vec<u32> = Buffer::all().iter().map(|b| b.id()).collect();
    let b = Buffer::visit(path).ok()?;
    let text = b.text();
    if !before.contains(&b.id()) {
        b.kill();
    }
    Some(text)
}

fn open_buffer(ctx: Ctx, kind: Kind) {
    let root = ctx.root.clone();
    let mut args: Vec<String> = Vec::new();
    match kind {
        Kind::Create => {}
        Kind::Amend => args.push("--amend".into()),
        Kind::Reword => args.extend(["--amend".into(), "--only".into()]),
    }
    args.extend(ctx.args.iter().cloned());
    let stat_args: Vec<String> = {
        let mut a: Vec<String> = vec!["diff".into(), "--cached".into(), "--stat".into(), "--no-color".into()];
        if ctx.has("--all") {
            a = vec!["diff".into(), "HEAD".into(), "--stat".into(), "--no-color".into()];
        }
        if kind != Kind::Create {
            // What the amended commit will hold: the last one's changes too.
            a = vec!["diff".into(), "--cached".into(), "--stat".into(), "--no-color".into(), "HEAD^".into()];
            if kind == Kind::Reword {
                a = vec!["show".into(), "--stat".into(), "--format=".into(), "--no-color".into(), "HEAD".into()];
            }
        }
        a
    };
    git::join(
        &root.clone(),
        vec![
            Run::new(&["log", "-1", "--format=%B", "HEAD"]),
            Run::new(&["rev-parse", "--git-path", "MERGE_MSG"]),
            Run::new(&["rev-parse", "--abbrev-ref", "HEAD"]),
            Run::owned(stat_args),
        ],
        move |outs| {
            let mut msg = String::new();
            if kind != Kind::Create {
                msg = outs[0].stdout.clone();
            } else if outs[1].ok() {
                let rel = outs[1].stdout.trim();
                let path = if rel.starts_with('/') {
                    rel.to_string()
                } else {
                    format!("{root}/{rel}")
                };
                // A merge, cherry-pick or revert left its message here.
                if ctx.op.is_some()
                    && let Some(text) = read_git_file(&path)
                {
                    msg = text;
                }
            }
            let branch = outs[2].stdout.trim().to_string();
            let short = root.rsplit('/').find(|p| !p.is_empty()).unwrap_or(&root);
            let name = match kind {
                Kind::Create => format!("*magit-commit: {short}*"),
                Kind::Amend => format!("*magit-commit (amend): {short}*"),
                Kind::Reword => format!("*magit-commit (reword): {short}*"),
            };
            let b = match Buffer::find(&name) {
                Some(b) => b,
                None => Buffer::create(&name),
            };
            let text = buffer_text(&message_of(&msg), kind, &branch, &outs[3].stdout);
            let _ = b.set_option("read-only", "");
            b.replace(
                Span {
                    start: Position { line: 0, column: 0 },
                    end: Position {
                        line: u32::MAX,
                        column: u32::MAX,
                    },
                },
                &text,
            );
            let _ = b.set_major_mode("magit-commit-mode");
            let _ = b.set_option("fill-column", "72");
            b.set_point(Position { line: 0, column: 0 });
            b.show();
            COMMITS.with(|c| {
                c.borrow_mut().insert(
                    b.id(),
                    Pending {
                        root: root.clone(),
                        args: args.clone(),
                    },
                )
            });
            message("C-c C-c to commit, C-c C-k to cancel");
        },
    );
}

/// Commit with the message in the current buffer.
pub fn finish() {
    let b = Buffer::current();
    let id = b.id();
    let Some(p) = COMMITS.with(|c| c.borrow().get(&id).cloned()) else {
        message("Not a Magit commit buffer");
        return;
    };
    let msg = message_of(&b.text());
    if !has_content(&msg) {
        message("Empty commit message (C-c C-k cancels)");
        return;
    }
    let mut args: Vec<String> = vec!["commit".into(), "--cleanup=strip".into(), "-F".into(), "-".into()];
    args.extend(p.args.iter().cloned());
    let root = p.root.clone();
    message("Committing...");
    git::exec(&p.root, Run::owned(args).input(&msg).logged(), move |out| {
        if !out.ok() {
            message(&format!("magit: commit failed: {}", out.error()));
            status::refresh_root(&root);
            return;
        }
        forget(id);
        if let Some(b) = Buffer::from_id(id).filter(|b| b.alive()) {
            b.kill();
        }
        // Back to the status buffer, as Magit does.
        if let Some(s) = status::buffer_for(&root).and_then(Buffer::from_id) {
            s.show();
        }
        let line = out
            .stdout
            .lines()
            .find(|l| l.trim_start().starts_with('['))
            .unwrap_or("Committed")
            .trim()
            .to_string();
        message(&line);
        status::refresh_root(&root);
    });
}

/// Give up on the commit in the current buffer.
pub fn cancel() {
    let b = Buffer::current();
    let id = b.id();
    let root = root_of(id);
    if root.is_none() {
        message("Not a Magit commit buffer");
        return;
    }
    forget(id);
    b.kill();
    if let Some(s) = root.and_then(|r| status::buffer_for(&r)).and_then(Buffer::from_id) {
        s.show();
    }
    message("Commit canceled");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_message_stops_at_the_scissors() {
        let text = buffer_text("Subject\n\n#123 is fixed\n", Kind::Create, "main", " a | 1 +\n");
        assert!(text.starts_with("Subject\n\n#123 is fixed\n\n# ------------------------ >8"));
        assert!(text.contains("# Committing on main.\n#  a | 1 +\n"));
        let msg = message_of(&format!("Typed q and more\n{text}"));
        assert_eq!(msg, "Typed q and more\nSubject\n\n#123 is fixed\n");
        assert!(!msg.contains("C-c C-c"));
        assert!(has_content(&msg));
        assert!(!has_content("\n# only a comment\n\n"));
    }
}
