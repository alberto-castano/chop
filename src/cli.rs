use crate::terminal;
use std::env;
use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;
use std::path::PathBuf;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Options {
    pub command: Command,
    pub roots: Vec<PathBuf>,
    pub dry_run: bool,
    pub non_interactive: bool,
    pub accept_data_loss: bool,
    pub exhaustive: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    All,
    Config,
    Help,
    Version,
}

pub fn parse() -> Result<Options, String> {
    parse_from(env::args_os().skip(1))
}

fn parse_from<I, S>(args: I) -> Result<Options, String>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let mut args = args.into_iter().map(Into::into);
    let Some(first) = args.next() else {
        return Ok(Options {
            command: Command::All,
            ..help_options()
        });
    };

    if matches!(first.as_bytes(), b"--help" | b"-h" | b"help") {
        return Ok(help_options());
    }
    if matches!(first.as_bytes(), b"--version" | b"-V") {
        return Ok(Options {
            command: Command::Version,
            ..help_options()
        });
    }
    let command = match first.as_bytes() {
        b"all" => Command::All,
        b"config" => Command::Config,
        _ => {
            return Err(format!(
                "unknown command: {}\n\n{}",
                terminal::path(Path::new(&first)),
                help()
            ));
        }
    };

    let mut options = Options {
        command,
        roots: Vec::new(),
        dry_run: false,
        non_interactive: false,
        accept_data_loss: false,
        exhaustive: false,
    };

    while let Some(argument) = args.next() {
        match argument.as_bytes() {
            b"--root" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--root needs a path".to_owned())?;
                if value.as_bytes().starts_with(b"-") {
                    return Err(format!(
                        "--root needs a path, but found option {}. Use --root=PATH for a path that starts with '-'.",
                        terminal::path(Path::new(&value))
                    ));
                }
                options.roots.push(PathBuf::from(value));
            }
            b"--dry-run" if command == Command::All => options.dry_run = true,
            b"--non-interactive" if command == Command::All => options.non_interactive = true,
            b"--accept-data-loss" if command == Command::All => options.accept_data_loss = true,
            b"--exhaustive" if command == Command::All => options.exhaustive = true,
            b"--help" | b"-h" => options.command = Command::Help,
            b"--version" | b"-V" => options.command = Command::Version,
            value if value.starts_with(b"--root=") => {
                let path = &value[b"--root=".len()..];
                if path.is_empty() {
                    return Err("--root needs a path".to_owned());
                }
                options
                    .roots
                    .push(PathBuf::from(OsString::from_vec(path.to_vec())));
            }
            _ => {
                return Err(format!(
                    "unknown option: {}\n\n{}",
                    terminal::path(Path::new(&argument)),
                    help()
                ));
            }
        }
    }

    Ok(options)
}

fn help_options() -> Options {
    Options {
        command: Command::Help,
        roots: Vec::new(),
        dry_run: false,
        non_interactive: false,
        accept_data_loss: false,
        exhaustive: false,
    }
}

pub fn help() -> &'static str {
    "chop removes linked Git worktrees. It never removes a main working directory.\n\n\
Usage:\n  chop\n  chop all [OPTIONS]\n  chop config [--root PATH]...\n  chop help\n\n\
Commands:\n  all                  Find and remove linked worktrees.\n  config               Set the folders that chop scans by default.\n\n\
Options:\n  --root PATH         Use PATH instead of saved roots. Repeat as needed.\n  --dry-run           Show the plan and make no changes.\n  --non-interactive   Never ask for input.\n  --accept-data-loss  Allow ready removal without interactive confirmation.\n  --exhaustive        Scan cache and build folders that are skipped by default.\n  -h, --help          Show this help.\n  -V, --version       Show the version.\n\n\
Plan:\n  Chop                Clean linked worktree.\n  Review              Local work or Git state needs your decision.\n  Keep                Bare or current worktree.\n\n\
Set NO_COLOR to disable terminal colors.\n\
Run `chop config` to change the saved roots.\n\n\
Chop deletes directories. This is not secure erase. Backups can keep copies."
}

#[cfg(test)]
mod tests {
    use super::{Command, parse_from};
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::path::PathBuf;

    #[test]
    fn no_command_opens_the_main_app() {
        let options = parse_from(Vec::<String>::new()).unwrap();
        assert_eq!(options.command, Command::All);
    }

    #[test]
    fn parses_repeated_roots() {
        let options = parse_from([
            "all".to_owned(),
            "--root".to_owned(),
            "/one".to_owned(),
            "--root".to_owned(),
            "/two".to_owned(),
            "--dry-run".to_owned(),
        ])
        .unwrap();

        assert_eq!(options.roots.len(), 2);
        assert!(options.dry_run);
    }

    #[test]
    fn rejects_implicit_destructive_command() {
        assert!(parse_from(["--dry-run".to_owned()]).is_err());
    }

    #[test]
    fn rejects_option_used_as_root_value() {
        let error = parse_from([
            "all".to_owned(),
            "--root".to_owned(),
            "--dry-run".to_owned(),
        ])
        .unwrap_err();

        assert!(error.contains("--root needs a path"));
    }

    #[test]
    fn parses_explicit_data_loss_consent() {
        let options = parse_from([
            "all".to_owned(),
            "--non-interactive".to_owned(),
            "--accept-data-loss".to_owned(),
        ])
        .unwrap();

        assert!(options.non_interactive);
        assert!(options.accept_data_loss);
    }

    #[test]
    fn parses_version_after_all() {
        let options = parse_from(["all".to_owned(), "--version".to_owned()]).unwrap();

        assert_eq!(options.command, Command::Version);
    }

    #[test]
    fn parses_config_roots() {
        let options = parse_from([
            "config".to_owned(),
            "--root".to_owned(),
            "/one".to_owned(),
            "--root=/two".to_owned(),
        ])
        .unwrap();

        assert_eq!(options.command, Command::Config);
        assert_eq!(
            options.roots,
            [PathBuf::from("/one"), PathBuf::from("/two")]
        );
    }

    #[test]
    fn rejects_removal_options_for_config() {
        let error = parse_from(["config".to_owned(), "--dry-run".to_owned()]).unwrap_err();

        assert!(error.contains("unknown option"));
    }

    #[test]
    fn preserves_non_utf8_root_bytes() {
        let root = OsString::from_vec(vec![b'/', b'r', 0xff, b't']);
        let options = parse_from([
            OsString::from("all"),
            OsString::from("--root"),
            root.clone(),
        ])
        .unwrap();

        assert_eq!(options.roots, [PathBuf::from(root)]);
    }
}
