// SPDX-License-Identifier: GPL-3.0-or-later
//! Magit's popups (transients) and what their actions do. Keys follow
//! Magit: `c` commit, `P` push, `F` pull, `f` fetch, `b` branch, `z`
//! stash, `r` rebase, `m` merge, `A` cherry-pick, `V` revert, `X` reset,
//! `t` tag, `l` log, `d` diff, `M` remote, `i` ignore, `B` bisect, and
//! `h` for all of them.

use crate::actions::{
    self, args, read_branch, read_commit, read_nonempty, read_remote, run_and_refresh,
};
use crate::commit::{self, Kind};
use crate::git::{self, Op, Run};
use crate::popup::{Action, Ctx, Popup, Switch, When, register};
use crate::status::{Section, StatusLine};
use crate::views;
use forge_plugin_sdk::{completing_read, message, read_string, y_or_n};

/// `base` followed by the switches turned on.
fn with_args(ctx: &Ctx, base: &[&str]) -> Vec<String> {
    let mut a = args(base);
    a.extend(ctx.args.iter().cloned());
    a
}

/// Like read_branch, but an empty answer means `default`.
fn read_branch_or(ctx: &Ctx, prompt: &str, default: &str, then: impl FnOnce(String) + 'static) {
    let (prompt, default) = (prompt.to_string(), default.to_string());
    git::run(
        &ctx.root,
        &["for-each-ref", git::REF_FORMAT, "refs/heads", "refs/remotes"],
        move |result| {
            let names: Vec<String> = git::parse_refs(&result.unwrap_or_default())
                .into_iter()
                .map(|b| b.name)
                .collect();
            completing_read(&prompt, &names, false, move |name| {
                let Some(name) = name else { return };
                let name = name.trim().to_string();
                then(if name.is_empty() { default } else { name });
            });
        },
    );
}

/// The current branch, or a message saying there is none.
fn with_branch(root: &str, then: impl FnOnce(String) + 'static) {
    git::run(root, &["symbolic-ref", "--short", "-q", "HEAD"], move |r| match r {
        Ok(b) if !b.trim().is_empty() => then(b.trim().to_string()),
        _ => message("HEAD is detached: check out a branch first"),
    });
}

// ------------------------------------------------------------------ commit

fn commit_create(ctx: Ctx) {
    commit::start(ctx, Kind::Create);
}
fn commit_amend(ctx: Ctx) {
    commit::start(ctx, Kind::Amend);
}
fn commit_reword(ctx: Ctx) {
    commit::start(ctx, Kind::Reword);
}
fn commit_extend(ctx: Ctx) {
    let a = with_args(&ctx, &["commit", "--amend", "--no-edit"]);
    run_and_refresh(&ctx.root, a, "commit --amend", false);
}
fn commit_fixup(ctx: Ctx) {
    let c2 = ctx.clone();
    read_commit(&ctx, "Fixup commit: ", move |c| {
        let a = with_args(&c2, &["commit", &format!("--fixup={c}")]);
        run_and_refresh(&c2.root, a, "commit --fixup", false);
    });
}
fn commit_squash(ctx: Ctx) {
    let c2 = ctx.clone();
    read_commit(&ctx, "Squash into commit: ", move |c| {
        let a = with_args(&c2, &["commit", &format!("--squash={c}"), "--no-edit"]);
        run_and_refresh(&c2.root, a, "commit --squash", false);
    });
}

pub static COMMIT: Popup = Popup {
    id: "commit",
    title: "Commit",
    op: None,
    switches: &[
        Switch { key: "-a", arg: "--all", desc: "stage all" },
        Switch { key: "-e", arg: "--allow-empty", desc: "allow empty" },
        Switch { key: "-n", arg: "--no-verify", desc: "no hooks" },
        Switch { key: "-s", arg: "--signoff", desc: "signoff" },
        Switch { key: "-R", arg: "--reset-author", desc: "reset author" },
    ],
    actions: &[
        Action { key: "c", name: "create", desc: "commit", when: When::Always, run: commit_create },
        Action { key: "e", name: "extend", desc: "extend", when: When::Always, run: commit_extend },
        Action { key: "w", name: "reword", desc: "reword", when: When::Always, run: commit_reword },
        Action { key: "a", name: "amend", desc: "amend", when: When::Always, run: commit_amend },
        Action { key: "f", name: "fixup", desc: "fixup", when: When::Always, run: commit_fixup },
        Action { key: "s", name: "squash", desc: "squash", when: When::Always, run: commit_squash },
    ],
};

// -------------------------------------------------------------------- push

fn push_to(ctx: &Ctx, remote: &str, refspec: &str, set_upstream: bool) {
    let mut a = with_args(ctx, &["push"]);
    if set_upstream {
        a.push("--set-upstream".into());
    }
    a.push(remote.into());
    a.push(refspec.into());
    run_and_refresh(&ctx.root, a, "push", false);
}

fn push_upstream(ctx: Ctx) {
    let root = ctx.root.clone();
    git::run(
        &root,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"],
        move |r| match r {
            Ok(up) => {
                let up = up.trim().to_string();
                match up.split_once('/') {
                    Some((remote, branch)) => {
                        let (remote, branch) = (remote.to_string(), branch.to_string());
                        push_to(&ctx, &remote, &format!("HEAD:refs/heads/{branch}"), false)
                    }
                    None => push_to(&ctx, ".", &format!("HEAD:refs/heads/{up}"), false),
                }
            }
            Err(_) => push_set_upstream(ctx),
        },
    );
}

fn push_set_upstream(ctx: Ctx) {
    let root = ctx.root.clone();
    with_branch(&root, move |branch| {
        let r = ctx.root.clone();
        read_remote(&r, "Push to remote (sets upstream): ", move |remote| {
            push_to(&ctx, &remote, &format!("HEAD:refs/heads/{branch}"), true);
        });
    });
}

fn push_elsewhere(ctx: Ctx) {
    let c2 = ctx.clone();
    read_branch(&ctx.root, "Push to (remote/branch): ", true, false, false, move |target| {
        match target.split_once('/') {
            Some((remote, branch)) => push_to(&c2, remote, &format!("HEAD:refs/heads/{branch}"), false),
            None => message("Give the target as remote/branch"),
        }
    });
}

fn push_other(ctx: Ctx) {
    let c2 = ctx.clone();
    read_branch(&ctx.root, "Push branch: ", false, true, false, move |branch| {
        let r = c2.root.clone();
        read_remote(&r, &format!("Push {branch} to remote: "), move |remote| {
            push_to(&c2, &remote, &format!("refs/heads/{branch}:refs/heads/{branch}"), false);
        });
    });
}

fn push_tag(ctx: Ctx) {
    let c2 = ctx.clone();
    git::run(&ctx.root, &["tag", "-l"], move |r| {
        let tags: Vec<String> = r.unwrap_or_default().lines().map(str::to_string).collect();
        if tags.is_empty() {
            message("No tags");
            return;
        }
        let first = tags.last().cloned();
        actions::choose("Push tag: ", tags, first, true, move |tag| {
            let r = c2.root.clone();
            read_remote(&r, &format!("Push {tag} to remote: "), move |remote| {
                push_to(&c2, &remote, &format!("refs/tags/{tag}"), false);
            });
        });
    });
}

fn push_tags(ctx: Ctx) {
    let r = ctx.root.clone();
    read_remote(&r, "Push all tags to remote: ", move |remote| {
        let mut a = with_args(&ctx, &["push", &remote, "--tags"]);
        a.dedup();
        run_and_refresh(&ctx.root, a, "push", false);
    });
}

pub static PUSH: Popup = Popup {
    id: "push",
    title: "Push",
    op: None,
    switches: &[
        Switch { key: "-f", arg: "--force-with-lease", desc: "force with lease" },
        Switch { key: "-F", arg: "--force", desc: "force" },
        Switch { key: "-n", arg: "--dry-run", desc: "dry run" },
        Switch { key: "-h", arg: "--no-verify", desc: "no hooks" },
        Switch { key: "-T", arg: "--follow-tags", desc: "follow tags" },
    ],
    actions: &[
        Action { key: "p", name: "upstream", desc: "to upstream", when: When::Always, run: push_upstream },
        Action { key: "u", name: "set-upstream", desc: "set upstream & push", when: When::Always, run: push_set_upstream },
        Action { key: "e", name: "elsewhere", desc: "elsewhere", when: When::Always, run: push_elsewhere },
        Action { key: "o", name: "other", desc: "another branch", when: When::Always, run: push_other },
        Action { key: "T", name: "tag", desc: "a tag", when: When::Always, run: push_tag },
        Action { key: "t", name: "tags", desc: "all tags", when: When::Always, run: push_tags },
    ],
};

// ------------------------------------------------------------- pull, fetch

fn pull_upstream(ctx: Ctx) {
    let a = with_args(&ctx, &["pull"]);
    let root = ctx.root.clone();
    git::run(&root, &["rev-parse", "--abbrev-ref", "@{upstream}"], move |r| match r {
        Ok(_) => run_and_refresh(&ctx.root, a, "pull", false),
        Err(_) => message("No upstream: pull from elsewhere with e"),
    });
}

fn pull_elsewhere(ctx: Ctx) {
    let r = ctx.root.clone();
    read_remote(&r, "Pull from remote: ", move |remote| {
        let c2 = ctx.clone();
        read_nonempty(&format!("Pull which branch of {remote}: "), "", move |branch| {
            let mut a = with_args(&c2, &["pull"]);
            a.extend([remote, branch]);
            run_and_refresh(&c2.root, a, "pull", false);
        });
    });
}

pub static PULL: Popup = Popup {
    id: "pull",
    title: "Pull",
    op: None,
    switches: &[
        Switch { key: "-r", arg: "--rebase", desc: "rebase" },
        Switch { key: "-f", arg: "--ff-only", desc: "fast-forward only" },
        Switch { key: "-A", arg: "--autostash", desc: "autostash" },
    ],
    actions: &[
        Action { key: "u", name: "upstream", desc: "from upstream", when: When::Always, run: pull_upstream },
        Action { key: "e", name: "elsewhere", desc: "elsewhere", when: When::Always, run: pull_elsewhere },
    ],
};

fn fetch_upstream(ctx: Ctx) {
    run_and_refresh(&ctx.root, with_args(&ctx, &["fetch"]), "fetch", false);
}
fn fetch_all(ctx: Ctx) {
    run_and_refresh(&ctx.root, with_args(&ctx, &["fetch", "--all"]), "fetch", false);
}
fn fetch_elsewhere(ctx: Ctx) {
    let r = ctx.root.clone();
    read_remote(&r, "Fetch remote: ", move |remote| {
        let mut a = with_args(&ctx, &["fetch"]);
        a.push(remote);
        run_and_refresh(&ctx.root, a, "fetch", false);
    });
}
fn fetch_submodules(ctx: Ctx) {
    run_and_refresh(
        &ctx.root,
        with_args(&ctx, &["fetch", "--recurse-submodules"]),
        "fetch",
        false,
    );
}

pub static FETCH: Popup = Popup {
    id: "fetch",
    title: "Fetch",
    op: None,
    switches: &[
        Switch { key: "-p", arg: "--prune", desc: "prune" },
        Switch { key: "-t", arg: "--tags", desc: "tags" },
    ],
    actions: &[
        Action { key: "u", name: "upstream", desc: "from upstream", when: When::Always, run: fetch_upstream },
        Action { key: "a", name: "all", desc: "all remotes", when: When::Always, run: fetch_all },
        Action { key: "e", name: "elsewhere", desc: "elsewhere", when: When::Always, run: fetch_elsewhere },
        Action { key: "m", name: "submodules", desc: "submodules", when: When::Always, run: fetch_submodules },
    ],
};

// ------------------------------------------------------------------ branch

fn branch_checkout(ctx: Ctx) {
    let root = ctx.root.clone();
    read_branch(&ctx.root, "Checkout: ", true, false, true, move |name| {
        run_and_refresh(&root, args(&["checkout", &name]), "checkout", false);
    });
}

fn branch_local(ctx: Ctx) {
    let root = ctx.root.clone();
    git::run(&ctx.root, &["for-each-ref", git::REF_FORMAT, "refs/remotes"], move |r| {
        let names: Vec<String> = git::parse_refs(&r.unwrap_or_default()).into_iter().map(|b| b.name).collect();
        if names.is_empty() {
            message("No remote branches (fetch first?)");
            return;
        }
        let first = names.first().cloned();
        actions::choose("Check out a local branch tracking: ", names, first, true, move |remote| {
            run_and_refresh(&root, args(&["checkout", "--track", &remote]), "checkout", false);
        });
    });
}

fn create_branch(ctx: Ctx, checkout: bool) {
    let c2 = ctx.clone();
    read_nonempty("Name for the new branch: ", "", move |name| {
        let default = c2.commit.clone().filter(|c| !c.starts_with("stash@")).unwrap_or_else(|| "HEAD".into());
        let c3 = c2.clone();
        read_branch_or(&c2, &format!("Start {name} at (empty: {default}): "), &default, move |start| {
            let a = if checkout {
                args(&["checkout", "-b", &name, &start])
            } else {
                args(&["branch", &name, &start])
            };
            run_and_refresh(&c3.root, a, "branch", !checkout);
            if !checkout {
                message(&format!("Created branch {name}"));
            }
        });
    });
}
fn branch_create_checkout(ctx: Ctx) {
    create_branch(ctx, true);
}
fn branch_create(ctx: Ctx) {
    create_branch(ctx, false);
}

fn branch_rename(ctx: Ctx) {
    let root = ctx.root.clone();
    read_branch(&ctx.root, "Rename branch: ", false, true, false, move |old| {
        let r = root.clone();
        read_nonempty(&format!("Rename {old} to: "), &old.clone(), move |new| {
            run_and_refresh(&r, args(&["branch", "-m", &old, &new]), "rename", false);
        });
    });
}

fn branch_delete(ctx: Ctx) {
    let root = ctx.root.clone();
    read_branch(&ctx.root, "Delete branch: ", true, true, true, move |name| {
        let r = root.clone();
        git::run(
            &root,
            &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{name}")],
            move |local| {
                if local.is_ok() {
                    let r2 = r.clone();
                    y_or_n(&format!("Delete branch {name}? "), move |yes| {
                        if !yes {
                            return;
                        }
                        let (r3, n3) = (r2.clone(), name.clone());
                        git::exec(&r2, Run::owned(args(&["branch", "-d", &name])).logged(), move |out| {
                            if out.ok() {
                                message(&format!("Deleted branch {n3}"));
                                crate::status::refresh_root(&r3);
                                return;
                            }
                            // Usually "not fully merged" (in whatever language).
                            let why = out.error();
                            let (r4, n4) = (r3.clone(), n3.clone());
                            let why = why.lines().next().unwrap_or("").trim_end_matches('.').to_string();
                            y_or_n(&format!("{why}. Force delete {n3}? "), move |yes| {
                                if yes {
                                    run_and_refresh(&r4, args(&["branch", "-D", &n4]), "delete branch", false);
                                }
                            });
                        });
                    });
                } else if let Some((remote, branch)) = name.split_once('/') {
                    let (remote, branch) = (remote.to_string(), branch.to_string());
                    y_or_n(&format!("Delete {branch} on the remote {remote}? "), move |yes| {
                        if yes {
                            run_and_refresh(&r, args(&["push", &remote, "--delete", &branch]), "delete remote branch", false);
                        }
                    });
                } else {
                    message(&format!("No branch {name}"));
                }
            },
        );
    });
}

fn branch_upstream(ctx: Ctx) {
    let root = ctx.root.clone();
    read_branch(&ctx.root, "Set upstream of the current branch to: ", true, true, false, move |up| {
        run_and_refresh(&root, args(&["branch", &format!("--set-upstream-to={up}")]), "set upstream", false);
    });
}

fn branch_reset(ctx: Ctx) {
    let c2 = ctx.clone();
    read_branch(&ctx.root, "Reset branch: ", false, true, false, move |branch| {
        let c3 = c2.clone();
        read_commit(&c2, &format!("Reset {branch} to: "), move |to| {
            let r = c3.root.clone();
            y_or_n(&format!("Point {branch} at {to}? "), move |yes| {
                if yes {
                    run_and_refresh(&r, args(&["branch", "-f", &branch, &to]), "reset branch", false);
                }
            });
        });
    });
}

pub static BRANCH: Popup = Popup {
    id: "branch",
    title: "Branch",
    op: None,
    switches: &[],
    actions: &[
        Action { key: "b", name: "checkout", desc: "checkout", when: When::Always, run: branch_checkout },
        Action { key: "l", name: "local", desc: "local from remote", when: When::Always, run: branch_local },
        Action { key: "c", name: "create-checkout", desc: "create & checkout", when: When::Always, run: branch_create_checkout },
        Action { key: "n", name: "create", desc: "create", when: When::Always, run: branch_create },
        Action { key: "m", name: "rename", desc: "rename", when: When::Always, run: branch_rename },
        Action { key: "x", name: "reset", desc: "reset", when: When::Always, run: branch_reset },
        Action { key: "k", name: "delete", desc: "delete", when: When::Always, run: branch_delete },
        Action { key: "u", name: "set-upstream", desc: "set upstream", when: When::Always, run: branch_upstream },
    ],
};

// ------------------------------------------------------------------- stash

fn stash_push(ctx: Ctx, extra: &'static [&'static str]) {
    read_string("Stash message (empty for the default): ", "", move |msg| {
        let Some(msg) = msg else { return };
        let mut a = with_args(&ctx, &["stash", "push"]);
        a.extend(extra.iter().map(|s| s.to_string()));
        if !msg.trim().is_empty() {
            a.extend(["-m".into(), msg.trim().to_string()]);
        }
        run_and_refresh(&ctx.root, a, "stash", false);
    });
}
fn stash_both(ctx: Ctx) {
    stash_push(ctx, &[]);
}
fn stash_index(ctx: Ctx) {
    stash_push(ctx, &["--staged"]);
}
fn stash_keep_index(ctx: Ctx) {
    stash_push(ctx, &["--keep-index"]);
}

/// The stash at point, or one picked from the list.
fn read_stash(ctx: &Ctx, prompt: &str, then: impl FnOnce(String) + 'static) {
    if let Some(s) = ctx.commit.clone().filter(|c| c.starts_with("stash@")) {
        then(s);
        return;
    }
    let prompt = prompt.to_string();
    git::run(&ctx.root, &["stash", "list", git::STASH_FORMAT], move |r| {
        let stashes = git::parse_stash_list(&r.unwrap_or_default());
        if stashes.is_empty() {
            message("No stashes");
            return;
        }
        let names: Vec<String> = stashes.into_iter().map(|s| s.name).collect();
        let first = names.first().cloned();
        actions::choose(&prompt, names, first, true, then);
    });
}

/// Apply or pop keeping the stash's index, as Magit does; when that
/// can't be done (staged changes in the way), without it.
fn stash_restore(root: String, op: &'static str, stash: String) {
    let r = root.clone();
    git::exec(&root, Run::owned(args(&["stash", op, "--index", &stash])).logged(), move |out| {
        if out.ok() {
            message(&format!("Stash {op}: {stash}"));
            crate::status::refresh_root(&r);
        } else {
            run_and_refresh(&r, args(&["stash", op, &stash]), &format!("stash {op}"), false);
        }
    });
}
fn stash_apply(ctx: Ctx) {
    let root = ctx.root.clone();
    read_stash(&ctx, "Apply stash: ", move |s| stash_restore(root, "apply", s));
}
fn stash_pop(ctx: Ctx) {
    let root = ctx.root.clone();
    read_stash(&ctx, "Pop stash: ", move |s| stash_restore(root, "pop", s));
}
fn stash_drop(ctx: Ctx) {
    let root = ctx.root.clone();
    read_stash(&ctx, "Drop stash: ", move |s| {
        y_or_n(&format!("Drop {s}? "), move |yes| {
            if yes {
                run_and_refresh(&root, args(&["stash", "drop", &s]), "stash drop", false);
            }
        })
    });
}
fn stash_show(ctx: Ctx) {
    let root = ctx.root.clone();
    read_stash(&ctx, "Show stash: ", move |s| views::show_stash(&root, &s));
}
fn stash_branch(ctx: Ctx) {
    let root = ctx.root.clone();
    read_stash(&ctx, "Branch from stash: ", move |s| {
        read_nonempty("Name for the new branch: ", "", move |b| {
            run_and_refresh(&root, args(&["stash", "branch", &b, &s]), "stash branch", false)
        })
    });
}
fn stash_list(ctx: Ctx) {
    let root = ctx.root.clone();
    git::run(&ctx.root, &["stash", "list", git::STASH_FORMAT], move |r| {
        let stashes = git::parse_stash_list(&r.unwrap_or_default());
        let mut text = String::from("Stashes\n\n");
        for s in &stashes {
            text.push_str(&format!("{} {}\n", s.name, s.message));
        }
        if stashes.is_empty() {
            text.push_str("(none)\n");
        }
        views::show_text(&root, "*magit-stashes*", &text);
    });
}

pub static STASH: Popup = Popup {
    id: "stash",
    title: "Stash",
    op: None,
    switches: &[
        Switch { key: "-u", arg: "--include-untracked", desc: "include untracked" },
        Switch { key: "-a", arg: "--all", desc: "include ignored too" },
    ],
    actions: &[
        Action { key: "z", name: "both", desc: "index & worktree", when: When::Always, run: stash_both },
        Action { key: "i", name: "index", desc: "index", when: When::Always, run: stash_index },
        Action { key: "x", name: "keep-index", desc: "keeping index", when: When::Always, run: stash_keep_index },
        Action { key: "a", name: "apply", desc: "apply", when: When::Always, run: stash_apply },
        Action { key: "p", name: "pop", desc: "pop", when: When::Always, run: stash_pop },
        Action { key: "k", name: "drop", desc: "drop", when: When::Always, run: stash_drop },
        Action { key: "v", name: "show", desc: "show", when: When::Always, run: stash_show },
        Action { key: "b", name: "branch", desc: "branch", when: When::Always, run: stash_branch },
        Action { key: "l", name: "list", desc: "list", when: When::Always, run: stash_list },
    ],
};

// ------------------------------------------------------------------ rebase

fn rebase_onto(ctx: Ctx, onto: String) {
    let mut a = with_args(&ctx, &["rebase"]);
    a.push(onto);
    run_and_refresh(&ctx.root, a, "rebase", false);
}
fn rebase_upstream(ctx: Ctx) {
    rebase_onto(ctx, "@{upstream}".into());
}
fn rebase_elsewhere(ctx: Ctx) {
    let c2 = ctx.clone();
    read_branch(&ctx.root, "Rebase onto: ", true, false, true, move |onto| rebase_onto(c2, onto));
}
fn rebase_autosquash(ctx: Ctx) {
    let c2 = ctx.clone();
    read_commit(&ctx, "Autosquash fixups since (base commit): ", move |base| {
        // -i with GIT_SEQUENCE_EDITOR=true: the todo autosquash wrote, as is.
        let mut a = with_args(&c2, &["rebase", "-i", "--autosquash"]);
        a.push(base);
        run_and_refresh(&c2.root, a, "rebase --autosquash", false);
    });
}
fn rebase_continue(ctx: Ctx) {
    run_and_refresh(&ctx.root, args(&["rebase", "--continue"]), "rebase --continue", false);
}
fn rebase_skip(ctx: Ctx) {
    run_and_refresh(&ctx.root, args(&["rebase", "--skip"]), "rebase --skip", false);
}
fn rebase_abort(ctx: Ctx) {
    y_or_n("Abort this rebase? ", move |yes| {
        if yes {
            run_and_refresh(&ctx.root, args(&["rebase", "--abort"]), "rebase --abort", false);
        }
    });
}

pub static REBASE: Popup = Popup {
    id: "rebase",
    title: "Rebase",
    op: Some(Op::Rebase),
    switches: &[
        Switch { key: "-A", arg: "--autostash", desc: "autostash" },
        Switch { key: "-r", arg: "--rebase-merges", desc: "rebase merges" },
        Switch { key: "-k", arg: "--keep-empty", desc: "keep empty" },
    ],
    actions: &[
        Action { key: "u", name: "upstream", desc: "onto upstream", when: When::Idle, run: rebase_upstream },
        Action { key: "e", name: "elsewhere", desc: "onto elsewhere", when: When::Idle, run: rebase_elsewhere },
        Action { key: "f", name: "autosquash", desc: "autosquash fixups", when: When::Idle, run: rebase_autosquash },
        Action { key: "r", name: "continue", desc: "continue", when: When::InProgress, run: rebase_continue },
        Action { key: "s", name: "skip", desc: "skip", when: When::InProgress, run: rebase_skip },
        Action { key: "a", name: "abort", desc: "abort", when: When::InProgress, run: rebase_abort },
    ],
};

// ------------------------------------------------------------------- merge

fn merge_with(ctx: Ctx, extra: &'static [&'static str]) {
    let c2 = ctx.clone();
    read_branch(&ctx.root, "Merge: ", true, false, true, move |branch| {
        let mut a = with_args(&c2, &["merge"]);
        a.extend(extra.iter().map(|s| s.to_string()));
        a.push(branch);
        run_and_refresh(&c2.root, a, "merge", false);
    });
}
fn merge_plain(ctx: Ctx) {
    merge_with(ctx, &["--no-edit"]);
}
fn merge_no_commit(ctx: Ctx) {
    merge_with(ctx, &["--no-commit", "--no-ff"]);
}
fn merge_squash(ctx: Ctx) {
    merge_with(ctx, &["--squash"]);
}
fn merge_preview(ctx: Ctx) {
    let root = ctx.root.clone();
    read_branch(&ctx.root, "Preview merging: ", true, false, true, move |branch| {
        views::show_diff(&root, &format!("Merging {branch} would change"), &[format!("HEAD...{branch}")]);
    });
}
fn merge_commit(ctx: Ctx) {
    commit::start(ctx, Kind::Create);
}
fn merge_abort(ctx: Ctx) {
    y_or_n("Abort this merge? ", move |yes| {
        if yes {
            run_and_refresh(&ctx.root, args(&["merge", "--abort"]), "merge --abort", false);
        }
    });
}

pub static MERGE: Popup = Popup {
    id: "merge",
    title: "Merge",
    op: Some(Op::Merge),
    switches: &[
        Switch { key: "-f", arg: "--ff-only", desc: "fast-forward only" },
        Switch { key: "-n", arg: "--no-ff", desc: "no fast-forward" },
    ],
    actions: &[
        Action { key: "m", name: "merge", desc: "merge", when: When::Idle, run: merge_plain },
        Action { key: "n", name: "no-commit", desc: "merge, don't commit", when: When::Idle, run: merge_no_commit },
        Action { key: "s", name: "squash", desc: "squash merge", when: When::Idle, run: merge_squash },
        Action { key: "p", name: "preview", desc: "preview", when: When::Idle, run: merge_preview },
        Action { key: "m", name: "commit", desc: "commit merge", when: When::InProgress, run: merge_commit },
        Action { key: "a", name: "abort", desc: "abort", when: When::InProgress, run: merge_abort },
    ],
};

// ------------------------------------------------------ cherry-pick, revert

fn pick(ctx: Ctx, no_commit: bool) {
    let c2 = ctx.clone();
    read_commit(&ctx, "Cherry-pick: ", move |c| {
        let mut a = with_args(&c2, &["cherry-pick"]);
        if no_commit {
            a.push("--no-commit".into());
        }
        a.push(c);
        run_and_refresh(&c2.root, a, "cherry-pick", false);
    });
}
fn pick_commit(ctx: Ctx) {
    pick(ctx, false);
}
fn pick_apply(ctx: Ctx) {
    pick(ctx, true);
}
fn sequencer(ctx: Ctx, cmd: &str, what: &str) {
    run_and_refresh(&ctx.root, args(&[cmd, what]), &format!("{cmd} {what}"), false);
}
fn pick_continue(ctx: Ctx) {
    sequencer(ctx, "cherry-pick", "--continue");
}
fn pick_skip(ctx: Ctx) {
    sequencer(ctx, "cherry-pick", "--skip");
}
fn pick_abort(ctx: Ctx) {
    sequencer(ctx, "cherry-pick", "--abort");
}

pub static CHERRY_PICK: Popup = Popup {
    id: "cherry-pick",
    title: "Cherry-pick",
    op: Some(Op::CherryPick),
    switches: &[
        Switch { key: "-x", arg: "-x", desc: "reference the commit" },
        Switch { key: "-s", arg: "--signoff", desc: "signoff" },
    ],
    actions: &[
        Action { key: "A", name: "pick", desc: "pick", when: When::Idle, run: pick_commit },
        Action { key: "a", name: "apply", desc: "apply (don't commit)", when: When::Idle, run: pick_apply },
        Action { key: "A", name: "continue", desc: "continue", when: When::InProgress, run: pick_continue },
        Action { key: "s", name: "skip", desc: "skip", when: When::InProgress, run: pick_skip },
        Action { key: "a", name: "abort", desc: "abort", when: When::InProgress, run: pick_abort },
    ],
};

fn revert(ctx: Ctx, no_commit: bool) {
    let c2 = ctx.clone();
    read_commit(&ctx, "Revert: ", move |c| {
        let mut a = with_args(&c2, &["revert"]);
        a.push(if no_commit { "--no-commit".into() } else { "--no-edit".into() });
        a.push(c);
        run_and_refresh(&c2.root, a, "revert", false);
    });
}
fn revert_commit(ctx: Ctx) {
    revert(ctx, false);
}
fn revert_changes(ctx: Ctx) {
    revert(ctx, true);
}
fn revert_continue(ctx: Ctx) {
    sequencer(ctx, "revert", "--continue");
}
fn revert_skip(ctx: Ctx) {
    sequencer(ctx, "revert", "--skip");
}
fn revert_abort(ctx: Ctx) {
    sequencer(ctx, "revert", "--abort");
}

pub static REVERT: Popup = Popup {
    id: "revert",
    title: "Revert",
    op: Some(Op::Revert),
    switches: &[Switch { key: "-s", arg: "--signoff", desc: "signoff" }],
    actions: &[
        Action { key: "V", name: "commit", desc: "revert commit", when: When::Idle, run: revert_commit },
        Action { key: "v", name: "changes", desc: "revert changes", when: When::Idle, run: revert_changes },
        Action { key: "V", name: "continue", desc: "continue", when: When::InProgress, run: revert_continue },
        Action { key: "s", name: "skip", desc: "skip", when: When::InProgress, run: revert_skip },
        Action { key: "a", name: "abort", desc: "abort", when: When::InProgress, run: revert_abort },
    ],
};

// ------------------------------------------------------------------- reset

fn reset_mode(ctx: Ctx, mode: &'static str) {
    let c2 = ctx.clone();
    read_commit(&ctx, &format!("Reset HEAD ({mode}) to: "), move |to| {
        let go = move || run_and_refresh(&c2.root, args(&["reset", &format!("--{mode}"), &to]), "reset", false);
        if mode == "hard" {
            y_or_n("Reset --hard throws away all uncommitted changes. Go ahead? ", move |yes| {
                if yes {
                    go();
                }
            });
        } else {
            go();
        }
    });
}
fn reset_mixed(ctx: Ctx) {
    reset_mode(ctx, "mixed");
}
fn reset_soft(ctx: Ctx) {
    reset_mode(ctx, "soft");
}
fn reset_hard(ctx: Ctx) {
    reset_mode(ctx, "hard");
}
fn reset_keep(ctx: Ctx) {
    reset_mode(ctx, "keep");
}
fn reset_index(ctx: Ctx) {
    let c2 = ctx.clone();
    read_commit(&ctx, "Reset the index to: ", move |to| {
        run_and_refresh(&c2.root, args(&["reset", "-q", &to, "--", "."]), "reset index", false);
    });
}
fn reset_file(ctx: Ctx) {
    let c2 = ctx.clone();
    let file = ctx.file.clone().unwrap_or_default();
    read_nonempty("Reset file: ", &file, move |file| {
        let c3 = c2.clone();
        read_branch_or(&c2, &format!("Reset {file} to (empty: HEAD): "), "HEAD", move |to| {
            let r = c3.root.clone();
            y_or_n(&format!("Replace {file} with its version in {to}? "), move |yes| {
                if yes {
                    run_and_refresh(&r, args(&["checkout", &to, "--", &file]), "reset file", false);
                }
            });
        });
    });
}

pub static RESET: Popup = Popup {
    id: "reset",
    title: "Reset",
    op: None,
    switches: &[],
    actions: &[
        Action { key: "m", name: "mixed", desc: "mixed (HEAD and index)", when: When::Always, run: reset_mixed },
        Action { key: "s", name: "soft", desc: "soft (HEAD only)", when: When::Always, run: reset_soft },
        Action { key: "h", name: "hard", desc: "hard (HEAD, index, worktree)", when: When::Always, run: reset_hard },
        Action { key: "k", name: "keep", desc: "keep (HEAD, index, keeping changes)", when: When::Always, run: reset_keep },
        Action { key: "i", name: "index", desc: "index only", when: When::Always, run: reset_index },
        Action { key: "f", name: "file", desc: "a file", when: When::Always, run: reset_file },
    ],
};

// --------------------------------------------------------------------- tag

fn tag_create(ctx: Ctx) {
    let c2 = ctx.clone();
    read_nonempty("Tag name: ", "", move |name| {
        let target = c2.commit.clone().filter(|c| !c.starts_with("stash@")).unwrap_or_else(|| "HEAD".into());
        let annotate = c2.has("--annotate");
        let c3 = c2.clone();
        let go = move |msg: Option<String>| {
            let mut a = args(&["tag"]);
            if c3.has("--force") {
                a.push("--force".into());
            }
            if let Some(m) = msg {
                a.extend(["-a".into(), "-m".into(), m]);
            }
            a.extend([name, target]);
            run_and_refresh(&c3.root, a, "tag", false);
        };
        if annotate {
            read_nonempty("Tag message: ", "", move |m| go(Some(m)));
        } else {
            go(None);
        }
    });
}

fn tag_delete(ctx: Ctx) {
    let root = ctx.root.clone();
    git::run(&ctx.root, &["tag", "-l"], move |r| {
        let tags: Vec<String> = r.unwrap_or_default().lines().map(str::to_string).collect();
        if tags.is_empty() {
            message("No tags");
            return;
        }
        actions::choose("Delete tag: ", tags, None, true, move |t| {
            y_or_n(&format!("Delete tag {t}? "), move |yes| {
                if yes {
                    run_and_refresh(&root, args(&["tag", "-d", &t]), "delete tag", false);
                }
            });
        });
    });
}

pub static TAG: Popup = Popup {
    id: "tag",
    title: "Tag",
    op: None,
    switches: &[
        Switch { key: "-a", arg: "--annotate", desc: "annotate" },
        Switch { key: "-f", arg: "--force", desc: "force" },
    ],
    actions: &[
        Action { key: "t", name: "create", desc: "create", when: When::Always, run: tag_create },
        Action { key: "k", name: "delete", desc: "delete", when: When::Always, run: tag_delete },
    ],
};

// --------------------------------------------------------------------- log

/// The log options every log gets, plus the switches.
fn log_args(ctx: &Ctx) -> Vec<String> {
    let mut a = args(&["-n", "256"]);
    a.extend(ctx.args.iter().cloned());
    a
}
fn log_current(ctx: Ctx) {
    views::show_log(&ctx.root, &[], &log_args(&ctx), &[], false);
}
fn log_other(ctx: Ctx) {
    let c2 = ctx.clone();
    read_branch(&ctx.root, "Log rev or range: ", true, false, false, move |rev| {
        views::show_log(&c2.root, &[rev], &log_args(&c2), &[], false)
    });
}
fn log_local(ctx: Ctx) {
    views::show_log(&ctx.root, &["--branches".into()], &log_args(&ctx), &[], false);
}
fn log_branches(ctx: Ctx) {
    views::show_log(&ctx.root, &["--branches".into(), "--remotes".into()], &log_args(&ctx), &[], false);
}
fn log_all(ctx: Ctx) {
    views::show_log(&ctx.root, &["--all".into()], &log_args(&ctx), &[], false);
}
fn log_file(ctx: Ctx) {
    let c2 = ctx.clone();
    let file = ctx.file.clone().unwrap_or_default();
    read_nonempty("Log of file: ", &file, move |file| {
        let mut a = log_args(&c2);
        a.push("--follow".into());
        views::show_log(&c2.root, &[], &a, &[file], false);
    });
}
fn log_reflog(ctx: Ctx) {
    let root = ctx.root.clone();
    git::run(&ctx.root, &["reflog", "-n", "256", "--format=%H%x00%h%x00%gd%x00%gs"], move |r| match r {
        Ok(out) => {
            let entries: Vec<git::LogEntry> = out
                .lines()
                .filter_map(|l| {
                    let f: Vec<&str> = l.split('\0').collect();
                    (f.len() >= 4).then(|| git::LogEntry {
                        hash: f[0].into(),
                        short: f[1].into(),
                        author: String::new(),
                        date: String::new(),
                        refs: f[2].into(),
                        subject: f[3].into(),
                    })
                })
                .collect();
            views::show_reflog(&root, &entries);
        }
        Err(e) => message(&format!("magit: reflog failed: {e}")),
    });
}

pub static LOG: Popup = Popup {
    id: "log",
    title: "Log",
    op: None,
    switches: &[
        Switch { key: "-m", arg: "--no-merges", desc: "no merges" },
        Switch { key: "-p", arg: "--first-parent", desc: "first parent" },
        Switch { key: "-r", arg: "--reverse", desc: "reverse" },
    ],
    actions: &[
        Action { key: "l", name: "current", desc: "current", when: When::Always, run: log_current },
        Action { key: "o", name: "other", desc: "other", when: When::Always, run: log_other },
        Action { key: "h", name: "head", desc: "HEAD", when: When::Always, run: log_current },
        Action { key: "L", name: "local", desc: "local branches", when: When::Always, run: log_local },
        Action { key: "b", name: "branches", desc: "all branches", when: When::Always, run: log_branches },
        Action { key: "a", name: "all", desc: "all references", when: When::Always, run: log_all },
        Action { key: "f", name: "file", desc: "a file", when: When::Always, run: log_file },
        Action { key: "O", name: "reflog", desc: "reflog", when: When::Always, run: log_reflog },
    ],
};

// -------------------------------------------------------------------- diff

fn diff_args(ctx: &Ctx, base: &[&str]) -> Vec<String> {
    let mut a = ctx.args.clone();
    a.extend(base.iter().map(|s| s.to_string()));
    a
}
fn diff_dwim(ctx: Ctx) {
    match (&ctx.target, &ctx.commit) {
        (Some(StatusLine::File { section, path } | StatusLine::Hunk { section, path, .. }), _) => {
            let a = match section {
                Section::Staged => diff_args(&ctx, &["--cached", "--", path]),
                Section::Untracked => diff_args(&ctx, &["--no-index", "--", "/dev/null", path]),
                _ => diff_args(&ctx, &["--", path]),
            };
            views::show_diff(&ctx.root, &format!("Changes to {path}"), &a);
        }
        (Some(StatusLine::Heading(Section::Staged)), _) => diff_staged(ctx),
        (_, Some(c)) if c.starts_with("stash@") => views::show_stash(&ctx.root, c),
        (_, Some(c)) => views::show_commit(&ctx.root, c),
        _ => diff_unstaged(ctx),
    }
}
fn diff_staged(ctx: Ctx) {
    views::show_diff(&ctx.root, "Staged changes", &diff_args(&ctx, &["--cached"]));
}
fn diff_unstaged(ctx: Ctx) {
    views::show_diff(&ctx.root, "Unstaged changes", &diff_args(&ctx, &[]));
}
fn diff_worktree(ctx: Ctx) {
    views::show_diff(&ctx.root, "Changes since HEAD", &diff_args(&ctx, &["HEAD"]));
}
fn diff_range(ctx: Ctx) {
    let c2 = ctx.clone();
    read_branch(&ctx.root, "Diff range (A..B or A...B or rev): ", true, false, false, move |range| {
        views::show_diff(&c2.root, &format!("Diff {range}"), &diff_args(&c2, &[&range]));
    });
}
fn diff_commit(ctx: Ctx) {
    let root = ctx.root.clone();
    read_commit(&ctx, "Show commit: ", move |c| views::show_commit(&root, &c));
}
fn diff_stash(ctx: Ctx) {
    let root = ctx.root.clone();
    read_stash(&ctx, "Show stash: ", move |s| views::show_stash(&root, &s));
}

pub static DIFF: Popup = Popup {
    id: "diff",
    title: "Diff",
    op: None,
    switches: &[
        Switch { key: "-w", arg: "--ignore-all-space", desc: "ignore whitespace" },
        Switch { key: "-b", arg: "--ignore-space-change", desc: "ignore space changes" },
        Switch { key: "-s", arg: "--stat", desc: "stat" },
    ],
    actions: &[
        Action { key: "d", name: "dwim", desc: "dwim", when: When::Always, run: diff_dwim },
        Action { key: "s", name: "staged", desc: "staged", when: When::Always, run: diff_staged },
        Action { key: "u", name: "unstaged", desc: "unstaged", when: When::Always, run: diff_unstaged },
        Action { key: "w", name: "worktree", desc: "worktree vs HEAD", when: When::Always, run: diff_worktree },
        Action { key: "r", name: "range", desc: "range", when: When::Always, run: diff_range },
        Action { key: "c", name: "commit", desc: "commit", when: When::Always, run: diff_commit },
        Action { key: "t", name: "stash", desc: "stash", when: When::Always, run: diff_stash },
    ],
};

// ------------------------------------------------------------------ remote

fn remote_add(ctx: Ctx) {
    let c2 = ctx.clone();
    read_nonempty("Remote name: ", "origin", move |name| {
        read_nonempty(&format!("URL of {name}: "), "", move |url| {
            let mut a = args(&["remote", "add"]);
            if c2.has("-f") {
                a.push("-f".into());
            }
            a.extend([name, url]);
            run_and_refresh(&c2.root, a, "remote add", false);
        });
    });
}
fn remote_rename(ctx: Ctx) {
    let r = ctx.root.clone();
    read_remote(&ctx.root, "Rename remote: ", move |old| {
        read_nonempty(&format!("Rename {old} to: "), &old.clone(), move |new| {
            run_and_refresh(&r, args(&["remote", "rename", &old, &new]), "remote rename", false);
        });
    });
}
fn remote_remove(ctx: Ctx) {
    let r = ctx.root.clone();
    read_remote(&ctx.root, "Remove remote: ", move |name| {
        y_or_n(&format!("Remove remote {name} (and its remote branches)? "), move |yes| {
            if yes {
                run_and_refresh(&r, args(&["remote", "remove", &name]), "remote remove", false);
            }
        });
    });
}
fn remote_url(ctx: Ctx) {
    let r = ctx.root.clone();
    read_remote(&ctx.root, "Set the URL of remote: ", move |name| {
        let r2 = r.clone();
        git::run(&r, &["remote", "get-url", &name.clone()], move |url| {
            let url = url.unwrap_or_default().trim().to_string();
            read_nonempty(&format!("URL of {name}: "), &url, move |new| {
                run_and_refresh(&r2, args(&["remote", "set-url", &name, &new]), "remote set-url", false);
            });
        });
    });
}
fn remote_prune(ctx: Ctx) {
    let r = ctx.root.clone();
    read_remote(&ctx.root, "Prune stale branches of remote: ", move |name| {
        run_and_refresh(&r, args(&["remote", "prune", &name]), "remote prune", false);
    });
}

pub static REMOTE: Popup = Popup {
    id: "remote",
    title: "Remote",
    op: None,
    switches: &[Switch { key: "-f", arg: "-f", desc: "fetch after add" }],
    actions: &[
        Action { key: "a", name: "add", desc: "add", when: When::Always, run: remote_add },
        Action { key: "r", name: "rename", desc: "rename", when: When::Always, run: remote_rename },
        Action { key: "k", name: "remove", desc: "remove", when: When::Always, run: remote_remove },
        Action { key: "u", name: "url", desc: "set url", when: When::Always, run: remote_url },
        Action { key: "p", name: "prune", desc: "prune stale branches", when: When::Always, run: remote_prune },
    ],
};

// ------------------------------------------------------------------ ignore

fn ignore_shared(ctx: Ctx) {
    actions::ignore(ctx, false);
}
fn ignore_private(ctx: Ctx) {
    actions::ignore(ctx, true);
}

pub static IGNORE: Popup = Popup {
    id: "gitignore",
    title: "Ignore",
    op: None,
    switches: &[],
    actions: &[
        Action { key: "t", name: "shared", desc: "in .gitignore", when: When::Always, run: ignore_shared },
        Action { key: "p", name: "private", desc: "privately (.git/info/exclude)", when: When::Always, run: ignore_private },
    ],
};

// ------------------------------------------------------------------ bisect

/// Bisect commands say where they are on stdout's first line.
fn bisect_run(ctx: &Ctx, a: Vec<String>) {
    let root = ctx.root.clone();
    git::exec(&ctx.root, Run::owned(a).logged(), move |out| {
        let said = out
            .stdout
            .lines()
            .find(|l| !l.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| out.error());
        message(&said);
        crate::status::refresh_root(&root);
    });
}
fn bisect_start(ctx: Ctx) {
    let c2 = ctx.clone();
    read_branch_or(&ctx, "Bad commit (empty: HEAD): ", "HEAD", move |bad| {
        let c3 = c2.clone();
        read_branch(&c2.root, "Good commit: ", true, false, false, move |good| {
            bisect_run(&c3, args(&["bisect", "start", &bad, &good]));
        });
    });
}
fn bisect_good(ctx: Ctx) {
    bisect_run(&ctx, args(&["bisect", "good"]));
}
fn bisect_bad(ctx: Ctx) {
    bisect_run(&ctx, args(&["bisect", "bad"]));
}
fn bisect_skip(ctx: Ctx) {
    bisect_run(&ctx, args(&["bisect", "skip"]));
}
fn bisect_reset(ctx: Ctx) {
    run_and_refresh(&ctx.root, args(&["bisect", "reset"]), "bisect reset", false);
}

pub static BISECT: Popup = Popup {
    id: "bisect",
    title: "Bisect",
    op: Some(Op::Bisect),
    switches: &[],
    actions: &[
        Action { key: "B", name: "start", desc: "start", when: When::Idle, run: bisect_start },
        Action { key: "g", name: "good", desc: "good", when: When::InProgress, run: bisect_good },
        Action { key: "b", name: "bad", desc: "bad", when: When::InProgress, run: bisect_bad },
        Action { key: "s", name: "skip", desc: "skip", when: When::InProgress, run: bisect_skip },
        Action { key: "r", name: "reset", desc: "reset", when: When::InProgress, run: bisect_reset },
    ],
};

// ---------------------------------------------------------------- dispatch

fn open_by_id(id: &str) {
    if let Some(p) = crate::popup::all().into_iter().find(|p| p.id == id) {
        crate::popup::open(p);
    }
}
macro_rules! opener {
    ($f:ident, $id:literal) => {
        fn $f(_: Ctx) {
            open_by_id($id);
        }
    };
}
opener!(open_commit, "commit");
opener!(open_push, "push");
opener!(open_pull, "pull");
opener!(open_fetch, "fetch");
opener!(open_branch, "branch");
opener!(open_stash, "stash");
opener!(open_rebase, "rebase");
opener!(open_merge, "merge");
opener!(open_pick, "cherry-pick");
opener!(open_revert, "revert");
opener!(open_reset, "reset");
opener!(open_tag, "tag");
opener!(open_log, "log");
opener!(open_diff, "diff");
opener!(open_remote, "remote");
opener!(open_ignore, "gitignore");
opener!(open_bisect, "bisect");
fn open_process(ctx: Ctx) {
    views::show_process_log(&ctx.root);
}
fn open_refs(ctx: Ctx) {
    actions::show_refs(ctx);
}
fn open_blame(ctx: Ctx) {
    actions::blame(ctx);
}
fn open_run(ctx: Ctx) {
    actions::run_git(ctx);
}

pub static DISPATCH: Popup = Popup {
    id: "dispatch",
    title: "Magit",
    op: None,
    switches: &[],
    actions: &[
        Action { key: "c", name: "commit", desc: "commit", when: When::Always, run: open_commit },
        Action { key: "P", name: "push", desc: "push", when: When::Always, run: open_push },
        Action { key: "F", name: "pull", desc: "pull", when: When::Always, run: open_pull },
        Action { key: "f", name: "fetch", desc: "fetch", when: When::Always, run: open_fetch },
        Action { key: "b", name: "branch", desc: "branch", when: When::Always, run: open_branch },
        Action { key: "z", name: "stash", desc: "stash", when: When::Always, run: open_stash },
        Action { key: "r", name: "rebase", desc: "rebase", when: When::Always, run: open_rebase },
        Action { key: "m", name: "merge", desc: "merge", when: When::Always, run: open_merge },
        Action { key: "A", name: "cherry-pick", desc: "cherry-pick", when: When::Always, run: open_pick },
        Action { key: "V", name: "revert", desc: "revert", when: When::Always, run: open_revert },
        Action { key: "X", name: "reset", desc: "reset", when: When::Always, run: open_reset },
        Action { key: "t", name: "tag", desc: "tag", when: When::Always, run: open_tag },
        Action { key: "l", name: "log", desc: "log", when: When::Always, run: open_log },
        Action { key: "d", name: "diff", desc: "diff", when: When::Always, run: open_diff },
        Action { key: "M", name: "remote", desc: "remote", when: When::Always, run: open_remote },
        Action { key: "i", name: "ignore", desc: "ignore", when: When::Always, run: open_ignore },
        Action { key: "B", name: "bisect", desc: "bisect", when: When::Always, run: open_bisect },
        Action { key: "y", name: "refs", desc: "refs", when: When::Always, run: open_refs },
        Action { key: "!", name: "run", desc: "run git", when: When::Always, run: open_run },
        Action { key: "$", name: "process", desc: "process log", when: When::Always, run: open_process },
    ],
};

fn file_stage(ctx: Ctx) {
    match &ctx.file {
        Some(f) => run_and_refresh(&ctx.root, args(&["add", "--", f]), "stage", false),
        None => message("No file here"),
    }
}
fn file_unstage(ctx: Ctx) {
    match &ctx.file {
        Some(f) => run_and_refresh(&ctx.root, args(&["restore", "--staged", "--", f]), "unstage", false),
        None => message("No file here"),
    }
}
fn file_diff(ctx: Ctx) {
    match &ctx.file {
        Some(f) => views::show_diff(&ctx.root, &format!("Changes to {f} since HEAD"), &args(&["HEAD", "--", f])),
        None => message("No file here"),
    }
}
fn file_log(ctx: Ctx) {
    match ctx.file.clone() {
        Some(f) => views::show_log(&ctx.root, &[], &args(&["-n", "256", "--follow"]), &[f], false),
        None => message("No file here"),
    }
}
fn file_commit(ctx: Ctx) {
    commit::start(ctx, Kind::Create);
}

/// What `C-c M-g` offers in a file's buffer.
pub static FILE: Popup = Popup {
    id: "file-dispatch",
    title: "This file",
    op: None,
    switches: &[],
    actions: &[
        Action { key: "s", name: "stage", desc: "stage", when: When::Always, run: file_stage },
        Action { key: "u", name: "unstage", desc: "unstage", when: When::Always, run: file_unstage },
        Action { key: "c", name: "commit", desc: "commit", when: When::Always, run: file_commit },
        Action { key: "d", name: "diff", desc: "diff", when: When::Always, run: file_diff },
        Action { key: "l", name: "log", desc: "log", when: When::Always, run: file_log },
        Action { key: "B", name: "blame", desc: "blame", when: When::Always, run: open_blame },
    ],
};

/// Every popup, its key in Magit buffers, and what M-x says it does.
pub const ALL: &[(&Popup, &str, &str)] = &[
    (&COMMIT, "c", "Commit: create, amend, extend, reword, fixup, squash."),
    (&PUSH, "P", "Push: to upstream, elsewhere, tags."),
    (&PULL, "F", "Pull: from upstream or elsewhere."),
    (&FETCH, "f", "Fetch: from upstream, all remotes, elsewhere."),
    (&BRANCH, "b", "Branch: checkout, create, rename, delete, upstream."),
    (&STASH, "z", "Stash: push, apply, pop, drop, show."),
    (&REBASE, "r", "Rebase: onto upstream or elsewhere; continue, skip, abort."),
    (&MERGE, "m", "Merge: merge, squash, preview; commit or abort."),
    (&CHERRY_PICK, "A", "Cherry-pick: pick or apply; continue, skip, abort."),
    (&REVERT, "V", "Revert: a commit or its changes; continue, skip, abort."),
    (&RESET, "X", "Reset: mixed, soft, hard, keep, index, a file."),
    (&TAG, "t", "Tag: create or delete."),
    (&LOG, "l", "Log: current, other, branches, all, a file, reflog."),
    (&DIFF, "d", "Diff: dwim, staged, unstaged, worktree, range, commit, stash."),
    (&REMOTE, "M", "Remote: add, rename, remove, set url, prune."),
    (&IGNORE, "i", "Ignore a file: in .gitignore or privately."),
    (&BISECT, "B", "Bisect: start; good, bad, skip, reset."),
    (&DISPATCH, "h", "Every Magit popup."),
    (&FILE, "", "Git commands for the file being visited."),
];

/// Register every popup.
pub fn register_all() -> Result<(), String> {
    crate::popup::register_quit()?;
    for (p, _, doc) in ALL {
        register(p, doc)?;
    }
    Ok(())
}
