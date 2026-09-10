# Chop guide

This guide explains the full Chop workflow. It also shows how to recover saved work.

For an overview and a quick start, see the [README](../README.md).

## Install and upgrade

Install Chop from the public Homebrew tap:

```sh
brew install alberto-castano/tap/chop
```

Upgrade Chop:

```sh
brew update
brew upgrade chop
```

Check the installed version:

```sh
chop --version
```

## Configure repository roots

A root is a folder that contains one or more Git repositories.

Start the root editor:

```sh
chop config
```

The path field starts at `~/`. Type a path to see matching folders.
Use Up and Down to select a suggestion. Press Tab to complete the path.
For example, type `~/Wo` and press Tab to complete `~/Work/` when that folder exists.
Press Enter to add the typed folder. Press Ctrl-U to clear the path field.
Press Ctrl-R to switch between the path field and scan roots. Press Delete to remove a selected root.
Press Ctrl-S to save the list. Running this command again edits the saved list.

You can also replace the list without the editor:

```sh
chop config --root ~/repos --root ~/work
```

Chop saves absolute paths. It removes duplicate roots.

The default config path is:

```text
~/.config/chop/roots
```

With `XDG_CONFIG_HOME`, the path is `$XDG_CONFIG_HOME/chop/roots`.

### Override roots for one run

Use `--root` after the `all` command:

```sh
chop all --root ~/repos
```

Repeat the option to scan more than one root:

```sh
chop all --root ~/repos --root /Volumes/code
```

These options do not change the saved config.

## Open the TUI

Run Chop without arguments:

```sh
chop
```

`chop all` starts the same workflow. It also accepts all run options.

The TUI stays open during these stages:

1. First setup, when the config file is missing or empty.
2. Repository scan.
3. Worktree plan.
4. Confirmation.
5. Removal progress.
6. Results.

Press Ctrl-C, `Esc`, or `q` to cancel. During removal, Chop finishes the active Git operation first.

## Read the groups

### Chop

This group contains clean linked worktrees. It also contains Review items moved with `x`.

A clean worktree has no local files or unsafe Git state. Chop rechecks it before removal.

### Review

This group needs your decision. The details panel explains the reason.

Common reasons include:

- Changed, staged, untracked, or ignored files.
- A detached commit without another reference.
- A worktree lock.
- An unfinished rebase, merge, or other Git operation.
- A nested repository or populated submodule.
- A stale worktree record.
- An inspection error.

### Keep

This group contains linked worktrees that Chop cannot remove.

It includes the worktree that contains your current directory. It can also include a bare repository record.

Main checkouts stay hidden. Chop always keeps them.

## Use the worktree plan

### Move and inspect

Click the worktree list or details panel to focus it. The focused panel has a warm border.
The mouse wheel and navigation keys control the focused panel. Tab switches focus without a mouse.
Click a worktree to select it. Group titles do not select a worktree.

| Key | Action |
| --- | --- |
| `↑` or `k` | Move up in the list or scroll details up. |
| `↓` or `j` | Move down in the list or scroll details down. |
| `Tab` | Switch panel focus. |
| `1` | Collapse or expand Chop. |
| `2` | Collapse or expand Review. |
| `3` | Collapse or expand Keep. |
| `Page Up` or `[` | Move or scroll up in the focused panel. |
| `Page Down` or `]` | Move or scroll down in the focused panel. |
| `Home` | Select the first row. |
| `End` | Select the last row. |
| `e` | Show scan errors. |
| `c` | Continue. |
| `q` or `Esc` | Cancel. |

### Choose a Review action

Select a Review worktree first.

| Key | Choice | Result |
| --- | --- | --- |
| `Shift-K` | Keep | Chop makes no change. |
| `v` | Refresh diff | Chop reloads changes in the details panel. |
| `p` | Stash + chop | Chop saves recoverable state before removal. |
| `x` | Chop anyway | Chop marks local state for deletion. |

Lowercase `k` moves up. Shift-K selects Keep.
Navigation skips group titles. The footer shows only actions available for the selected worktree.

Chop blocks the save choice when it cannot preserve the worktree safely.

The details panel loads changes in the background when you select a worktree.
It shows staged and unstaged patches, with additions in green and deletions in red.
It previews up to 20 untracked files, each no larger than 64 KiB. Git identifies binary changes without displaying binary contents.
Ignored paths appear in the status list; Chop does not preview their contents.
The preview stops at 2,000 lines or 200,000 bytes. Focus the details panel to scroll, or press `v` to refresh.

## Stash + chop

The save action protects recoverable Git state before removal.

Chop can:

- Stash staged, changed, untracked, and ignored files.
- Create rescue branches for private commits.
- Remove the linked worktree after the save checks pass.

Chop shows saved reference names on the result screen.

### Recover a stash

List stashes in the repository's main checkout:

```sh
git stash list
```

Inspect a stash before applying it:

```sh
git stash show --stat stash@{0}
git stash show --patch stash@{0}
```

Apply it without deleting the stash:

```sh
git stash apply stash@{0}
```

### Recover a rescue branch

The result screen prints the branch name. Rescue branches start with `chop-rescue-`.

List them:

```sh
git branch --list 'chop-rescue-*'
```

Create a new worktree from one:

```sh
git worktree add ../recovered-worktree chop-rescue-<name>
```

## Chop anyway

Press `x` on a Review worktree to start this choice.

Chop shows the full path. Type this exact text:

```text
CHOP <full-worktree-path>
```

The item then moves into the Chop group. It has a red `CHOP ANYWAY` label.

Press `u` to move it back into Review before execution.

This choice does not save changed, untracked, or ignored files.

## Confirm the plan

Press `c` after every Review item has a choice.

The confirmation screen lists all removal candidates. It marks saved items and local data loss.

Type the shown batch text. For three candidates, enter:

```text
CHOP 3
```

Nothing is removed before this confirmation.

The `--accept-data-loss` option skips this step only for clean worktrees. Saved and forced items still need confirmation.

## Preview changes

Use dry-run mode:

```sh
chop all --dry-run
```

Dry-run mode scans real repositories. It does not save config or remove worktrees.

## Use Chop in scripts

Use static output and no prompts:

```sh
chop all --non-interactive --accept-data-loss
```

This mode removes only clean linked worktrees. It keeps every Review worktree.

Pass explicit roots in automation:

```sh
chop all \
  --root /srv/repos \
  --non-interactive \
  --accept-data-loss
```

Exit codes:

| Code | Meaning |
| --- | --- |
| `0` | The run completed. |
| `1` | A scan, inspection, or removal failed. |
| `2` | The run was canceled, or a choice remains. |

## Scan large folders

The normal scan skips common cache and build folders. This keeps scans fast.

Use `--exhaustive` when repositories can exist inside those folders:

```sh
chop all --exhaustive --dry-run
```

Exhaustive scans can take much longer.

## Disable colors

Set `NO_COLOR`:

```sh
NO_COLOR=1 chop all --dry-run
```

Piped output uses the static format.

## Troubleshooting

### No roots are configured

Run:

```sh
chop config
```

For scripts, pass at least one `--root` option.

### A worktree moved to Review

Read its details. Chop found local state or a Git condition that blocks clean removal.

Inspect the inline diff and press `v` to refresh it. Choose Keep, save it, or use Chop anyway.

### State changed after the scan

Another process changed the worktree. Cancel the run and start Chop again.

### Chop cannot save a worktree

Finish active Git operations first. Check nested repositories, submodules, index flags, and repository filters.

Keep the worktree if you are not certain.

### Homebrew still runs an old version

Refresh the tap and reinstall:

```sh
brew update
brew reinstall alberto-castano/tap/chop
chop --version
```

If the old binary remains, inspect its path:

```sh
command -v chop
brew list --versions chop
```

## Development

Run the checks before submitting a code change:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Release builds optimize for binary size with link-time optimization and stripped symbols.
These settings also apply to `cargo install`. Symbol stripping limits crash diagnostics.

See the [release guide](releasing.md) for version preparation, Actions setup, and Homebrew updates.
