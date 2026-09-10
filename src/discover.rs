use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const SKIPPED_DIRECTORIES: &[&str] = &[
    ".Trash",
    ".cache",
    ".gradle",
    ".npm",
    ".pnpm-store",
    ".rustup",
    ".venv",
    "Library",
    "build",
    "dist",
    "node_modules",
    "target",
    "venv",
];

#[derive(Debug, Default)]
pub struct ScanResult {
    pub candidates: Vec<PathBuf>,
    pub unreadable: Vec<(PathBuf, String)>,
}

pub fn find_git_candidates(roots: &[PathBuf], exhaustive: bool) -> ScanResult {
    let mut result = ScanResult::default();
    let mut candidates = HashSet::new();
    let mut stack = roots.to_vec();

    while let Some(directory) = stack.pop() {
        let metadata = match fs::symlink_metadata(&directory) {
            Ok(metadata) => metadata,
            Err(error) => {
                result.unreadable.push((directory, error.to_string()));
                continue;
            }
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }

        let marker = directory.join(".git");
        let is_bare = is_bare_repository(&directory);
        if marker.is_dir() || marker.is_file() || is_bare {
            candidates.insert(directory.clone());
        }
        if is_bare {
            continue;
        }

        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                result.unreadable.push((directory, error.to_string()));
                continue;
            }
        };

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    result
                        .unreadable
                        .push((directory.clone(), error.to_string()));
                    continue;
                }
            };
            if entry.file_name() == ".git" {
                continue;
            }
            if !exhaustive && should_skip(&entry.path()) {
                continue;
            }
            match entry.file_type() {
                Ok(file_type) if file_type.is_dir() && !file_type.is_symlink() => {
                    stack.push(entry.path());
                }
                Ok(_) => {}
                Err(error) => result.unreadable.push((entry.path(), error.to_string())),
            }
        }
    }

    result.candidates = candidates.into_iter().collect();
    result.candidates.sort();
    result
}

fn should_skip(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| SKIPPED_DIRECTORIES.contains(&name))
}

pub(crate) fn is_in_skipped_directory(path: &Path, roots: &[PathBuf]) -> bool {
    let mut matched = false;
    for relative in roots.iter().filter_map(|root| path.strip_prefix(root).ok()) {
        matched = true;
        let skipped = relative.components().any(|component| {
            component
                .as_os_str()
                .to_str()
                .is_some_and(|name| SKIPPED_DIRECTORIES.contains(&name))
        });
        if !skipped {
            return false;
        }
    }
    matched
}

fn looks_like_bare_repository(path: &Path) -> bool {
    path.join("HEAD").is_file() && path.join("config").is_file() && path.join("objects").is_dir()
}

fn is_bare_repository(path: &Path) -> bool {
    if !looks_like_bare_repository(path) {
        return false;
    }
    let output = Command::new("git")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--is-bare-repository"])
        .output();
    output.is_ok_and(|output| output.status.success() && output.stdout == b"true\n")
}

#[cfg(test)]
mod tests {
    use super::find_git_candidates;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_PATH: AtomicU64 = AtomicU64::new(0);

    fn temp_path() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "chop-discovery-{}-{unique}-{sequence}",
            std::process::id()
        ))
    }

    #[test]
    fn finds_directories_and_files_named_dot_git() {
        let root = temp_path();
        let main = root.join("main");
        let linked = root.join("linked");
        fs::create_dir_all(main.join(".git")).unwrap();
        fs::create_dir_all(&linked).unwrap();
        fs::write(linked.join(".git"), "gitdir: elsewhere").unwrap();

        let result = find_git_candidates(std::slice::from_ref(&root), false);

        assert_eq!(result.candidates, vec![linked, main]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn skips_build_folders_unless_exhaustive() {
        let root = temp_path();
        let repo = root.join("target/repo");
        fs::create_dir_all(repo.join(".git")).unwrap();

        assert!(
            find_git_candidates(std::slice::from_ref(&root), false)
                .candidates
                .is_empty()
        );
        assert_eq!(
            find_git_candidates(std::slice::from_ref(&root), true).candidates,
            vec![repo]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn finds_bare_repository_shape() {
        let root = temp_path();
        let bare = root.join("project.git");
        fs::create_dir_all(&root).unwrap();
        let output = Command::new("git")
            .args(["init", "--bare"])
            .arg(&bare)
            .output()
            .unwrap();
        assert!(output.status.success());
        fs::create_dir_all(bare.join("objects/aa/not-a-repository/.git")).unwrap();

        let result = find_git_candidates(std::slice::from_ref(&root), false);

        assert_eq!(result.candidates, vec![bare]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn false_bare_shape_does_not_hide_nested_repository() {
        let root = temp_path();
        let fake = root.join("not-a-repository");
        let nested = fake.join("objects/nested");
        fs::create_dir_all(nested.join(".git")).unwrap();
        fs::write(fake.join("HEAD"), "not a Git head\n").unwrap();
        fs::write(fake.join("config"), "not Git config\n").unwrap();

        let result = find_git_candidates(std::slice::from_ref(&root), false);

        assert_eq!(result.candidates, vec![nested]);
        fs::remove_dir_all(root).unwrap();
    }
}
