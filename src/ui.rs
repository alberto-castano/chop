use crate::git;
use crate::model::{Repository, State, Worktree};
use crate::terminal;
use std::fmt::Write as FmtWrite;
use std::io::{self, IsTerminal, Write};

#[derive(Debug, Default)]
pub struct Outcome {
    pub removed: usize,
    pub preserved: usize,
    pub kept: usize,
    pub needs_choice: usize,
    pub failed: usize,
}

const LOGO: &str = r"        __
  _____/ /_  ____  ____
 / ___/ __ \/ __ \/ __ \
/ /__/ / / / /_/ / /_/ /
\___/_/ /_/\____/ .___/
               /_/";

pub fn print_brand() {
    let color = terminal::color_enabled();
    println!("{}", terminal::info(color, LOGO));
    println!("Remove linked Git worktrees without guessing about local work.");
}

pub fn print_plan(repositories: &[Repository], dry_run: bool, ready_removal_available: bool) {
    print!(
        "{}",
        render_plan(
            repositories,
            dry_run,
            ready_removal_available,
            terminal::color_enabled()
        )
    );
}

pub fn print_candidate_summary(repositories: &[Repository]) {
    print!(
        "{}",
        render_candidate_summary(repositories, terminal::color_enabled())
    );
}

fn render_candidate_summary(repositories: &[Repository], color: bool) -> String {
    let candidates = removal_candidates(repositories);
    let mut output = String::new();
    writeln!(output, "\n{}", terminal::bold(color, "Removal candidates")).unwrap();

    if candidates.is_empty() {
        writeln!(output, "  Chop found no clean linked worktrees to remove.").unwrap();
        return output;
    }

    writeln!(
        output,
        "  Chop found {} clean removal candidate{}.",
        candidates.len(),
        plural(candidates.len())
    )
    .unwrap();
    write_worktree_list(&mut output, &candidates, color);
    output
}

fn render_plan(
    repositories: &[Repository],
    dry_run: bool,
    ready_removal_available: bool,
    color: bool,
) -> String {
    let mut output = String::new();
    let mut remove = 0;
    let mut review = 0;
    let mut keep = 0;
    for worktree in repositories
        .iter()
        .flat_map(|repository| &repository.worktrees)
        .filter(|worktree| !is_main_checkout(worktree))
    {
        match worktree.state() {
            State::Ready => remove += 1,
            State::NeedsChoice(_) => review += 1,
            State::Protected(_) => keep += 1,
        }
    }

    writeln!(output, "\n{}", terminal::bold(color, "Plan")).unwrap();
    writeln!(
        output,
        "  {}  {} clean linked worktree{} {}",
        terminal::good(color, &format!("{:<8}", "✓ Chop")),
        remove,
        plural(remove),
        if !ready_removal_available {
            "will stay because this run has no removal consent."
        } else if dry_run {
            "would be removed."
        } else {
            "can be removed after confirmation."
        }
    )
    .unwrap();
    writeln!(
        output,
        "  {}  {} worktree{} need{} your decision.",
        terminal::warn(color, &format!("{:<8}", "! Review")),
        review,
        plural(review),
        if review == 1 { "s" } else { "" }
    )
    .unwrap();
    writeln!(
        output,
        "  {}  {} bare or current worktree{} will stay.",
        terminal::info(color, &format!("{:<8}", "◆ Keep")),
        keep,
        plural(keep)
    )
    .unwrap();

    for repository in repositories {
        if repository.worktrees.iter().all(is_main_checkout) {
            continue;
        }
        let repository_path = repository
            .worktrees
            .iter()
            .find(|worktree| worktree.is_main)
            .map(|worktree| worktree.path.as_path())
            .or_else(|| {
                (repository
                    .common_dir
                    .file_name()
                    .is_some_and(|name| name == ".git"))
                .then(|| repository.common_dir.parent())
                .flatten()
            })
            .unwrap_or(repository.common_dir.as_path());
        let repository_name = repository_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| terminal::path(repository_path));
        writeln!(
            output,
            "\n{}",
            terminal::bold(color, &terminal::text(&repository_name))
        )
        .unwrap();
        for worktree in &repository.worktrees {
            if is_main_checkout(worktree) {
                continue;
            }
            let state = worktree.state();
            let (label, reasons) = describe(&state, dry_run, ready_removal_available, color);
            writeln!(
                output,
                "\n  {}  {}",
                label,
                terminal::bold(color, &worktree_reference(worktree))
            )
            .unwrap();
            writeln!(
                output,
                "      {}",
                terminal::muted(color, &terminal::path(&worktree.path))
            )
            .unwrap();
            for reason in reasons {
                writeln!(output, "      {}", terminal::text(&reason)).unwrap();
            }
        }
    }
    output
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

pub(crate) fn worktree_reference(worktree: &Worktree) -> String {
    if let Some(branch) = &worktree.branch {
        return terminal::text(branch.strip_prefix("refs/heads/").unwrap_or(branch));
    }
    let short = worktree.head.get(..12).unwrap_or(&worktree.head);
    format!("detached @ {}", terminal::text(short))
}

fn removal_candidates(repositories: &[Repository]) -> Vec<&Worktree> {
    repositories
        .iter()
        .flat_map(|repository| &repository.worktrees)
        .filter(|worktree| worktree.state() == State::Ready)
        .collect()
}

fn write_worktree_list(output: &mut String, worktrees: &[&Worktree], color: bool) {
    for worktree in worktrees {
        writeln!(
            output,
            "\n  {}  {}",
            terminal::good(color, "✓"),
            terminal::bold(color, &worktree_reference(worktree))
        )
        .unwrap();
        writeln!(
            output,
            "     {}",
            terminal::muted(color, &terminal::path(&worktree.path))
        )
        .unwrap();
    }
}

pub fn execute(
    repositories: &[Repository],
    dry_run: bool,
    non_interactive: bool,
    accept_data_loss: bool,
) -> Outcome {
    let mut outcome = Outcome::default();
    let candidates = removal_candidates(repositories);
    let can_ask = !non_interactive && io::stdin().is_terminal();
    let can_remove_ready = accept_data_loss || (can_ask && (dry_run || confirm_ready(&candidates)));

    for repository in repositories {
        for worktree in &repository.worktrees {
            if is_main_checkout(worktree) {
                continue;
            }
            match worktree.state() {
                State::Ready if can_remove_ready => {
                    remove_ready(worktree, dry_run, &mut outcome);
                }
                State::Ready => {
                    outcome.kept += 1;
                    outcome.needs_choice += 1;
                }
                State::Protected(_) => outcome.kept += 1,
                State::NeedsChoice(_)
                    if dry_run || non_interactive || !io::stdin().is_terminal() =>
                {
                    outcome.kept += 1;
                    outcome.needs_choice += 1;
                }
                State::NeedsChoice(_) => choose(worktree, &mut outcome),
            }
        }
    }

    outcome
}

pub fn ready_removal_available(non_interactive: bool, accept_data_loss: bool) -> bool {
    accept_data_loss || (!non_interactive && io::stdin().is_terminal())
}

fn is_main_checkout(worktree: &Worktree) -> bool {
    worktree.is_main && !worktree.flags.bare
}

fn confirm_ready(candidates: &[&Worktree]) -> bool {
    let count = candidates.len();
    if count == 0 {
        return true;
    }
    let color = terminal::color_enabled();
    println!(
        "\n{}",
        terminal::bold(
            color,
            &format!("Remove {count} clean worktree{}?", plural(count))
        )
    );
    let mut list = String::new();
    write_worktree_list(&mut list, candidates, color);
    print!("{list}");
    println!();
    println!("  Git branches will stay.");
    println!("  Worktree directories will be deleted.");
    println!("  A program can create files after the scan. Those files can also be deleted.");
    let expected = format!("CHOP {count}");
    println!(
        "\n  Type {} to continue. Press Enter to cancel.",
        terminal::bold(color, &expected)
    );
    prompt("  > ").is_some_and(|answer| answer == expected)
}

fn remove_ready(worktree: &Worktree, dry_run: bool, outcome: &mut Outcome) {
    if dry_run {
        outcome.removed += 1;
        return;
    }
    match git::remove_ready(worktree) {
        Ok(()) => {
            println!(
                "{} {}",
                terminal::good(terminal::color_enabled(), "Removed"),
                terminal::path(&worktree.path)
            );
            outcome.removed += 1;
        }
        Err(error) => {
            eprintln!(
                "Kept {}: {}",
                terminal::path(&worktree.path),
                terminal::text(&error)
            );
            outcome.failed += 1;
        }
    }
}

fn choose(worktree: &Worktree, outcome: &mut Outcome) {
    loop {
        let color = terminal::color_enabled();
        let can_preserve = can_preserve(worktree);
        let can_view = can_view(worktree);
        println!(
            "\n{}  {}",
            terminal::warn(color, "Review"),
            terminal::bold(color, &worktree_reference(worktree))
        );
        println!(
            "  {}",
            terminal::muted(color, &terminal::path(&worktree.path))
        );
        println!("\n  {}", terminal::bold(color, "Why chop stopped"));
        if let State::NeedsChoice(reasons) = worktree.state() {
            for reason in reasons {
                println!("    • {}", terminal::text(&reason));
            }
        }
        println!("\n  {}", terminal::bold(color, "Choose an action"));
        println!(
            "    {}  Keep this worktree. This is the default.",
            terminal::info(color, "[k]")
        );
        if worktree.exists {
            if can_view {
                println!(
                    "    {}  Show the files that need attention.",
                    terminal::info(color, "[v]")
                );
            }
            if can_preserve {
                println!(
                    "    {}  Save local work, then remove the worktree.",
                    terminal::good(color, "[p]")
                );
            }
            println!(
                "    {}  Chop anyway. This deletes local work.",
                terminal::danger(color, "[x]")
            );
        } else {
            if can_preserve {
                println!(
                    "    {}  Save private commits, then remove Git's record.",
                    terminal::good(color, "[p]")
                );
            }
            println!(
                "    {}  Chop anyway without saving Git's record.",
                terminal::danger(color, "[x]")
            );
        }

        let Some(choice) = prompt("\n  Choice [k]: ") else {
            keep(worktree, outcome);
            return;
        };
        match choice.trim().to_ascii_lowercase().as_str() {
            "" | "k" | "keep" => {
                keep(worktree, outcome);
                return;
            }
            "v" | "view" if can_view => match git::status_summary(worktree) {
                Ok(status) => {
                    println!("\n  {}", terminal::bold(color, "Git status"));
                    for line in status.lines() {
                        println!("    {}", terminal::text(line));
                    }
                }
                Err(error) => eprintln!(
                    "{} {}",
                    terminal::danger(color, "Could not read status:"),
                    terminal::text(&error)
                ),
            },
            "p" | "preserve" if can_preserve => match git::preserve_and_remove(worktree) {
                Ok(saved) => {
                    println!(
                        "{} {}",
                        terminal::good(color, "Saved and removed"),
                        terminal::path(&worktree.path)
                    );
                    for item in saved {
                        println!("  Saved as {}", terminal::text(&item));
                    }
                    outcome.preserved += 1;
                    outcome.removed += 1;
                    return;
                }
                Err(error) => {
                    eprintln!(
                        "{} {}",
                        terminal::danger(color, "Could not save this worktree:"),
                        terminal::text(&error.message)
                    );
                    for item in error.saved {
                        eprintln!("  Already saved as {}", terminal::text(&item));
                    }
                }
            },
            "x" | "chop" => {
                let expected = format!("CHOP {}", terminal::path(&worktree.path));
                println!("\n{}", terminal::danger(color, "Chop anyway"));
                if worktree.exists {
                    println!("  This deletes changed, untracked, and ignored files.");
                    println!("  Git cannot restore those files.");
                } else {
                    println!("  This deletes Git's remaining record for the missing worktree.");
                }
                println!(
                    "\n  Type {} to continue. Press Enter to cancel.",
                    terminal::bold(color, &expected)
                );
                if prompt("  > ").is_some_and(|answer| answer == expected) {
                    match git::force_remove(worktree) {
                        Ok(()) => {
                            println!(
                                "{} {}",
                                terminal::danger(color, "Chopped"),
                                terminal::path(&worktree.path)
                            );
                            outcome.removed += 1;
                            return;
                        }
                        Err(error) => {
                            eprintln!(
                                "{} {}: {}",
                                terminal::danger(color, "Could not chop"),
                                terminal::path(&worktree.path),
                                terminal::text(&error)
                            );
                            outcome.failed += 1;
                            return;
                        }
                    }
                } else {
                    println!("{}", terminal::info(color, "Chop canceled."));
                }
            }
            _ => eprintln!("Choose one of the actions shown above."),
        }
    }
}

pub(crate) fn can_preserve(worktree: &Worktree) -> bool {
    !worktree.has_hidden_index_flags
        && !worktree.has_executable_filters
        && !worktree.has_initialized_submodules
        && !worktree.has_nested_repositories
        && worktree.operations.is_empty()
        && worktree.inspection_error.is_none()
        && !worktree.is_unborn()
        && (worktree.exists || worktree.local_entries.is_empty())
        && (worktree.exists
            || (worktree.branch.is_none() && worktree.detached_references.is_empty())
            || !worktree.unreferenced_private_commits.is_empty())
}

pub(crate) fn can_view(worktree: &Worktree) -> bool {
    worktree.exists
        && !worktree.has_hidden_index_flags
        && !worktree.has_executable_filters
        && !worktree.has_initialized_submodules
        && !worktree.has_nested_repositories
}

fn keep(worktree: &Worktree, outcome: &mut Outcome) {
    println!(
        "{} {}",
        terminal::info(terminal::color_enabled(), "Kept"),
        terminal::path(&worktree.path)
    );
    outcome.kept += 1;
}

fn prompt(message: &str) -> Option<String> {
    print!("{message}");
    io::stdout().flush().ok()?;
    let mut answer = String::new();
    if io::stdin().read_line(&mut answer).ok()? == 0 {
        None
    } else {
        Some(without_line_ending(answer))
    }
}

fn without_line_ending(mut value: String) -> String {
    if value.ends_with('\n') {
        value.pop();
        if value.ends_with('\r') {
            value.pop();
        }
    }
    value
}

fn describe(
    state: &State,
    dry_run: bool,
    ready_removal_available: bool,
    color: bool,
) -> (String, Vec<String>) {
    match state {
        State::Ready => (
            if ready_removal_available {
                terminal::good(
                    color,
                    if dry_run {
                        "✓ Would chop"
                    } else {
                        "✓ Chop"
                    },
                )
            } else {
                terminal::info(color, "◆ Keep")
            },
            vec![if !ready_removal_available {
                "Clean, but this run has no removal consent. It will stay.".to_owned()
            } else if dry_run {
                "Clean. This linked worktree would be removed.".to_owned()
            } else {
                "Clean. Chop will remove it after one confirmation.".to_owned()
            }],
        ),
        State::Protected(reasons) => (terminal::info(color, "◆ Keep"), reasons.clone()),
        State::NeedsChoice(reasons) => {
            let mut explanations = reasons.clone();
            explanations.push(
                "Chop keeps this worktree unless an interactive run chooses another action."
                    .to_owned(),
            );
            (terminal::warn(color, "! Review"), explanations)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{execute, render_candidate_summary, render_plan, without_line_ending};
    use crate::model::{Repository, Worktree, WorktreeFlags};
    use std::path::PathBuf;

    #[test]
    fn removes_only_the_line_ending() {
        assert_eq!(
            without_line_ending("DELETE path  \n".to_owned()),
            "DELETE path  "
        );
        assert_eq!(
            without_line_ending("DELETE path\r\n".to_owned()),
            "DELETE path"
        );
        assert_eq!(
            without_line_ending("DELETE path  ".to_owned()),
            "DELETE path  "
        );
    }

    #[test]
    fn plan_explains_each_action_without_internal_state_names() {
        let repository = repository_with_each_state();

        let output = render_plan(&[repository], true, true, false);
        assert!(output.contains("✓ Chop    1 clean linked worktree would be removed."));
        assert!(output.contains("! Review  1 worktree needs your decision."));
        assert!(output.contains("1 bare or current worktree will stay."));
        assert!(output.contains("This worktree contains your current directory."));
        assert!(!output.contains("/repo\n"));
        assert!(output.contains("1 changed, untracked, or ignored path needs a decision."));
        assert!(output.contains(
            "Chop keeps this worktree unless an interactive run chooses another action."
        ));
        assert!(!output.contains("PROTECTED"));
        assert!(!output.contains("CHOICE"));
    }

    #[test]
    fn candidate_summary_lists_only_clean_linked_worktrees() {
        let output = render_candidate_summary(&[repository_with_each_state()], false);

        assert!(output.contains("Removal candidates"));
        assert!(output.contains("Chop found 1 clean removal candidate."));
        assert!(output.contains("✓  clean"));
        assert!(output.contains("/worktrees/clean"));
        assert!(!output.contains("/worktrees/changed"));
        assert!(!output.contains("/repo\n"));
    }

    #[test]
    fn empty_candidate_summary_explains_that_none_matched() {
        let mut repository = repository_with_each_state();
        repository.worktrees.retain(|worktree| worktree.is_main);

        let output = render_candidate_summary(&[repository], false);

        assert!(output.contains("Chop found no clean linked worktrees to remove."));
    }

    #[test]
    fn non_interactive_dry_run_uses_real_consent_rules() {
        let repositories = [repository_with_each_state()];

        let without_consent = execute(&repositories, true, true, false);
        let with_consent = execute(&repositories, true, true, true);

        assert_eq!(without_consent.removed, 0);
        assert_eq!(without_consent.kept, 3);
        assert_eq!(with_consent.removed, 1);
        assert_eq!(with_consent.kept, 2);
    }

    #[test]
    fn static_plan_explains_missing_removal_consent() {
        let output = render_plan(&[repository_with_each_state()], true, false, false);

        assert!(output.contains("will stay because this run has no removal consent"));
        assert!(output.contains("Clean, but this run has no removal consent. It will stay."));
        assert!(output.contains("◆ Keep  clean"));
        assert!(!output.contains("Would chop"));
        assert!(!output.contains("would be removed"));
    }

    fn repository_with_each_state() -> Repository {
        let main = Worktree {
            common_dir: PathBuf::from("/repo/.git"),
            path: PathBuf::from("/repo"),
            head: "0123456789abcdef".to_owned(),
            branch: Some("refs/heads/main".to_owned()),
            flags: WorktreeFlags::default(),
            is_main: true,
            exists: true,
            contains_current_dir: false,
            local_entries: Vec::new(),
            operations: Vec::new(),
            has_initialized_submodules: false,
            has_nested_repositories: false,
            has_hidden_index_flags: false,
            has_executable_filters: false,
            detached_references: Vec::new(),
            unreferenced_private_commits: Vec::new(),
            inspection_error: None,
        };
        let mut clean = main.clone();
        clean.path = PathBuf::from("/worktrees/clean");
        clean.branch = Some("refs/heads/clean".to_owned());
        clean.is_main = false;
        let mut changed = clean.clone();
        changed.path = PathBuf::from("/worktrees/changed");
        changed.branch = Some("refs/heads/changed".to_owned());
        changed.local_entries.push(" M file".to_owned());
        let mut current = clean.clone();
        current.path = PathBuf::from("/worktrees/current");
        current.branch = Some("refs/heads/current".to_owned());
        current.contains_current_dir = true;
        Repository {
            common_dir: PathBuf::from("/repo/.git"),
            worktrees: vec![main, clean, changed, current],
        }
    }
}
