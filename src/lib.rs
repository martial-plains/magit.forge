// SPDX-License-Identifier: GPL-3.0-or-later
//! Magit for Forge: a Git interface in the Emacs tradition.
//!
//! `M-x magit-status` (or `C-x g`) opens the status buffer for the
//! current file's repository: untracked, unstaged and staged changes,
//! stashes, unpushed and unpulled commits, and an operation in progress
//! (rebase, merge, cherry-pick, revert, bisect). Point actions work on
//! what point is on, or on the region: `s` stages a file, a hunk or the
//! lines selected in a hunk, `u` unstages, `k` discards, `RET` visits.
//! The prefix keys open popups with switches, as in Magit: `c` commit,
//! `P` push, `F` pull, `f` fetch, `b` branch, `z` stash, `r` rebase,
//! `m` merge, `A` cherry-pick, `V` revert, `X` reset, `t` tag, `l` log,
//! `d` diff, `M` remote, `i` ignore, `B` bisect; `h` lists them and `?`
//! lists every key.
//!
//! Everything shells out to `git` (named in the manifest's `processes`
//! permission), so nothing here invents merging. Needs Forge 0.1.9 (for
//! git's error output and read-only buffers).
//!
//!   rustup target add wasm32-wasip2
//!   cargo build --release --target wasm32-wasip2
//!   forge --plugin target/wasm32-wasip2/release/forge_magit.wasm

pub mod actions;
pub mod commit;
pub mod diff;
pub mod git;
pub mod popup;
pub mod popups;
pub mod status;
pub mod views;

use forge_plugin_sdk::{
    Buffer, EventKind, Scope, bind_in_mode, command, event_buffer, forge_plugin, keymap, message,
    modes, on,
};

/// The modes of Magit's read-only buffers, which share most keys.
const LIST_MODES: &[&str] = &[
    "magit-status-mode",
    "magit-log-mode",
    "magit-diff-mode",
    "magit-view-mode",
];

/// Keys every Magit list buffer has (status, log, revision, ...).
const KEYS: &[(&str, &str, &str)] = &[
    ("n", "magit-section-forward", "next section (file, hunk, commit)"),
    ("p", "magit-section-backward", "previous section"),
    ("M-n", "magit-section-forward-sibling", "next sibling"),
    ("M-p", "magit-section-backward-sibling", "previous sibling"),
    ("^", "magit-section-up", "parent section"),
    ("TAB", "magit-section-toggle", "show/hide a file's diff or a section"),
    ("RET", "magit-visit-thing", "visit the file (at the line), show the commit or stash"),
    ("s", "magit-stage", "stage the file, hunk, lines or section at point"),
    ("S", "magit-stage-modified", "stage all tracked changes"),
    ("u", "magit-unstage", "unstage the file, hunk, lines or section at point"),
    ("U", "magit-unstage-all", "unstage everything"),
    ("k", "magit-discard", "discard the change at point (asks), drop a stash"),
    ("v", "magit-reverse", "reverse a staged change in the worktree"),
    ("x", "magit-reset-quickly", "reset HEAD to the commit at point"),
    ("y", "magit-show-refs", "list branches and tags"),
    ("!", "magit-run-git", "run a git command"),
    ("$", "magit-process", "the git commands run, and what they said"),
    ("g", "magit-refresh", "refresh"),
    ("q", "magit-quit", "close this buffer"),
    ("?", "magit-help", "this list"),
];

/// The buffer kinds a generic key acts on.
enum Here {
    Status(u32, String),
    View(u32, String),
    Other,
}

fn here() -> Here {
    let id = Buffer::current().id();
    if let Some(root) = status::root_of(id) {
        Here::Status(id, root)
    } else if let Some(root) = views::root_of(id) {
        Here::View(id, root)
    } else {
        Here::Other
    }
}

fn motion(f: fn(&[status::StatusLine], usize) -> Option<usize>, view_by: isize, none: &str) {
    match here() {
        Here::Status(id, _) => {
            if !status::move_point(id, f) {
                message(none);
            }
        }
        Here::View(id, _) => {
            if !views::next_stop(id, view_by) {
                message(none);
            }
        }
        Here::Other => message("Not in a Magit buffer"),
    }
}

fn help_text() -> String {
    let mut t = String::from("Magit keys\n\nIn status, log and revision buffers:\n");
    for (k, _, d) in KEYS {
        t.push_str(&format!("  {k:<5} {d}\n"));
    }
    t.push_str("\nPopups (then a switch like -f, or an action key; q or C-g closes):\n");
    for (p, k, d) in popups::ALL {
        if !k.is_empty() {
            t.push_str(&format!("  {k:<5} {d}\n"));
        }
        let _ = p;
    }
    t.push_str(
        "\nWith the region in a hunk, s, u and k act on just those lines;\n\
         over several files, on those files.\n\
         \nIn a commit message: C-c C-c commits, C-c C-k cancels.\n\
         \nAnywhere: C-x g opens the status, C-x M-g the Magit popup,\n\
         C-c M-g the file popup (stage, diff, log, blame this file).\n",
    );
    t
}

fn init() -> Result<(), String> {
    for mode in LIST_MODES.iter().chain(&["magit-commit-mode"]) {
        modes::define_major_mode(mode, None, &[])?;
    }

    // ---- entry points
    command(
        "magit-status",
        "Show the status of the current file's Git repository.",
        |_| {
            if let Here::Status(id, _) = here() {
                status::refresh(id);
                return Ok(());
            }
            popup::context(|ctx| status::open(&ctx.root));
            Ok(())
        },
    )?;
    command("magit-init", "Create a Git repository and show its status.", |_| {
        let initial = Buffer::current()
            .path()
            .and_then(|p| forge_plugin_sdk::files::name_directory(&p))
            .unwrap_or_else(|| "~/".into());
        actions::read_nonempty("Create repository in: ", &initial, |dir| {
            let dir = dir.trim_end_matches('/').to_string();
            let d2 = dir.clone();
            git::run("/", &["init", "--", &dir], move |r| match r {
                Ok(_) => {
                    message(&format!("Initialized {d2}"));
                    status::open(&d2);
                }
                Err(e) => message(&format!("magit: init failed: {e}")),
            });
        });
        Ok(())
    })?;
    command("magit-clone", "Clone a repository and show its status.", |_| {
        actions::read_nonempty("Clone URL: ", "", |url| {
            let name = url
                .trim_end_matches('/')
                .rsplit(['/', ':'])
                .next()
                .unwrap_or("repo")
                .trim_end_matches(".git")
                .to_string();
            actions::read_nonempty("Clone into: ", &format!("~/{name}"), move |dir| {
                let dir = dir.trim_end_matches('/').to_string();
                let parent = dir.rsplit_once('/').map(|(p, _)| p.to_string()).filter(|p| !p.is_empty()).unwrap_or_else(|| "/".into());
                message(&format!("Cloning {url}..."));
                let d2 = dir.clone();
                git::exec(&parent, git::Run::new(&["clone", "--", &url, &dir]).logged(), move |out| {
                    if out.ok() {
                        message(&format!("Cloned into {d2}"));
                        status::open(&d2);
                    } else {
                        message(&format!("magit: clone failed: {}", out.error()));
                    }
                });
            });
        });
        Ok(())
    })?;
    command("magit-blame", "Blame the file at point or the one being visited.", |_| {
        popup::context(actions::blame);
        Ok(())
    })?;
    command("magit-process", "Show the git commands run in this repository.", |_| {
        popup::context(|ctx| views::show_process_log(&ctx.root));
        Ok(())
    })?;
    command("magit-show-refs", "List branches and tags.", |_| {
        popup::context(actions::show_refs);
        Ok(())
    })?;
    command("magit-run-git", "Run a git command in the repository.", |_| {
        popup::context(actions::run_git);
        Ok(())
    })?;
    command("magit-help", "List Magit's keys.", |_| {
        let root = match here() {
            Here::Status(_, r) | Here::View(_, r) => r,
            Here::Other => String::new(),
        };
        views::show_text(&root, "*magit-help*", &help_text());
        Ok(())
    })?;

    // ---- sections and motion
    command("magit-section-forward", "Move to the next section.", |_| {
        motion(|l, r| status::next_stop(l, r, 1, false), 1, "No next section");
        Ok(())
    })?;
    command("magit-section-backward", "Move to the previous section.", |_| {
        motion(|l, r| status::next_stop(l, r, -1, false), -1, "No previous section");
        Ok(())
    })?;
    command("magit-section-forward-sibling", "Move to the next sibling section.", |_| {
        motion(|l, r| status::next_stop(l, r, 1, true), 1, "No next sibling");
        Ok(())
    })?;
    command("magit-section-backward-sibling", "Move to the previous sibling section.", |_| {
        motion(|l, r| status::next_stop(l, r, -1, true), -1, "No previous sibling");
        Ok(())
    })?;
    command("magit-section-up", "Move to the parent section.", |_| {
        motion(status::parent, -1, "No parent section");
        Ok(())
    })?;
    command(
        "magit-section-toggle",
        "Show or hide the diff of the file at point, or fold the section.",
        |_| {
            match here() {
                Here::Status(id, _) => status::toggle(id),
                _ => message("Nothing to fold here"),
            }
            Ok(())
        },
    )?;
    command(
        "magit-visit-thing",
        "Visit the file at point (at its line), or show the commit or stash.",
        |_| {
            actions::visit();
            Ok(())
        },
    )?;

    // ---- applying changes
    command("magit-stage", "Stage the file, hunk, lines or section at point.", |_| {
        actions::stage();
        Ok(())
    })?;
    command("magit-stage-modified", "Stage all changes to tracked files.", |_| {
        actions::stage_all();
        Ok(())
    })?;
    command("magit-unstage", "Unstage the file, hunk, lines or section at point.", |_| {
        actions::unstage();
        Ok(())
    })?;
    command("magit-unstage-all", "Unstage everything.", |_| {
        actions::unstage_all();
        Ok(())
    })?;
    command("magit-discard", "Discard the change at point (asks first).", |_| {
        actions::discard();
        Ok(())
    })?;
    command("magit-reverse", "Reverse the staged change at point in the worktree.", |_| {
        actions::reverse();
        Ok(())
    })?;
    command("magit-reset-quickly", "Reset HEAD to the commit at point.", |_| {
        popup::context(actions::reset_quickly);
        Ok(())
    })?;

    // ---- refresh and quit
    command("magit-refresh", "Refresh this Magit buffer.", |_| {
        match here() {
            Here::Status(id, _) => status::refresh(id),
            Here::View(id, _) => {
                views::refresh(id);
            }
            Here::Other => message("Not in a Magit buffer"),
        }
        Ok(())
    })?;
    command("magit-quit", "Close this Magit buffer.", |_| {
        let b = Buffer::current();
        let id = b.id();
        let back = match here() {
            Here::Status(..) => {
                status::forget(id);
                None
            }
            Here::View(_, root) => {
                views::forget(id);
                status::buffer_for(&root)
            }
            Here::Other => None,
        };
        b.kill();
        if let Some(s) = back.and_then(Buffer::from_id) {
            s.show();
        }
        Ok(())
    })?;

    // ---- commit buffers
    command("magit-commit-finish", "Commit with this message.", |_| {
        commit::finish();
        Ok(())
    })?;
    command("magit-commit-cancel", "Abandon this commit.", |_| {
        commit::cancel();
        Ok(())
    })?;
    bind_in_mode("magit-commit-mode", "C-c C-c", "magit-commit-finish")?;
    bind_in_mode("magit-commit-mode", "C-c C-k", "magit-commit-cancel")?;

    // ---- popups and keys
    popups::register_all()?;
    for mode in LIST_MODES {
        for (k, cmd, _) in KEYS {
            bind_in_mode(mode, k, cmd)?;
        }
        for (p, k, _) in popups::ALL {
            if !k.is_empty() {
                bind_in_mode(mode, k, &p.command())?;
            }
        }
    }
    // As in Magit; the user's own bindings win if these are taken.
    let _ = keymap::bind(&Scope::Global, "C-x g", "magit-status");
    let _ = keymap::bind(&Scope::Global, "C-x M-g", "magit-dispatch");
    let _ = keymap::bind(&Scope::Global, "C-c M-g", "magit-file-dispatch");

    // ---- events
    on(EventKind::BufferSaved, |e| {
        if let Some(path) = Buffer::from_id(event_buffer(e)).and_then(|b| b.path()) {
            status::refresh_for_path(&path);
        }
    });
    on(EventKind::BufferKilled, |e| {
        let id = event_buffer(e);
        status::forget(id);
        views::forget(id);
        commit::forget(id);
    });

    forge_plugin_sdk::extend_menu(
        "Tools",
        &[
            forge_plugin_sdk::menu_item("Magit Status", "magit-status"),
            forge_plugin_sdk::menu_item("Magit Commands", "magit-dispatch"),
        ],
    )?;
    Ok(())
}

forge_plugin!(init);
