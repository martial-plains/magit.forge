# magit for Forge

A Magit-style Git interface. `C-x g` (or `M-x magit-status`) opens the
status buffer for the current file's repository:

```
Head:     main Fix the parser
Merge:    origin/main [ahead 1] Release 1.2
Tag:      v1.2 (3)

Untracked files (1)
notes.txt

Unstaged changes (2)
unmerged   src/lexer.rs
modified   src/parser.rs
@@ -10,7 +10,7 @@ fn parse()
-    let x = old();
+    let x = new();

Staged changes (1)
new file   src/token.rs

Stashes (1)
stash@{0} On main: experiment

Unmerged into origin/main (1)
1a2b3c4 (HEAD -> main) Fix the parser
```

Empty sections are left out. While a rebase, merge, cherry-pick, revert
or bisect is in progress, the Head section says so, and a rebase lists
its sequence (what's left, where it stopped, what's done).

## Keys in Magit buffers

| Key | What |
|-----|------|
| `n` `p` | next / previous section: heading, file, hunk, stash, commit |
| `M-n` `M-p` `^` | next / previous sibling, parent section |
| `TAB` | show or hide a file's diff; on a heading, fold the section |
| `RET` | visit the file (at the diff line's line), show a commit or stash |
| `s` | stage the file, hunk, or section at point |
| `u` | unstage it |
| `k` | discard it (asks); on a conflict, keep ours or theirs; drop a stash |
| `v` | reverse a staged change in the worktree |
| `S` `U` | stage all tracked changes / unstage everything |
| `x` | reset HEAD to the commit at point |
| `y` | list branches and tags |
| `!` | run any git command |
| `$` | the git commands run, and everything they said |
| `g` `q` `?` | refresh, close, list all keys |

With the region inside a hunk, `s`, `u` and `k` act on just the
selected lines; over several files, on those files.

## Popups

A prefix key opens a popup, as in Magit: toggle switches with `-` and a
letter, then pick an action (`q` or `C-g` closes it). Forge lists the
popup's keys in a panel while it is open.

| Key | Popup | Switches | Actions |
|-----|-------|----------|---------|
| `c` | Commit | `-a` all, `-e` allow empty, `-n` no hooks, `-s` signoff, `-R` reset author | `c` commit, `e` extend, `w` reword, `a` amend, `f` fixup, `s` squash |
| `P` | Push | `-f` force-with-lease, `-F` force, `-n` dry run, `-h` no hooks, `-T` follow tags | `p` upstream (sets it if missing), `u` set upstream, `e` elsewhere, `o` other branch, `T` a tag, `t` all tags |
| `F` | Pull | `-r` rebase, `-f` ff-only, `-A` autostash | `u` upstream, `e` elsewhere |
| `f` | Fetch | `-p` prune, `-t` tags | `u` upstream, `a` all, `e` elsewhere, `m` submodules |
| `b` | Branch | | `b` checkout, `l` local from remote, `c` create & checkout, `n` create, `m` rename, `x` reset, `k` delete, `u` set upstream |
| `z` | Stash | `-u` untracked, `-a` all | `z` both, `i` index, `x` keeping index, `a` apply, `p` pop, `k` drop, `v` show, `b` branch, `l` list |
| `r` | Rebase | `-A` autostash, `-r` rebase merges, `-k` keep empty | `u` onto upstream, `e` elsewhere, `f` autosquash; in progress: `r` continue, `s` skip, `a` abort |
| `m` | Merge | `-f` ff-only, `-n` no-ff | `m` merge, `n` no commit, `s` squash, `p` preview; in progress: `m` commit, `a` abort |
| `A` | Cherry-pick | `-x`, `-s` signoff | `A` pick, `a` apply; in progress: `A` continue, `s` skip, `a` abort |
| `V` | Revert | `-s` signoff | `V` revert, `v` changes only; in progress: `V` continue, `s` skip, `a` abort |
| `X` | Reset | | `m` mixed, `s` soft, `h` hard (asks), `k` keep, `i` index, `f` a file |
| `t` | Tag | `-a` annotate, `-f` force | `t` create, `k` delete |
| `l` | Log | `-m` no merges, `-p` first parent, `-r` reverse | `l` current, `o` other, `h` HEAD, `L` local, `b` branches, `a` all, `f` a file, `O` reflog |
| `d` | Diff | `-w` ignore whitespace, `-b` ignore space change, `-s` stat | `d` what's at point, `s` staged, `u` unstaged, `w` worktree, `r` range, `c` commit, `t` stash |
| `M` | Remote | `-f` fetch after add | `a` add, `r` rename, `k` remove, `u` set url, `p` prune |
| `i` | Ignore | | `t` in .gitignore, `p` privately (.git/info/exclude) |
| `B` | Bisect | | `B` start; in progress: `g` good, `b` bad, `s` skip, `r` reset |
| `h` | Every popup | | |

Actions that need a commit use the one at point (in the status, log,
revision, blame or refs buffer), else ask; branch questions complete and
offer a default.

Anywhere: `C-x g` opens the status, `C-x M-g` the popup of popups, and
`C-c M-g` a popup for the file being visited (stage, unstage, commit,
diff, log, blame).

## Committing

`c c` opens a message buffer; `C-c C-c` commits, `C-c C-k` cancels.
Everything below the scissors line is cut off, and git's own cleanup
removes comment lines. With nothing staged, it offers to commit
everything. During a merge, cherry-pick or revert the message git
prepared is filled in. `c a` and `c w` work with nothing staged.

## How it runs git

Everything shells out to `git` (named in `Cargo.toml` under
`[package.metadata.forge.permissions]`, granted on first load), so
merging, rebasing and pushing behave exactly like the command line. Each
git gets `GIT_EDITOR=true` (so `rebase --continue` and friends keep the
message they propose instead of waiting for an editor that isn't there),
`GIT_TERMINAL_PROMPT=0` (a password prompt fails instead of hanging; use
a credential helper or ssh agent) and `GIT_OPTIONAL_LOCKS=0`. Hunks are
applied with `git apply`, from diffs taken with plain `a/` `b/` prefixes
and no external diff or textconv, so user configuration can't break them.
Failures show git's own error; `$` shows the full output.

The status refreshes after every command and when a file in the
repository is saved. Status, log and revision buffers are read-only.

Needs Forge 0.1.9 (git's error output for plugins, read-only buffers,
keypad panels).

```sh
rustup target add wasm32-wasip2
cargo build --release --target wasm32-wasip2
forge --plugin target/wasm32-wasip2/release/forge_magit.wasm
```

Not here (yet): editing an interactive rebase's todo, worktrees,
submodules, notes, patches and blame overlays in the file itself.


## Install with use_package

Add this to `~/.config/forge/init.rhai`:

```rhai
use_package("magit", #{
  vc: "https://github.com/martial-plains/forge-plugin-magit",
  pin: "v0.1.0",
  commands: ["magit-status", "magit-dispatch"],
  grant: #{ processes: ["git"] },
});
```

The `processes` grant lets Magit run `git` outside the sandbox. Forge asks before
running the package build command the first time.

## Building

This plugin is built against the Forge plugin SDK (`forge-plugin-sdk`),
taken from the Forge repository named in `Cargo.toml` (change the `git =`
URL if Forge lives elsewhere).

    rustup target add wasm32-wasip2
    cargo build --release --target wasm32-wasip2

Copy the resulting `.wasm` to `~/.config/forge/plugins/`.

## License

GPL-3.0-or-later. See the license file in this directory.
