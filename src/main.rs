mod cli;
mod config;
mod discover;
mod git;
mod model;
mod terminal;
mod tui;
mod ui;

use cli::{Command, Options};
use std::env;
use std::process::ExitCode;

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            print_error(&error);
            ExitCode::FAILURE
        }
    }
}

fn print_error(error: &str) {
    let mut lines = format_error_lines(error).into_iter();
    let first = lines.next().unwrap_or_default();
    eprintln!("chop: {first}");
    for line in lines {
        eprintln!("{line}");
    }
}

fn format_error_lines(error: &str) -> Vec<String> {
    error.lines().map(terminal::text).collect()
}

fn run() -> Result<ExitCode, String> {
    let options = cli::parse()?;
    match options.command {
        Command::Help => {
            println!("{}", cli::help());
            return Ok(ExitCode::SUCCESS);
        }
        Command::Version => {
            println!("chop {}", env!("CARGO_PKG_VERSION"));
            return Ok(ExitCode::SUCCESS);
        }
        Command::Config => return run_config(options),
        Command::All => {}
    }
    run_all(options)
}

fn run_config(options: Options) -> Result<ExitCode, String> {
    let current_dir = env::current_dir()
        .and_then(|path| path.canonicalize())
        .map_err(|error| error.to_string())?;
    if options.roots.is_empty() && terminal::supports_tui() {
        return Ok(if tui::configure(&current_dir)? {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(2)
        });
    }
    ui::print_brand();
    config::configure(&options.roots, &current_dir)?;
    Ok(ExitCode::SUCCESS)
}

fn run_all(options: Options) -> Result<ExitCode, String> {
    let color = terminal::color_enabled();
    let current_dir = env::current_dir()
        .and_then(|path| path.canonicalize())
        .map_err(|error| error.to_string())?;
    if !options.non_interactive && terminal::supports_tui() {
        return match tui::run_all(&options, &current_dir)? {
            tui::RunResult::Canceled => Ok(ExitCode::from(2)),
            tui::RunResult::Complete {
                outcome,
                scan_failed,
            } if outcome.failed > 0 || scan_failed => Ok(ExitCode::FAILURE),
            tui::RunResult::Complete { outcome, .. }
                if !options.dry_run && outcome.needs_choice > 0 =>
            {
                Ok(ExitCode::from(2))
            }
            tui::RunResult::Complete { .. } => Ok(ExitCode::SUCCESS),
        };
    }
    ui::print_brand();
    if options.dry_run {
        println!(
            "{}",
            terminal::warn(color, "Dry run. Nothing will be changed.")
        );
    }
    let roots = config::roots_for_run(
        &options.roots,
        &current_dir,
        options.dry_run,
        options.non_interactive,
    )?;
    println!("\n{}", terminal::bold(color, "Scanning"));
    for root in &roots {
        println!("  {}", terminal::muted(color, &terminal::path(root)));
    }

    let scan = discover::find_git_candidates(&roots, options.exhaustive);
    let (repositories, git_errors) = git::inspect_repositories_with_options(
        &scan.candidates,
        &current_dir,
        &roots,
        options.exhaustive,
    );

    println!(
        "\n{}  {} Git repositories, {} worktrees",
        terminal::good(color, "Scan complete"),
        repositories.len(),
        repositories
            .iter()
            .map(|repository| repository.worktrees.len())
            .sum::<usize>()
    );
    if !scan.unreadable.is_empty() {
        eprintln!(
            "\n{} {} path(s).",
            terminal::danger(color, "Could not scan"),
            scan.unreadable.len()
        );
        for (path, error) in scan.unreadable.iter().take(10) {
            eprintln!("  {}: {}", terminal::path(path), terminal::text(error));
        }
        if scan.unreadable.len() > 10 {
            eprintln!("  {} more", scan.unreadable.len() - 10);
        }
    }
    if !git_errors.is_empty() {
        eprintln!(
            "\n{} {} Git candidate(s).",
            terminal::danger(color, "Could not inspect"),
            git_errors.len()
        );
        for error in git_errors.iter().take(10) {
            eprintln!("  {}", terminal::text(error));
        }
        if git_errors.len() > 10 {
            eprintln!("  {} more", git_errors.len() - 10);
        }
    }

    ui::print_plan(
        &repositories,
        options.dry_run,
        ui::ready_removal_available(options.non_interactive, options.accept_data_loss),
    );

    let outcome = ui::execute(
        &repositories,
        options.dry_run,
        options.non_interactive,
        options.accept_data_loss,
    );
    println!("\n{}", terminal::bold(color, "Result"));
    if options.dry_run {
        println!(
            "  {} {} worktree(s)",
            terminal::good(color, &format!("{:<12}", "Would remove")),
            outcome.removed
        );
        println!(
            "  {} {} worktree(s)",
            terminal::info(color, &format!("{:<12}", "Would keep")),
            outcome.kept
        );
    } else {
        println!(
            "  {} {} worktree(s)",
            terminal::good(color, &format!("{:<12}", "Removed")),
            outcome.removed
        );
        if outcome.preserved > 0 {
            println!("  {:<12} {} before removal", "Saved", outcome.preserved);
        }
        println!(
            "  {} {} worktree(s)",
            terminal::info(color, &format!("{:<12}", "Kept")),
            outcome.kept
        );
    }
    if outcome.failed > 0 {
        println!(
            "  {} {} worktree(s)",
            terminal::danger(color, &format!("{:<12}", "Failed")),
            outcome.failed
        );
    }
    ui::print_candidate_summary(&repositories);

    if outcome.failed > 0 || !scan.unreadable.is_empty() || !git_errors.is_empty() {
        Ok(ExitCode::FAILURE)
    } else if !options.dry_run && outcome.needs_choice > 0 {
        Ok(ExitCode::from(2))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

#[cfg(test)]
mod tests {
    use super::format_error_lines;

    #[test]
    fn error_output_keeps_line_breaks_and_escapes_controls() {
        assert_eq!(
            format_error_lines("bad \u{1b}[31m\n\nUsage:\n  chop all"),
            vec!["bad \\x1b[31m", "", "Usage:", "  chop all"]
        );
    }
}
