// SPDX-License-Identifier: GPL-3.0-or-later
//! Transient popups, the way Magit's prefix keys work: `P` opens the
//! push popup, `-f` toggles `--force-with-lease`, `p` pushes with what's
//! switched on. The popup is a transient keymap over everything else
//! while it is open, and the echo area lists its switches and actions.
//!
//! An action runs with the context captured when the popup opened: the
//! repository, what point was on (a file, a commit), the operation in
//! progress. Run from M-x without the popup, it builds that context
//! itself, with no switches on.

use crate::git::{self, Op};
use crate::status::{self, StatusLine};
use crate::views;
use forge_plugin_sdk::{
    Buffer, TransientOpts, bind_transient, command, message, pop_transient, push_transient,
    read_string,
};
use std::cell::RefCell;
use std::collections::BTreeSet;

/// Where a popup was opened, and what it acts on.
#[derive(Debug, Clone, Default)]
pub struct Ctx {
    pub root: String,
    /// The status buffer it came from, if it did.
    pub status: Option<u32>,
    /// What point was on in that status buffer.
    pub target: Option<StatusLine>,
    /// A commit point was on (status, log, revision or blame buffer).
    pub commit: Option<String>,
    /// A file point was on, relative to the root.
    pub file: Option<String>,
    /// The line in that file (1-based), when it came from a file buffer.
    pub line: usize,
    /// The operation in progress, as the status buffer last saw it.
    pub op: Option<Op>,
    /// The switches turned on (`--force-with-lease`, ...).
    pub args: Vec<String>,
}

impl Ctx {
    pub fn has(&self, arg: &str) -> bool {
        self.args.iter().any(|a| a == arg)
    }

    /// The switches as &str, for building argument lists.
    pub fn arg_strs(&self) -> Vec<&str> {
        self.args.iter().map(String::as_str).collect()
    }
}

/// A switch: `-f` toggles `--force-with-lease`.
pub struct Switch {
    pub key: &'static str,
    pub arg: &'static str,
    pub desc: &'static str,
}

/// When an action is offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    Always,
    /// Only while the popup's operation is in progress (continue, abort).
    InProgress,
    /// Only while it isn't (start one).
    Idle,
}

pub struct Action {
    pub key: &'static str,
    /// The command is `magit-<popup>-<name>`.
    pub name: &'static str,
    pub desc: &'static str,
    pub when: When,
    pub run: fn(Ctx),
}

pub struct Popup {
    /// `push`: the popup command is `magit-push`, its map
    /// `magit-popup-push`.
    pub id: &'static str,
    pub title: &'static str,
    /// The operation whose state picks between In-progress and Idle actions.
    pub op: Option<Op>,
    pub switches: &'static [Switch],
    pub actions: &'static [Action],
}

impl Popup {
    pub fn command(&self) -> String {
        format!("magit-{}", self.id)
    }

    /// The transient map for the popup: one per state when the popup has
    /// an operation (start one / continue it), so each lists only what
    /// applies. Its name is the panel's title.
    fn map(&self, in_progress: bool) -> String {
        let name = self.title.to_lowercase();
        if in_progress {
            format!("magit {name} (in progress)")
        } else {
            format!("magit {name}")
        }
    }

    /// The maps to bind an action in.
    fn maps_for(&self, when: When) -> Vec<String> {
        match (self.op, when) {
            (None, _) => vec![self.map(false)],
            (Some(_), When::Always) => vec![self.map(false), self.map(true)],
            (Some(_), When::Idle) => vec![self.map(false)],
            (Some(_), When::InProgress) => vec![self.map(true)],
        }
    }

    pub fn action_command(&self, a: &Action) -> String {
        format!("magit-{}-{}", self.id, a.name)
    }

    fn switch_command(&self, s: &Switch) -> String {
        format!("magit-{}-toggle-{}", self.id, s.arg.trim_start_matches('-'))
    }

    fn in_progress(&self, ctx: &Ctx) -> Option<bool> {
        let op = self.op?;
        Some(ctx.op == Some(op))
    }

    fn offered(&self, a: &Action, ctx: &Ctx) -> bool {
        match (a.when, self.in_progress(ctx)) {
            (When::Always, _) | (_, None) => true,
            (When::InProgress, Some(p)) => p,
            (When::Idle, Some(p)) => !p,
        }
    }

    /// The echo-area line while the popup is up: its name and the
    /// switches turned on. (Forge lists the keys themselves in a panel,
    /// as it does for any keypad.)
    pub fn status_line(&self, on: &BTreeSet<String>) -> String {
        let on: Vec<&str> = self
            .switches
            .iter()
            .filter(|s| on.contains(s.arg))
            .map(|s| s.arg)
            .collect();
        if on.is_empty() {
            format!("{} (q quits)", self.title)
        } else {
            format!("{} {} (q quits)", self.title, on.join(" "))
        }
    }

    /// Everything the popup offers on one line: switches (on ones in
    /// brackets), then the actions that apply.
    pub fn describe(&self, ctx: &Ctx, on: &BTreeSet<String>) -> String {
        let mut parts = vec![self.title.to_string()];
        if !self.switches.is_empty() {
            let sw: Vec<String> = self
                .switches
                .iter()
                .map(|s| {
                    if on.contains(s.arg) {
                        format!("[{} {}]", s.key, s.desc)
                    } else {
                        format!("{} {}", s.key, s.desc)
                    }
                })
                .collect();
            parts.push(sw.join("  "));
        }
        let acts: Vec<String> = self
            .actions
            .iter()
            .filter(|a| self.offered(a, ctx))
            .map(|a| format!("{} {}", a.key, a.desc))
            .collect();
        parts.push(acts.join("  "));
        parts.push("q quit".to_string());
        parts.join(" | ")
    }
}

/// The open popup: which one, its context, and the switches on.
struct Active {
    popup: &'static Popup,
    /// The transient map that is up.
    map: String,
    ctx: Ctx,
    on: BTreeSet<String>,
}

thread_local! {
    static ACTIVE: RefCell<Option<Active>> = const { RefCell::new(None) };
    static POPUPS: RefCell<Vec<&'static Popup>> = const { RefCell::new(Vec::new()) };
}

fn show(a: &Active) {
    message(&a.popup.status_line(&a.on));
}

/// Close the open popup, if any, giving back its context and switches.
fn close() -> Option<Active> {
    let a = ACTIVE.with(|a| a.borrow_mut().take())?;
    let _ = pop_transient(&a.map);
    Some(a)
}

/// Register a popup: its command, the switch and action commands, and
/// the transient map binding their keys.
pub fn register(p: &'static Popup, doc: &str) -> Result<(), String> {
    POPUPS.with(|ps| ps.borrow_mut().push(p));
    command(&p.command(), doc, move |_| {
        open(p);
        Ok(())
    })?;
    for s in p.switches {
        let arg = s.arg;
        command(
            &p.switch_command(s),
            &format!("Toggle {arg} ({})", s.desc),
            move |_| {
                ACTIVE.with(|a| {
                    if let Some(a) = a.borrow_mut().as_mut() {
                        if !a.on.remove(arg) {
                            a.on.insert(arg.to_string());
                        }
                        show(a);
                    }
                });
                Ok(())
            },
        )?;
        for m in p.maps_for(When::Always) {
            bind_transient(&m, &keys_of(s.key), &p.switch_command(s))?;
        }
    }
    for a in p.actions {
        let run = a.run;
        let doc = format!("{}: {}", p.title, a.desc);
        command(&p.action_command(a), &doc, move |_| {
            match close() {
                Some(active) if std::ptr::eq(active.popup, p) => {
                    let mut ctx = active.ctx;
                    ctx.args = active.on.into_iter().collect();
                    run(ctx);
                }
                _ => context(run),
            }
            Ok(())
        })?;
    }
    for a in p.actions {
        for m in p.maps_for(a.when) {
            bind_transient(&m, &keys_of(a.key), &p.action_command(a))?;
        }
    }
    for m in p.maps_for(When::Always) {
        bind_transient(&m, "q", "magit-popup-quit")?;
        bind_transient(&m, "C-g", "magit-popup-quit")?;
    }
    Ok(())
}

/// "-f" is typed as two keys.
fn keys_of(key: &str) -> String {
    let chars: Vec<String> = key.chars().map(|c| c.to_string()).collect();
    if chars.len() == 2 && chars[0] == "-" {
        chars.join(" ")
    } else {
        key.to_string()
    }
}

pub fn register_quit() -> Result<(), String> {
    command("magit-popup-quit", "Close the popup", |_| {
        if close().is_some() {
            message("Quit");
        }
        Ok(())
    })
}

/// Open popup `p` for the current buffer.
pub fn open(p: &'static Popup) {
    // A popup opened from another popup (magit-dispatch) replaces it.
    close();
    context(move |ctx| {
        let map = p.map(p.in_progress(&ctx) == Some(true));
        let a = Active {
            popup: p,
            map: map.clone(),
            ctx,
            on: BTreeSet::new(),
        };
        show(&a);
        ACTIVE.with(|slot| *slot.borrow_mut() = Some(a));
        if let Err(e) = push_transient(&map, TransientOpts::default().on_unbound("swallow")) {
            message(&format!("magit: {e}"));
            ACTIVE.with(|slot| slot.borrow_mut().take());
        }
    });
}

// ---------------------------------------------------------------- context

/// The context of the current buffer: a status buffer, a Magit view, a
/// commit message, or a file in a repository (which asks git where its
/// root is). Outside all of these, asks for a repository.
pub fn context(then: impl FnOnce(Ctx) + 'static) {
    let b = Buffer::current();
    let id = b.id();
    let row = b.point().line as usize;
    if let Some(root) = status::root_of(id) {
        let target = status::point_target(id);
        let commit = target.as_ref().and_then(|t| match t {
            StatusLine::Commit { hash, .. } => Some(hash.clone()),
            StatusLine::Info { hash } => hash.clone(),
            StatusLine::Stash { name } => Some(name.clone()),
            _ => None,
        });
        let op = status::with_view(id, |v| v.op()).flatten();
        then(Ctx {
            root,
            status: Some(id),
            file: target.as_ref().and_then(|t| t.path().map(str::to_string)),
            target,
            commit,
            line: 1,
            op,
            args: Vec::new(),
        });
        return;
    }
    if let Some(v) = views::view(id) {
        let (commit, file, line) = match views::target(id, row) {
            Some(views::Target::Commit(h)) => (Some(h), None, 1),
            Some(views::Target::File(p, l)) => (None, Some(p), l),
            None => (None, None, 1),
        };
        let file = file.or(match &v.source {
            views::Source::Blame(p) => Some(p.clone()),
            _ => None,
        });
        let commit = commit.or(match &v.source {
            views::Source::Show(r) => Some(r.clone()),
            _ => None,
        });
        with_op(v.root.clone(), move |root, op| {
            then(Ctx {
                root,
                commit,
                file,
                line,
                op,
                ..Default::default()
            })
        });
        return;
    }
    if let Some(root) = crate::commit::root_of(id) {
        with_op(root, move |root, op| {
            then(Ctx {
                root,
                op,
                ..Default::default()
            })
        });
        return;
    }
    if let Some(path) = b.path() {
        let (dir, name) = match path.rsplit_once('/') {
            Some((d, n)) => (if d.is_empty() { "/".to_string() } else { d.to_string() }, n.to_string()),
            None => (".".to_string(), path.clone()),
        };
        let line = row + 1;
        git::run(
            &dir,
            &["rev-parse", "--show-toplevel", "--show-prefix"],
            move |result| match result {
                Ok(out) => {
                    let mut lines = out.lines();
                    let root = lines.next().unwrap_or("").trim().to_string();
                    let prefix = lines.next().unwrap_or("").trim().to_string();
                    let file = format!("{prefix}{name}");
                    with_op(root, move |root, op| {
                        then(Ctx {
                            root,
                            file: Some(file),
                            line,
                            op,
                            ..Default::default()
                        })
                    });
                }
                Err(_) => message("Not inside a Git repository"),
            },
        );
        return;
    }
    // No file here: the only open repository, or ask.
    let open = status::open_views();
    if open.len() == 1 {
        let root = open[0].1.clone();
        with_op(root, move |root, op| {
            then(Ctx {
                root,
                op,
                ..Default::default()
            })
        });
        return;
    }
    read_string("Git repository: ", "~/", move |dir| {
        let Some(dir) = dir.filter(|d| !d.trim().is_empty()) else {
            return;
        };
        let dir = dir.trim().trim_end_matches('/').to_string();
        git::run(&dir, &["rev-parse", "--show-toplevel"], move |result| match result {
            Ok(out) => {
                let root = out.trim().to_string();
                with_op(root, move |root, op| {
                    then(Ctx {
                        root,
                        op,
                        ..Default::default()
                    })
                })
            }
            Err(e) => message(&format!("Not a Git repository: {e}")),
        });
    });
}

/// The operation in progress in `root`: from its status buffer when one
/// is open, else asked of git.
fn with_op(root: String, then: impl FnOnce(String, Option<Op>) + 'static) {
    if let Some(b) = status::buffer_for(&root)
        && let Some(op) = status::with_view(b, |v| v.op())
    {
        then(root, op);
        return;
    }
    let r = root.clone();
    git::join(
        &root,
        vec![
            git::Run::new(&["status", "--untracked-files=no"]).c_locale(),
            git::Run::new(&["rev-parse", "--verify", "--quiet", "MERGE_HEAD"]),
        ],
        move |outs| {
            let op = if outs[1].ok() {
                Some(Op::Merge)
            } else {
                git::parse_sequence(&outs[0].stdout).op
            };
            then(r, op)
        },
    );
}

/// Every popup, for `?` and magit-dispatch.
pub fn all() -> Vec<&'static Popup> {
    POPUPS.with(|p| p.borrow().clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nothing(_: Ctx) {}

    static P: Popup = Popup {
        id: "rebase",
        title: "Rebase",
        op: Some(Op::Rebase),
        switches: &[Switch {
            key: "-A",
            arg: "--autostash",
            desc: "autostash",
        }],
        actions: &[
            Action {
                key: "u",
                name: "upstream",
                desc: "onto upstream",
                when: When::Idle,
                run: nothing,
            },
            Action {
                key: "r",
                name: "continue",
                desc: "continue",
                when: When::InProgress,
                run: nothing,
            },
        ],
    };

    #[test]
    fn the_popup_line_shows_switches_and_the_actions_that_apply() {
        let mut on = BTreeSet::new();
        let idle = Ctx::default();
        assert_eq!(
            P.describe(&idle, &on),
            "Rebase | -A autostash | u onto upstream | q quit"
        );
        on.insert("--autostash".to_string());
        let busy = Ctx {
            op: Some(Op::Rebase),
            ..Default::default()
        };
        assert_eq!(
            P.describe(&busy, &on),
            "Rebase | [-A autostash] | r continue | q quit"
        );
        assert_eq!(P.status_line(&on), "Rebase --autostash (q quits)");
        assert_eq!(P.status_line(&BTreeSet::new()), "Rebase (q quits)");
        assert_eq!(P.action_command(&P.actions[1]), "magit-rebase-continue");
        assert_eq!(P.switch_command(&P.switches[0]), "magit-rebase-toggle-autostash");
        assert_eq!(keys_of("-A"), "- A");
        assert_eq!(P.maps_for(When::Idle), ["magit rebase"]);
        assert_eq!(P.maps_for(When::Always), ["magit rebase", "magit rebase (in progress)"]);
        assert_eq!(keys_of("u"), "u");
    }
}
