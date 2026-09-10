use crate::terminal;
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn roots_for_run(
    cli_roots: &[PathBuf],
    current_dir: &Path,
    dry_run: bool,
    non_interactive: bool,
) -> Result<Vec<PathBuf>, String> {
    if !cli_roots.is_empty() {
        return normalize_roots(cli_roots, current_dir);
    }

    let path = config_path()?;
    if let Some(roots) = read_roots(&path)? {
        return Ok(roots);
    }

    if !can_prompt_for_first_setup(non_interactive, io::stdin().is_terminal()) {
        return Err("no roots are configured. Run `chop config` or pass `--root PATH`.".to_owned());
    }

    println!("\nFirst setup");
    println!("  Chop needs one or more folders to scan.");
    let roots = prompt_for_roots(&[], current_dir)?;
    finish_first_setup(&path, &roots, dry_run)?;
    Ok(roots)
}

fn can_prompt_for_first_setup(non_interactive: bool, stdin_is_terminal: bool) -> bool {
    !non_interactive && stdin_is_terminal
}

fn finish_first_setup(path: &Path, roots: &[PathBuf], dry_run: bool) -> Result<(), String> {
    if dry_run {
        println!("\n  Dry run. Chop did not save these roots.");
        return Ok(());
    }
    write_roots(path, roots)?;
    print_saved(path, roots);
    Ok(())
}

pub fn configure(cli_roots: &[PathBuf], current_dir: &Path) -> Result<(), String> {
    let path = config_path()?;
    let roots = if cli_roots.is_empty() {
        if !io::stdin().is_terminal() {
            return Err("`chop config` needs a terminal or at least one `--root PATH`.".to_owned());
        }
        let current = read_roots(&path)?.unwrap_or_default();
        print_current(&path, &current);
        prompt_for_roots(&current, current_dir)?
    } else {
        normalize_roots(cli_roots, current_dir)?
    };
    write_roots(&path, &roots)?;
    print_saved(&path, &roots);
    Ok(())
}

pub(crate) fn config_path() -> Result<PathBuf, String> {
    if let Some(directory) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(directory).join("chop/roots"));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| "HOME is not set. Pass `--root PATH`.".to_owned())?;
    Ok(PathBuf::from(home).join(".config/chop/roots"))
}

fn prompt_for_roots(current: &[PathBuf], current_dir: &Path) -> Result<Vec<PathBuf>, String> {
    if current.is_empty() {
        println!("  Enter one root per line. Press Enter after the last root.");
    } else {
        println!("\nReplacement roots");
        println!("  Enter one root per line.");
        println!("  Press Enter first to keep the current roots.");
    }

    let mut values = Vec::new();
    loop {
        let number = values.len() + 1;
        print!("  Root {number}: ");
        io::stdout()
            .flush()
            .map_err(|error| format!("could not write the prompt: {error}"))?;
        let mut input = String::new();
        let read = io::stdin()
            .read_line(&mut input)
            .map_err(|error| format!("could not read the root: {error}"))?;
        if read == 0 {
            return Err("configuration canceled".to_owned());
        }
        let input = input.trim_end_matches(['\r', '\n']);
        if input.is_empty() {
            if values.is_empty() && !current.is_empty() {
                return Ok(current.to_vec());
            }
            if values.is_empty() {
                println!("  Add at least one root.");
                continue;
            }
            break;
        }
        values.push(PathBuf::from(input));
    }
    normalize_roots(&values, current_dir)
}

pub(crate) fn normalize_roots(
    roots: &[PathBuf],
    current_dir: &Path,
) -> Result<Vec<PathBuf>, String> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut normalized = Vec::new();
    let mut seen = HashSet::new();
    for root in roots {
        let expanded = expand_home(root, home.as_deref());
        let absolute = if expanded.is_absolute() {
            expanded
        } else {
            current_dir.join(expanded)
        };
        let absolute = absolute.canonicalize().map_err(|error| {
            format!("could not open root {}: {error}", terminal::path(&absolute))
        })?;
        if !absolute.is_dir() {
            return Err(format!(
                "root {} is not a directory",
                terminal::path(&absolute)
            ));
        }
        if absolute.as_os_str().as_bytes().contains(&b'\n')
            || absolute.as_os_str().as_bytes().contains(&b'\r')
        {
            return Err("a root cannot contain a line break".to_owned());
        }
        if seen.insert(absolute.clone()) {
            normalized.push(absolute);
        }
    }
    if normalized.is_empty() {
        return Err("add at least one root".to_owned());
    }
    Ok(normalized)
}

pub(crate) fn expand_home(path: &Path, home: Option<&Path>) -> PathBuf {
    let Some(home) = home else {
        return path.to_path_buf();
    };
    if path == Path::new("~") {
        return home.to_path_buf();
    }
    match path.strip_prefix("~/") {
        Ok(relative) => home.join(relative),
        Err(_) => path.to_path_buf(),
    }
}

pub(crate) fn read_roots(path: &Path) -> Result<Option<Vec<PathBuf>>, String> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!("could not read {}: {error}", terminal::path(path)));
        }
    };
    let mut roots = Vec::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let root = PathBuf::from(OsString::from_vec(line.to_vec()));
        if !root.is_absolute() {
            return Err(format!(
                "config root must be absolute: {}",
                terminal::path(&root)
            ));
        }
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    Ok((!roots.is_empty()).then_some(roots))
}

pub(crate) fn write_roots(path: &Path, roots: &[PathBuf]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "config path has no parent directory".to_owned())?;
    fs::create_dir_all(parent).map_err(|error| {
        format!(
            "could not create config directory {}: {error}",
            terminal::path(parent)
        )
    })?;
    let temp = parent.join(format!(".roots.{}.{}.tmp", std::process::id(), timestamp()));
    let result = (|| -> Result<(), String> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
            .map_err(|error| format!("could not create the config file: {error}"))?;
        for root in roots {
            file.write_all(root.as_os_str().as_bytes())
                .and_then(|_| file.write_all(b"\n"))
                .map_err(|error| format!("could not write the config file: {error}"))?;
        }
        file.sync_all()
            .map_err(|error| format!("could not save the config file: {error}"))?;
        fs::rename(&temp, path).map_err(|error| format!("could not replace the config: {error}"))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn print_current(path: &Path, roots: &[PathBuf]) {
    println!("Current configuration");
    println!("  {}", terminal::path(path));
    if roots.is_empty() {
        println!("  No roots are saved.");
    } else {
        for root in roots {
            println!("  • {}", terminal::path(root));
        }
    }
}

fn print_saved(path: &Path, roots: &[PathBuf]) {
    let color = terminal::color_enabled();
    println!("\n{}", terminal::good(color, "Configuration saved"));
    println!("  {}", terminal::muted(color, &terminal::path(path)));
    for root in roots {
        println!("  {} {}", terminal::good(color, "✓"), terminal::path(root));
    }
}

fn timestamp() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

#[cfg(test)]
mod tests {
    use super::{
        can_prompt_for_first_setup, expand_home, finish_first_setup, normalize_roots, read_roots,
        write_roots,
    };
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::ffi::OsStringExt;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn expands_tilde_without_expanding_tilde_user() {
        let home = Path::new("/Users/example");

        assert_eq!(expand_home(Path::new("~"), Some(home)), home);
        assert_eq!(
            expand_home(Path::new("~/repos"), Some(home)),
            Path::new("/Users/example/repos")
        );
        assert_eq!(
            expand_home(Path::new("~other/repos"), Some(home)),
            Path::new("~other/repos")
        );
    }

    #[test]
    fn missing_and_empty_config_need_setup() {
        let directory = temp_path("empty");
        fs::create_dir_all(&directory).unwrap();
        let config = directory.join("roots");

        assert_eq!(read_roots(&config).unwrap(), None);
        for contents in ["", "\n\r\n\n"] {
            fs::write(&config, contents).unwrap();
            assert_eq!(read_roots(&config).unwrap(), None);
        }

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn saves_and_loads_spaces_and_non_utf8_paths() {
        let directory = temp_path("round-trip");
        fs::create_dir_all(&directory).unwrap();
        let config = directory.join("config/roots");
        let invalid = directory.join(OsString::from_vec(vec![b'r', 0xff, b't']));
        let roots = vec![directory.join("root with spaces"), invalid];

        write_roots(&config, &roots).unwrap();

        assert_eq!(read_roots(&config).unwrap(), Some(roots));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn normalizes_relative_roots_and_removes_duplicates() {
        let directory = temp_path("normalize");
        let root = directory.join("repos");
        fs::create_dir_all(&root).unwrap();

        let roots = normalize_roots(
            &[PathBuf::from("repos"), PathBuf::from("repos")],
            &directory,
        )
        .unwrap();

        assert_eq!(roots, vec![root.canonicalize().unwrap()]);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn dry_run_does_not_save_first_setup() {
        let directory = temp_path("dry-run");
        let config = directory.join("config/roots");

        finish_first_setup(&config, &[PathBuf::from("/repos")], true).unwrap();

        assert!(!config.exists());
    }

    #[test]
    fn non_interactive_mode_never_starts_first_setup() {
        assert!(!can_prompt_for_first_setup(true, true));
        assert!(!can_prompt_for_first_setup(false, false));
        assert!(can_prompt_for_first_setup(false, true));
    }

    fn temp_path(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "chop-config-{}-{unique}-{name}",
            std::process::id()
        ))
    }
}
