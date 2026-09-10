# Changelog

## Unreleased

## 0.3.0 - 2026-09-10

- Add space around the scan status and preserve the tree outline throughout the chopping animation.

- Use the same warm-colored ASCII log header across setup, scanning, worktree plans, progress, results, and saved notices.

- Add a manual GitHub Actions release process with source installation checks and automated Homebrew updates.

- Reduce release binary size with size optimization, link-time optimization, and symbol stripping.

- Rename the preservation action to `stash + chop`.

- Show an animated ASCII axe and tree in a compact scan view with elapsed time and scan status.

- Click panels or press Tab to switch focus; route mouse and keyboard scrolling to the focused panel.

- Stop worktree navigation at the first and last item instead of wrapping around.

- Load staged, unstaged, and small untracked file diffs in the details panel; use `v` to refresh.

- Skip group titles during navigation and toggle groups with `1`, `2`, and `3`.
- Show available worktree actions in the footer and label Keep as `Shift-K`.

- Open first setup when the saved root configuration is empty.
- Complete folder paths as you type in the root editor, with Tab completion and Enter to add roots.
- Introduce the root editor with an ASCII log and Chop lettering in warm colors, plus its purpose.
- Distinguish action and footer keys with warm brackets and fit shortcut rows to the terminal width.

## 0.2.0 - 2026-09-05

- Open the TUI when `chop` runs without a command.
- Hide main worktrees from plans and results.
- Move a Review worktree into Chop after exact path confirmation.
- Keep setup, scanning, review choices, confirmation, progress, and results in
  one TUI session.
- Configure scan roots in the TUI.
- Show each Review choice in the worktree plan before execution.

## 0.1.0 - 2026-09-04

- Find linked Git worktrees under one or more roots.
- Remove clean worktrees after explicit confirmation.
- Review, preserve, or delete worktrees with local state.
- Save default roots with `chop config`.
- Override saved roots with repeated `--root` options.
- Inspect Chop, Review, and Keep groups in a collapsible TUI.
- Show a colored static plan and a final removal summary when needed.
- Protect nested repositories, index locks, and hidden executable-bit changes.
