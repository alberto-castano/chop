use crate::discover;
use crate::model::{Repository, State, Worktree, WorktreeFlags};
use crate::terminal;
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read};
use std::os::unix::ffi::OsStringExt;
use std::path::Component;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(test)]
pub fn inspect_repositories(
    candidates: &[PathBuf],
    current_dir: &Path,
    roots: &[PathBuf],
) -> (Vec<Repository>, Vec<String>) {
    inspect_repositories_with_options(candidates, current_dir, roots, false)
}

pub fn inspect_repositories_with_options(
    candidates: &[PathBuf],
    current_dir: &Path,
    roots: &[PathBuf],
    exhaustive: bool,
) -> (Vec<Repository>, Vec<String>) {
    let mut by_common_dir = HashMap::<PathBuf, PathBuf>::new();
    let mut errors = Vec::new();

    for candidate in candidates {
        match common_dir(candidate) {
            Ok(common) => {
                by_common_dir
                    .entry(common)
                    .or_insert_with(|| candidate.clone());
            }
            Err(error) => errors.push(format!("{}: {error}", candidate.display())),
        }
    }

    let mut repositories = Vec::new();
    for (common, candidate) in by_common_dir {
        match inspect_repository(&candidate, common, current_dir, roots, exhaustive) {
            Ok(repository) => repositories.push(repository),
            Err(error) => errors.push(format!("{}: {error}", candidate.display())),
        }
    }
    repositories.sort_by(|left, right| left.common_dir.cmp(&right.common_dir));
    (repositories, errors)
}

fn common_dir(candidate: &Path) -> Result<PathBuf, String> {
    let output = git(
        candidate,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    canonical_or_owned(&one_path(&output.stdout))
}

fn inspect_repository(
    candidate: &Path,
    common_dir: PathBuf,
    current_dir: &Path,
    roots: &[PathBuf],
    exhaustive: bool,
) -> Result<Repository, String> {
    let output = git(candidate, ["worktree", "list", "--porcelain", "-z"])?;
    let mut raw = parse_porcelain(&output.stdout)?;
    if raw.is_empty() {
        return Err("Git returned no worktrees".to_owned());
    }
    resolve_existing_paths(&mut raw)?;
    let worktrees = raw
        .into_iter()
        .enumerate()
        .filter(|(_, item)| {
            roots.iter().any(|root| item.path.starts_with(root))
                && (exhaustive || !discover::is_in_skipped_directory(&item.path, roots))
        })
        .map(|(index, item)| inspect_worktree(item, &common_dir, index == 0, current_dir))
        .collect();

    Ok(Repository {
        common_dir,
        worktrees,
    })
}

fn resolve_existing_paths(worktrees: &mut [RawWorktree]) -> Result<(), String> {
    for worktree in worktrees {
        if worktree
            .path
            .try_exists()
            .map_err(|error| format!("could not inspect {}: {error}", worktree.path.display()))?
        {
            worktree.path = worktree.path.canonicalize().map_err(|error| {
                format!("could not resolve {}: {error}", worktree.path.display())
            })?;
        }
    }
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
struct RawWorktree {
    path: PathBuf,
    head: String,
    branch: Option<String>,
    flags: WorktreeFlags,
}

fn parse_porcelain(bytes: &[u8]) -> Result<Vec<RawWorktree>, String> {
    let mut records = Vec::new();
    let mut fields = Vec::new();
    for field in bytes.split(|byte| *byte == 0) {
        if field.is_empty() {
            if !fields.is_empty() {
                records.push(parse_record(&fields)?);
                fields.clear();
            }
        } else {
            fields.push(field);
        }
    }
    if !fields.is_empty() {
        records.push(parse_record(&fields)?);
    }
    Ok(records)
}

fn parse_record(fields: &[&[u8]]) -> Result<RawWorktree, String> {
    let mut path = None;
    let mut head = String::new();
    let mut branch = None;
    let mut flags = WorktreeFlags::default();

    for field in fields {
        let split = field.iter().position(|byte| *byte == b' ');
        let (key, value) = split.map_or((*field, &[][..]), |index| {
            (&field[..index], &field[index + 1..])
        });
        match key {
            b"worktree" => path = Some(PathBuf::from(OsString::from_vec(value.to_vec()))),
            b"HEAD" => head = String::from_utf8_lossy(value).into_owned(),
            b"branch" => branch = Some(String::from_utf8_lossy(value).into_owned()),
            b"bare" => flags.bare = true,
            b"locked" => flags.locked = Some(String::from_utf8_lossy(value).into_owned()),
            b"prunable" => flags.prunable = Some(String::from_utf8_lossy(value).into_owned()),
            b"detached" => {}
            _ => {}
        }
    }

    Ok(RawWorktree {
        path: path.ok_or_else(|| "worktree record has no path".to_owned())?,
        head,
        branch,
        flags,
    })
}

fn inspect_worktree(
    raw: RawWorktree,
    common_dir: &Path,
    is_main: bool,
    current_dir: &Path,
) -> Worktree {
    let (exists, existence_error) = match raw.path.try_exists() {
        Ok(exists) => (exists, None),
        Err(error) => (false, Some(format!("could not inspect path: {error}"))),
    };
    let contains_current_dir = exists && current_dir.starts_with(&raw.path);
    let mut worktree = Worktree {
        common_dir: common_dir.to_path_buf(),
        path: raw.path,
        head: raw.head,
        branch: raw.branch,
        flags: raw.flags,
        is_main,
        exists,
        contains_current_dir,
        local_entries: Vec::new(),
        operations: Vec::new(),
        has_initialized_submodules: false,
        has_nested_repositories: false,
        has_hidden_index_flags: false,
        has_executable_filters: false,
        detached_references: Vec::new(),
        unreferenced_private_commits: Vec::new(),
        inspection_error: existence_error,
    };

    if worktree.branch.is_none() {
        match references_containing(&worktree.common_dir, &worktree.head) {
            Ok(references) => worktree.detached_references = references,
            Err(error) => worktree.inspection_error = Some(error),
        }
    }
    if !is_main {
        match has_private_submodule_repositories(&worktree.common_dir, &worktree.path) {
            Ok(has_submodules) => worktree.has_initialized_submodules = has_submodules,
            Err(error) => worktree.inspection_error = Some(error),
        }
        match find_worktree_record(&worktree.common_dir, &worktree.path)
            .and_then(|record| operations_in_git_dir(&record))
        {
            Ok(operations) => worktree.operations = operations,
            Err(error) => worktree.inspection_error = Some(error),
        }
        match find_worktree_record(&worktree.common_dir, &worktree.path)
            .and_then(|record| has_hidden_index_flags_in_git_dir(&record))
        {
            Ok(has_flags) => worktree.has_hidden_index_flags = has_flags,
            Err(error) => worktree.inspection_error = Some(error),
        }
        if !exists && !worktree.is_unborn() {
            match find_worktree_record(&worktree.common_dir, &worktree.path)
                .and_then(|record| missing_index_entries(&record, &worktree.head))
            {
                Ok(entries) => worktree.local_entries = entries,
                Err(error) => worktree.inspection_error = Some(error),
            }
        }
        match unreferenced_private_commits(&worktree.common_dir, &worktree.path) {
            Ok(commits) => worktree.unreferenced_private_commits = commits,
            Err(error) => worktree.inspection_error = Some(error),
        }
    }
    if !exists {
        return worktree;
    }

    let inspection = (|| -> Result<(), String> {
        worktree.has_nested_repositories = has_nested_repositories(&worktree.path)?;
        worktree.has_executable_filters = has_executable_filters(&worktree.path)?;
        if !worktree.has_initialized_submodules {
            worktree.has_initialized_submodules = has_initialized_submodules(&worktree.path)?;
        }
        if !worktree.has_hidden_index_flags {
            worktree.has_hidden_index_flags = has_hidden_index_flags(&worktree.path)?;
        }
        if !worktree.has_executable_filters
            && !worktree.has_initialized_submodules
            && !worktree.has_nested_repositories
        {
            let status = git(
                &worktree.path,
                [
                    "status",
                    "--porcelain=v1",
                    "-z",
                    "--untracked-files=all",
                    "--ignored=matching",
                    "--ignore-submodules=none",
                ],
            )?;
            worktree.local_entries = checked_status_entries(&status)?;
        }
        worktree.operations = operations(&worktree.path)?;
        Ok(())
    })();
    if let Err(error) = inspection {
        worktree.inspection_error = Some(error);
    }
    worktree
}

fn has_nested_repositories(worktree: &Path) -> Result<bool, String> {
    let scan = discover::find_git_candidates(&[worktree.to_path_buf()], true);
    if let Some((path, error)) = scan.unreadable.first() {
        return Err(format!(
            "could not inspect nested path {}: {error}",
            terminal::path(path)
        ));
    }
    Ok(scan
        .candidates
        .iter()
        .any(|candidate| candidate != worktree))
}

fn references_containing(common_dir: &Path, head: &str) -> Result<Vec<String>, String> {
    let contains = format!("--contains={head}");
    let refs = git_dir(
        common_dir,
        [
            "for-each-ref",
            "--format=%(refname)",
            &contains,
            "refs/heads",
            "refs/tags",
            "refs/remotes",
        ],
    )?;
    lines(&refs.stdout)
}

fn has_initialized_submodules(worktree: &Path) -> Result<bool, String> {
    let output = git(worktree, ["ls-files", "--stage", "-z"])?;
    for record in output.stdout.split(|byte| *byte == 0) {
        let Some(tab) = record.iter().position(|byte| *byte == b'\t') else {
            continue;
        };
        if record.starts_with(b"160000 ") {
            let path = OsString::from_vec(record[tab + 1..].to_vec());
            let directory = worktree.join(path);
            let marker = directory.join(".git");
            if marker
                .try_exists()
                .map_err(|error| format!("could not inspect {}: {error}", marker.display()))?
            {
                return Ok(true);
            }
            match fs::read_dir(&directory) {
                Ok(mut entries) => match entries.next() {
                    Some(Ok(_)) => return Ok(true),
                    Some(Err(error)) => {
                        return Err(format!(
                            "could not inspect {}: {error}",
                            directory.display()
                        ));
                    }
                    None => {}
                },
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) => {}
                Err(error) => {
                    return Err(format!(
                        "could not inspect {}: {error}",
                        directory.display()
                    ));
                }
            }
        }
    }
    Ok(false)
}

fn has_private_submodule_repositories(common_dir: &Path, worktree: &Path) -> Result<bool, String> {
    let modules = find_worktree_record(common_dir, worktree)?.join("modules");
    modules
        .try_exists()
        .map_err(|error| format!("could not inspect {}: {error}", modules.display()))
}

fn has_hidden_index_flags(worktree: &Path) -> Result<bool, String> {
    let verbose = git(worktree, ["ls-files", "-v", "-z"])?;
    if verbose
        .stdout
        .split(|byte| *byte == 0)
        .any(|record| record.first().is_some_and(u8::is_ascii_lowercase))
    {
        return Ok(true);
    }

    let tagged = git(worktree, ["ls-files", "-t", "-z"])?;
    Ok(tagged
        .stdout
        .split(|byte| *byte == 0)
        .any(|record| record.first() == Some(&b'S')))
}

fn has_hidden_index_flags_in_git_dir(record: &Path) -> Result<bool, String> {
    let verbose = git_dir(record, ["ls-files", "-v", "-z"])?;
    if verbose
        .stdout
        .split(|byte| *byte == 0)
        .any(|entry| entry.first().is_some_and(u8::is_ascii_lowercase))
    {
        return Ok(true);
    }

    let tagged = git_dir(record, ["ls-files", "-t", "-z"])?;
    Ok(tagged
        .stdout
        .split(|byte| *byte == 0)
        .any(|entry| entry.first() == Some(&b'S')))
}

fn has_executable_filters(worktree: &Path) -> Result<bool, String> {
    if config_scope_has_executable_filters(worktree, "--local")? {
        return Ok(true);
    }
    if worktree_config_enabled(worktree)? {
        return config_scope_has_executable_filters(worktree, "--worktree");
    }
    Ok(false)
}

fn worktree_config_enabled(worktree: &Path) -> Result<bool, String> {
    let output = git_command()
        .arg("--no-optional-locks")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .arg("-C")
        .arg(worktree)
        .args([
            "config",
            "--includes",
            "--local",
            "--get",
            "--bool",
            "extensions.worktreeConfig",
        ])
        .output()
        .map_err(|error| format!("could not inspect Git worktree config: {error}"))?;
    match output.status.code() {
        Some(0) => Ok(output.stdout.starts_with(b"true")),
        Some(1) => Ok(false),
        _ => Err(String::from_utf8_lossy(&output.stderr).trim().to_owned()),
    }
}

fn config_scope_has_executable_filters(worktree: &Path, scope: &str) -> Result<bool, String> {
    let output = git_command()
        .arg("--no-optional-locks")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .arg("-C")
        .arg(worktree)
        .args([
            "config",
            "--includes",
            scope,
            "--null",
            "--get-regexp",
            r"^filter\..*\.(clean|process|smudge)$",
        ])
        .output()
        .map_err(|error| format!("could not inspect Git filters: {error}"))?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(String::from_utf8_lossy(&output.stderr).trim().to_owned()),
    }
}

fn unreferenced_private_commits(common_dir: &Path, worktree: &Path) -> Result<Vec<String>, String> {
    let record = find_worktree_record(common_dir, worktree)?;
    let mut commits = HashSet::new();
    collect_private_reachable_commits(common_dir, &record, &mut commits)?;
    collect_pseudoref_commits(&record, &mut commits)?;

    let mut unreferenced = Vec::new();
    for commit in commits {
        if references_containing(common_dir, &commit)?.is_empty() {
            unreferenced.push(commit);
        }
    }
    unreferenced.sort();
    Ok(unreferenced)
}

fn collect_private_reachable_commits(
    common_dir: &Path,
    record: &Path,
    commits: &mut HashSet<String>,
) -> Result<(), String> {
    let arguments = ["rev-list", "--single-worktree", "--reflog", "--all"];
    let shared = lines(&git_dir(common_dir, arguments)?.stdout)?
        .into_iter()
        .collect::<HashSet<_>>();
    for commit in lines(&git_dir(record, arguments)?.stdout)? {
        if !shared.contains(&commit) {
            add_object_name(commit.as_bytes(), commits);
        }
    }
    Ok(())
}

fn collect_pseudoref_commits(record: &Path, commits: &mut HashSet<String>) -> Result<(), String> {
    const PSEUDOREFS: &[&str] = &[
        "BISECT_HEAD",
        "CHERRY_PICK_HEAD",
        "FETCH_HEAD",
        "MERGE_HEAD",
        "ORIG_HEAD",
        "REBASE_HEAD",
        "REVERT_HEAD",
    ];
    for name in PSEUDOREFS {
        let path = record.join(name);
        let exists = path
            .try_exists()
            .map_err(|error| format!("could not inspect {}: {error}", path.display()))?;
        if !exists {
            continue;
        }
        let kind = fs::symlink_metadata(&path)
            .map_err(|error| format!("could not inspect {}: {error}", path.display()))?
            .file_type();
        if !kind.is_file() {
            return Err(format!("{} is not a regular file", path.display()));
        }
        let value = fs::read(&path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        for line in value.split(|byte| *byte == b'\n') {
            if let Some(value) = line.split(|byte| byte.is_ascii_whitespace()).next() {
                let Some(commit) = object_name(value) else {
                    continue;
                };
                let commit_object = format!("{commit}^{{commit}}");
                git_dir(record, ["cat-file", "-e", &commit_object])?;
                commits.insert(commit);
            }
        }
    }
    Ok(())
}

fn add_object_name(value: &[u8], commits: &mut HashSet<String>) {
    if let Some(value) = object_name(value) {
        commits.insert(value);
    }
}

fn object_name(value: &[u8]) -> Option<String> {
    if value.len() >= 40
        && value.iter().all(u8::is_ascii_hexdigit)
        && value.iter().any(|byte| *byte != b'0')
    {
        Some(String::from_utf8_lossy(value).into_owned())
    } else {
        None
    }
}

fn operations(worktree: &Path) -> Result<Vec<String>, String> {
    let output = git(
        worktree,
        ["rev-parse", "--path-format=absolute", "--git-dir"],
    )?;
    let git_dir = one_path(&output.stdout);
    operations_in_git_dir(&git_dir)
}

fn operations_in_git_dir(git_dir: &Path) -> Result<Vec<String>, String> {
    let checks = [
        ("MERGE_HEAD", "merge"),
        ("CHERRY_PICK_HEAD", "cherry-pick"),
        ("REVERT_HEAD", "revert"),
        ("BISECT_LOG", "bisect"),
        ("index.lock", "index update"),
        ("rebase-apply", "rebase"),
        ("rebase-merge", "rebase"),
        ("sequencer", "sequencer"),
    ];
    let mut found = Vec::new();
    for (marker, name) in checks {
        let path = git_dir.join(marker);
        if path
            .try_exists()
            .map_err(|error| format!("could not inspect {}: {error}", path.display()))?
            && !found.iter().any(|item| item == name)
        {
            found.push(name.to_owned());
        }
    }
    Ok(found)
}

fn missing_index_entries(record: &Path, head: &str) -> Result<Vec<String>, String> {
    let output = git_dir(
        record,
        ["diff-index", "--cached", "--name-only", "-z", head, "--"],
    )?;
    Ok(nul_fields(&output.stdout))
}

pub fn remove_ready(worktree: &Worktree) -> Result<(), String> {
    validate_removal_target(worktree)?;
    let refreshed = refresh(worktree)?;
    ensure_unchanged(worktree, &refreshed)?;
    if refreshed.state() != State::Ready {
        return Err(format!(
            "state changed before removal: {:?}",
            refreshed.state()
        ));
    }
    remove_without_force(&refreshed)
}

fn remove_without_force(worktree: &Worktree) -> Result<(), String> {
    git_dir_with_path(
        &worktree.common_dir,
        [
            OsStr::new("worktree"),
            OsStr::new("remove"),
            OsStr::new("--"),
        ],
        &worktree.path,
    )?;
    Ok(())
}

#[derive(Debug)]
pub struct PreserveFailure {
    pub message: String,
    pub saved: Vec<String>,
}

pub fn preserve_and_remove(worktree: &Worktree) -> Result<Vec<String>, PreserveFailure> {
    let mut saved = Vec::new();
    validate_removal_target(worktree).map_err(|error| preserve_failure(error, &saved))?;
    let current = refresh(worktree).map_err(|error| preserve_failure(error, &saved))?;
    ensure_unchanged(worktree, &current).map_err(|error| preserve_failure(error, &saved))?;
    if let Some(error) = &current.inspection_error {
        return Err(preserve_failure(
            format!("fix the inspection failure first: {error}"),
            &saved,
        ));
    }
    if current.is_unborn() {
        return Err(preserve_failure(
            "the branch has no commit; keep it or delete it explicitly".to_owned(),
            &saved,
        ));
    }
    if !current.exists && !current.local_entries.is_empty() {
        return Err(preserve_failure(
            "the directory is missing, so staged index data cannot be stashed".to_owned(),
            &saved,
        ));
    }
    if !current.operations.is_empty() {
        return Err(preserve_failure(
            "finish or abort the current Git operation first".to_owned(),
            &saved,
        ));
    }
    if current.has_initialized_submodules {
        return Err(preserve_failure(
            "remove or empty the submodule directories first".to_owned(),
            &saved,
        ));
    }
    if current.has_nested_repositories {
        return Err(preserve_failure(
            "remove or move the nested Git repository first".to_owned(),
            &saved,
        ));
    }
    if current.has_hidden_index_flags {
        return Err(preserve_failure(
            "clear assume-unchanged and skip-worktree index flags first".to_owned(),
            &saved,
        ));
    }
    if current.has_executable_filters {
        return Err(preserve_failure(
            "remove executable Git clean, smudge, and process filters first".to_owned(),
            &saved,
        ));
    }

    let mut rescue = current.unreferenced_private_commits.clone();
    if current.branch.is_none() && current.detached_references.is_empty() {
        rescue.push(current.head.clone());
    }
    rescue.sort();
    rescue.dedup();
    for commit in rescue {
        let name = rescue_branch_name(&commit);
        git_dir(&current.common_dir, ["branch", &name, &commit])
            .map_err(|error| preserve_failure(error, &saved))?;
        saved.push(format!("branch {name}"));
    }

    if !current.exists {
        remove_missing_record(&current).map_err(|error| preserve_failure(error, &saved))?;
        return Ok(saved);
    }

    if !current.local_entries.is_empty() {
        let message = format!(
            "chop backup {} {}",
            timestamp(),
            terminal::path(&current.path)
        );
        let before = stash_ref(&current.path).map_err(|error| preserve_failure(error, &saved))?;
        git(
            &current.path,
            [
                "-c",
                "user.name=chop",
                "-c",
                "user.email=chop@localhost",
                "stash",
                "push",
                "--all",
                "--message",
                &message,
            ],
        )
        .map_err(|error| preserve_failure(error, &saved))?;
        let after = stash_ref(&current.path).map_err(|error| preserve_failure(error, &saved))?;
        if after == before {
            return Err(preserve_failure(
                "Git did not create a stash, so the worktree was kept".to_owned(),
                &saved,
            ));
        }
        let hash = after.as_deref().ok_or_else(|| {
            preserve_failure("Git removed refs/stash unexpectedly".to_owned(), &saved)
        })?;
        saved.push(format!("stash {}", short(hash)));

        if !local_entries(&current.path)
            .map_err(|error| preserve_failure(error, &saved))?
            .is_empty()
        {
            return Err(preserve_failure(
                "Git still reports local files after the stash".to_owned(),
                &saved,
            ));
        }
    }

    let lock_reason = current.flags.locked.as_deref();
    if lock_reason.is_some() {
        git_dir_with_path(
            &current.common_dir,
            [
                OsStr::new("worktree"),
                OsStr::new("unlock"),
                OsStr::new("--"),
            ],
            &current.path,
        )
        .map_err(|error| preserve_failure(error, &saved))?;
    }

    let removal = (|| -> Result<(), String> {
        let mut refreshed = refresh(&current)?;
        if !local_entries(&current.path)?.is_empty() {
            return Err(
                "local files appeared after preservation; the worktree was kept".to_owned(),
            );
        }
        refreshed.local_entries.clear();
        match refreshed.state() {
            State::Ready => remove_without_force(&refreshed),
            state => Err(format!(
                "the worktree is still not ready after preservation: {state:?}"
            )),
        }
    })();
    if let Err(error) = removal {
        let message = match lock_reason {
            Some(reason) => match restore_worktree_lock(&current, reason) {
                Ok(()) => format!("{error}. Chop restored the worktree lock"),
                Err(lock_error) => {
                    format!("{error}. Chop could not restore the worktree lock: {lock_error}")
                }
            },
            None => error,
        };
        return Err(preserve_failure(message, &saved));
    }
    Ok(saved)
}

fn restore_worktree_lock(worktree: &Worktree, reason: &str) -> Result<(), String> {
    if reason.is_empty() {
        git_dir_with_path(
            &worktree.common_dir,
            [OsStr::new("worktree"), OsStr::new("lock"), OsStr::new("--")],
            &worktree.path,
        )?;
    } else {
        git_dir_with_path(
            &worktree.common_dir,
            [
                OsStr::new("worktree"),
                OsStr::new("lock"),
                OsStr::new("--reason"),
                OsStr::new(reason),
                OsStr::new("--"),
            ],
            &worktree.path,
        )?;
    }
    Ok(())
}

fn preserve_failure(message: String, saved: &[String]) -> PreserveFailure {
    PreserveFailure {
        message,
        saved: saved.to_vec(),
    }
}

fn stash_ref(worktree: &Path) -> Result<Option<String>, String> {
    let output = run_git(worktree, ["rev-parse", "--verify", "--quiet", "refs/stash"])?;
    match output.status.code() {
        Some(0) => one_line(&output.stdout).map(|value| Some(value.to_owned())),
        Some(1) => Ok(None),
        _ => Err(String::from_utf8_lossy(&output.stderr).trim().to_owned()),
    }
}

fn local_entries(worktree: &Path) -> Result<Vec<String>, String> {
    let output = git(
        worktree,
        [
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignored=matching",
            "--ignore-submodules=none",
        ],
    )?;
    checked_status_entries(&output)
}

pub fn force_remove(worktree: &Worktree) -> Result<(), String> {
    validate_removal_target(worktree)?;
    let current = refresh(worktree)?;
    ensure_unchanged(worktree, &current)?;
    validate_removal_target(&current)?;
    if !current.exists {
        return remove_missing_record(&current);
    }
    git_dir_with_path(
        &current.common_dir,
        [
            OsStr::new("worktree"),
            OsStr::new("remove"),
            OsStr::new("--force"),
            OsStr::new("--force"),
            OsStr::new("--"),
        ],
        &current.path,
    )?;
    Ok(())
}

fn remove_missing_record(worktree: &Worktree) -> Result<(), String> {
    let records = worktree.common_dir.join("worktrees");
    let record = find_worktree_record(&worktree.common_dir, &worktree.path)?;
    if worktree
        .path
        .try_exists()
        .map_err(|error| format!("could not inspect {}: {error}", worktree.path.display()))?
    {
        return Err("the missing worktree directory returned; inspect it again".to_owned());
    }

    let quarantine = records.join(format!(
        ".chop-delete-{}-{}",
        std::process::id(),
        timestamp()
    ));
    fs::rename(&record, &quarantine)
        .map_err(|error| format!("could not isolate the Git record: {error}"))?;
    if worktree
        .path
        .try_exists()
        .map_err(|error| format!("could not inspect {}: {error}", worktree.path.display()))?
    {
        let _ = fs::rename(&quarantine, &record);
        return Err("the missing worktree directory returned; its record was restored".to_owned());
    }
    if let Err(error) = fs::remove_dir_all(&quarantine) {
        let restore = fs::rename(&quarantine, &record);
        return match restore {
            Ok(()) => Err(format!("could not delete the Git record: {error}")),
            Err(restore_error) => Err(format!(
                "could not delete or restore the isolated Git record: {error}; {restore_error}"
            )),
        };
    }
    Ok(())
}

fn find_worktree_record(common_dir: &Path, worktree: &Path) -> Result<PathBuf, String> {
    let records = common_dir.join("worktrees");
    let mut matches = Vec::new();
    let entries = fs::read_dir(&records)
        .map_err(|error| format!("could not read {}: {error}", records.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let metadata = entry
            .file_type()
            .map_err(|error| format!("could not inspect {}: {error}", entry.path().display()))?;
        if !metadata.is_dir() || metadata.is_symlink() {
            continue;
        }
        let gitdir_path = entry.path().join("gitdir");
        let gitdir = match fs::read(&gitdir_path) {
            Ok(value) => resolve_metadata_link(&entry.path(), one_path(&value))?,
            Err(_) => continue,
        };
        if gitdir.parent() == Some(worktree) {
            matches.push(entry.path());
        }
    }

    let [record] = matches.as_slice() else {
        return Err(format!(
            "expected one metadata record for {}, found {}",
            worktree.display(),
            matches.len()
        ));
    };
    Ok(record.clone())
}

fn resolve_metadata_link(record: &Path, value: PathBuf) -> Result<PathBuf, String> {
    if value.is_absolute() {
        Ok(value)
    } else {
        let absolute = std::path::absolute(record.join(value))
            .map_err(|error| format!("could not resolve relative Git metadata: {error}"))?;
        Ok(normalize_lexically(&absolute))
    }
}

fn normalize_lexically(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    normalized
}

fn refresh(worktree: &Worktree) -> Result<Worktree, String> {
    let output = git_dir(
        &worktree.common_dir,
        ["worktree", "list", "--porcelain", "-z"],
    )?;
    let mut worktrees = parse_porcelain(&output.stdout)?;
    resolve_existing_paths(&mut worktrees)?;
    let raw = worktrees
        .into_iter()
        .enumerate()
        .find(|(_, item)| item.path == worktree.path)
        .ok_or_else(|| "worktree record disappeared during refresh".to_owned())?;
    let current_dir = std::env::current_dir()
        .and_then(|path| path.canonicalize())
        .map_err(|error| error.to_string())?;
    Ok(inspect_worktree(
        raw.1,
        &worktree.common_dir,
        raw.0 == 0,
        &current_dir,
    ))
}

fn ensure_unchanged(before: &Worktree, after: &Worktree) -> Result<(), String> {
    let references_only_grew = before
        .detached_references
        .iter()
        .all(|reference| after.detached_references.contains(reference));
    let private_commits_unchanged =
        before.unreferenced_private_commits == after.unreferenced_private_commits;
    let mut normalized = after.clone();
    normalized.detached_references = before.detached_references.clone();
    normalized.unreferenced_private_commits = before.unreferenced_private_commits.clone();
    if !references_only_grew || !private_commits_unchanged || before != &normalized {
        Err("worktree state changed after the scan; inspect it again".to_owned())
    } else {
        Ok(())
    }
}

fn validate_removal_target(worktree: &Worktree) -> Result<(), String> {
    if worktree.is_main || worktree.flags.bare {
        return Err("refusing to remove a main or bare working directory".to_owned());
    }
    if worktree.contains_current_dir {
        return Err("refusing to remove the current working directory".to_owned());
    }
    if worktree.path.parent().is_none() {
        return Err("refusing to remove a filesystem root".to_owned());
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = canonical_or_owned(Path::new(&home))?;
        let target = canonical_or_owned(&worktree.path)?;
        if target == home {
            return Err("refusing to remove the home directory".to_owned());
        }
    }
    if worktree.exists {
        let actual_common = common_dir(&worktree.path)?;
        if actual_common != worktree.common_dir {
            return Err("the directory now belongs to a different Git repository".to_owned());
        }
        let output = git(
            &worktree.path,
            ["rev-parse", "--path-format=absolute", "--show-toplevel"],
        )?;
        let actual_top = canonical_or_owned(&one_path(&output.stdout))?;
        let expected_top = canonical_or_owned(&worktree.path)?;
        if actual_top != expected_top {
            return Err("the path is not the root of this worktree".to_owned());
        }
    }
    Ok(())
}

fn canonical_or_owned(path: &Path) -> Result<PathBuf, String> {
    match path.canonicalize() {
        Ok(path) => Ok(path),
        Err(_) if !path.try_exists().map_err(|error| error.to_string())? => Ok(path.to_path_buf()),
        Err(error) => Err(format!("could not resolve {}: {error}", path.display())),
    }
}

fn rescue_branch_name(head: &str) -> String {
    format!("chop-rescue-{}-{}", timestamp(), short(head))
}

fn timestamp() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn short(value: &str) -> &str {
    value.get(..value.len().min(12)).unwrap_or(value)
}

pub fn status_summary(worktree: &Worktree) -> Result<String, String> {
    if !worktree.exists {
        return Ok("The directory does not exist.".to_owned());
    }
    let output = git(
        &worktree.path,
        [
            "status",
            "--short",
            "--branch",
            "--untracked-files=all",
            "--ignored=matching",
        ],
    )?;
    reject_status_warnings(&output)?;
    let mut text = String::from_utf8(output.stdout)
        .map_err(|_| "Git status output is not UTF-8".to_owned())?;
    if text.trim().is_empty() {
        text.push_str("The worktree is clean.\n");
    }
    Ok(text)
}

pub fn diff_preview(worktree: &Worktree) -> Result<String, String> {
    let (status, truncated) = preview_git(
        &worktree.path,
        [
            "status",
            "--short",
            "--branch",
            "--untracked-files=all",
            "--ignored=matching",
        ],
        false,
    )?;
    reject_status_warnings(&status)?;
    let mut sections = vec![if truncated {
        String::from_utf8_lossy(&status.stdout).into_owned()
    } else {
        String::from_utf8(status.stdout).map_err(|_| "Git status output is not UTF-8".to_owned())?
    }];
    if truncated {
        return Ok(format_preview(sections, true));
    }
    for (title, cached) in [("Staged changes", true), ("Unstaged changes", false)] {
        let mut arguments = vec![
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--no-renames",
            "--ignore-submodules=all",
            "--unified=3",
        ];
        if cached {
            arguments.push("--cached");
        }
        arguments.push("--");
        let (output, truncated) = preview_git(&worktree.path, arguments, false)?;
        if !output.stdout.is_empty() {
            sections.push(format!(
                "{title}\n{}",
                String::from_utf8_lossy(&output.stdout)
            ));
        }
        if truncated {
            return Ok(format_preview(sections, true));
        }
    }
    let (untracked, truncated) = preview_git(
        &worktree.path,
        ["ls-files", "--others", "--exclude-standard", "-z"],
        false,
    )?;
    if truncated {
        return Ok(format_preview(sections, true));
    }
    let paths: Vec<_> = untracked
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .collect();
    for bytes in paths.iter().take(20) {
        let path = worktree.path.join(OsString::from_vec(bytes.to_vec()));
        let metadata = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
        if !metadata.is_file() || metadata.len() > 64 * 1024 {
            sections.push(format!(
                "Untracked: {} (preview omitted: large file or non-regular path)",
                terminal::path(&path)
            ));
            continue;
        }
        let (output, truncated) = preview_git(
            &worktree.path,
            [
                OsStr::new("diff"),
                OsStr::new("--no-index"),
                OsStr::new("--no-ext-diff"),
                OsStr::new("--no-textconv"),
                OsStr::new("--no-color"),
                OsStr::new("--"),
                OsStr::new("/dev/null"),
                path.as_os_str(),
            ],
            true,
        )?;
        sections.push(format!(
            "Untracked file\n{}",
            String::from_utf8_lossy(&output.stdout)
        ));
        if truncated {
            return Ok(format_preview(sections, true));
        }
    }
    if paths.len() > 20 {
        sections.push("Only the first 20 untracked files are previewed.".to_owned());
    }
    sections.push("Ignored paths are listed above; their contents are not previewed.".to_owned());
    Ok(format_preview(sections, false))
}

const PREVIEW_BYTES: usize = 200_000;

fn format_preview(sections: Vec<String>, mut truncated: bool) -> String {
    let mut preview = String::new();
    for (index, line) in sections
        .iter()
        .flat_map(|section| section.lines().chain(std::iter::once("")))
        .enumerate()
    {
        if index >= 2000 || preview.len() + line.len() + 1 > PREVIEW_BYTES {
            truncated = true;
            break;
        }
        preview.push_str(line);
        preview.push('\n');
    }
    if truncated {
        preview.push_str("\nPreview truncated. Inspect the remaining changes in Git.\n");
    }
    preview
}

fn preview_git<I, S>(
    directory: &Path,
    arguments: I,
    allow_diff_exit: bool,
) -> Result<(Output, bool), String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut child = repository_command(directory, arguments)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not run Git: {error}"))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    // Drain stderr concurrently so warnings cannot block Git's stdout pipe.
    std::thread::scope(|scope| {
        let error_reader = scope.spawn(move || -> io::Result<Vec<u8>> {
            let mut bytes = Vec::new();
            stderr
                .by_ref()
                .take(PREVIEW_BYTES as u64)
                .read_to_end(&mut bytes)?;
            io::copy(&mut stderr, &mut io::sink())?;
            Ok(bytes)
        });
        let mut bytes = Vec::new();
        let read = stdout
            .take(PREVIEW_BYTES as u64 + 1)
            .read_to_end(&mut bytes);
        let truncated = bytes.len() > PREVIEW_BYTES;
        if truncated || read.is_err() {
            let _ = child.kill();
        }
        // Always reap the child, including read failures and truncated output.
        let status = child.wait();
        let errors = error_reader.join().expect("stderr reader panicked");
        read.map_err(|error| format!("could not read Git output: {error}"))?;
        let output = Output {
            status: status.map_err(|error| format!("could not wait for Git: {error}"))?,
            stdout: bytes,
            stderr: errors.map_err(|error| format!("could not read Git errors: {error}"))?,
        };
        if !(truncated
            || output.status.success()
            || allow_diff_exit && output.status.code() == Some(1))
        {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
        }
        Ok((output, truncated))
    })
}

fn checked_status_entries(output: &Output) -> Result<Vec<String>, String> {
    reject_status_warnings(output)?;
    Ok(nul_fields(&output.stdout))
}

fn reject_status_warnings(output: &Output) -> Result<(), String> {
    let warning = String::from_utf8_lossy(&output.stderr);
    let warning = warning.trim();
    if warning.is_empty() {
        Ok(())
    } else {
        Err(format!("Git status could not inspect all files: {warning}"))
    }
}

fn git<I, S>(directory: &Path, arguments: I) -> Result<Output, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = run_git(directory, arguments)?;
    if output.status.success() {
        Ok(output)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(stderr.trim().to_owned())
    }
}

fn run_git<I, S>(directory: &Path, arguments: I) -> Result<Output, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    repository_command(directory, arguments)
        .output()
        .map_err(|error| format!("could not run Git: {error}"))
}

fn repository_command<I, S>(directory: &Path, arguments: I) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = git_command();
    command
        .arg("--no-optional-locks")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fileMode=true",
        ])
        .arg("-C")
        .arg(directory)
        .args(arguments);
    command
}

fn git_dir<I, S>(common_dir: &Path, arguments: I) -> Result<Output, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = git_command()
        .arg("--no-optional-locks")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fileMode=true",
        ])
        .arg("--git-dir")
        .arg(common_dir)
        .args(arguments)
        .output()
        .map_err(|error| format!("could not run Git: {error}"))?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

fn git_dir_with_path<const N: usize>(
    common_dir: &Path,
    arguments: [&OsStr; N],
    path: &Path,
) -> Result<Output, String> {
    let output = git_command()
        .arg("--no-optional-locks")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fileMode=true",
        ])
        .arg("--git-dir")
        .arg(common_dir)
        .args(arguments)
        .arg(path)
        .output()
        .map_err(|error| format!("could not run Git: {error}"))?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

fn git_command() -> Command {
    let mut command = base_git_command();
    command.env("GIT_CONFIG_NOSYSTEM", "1");
    command.env("GIT_CONFIG_GLOBAL", "/dev/null");
    command.env("GIT_NO_LAZY_FETCH", "1");
    add_safe_directories(&mut command, safe_directories());
    command
}

fn safe_directories() -> &'static [OsString] {
    static SAFE_DIRECTORIES: OnceLock<Vec<OsString>> = OnceLock::new();
    SAFE_DIRECTORIES.get_or_init(|| {
        let mut values = Vec::new();
        for scope in ["--system", "--global"] {
            let output = base_git_command()
                .args(["config", scope, "--null", "--get-all", "safe.directory"])
                .output();
            let Ok(output) = output else {
                continue;
            };
            if output.status.success() {
                let mut fields = output.stdout.split(|byte| *byte == 0).collect::<Vec<_>>();
                if fields.last().is_some_and(|value| value.is_empty()) {
                    fields.pop();
                }
                values.extend(
                    fields
                        .into_iter()
                        .map(|value| OsString::from_vec(value.to_vec())),
                );
            }
        }
        values
    })
}

fn add_safe_directories(command: &mut Command, directories: &[OsString]) {
    for directory in directories {
        let mut setting = OsString::from("safe.directory=");
        setting.push(directory);
        command.arg("-c").arg(setting);
    }
}

fn base_git_command() -> Command {
    const REPOSITORY_ENVIRONMENT: &[&str] = &[
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_PREFIX",
        "GIT_CEILING_DIRECTORIES",
        "GIT_DISCOVERY_ACROSS_FILESYSTEM",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_NOSYSTEM",
        "GIT_NO_LAZY_FETCH",
    ];

    let mut command = Command::new("git");
    for name in REPOSITORY_ENVIRONMENT {
        command.env_remove(name);
    }
    command
}

fn one_line(bytes: &[u8]) -> Result<&str, String> {
    std::str::from_utf8(bytes)
        .map(str::trim)
        .map_err(|_| "Git output is not UTF-8".to_owned())
}

fn one_path(bytes: &[u8]) -> PathBuf {
    let end = bytes
        .iter()
        .rposition(|byte| !matches!(byte, b'\n' | b'\r'))
        .map_or(0, |index| index + 1);
    PathBuf::from(OsString::from_vec(bytes[..end].to_vec()))
}

fn lines(bytes: &[u8]) -> Result<Vec<String>, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "Git output is not UTF-8".to_owned())?;
    Ok(text.lines().map(str::to_owned).collect())
}

fn nul_fields(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .map(|field| String::from_utf8_lossy(field).into_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        RawWorktree, add_safe_directories, checked_status_entries, ensure_unchanged, force_remove,
        git, git_command, inspect_repositories, inspect_repositories_with_options, parse_porcelain,
        preserve_and_remove, remove_ready, restore_worktree_lock, stash_ref, status_summary,
    };
    use crate::model::{State, WorktreeFlags};
    use std::collections::HashMap;
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::ExitStatusExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, ExitStatus, Output};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn parses_nul_porcelain_with_spaces() {
        let input = b"worktree /tmp/main repo\0HEAD abc\0branch refs/heads/main\0\0worktree /tmp/linked\0HEAD def\0detached\0locked in use\0\0";

        let parsed = parse_porcelain(input).unwrap();

        assert_eq!(
            parsed,
            vec![
                RawWorktree {
                    path: PathBuf::from("/tmp/main repo"),
                    head: "abc".to_owned(),
                    branch: Some("refs/heads/main".to_owned()),
                    flags: WorktreeFlags::default(),
                },
                RawWorktree {
                    path: PathBuf::from("/tmp/linked"),
                    head: "def".to_owned(),
                    branch: None,
                    flags: WorktreeFlags {
                        locked: Some("in use".to_owned()),
                        ..WorktreeFlags::default()
                    },
                },
            ]
        );
    }

    #[test]
    fn successful_status_warning_fails_inspection() {
        let output = Output {
            status: ExitStatus::from_raw(0),
            stdout: Vec::new(),
            stderr: b"warning: could not open directory 'private/': Permission denied\n".to_vec(),
        };

        let error = checked_status_entries(&output).unwrap_err();

        assert!(error.contains("could not inspect all files"));
        assert!(error.contains("Permission denied"));
    }

    #[test]
    fn restores_worktree_lock_with_original_reason() {
        let repository = TestRepository::new("restore lock");
        let linked = repository.add_worktree("linked", "locked-topic");
        let path = linked.to_str().unwrap();
        repository.git(["worktree", "lock", "--reason", "in use", path]);
        let (repositories, errors) = inspect_repositories(
            std::slice::from_ref(&repository.main),
            &repository.main,
            std::slice::from_ref(&repository.root),
        );
        assert!(errors.is_empty());
        let item = repositories[0]
            .worktrees
            .iter()
            .find(|item| item.path == linked)
            .unwrap();
        repository.git(["worktree", "unlock", path]);

        restore_worktree_lock(item, "in use").unwrap();

        let list = repository.git_output(["worktree", "list", "--porcelain"]);
        assert!(list.contains("locked in use"));
    }

    #[test]
    fn parses_non_utf8_worktree_path() {
        let mut input = b"worktree /tmp/worktree-".to_vec();
        input.push(0xff);
        input.extend_from_slice(b"\0HEAD abc\0branch refs/heads/main\0\0");

        let parsed = parse_porcelain(&input).unwrap();

        assert_eq!(parsed[0].path.as_os_str().as_bytes(), b"/tmp/worktree-\xff");
    }

    #[test]
    fn git_command_clears_repository_environment() {
        let command = git_command();
        let environment = command.get_envs().collect::<HashMap<_, _>>();
        let removed = environment
            .iter()
            .filter_map(|(name, value)| value.is_none().then_some(*name))
            .collect::<Vec<_>>();

        assert!(removed.contains(&OsStr::new("GIT_DIR")));
        assert!(removed.contains(&OsStr::new("GIT_INDEX_FILE")));
        assert!(removed.contains(&OsStr::new("GIT_CONFIG_COUNT")));
        assert_eq!(
            environment.get(OsStr::new("GIT_CONFIG_NOSYSTEM")),
            Some(&Some(OsStr::new("1")))
        );
        assert_eq!(
            environment.get(OsStr::new("GIT_CONFIG_GLOBAL")),
            Some(&Some(OsStr::new("/dev/null")))
        );
        assert_eq!(
            environment.get(OsStr::new("GIT_NO_LAZY_FETCH")),
            Some(&Some(OsStr::new("1")))
        );
    }

    #[test]
    fn adds_trusted_directories_to_isolated_git_commands() {
        let mut command = Command::new("git");

        add_safe_directories(
            &mut command,
            &[OsString::from("/shared/one"), OsString::from("/shared/two")],
        );

        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                OsStr::new("-c"),
                OsStr::new("safe.directory=/shared/one"),
                OsStr::new("-c"),
                OsStr::new("safe.directory=/shared/two"),
            ]
        );
    }

    #[test]
    fn changed_local_state_fails_refresh_guard() {
        let before = worktree_for_refresh_guard();
        let mut after = before.clone();
        after.local_entries.push("? new-file".to_owned());

        assert!(ensure_unchanged(&before, &after).is_err());
    }

    #[test]
    fn private_commit_disappearance_fails_refresh_guard() {
        let mut before = worktree_for_refresh_guard();
        before.branch = None;
        before
            .unreferenced_private_commits
            .push(before.head.clone());
        let mut after = before.clone();
        after
            .detached_references
            .push("refs/heads/chop-rescue".to_owned());
        after.unreferenced_private_commits.clear();

        assert!(ensure_unchanged(&before, &after).is_err());
    }

    #[test]
    fn diff_preview_includes_staged_unstaged_and_untracked_changes() {
        let repository = TestRepository::new("diff preview");
        let linked = repository.add_worktree("linked", "diff-topic");
        fs::write(linked.join("tracked.txt"), "staged content\n").unwrap();
        git(&linked, ["add", "tracked.txt"]).unwrap();
        fs::write(linked.join("tracked.txt"), "unstaged content\n").unwrap();
        fs::write(linked.join("untracked.txt"), "untracked content\n").unwrap();
        fs::write(linked.join("binary.bin"), [0, 1, 2]).unwrap();
        fs::write(linked.join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(
            linked.join("ignored.txt"),
            "ignored contents must not appear\n",
        )
        .unwrap();
        let marker = repository.root.join("external-diff-ran");
        repository.git([
            "config",
            "diff.external",
            &format!("touch '{}'", marker.display()),
        ]);
        let (repositories, errors) = inspect_repositories(
            std::slice::from_ref(&repository.main),
            &repository.main,
            std::slice::from_ref(&repository.root),
        );
        assert!(errors.is_empty());
        let item = repositories[0]
            .worktrees
            .iter()
            .find(|item| item.path == linked)
            .unwrap();
        let preview = super::diff_preview(item).unwrap();
        assert!(preview.contains("Staged changes"));
        assert!(preview.contains("+staged content"));
        assert!(preview.contains("Unstaged changes"));
        assert!(preview.contains("+unstaged content"));
        assert!(preview.contains("+untracked content"));
        assert!(preview.contains("Binary files"));
        assert!(preview.contains("!! ignored.txt"));
        assert!(!preview.contains("ignored contents must not appear"));
        assert!(!marker.exists());
    }

    #[test]
    fn diff_preview_bounds_large_tracked_output() {
        let repository = TestRepository::new("large diff preview");
        fs::write(
            repository.main.join("large.txt"),
            "changed line\n".repeat(100_000),
        )
        .unwrap();
        repository.git(["add", "large.txt"]);
        let (output, truncated) = super::preview_git(
            &repository.main,
            ["diff", "--cached", "--no-ext-diff", "--no-textconv", "--"],
            false,
        )
        .unwrap();
        assert!(truncated);
        assert_eq!(output.stdout.len(), super::PREVIEW_BYTES + 1);

        let (repositories, errors) = inspect_repositories(
            std::slice::from_ref(&repository.main),
            &repository.main,
            std::slice::from_ref(&repository.root),
        );
        assert!(errors.is_empty());
        let preview = super::diff_preview(&repositories[0].worktrees[0]).unwrap();
        assert!(preview.contains("Staged changes"));
        assert!(preview.contains("+changed line"));
        assert!(preview.contains("Preview truncated."));
        assert!(preview.len() < super::PREVIEW_BYTES + 100);
        assert!(preview.lines().count() < 2010);
    }

    #[test]
    fn status_summary_shows_untracked_files_despite_repository_config() {
        let repository = TestRepository::new("status untracked");
        let linked = repository.add_worktree("linked", "status-topic");
        repository.git(["config", "status.showUntrackedFiles", "no"]);
        fs::write(linked.join("untracked.txt"), "local\n").unwrap();
        let (repositories, errors) = inspect_repositories(
            std::slice::from_ref(&repository.main),
            &repository.main,
            std::slice::from_ref(&repository.root),
        );
        assert!(errors.is_empty());
        let item = repositories[0]
            .worktrees
            .iter()
            .find(|item| item.path == linked)
            .unwrap();

        let status = status_summary(item).unwrap();

        assert!(status.contains("?? untracked.txt"));
    }

    fn worktree_for_refresh_guard() -> crate::model::Worktree {
        crate::model::Worktree {
            common_dir: PathBuf::from("/repo/.git"),
            path: PathBuf::from("/worktree"),
            head: "abc".to_owned(),
            branch: Some("refs/heads/main".to_owned()),
            flags: WorktreeFlags::default(),
            is_main: false,
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
        }
    }

    #[test]
    fn removes_clean_linked_worktree_but_keeps_main_and_branch() {
        let repository = TestRepository::new("clean removal");
        let linked = repository.add_worktree("linked clean", "topic");
        let (repositories, errors) = inspect_repositories(
            std::slice::from_ref(&repository.main),
            &repository.main,
            std::slice::from_ref(&repository.root),
        );
        assert!(errors.is_empty());
        let item = repositories[0]
            .worktrees
            .iter()
            .find(|item| item.path == linked)
            .unwrap();
        assert_eq!(item.state(), State::Ready);

        remove_ready(item).unwrap();

        assert!(!linked.exists());
        assert!(repository.main.exists());
        assert!(repository.git_success(["show-ref", "--verify", "refs/heads/topic"]));
    }

    #[test]
    fn stashes_ignored_and_untracked_files_before_removal() {
        let repository = TestRepository::new("preserve local files");
        fs::write(repository.main.join(".gitignore"), "*.secret\n").unwrap();
        repository.git(["add", ".gitignore"]);
        repository.git(["commit", "-m", "add ignore rule"]);
        let linked = repository.add_worktree("linked dirty", "dirty-topic");
        fs::write(linked.join("note.txt"), "untracked\n").unwrap();
        fs::write(linked.join("key.secret"), "ignored\n").unwrap();
        let (repositories, errors) = inspect_repositories(
            std::slice::from_ref(&repository.main),
            &repository.main,
            std::slice::from_ref(&repository.root),
        );
        assert!(errors.is_empty());
        let item = repositories[0]
            .worktrees
            .iter()
            .find(|item| item.path == linked)
            .unwrap();
        assert!(matches!(item.state(), State::NeedsChoice(_)));

        let saved = preserve_and_remove(item).unwrap();

        assert!(!linked.exists());
        assert!(saved.iter().any(|item| item.starts_with("stash ")));
        assert!(repository.git_success(["show-ref", "--verify", "refs/stash"]));
    }

    #[test]
    fn preservation_does_not_need_user_identity_config() {
        let repository = TestRepository::new("preserve without identity");
        let linked = repository.add_worktree("linked dirty", "dirty-topic");
        repository.git(["config", "--unset", "user.name"]);
        repository.git(["config", "--unset", "user.email"]);
        fs::write(linked.join("note.txt"), "untracked\n").unwrap();
        let (repositories, errors) = inspect_repositories(
            std::slice::from_ref(&repository.main),
            &repository.main,
            std::slice::from_ref(&repository.root),
        );
        assert!(errors.is_empty());
        let item = repositories[0]
            .worktrees
            .iter()
            .find(|item| item.path == linked)
            .unwrap();

        let saved = preserve_and_remove(item).unwrap();

        assert!(!linked.exists());
        assert!(saved.iter().any(|item| item.starts_with("stash ")));
    }

    #[test]
    fn inspection_does_not_run_repository_fsmonitor_hook() {
        let repository = TestRepository::new("fsmonitor disabled");
        let marker = repository.root.join("hook-ran");
        let hook = repository.root.join("fsmonitor-hook");
        fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
        let mut permissions = fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hook, permissions).unwrap();
        repository.git([
            "config",
            "core.fsmonitor",
            hook.to_str().expect("temporary path is UTF-8"),
        ]);

        let (_, errors) = inspect_repositories(
            std::slice::from_ref(&repository.main),
            &repository.main,
            std::slice::from_ref(&repository.root),
        );

        assert!(errors.is_empty());
        assert!(!marker.exists());
    }

    #[test]
    fn active_index_lock_blocks_removal() {
        let repository = TestRepository::new("index lock");
        let linked = repository.add_worktree("linked", "locked-index-topic");
        let git_dir = PathBuf::from(
            repository
                .git_output_at(&linked, ["rev-parse", "--absolute-git-dir"])
                .trim(),
        );
        fs::write(git_dir.join("index.lock"), "pending index data").unwrap();

        let (repositories, errors) = inspect_repositories(
            std::slice::from_ref(&repository.main),
            &repository.main,
            std::slice::from_ref(&repository.root),
        );
        let item = repositories[0]
            .worktrees
            .iter()
            .find(|item| item.path == linked)
            .unwrap();

        assert!(errors.is_empty());
        assert_eq!(item.operations, vec!["index update"]);
        assert!(matches!(item.state(), State::NeedsChoice(_)));
        assert!(remove_ready(item).is_err());
        assert!(linked.exists());
    }

    #[test]
    fn nested_repository_blocks_parent_removal() {
        let repository = TestRepository::new("nested repository");
        let nested = repository.main.join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("tracked.txt"), "nested file\n").unwrap();
        repository.git(["add", "nested/tracked.txt"]);
        repository.git(["commit", "-m", "add nested directory"]);
        let linked = repository.add_worktree("linked", "nested-topic");
        let output = Command::new("git")
            .arg("-C")
            .arg(linked.join("nested"))
            .args(["init", "--initial-branch=main"])
            .output()
            .unwrap();
        assert!(output.status.success());

        let (repositories, errors) = inspect_repositories(
            std::slice::from_ref(&repository.main),
            &repository.main,
            std::slice::from_ref(&repository.root),
        );
        let item = repositories[0]
            .worktrees
            .iter()
            .find(|item| item.path == linked)
            .unwrap();

        assert!(errors.is_empty());
        assert!(item.has_nested_repositories);
        assert!(matches!(item.state(), State::NeedsChoice(_)));
        assert!(remove_ready(item).is_err());
        assert!(linked.join("nested/.git").exists());
    }

    #[test]
    fn file_mode_change_is_detected_and_preserved() {
        let repository = TestRepository::new("file mode");
        let linked = repository.add_worktree("linked", "mode-topic");
        repository.git(["config", "core.fileMode", "false"]);
        let tracked = linked.join("tracked.txt");
        let mut permissions = fs::metadata(&tracked).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&tracked, permissions).unwrap();

        let (repositories, errors) = inspect_repositories(
            std::slice::from_ref(&repository.main),
            &repository.main,
            std::slice::from_ref(&repository.root),
        );
        let item = repositories[0]
            .worktrees
            .iter()
            .find(|item| item.path == linked)
            .unwrap();

        assert!(errors.is_empty());
        assert!(!item.local_entries.is_empty());
        let saved = preserve_and_remove(item).unwrap();
        assert!(saved.iter().any(|item| item.starts_with("stash ")));
        assert!(!linked.exists());
        assert!(
            repository
                .git_output(["ls-tree", "refs/stash", "tracked.txt"])
                .starts_with("100755 ")
        );
    }

    #[test]
    fn inspection_does_not_run_repository_clean_filter() {
        let repository = TestRepository::new("clean filter disabled");
        fs::write(
            repository.main.join(".gitattributes"),
            "tracked.txt filter=evil\n",
        )
        .unwrap();
        repository.git(["add", ".gitattributes"]);
        repository.git(["commit", "-m", "add attributes"]);
        let marker = repository.root.join("filter-ran");
        let filter = repository.root.join("clean-filter");
        fs::write(
            &filter,
            format!("#!/bin/sh\ntouch '{}'\ncat\n", marker.display()),
        )
        .unwrap();
        let mut permissions = fs::metadata(&filter).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&filter, permissions).unwrap();
        let included_config = repository.root.join("included-filter-config");
        fs::write(
            &included_config,
            format!("[filter \"evil\"]\n\tclean = {}\n", filter.display()),
        )
        .unwrap();
        repository.git([
            "config",
            "include.path",
            included_config.to_str().expect("temporary path is UTF-8"),
        ]);
        fs::write(repository.main.join("tracked.txt"), "changed\n").unwrap();

        let (repositories, errors) = inspect_repositories(
            std::slice::from_ref(&repository.main),
            &repository.main,
            std::slice::from_ref(&repository.root),
        );

        assert!(errors.is_empty());
        assert!(repositories[0].worktrees[0].has_executable_filters);
        assert!(!marker.exists());
    }

    #[test]
    fn unpopulated_submodule_directory_with_files_needs_a_choice() {
        let repository = TestRepository::new("unpopulated submodule data");
        let head = repository.git_output(["rev-parse", "HEAD"]);
        repository.git_at(
            &repository.main,
            [
                "update-index",
                "--add",
                "--cacheinfo",
                "160000",
                head.trim(),
                "modules/demo",
            ],
        );
        repository.git(["commit", "-m", "add gitlink"]);
        let linked = repository.add_worktree("submodule-data", "submodule-data-topic");
        let submodule = linked.join("modules/demo");
        fs::create_dir_all(&submodule).unwrap();
        fs::write(submodule.join("notes.txt"), "local data\n").unwrap();

        let (repositories, errors) = inspect_repositories(
            std::slice::from_ref(&repository.main),
            &repository.main,
            std::slice::from_ref(&repository.root),
        );
        let item = repositories[0]
            .worktrees
            .iter()
            .find(|item| item.path == linked)
            .unwrap();

        assert!(errors.is_empty());
        assert!(item.has_initialized_submodules);
        assert!(matches!(item.state(), State::NeedsChoice(_)));
    }

    #[test]
    fn rescues_commit_retained_only_by_private_reflog() {
        let repository = TestRepository::new("private reflog");
        let linked = repository.add_worktree("reflog", "reflog-topic");
        repository.git_at(&linked, ["checkout", "--detach"]);
        fs::write(linked.join("orphan.txt"), "private history\n").unwrap();
        repository.git_at(&linked, ["add", "orphan.txt"]);
        repository.git_at(&linked, ["commit", "-m", "orphan commit"]);
        repository.git_at(&linked, ["checkout", "reflog-topic"]);

        let (repositories, errors) = inspect_repositories(
            std::slice::from_ref(&repository.main),
            &repository.main,
            std::slice::from_ref(&repository.root),
        );
        let item = repositories[0]
            .worktrees
            .iter()
            .find(|item| item.path == linked)
            .unwrap();

        assert!(errors.is_empty());
        assert_eq!(item.unreferenced_private_commits.len(), 1);
        assert!(matches!(item.state(), State::NeedsChoice(_)));
        let hooks = repository.root.join("hooks");
        fs::create_dir(&hooks).unwrap();
        let marker = repository.root.join("reference-hook-ran");
        let hook = hooks.join("reference-transaction");
        fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
        let mut permissions = fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hook, permissions).unwrap();
        repository.git([
            "config",
            "core.hooksPath",
            hooks.to_str().expect("temporary path is UTF-8"),
        ]);
        let saved = preserve_and_remove(item).unwrap();
        assert!(saved.iter().any(|item| item.starts_with("branch ")));
        assert!(!linked.exists());
        assert!(!marker.exists());
    }

    #[test]
    fn finds_private_head_reflog_commit_with_reftable_backend() {
        let repository = TestRepository::new_reftable("reftable reflog");
        let linked = repository.add_worktree("reftable", "reftable-topic");
        repository.git_at(&linked, ["checkout", "--detach"]);
        repository.git_at(&linked, ["commit", "--allow-empty", "-m", "orphan commit"]);
        repository.git_at(&linked, ["checkout", "reftable-topic"]);

        let (repositories, errors) = inspect_repositories(
            std::slice::from_ref(&repository.main),
            &repository.main,
            std::slice::from_ref(&repository.root),
        );
        let item = repositories[0]
            .worktrees
            .iter()
            .find(|item| item.path == linked)
            .unwrap();

        assert!(errors.is_empty());
        assert_eq!(item.unreferenced_private_commits.len(), 1);
        assert!(matches!(item.state(), State::NeedsChoice(_)));
    }

    #[test]
    fn finds_private_ref_with_reftable_backend() {
        let repository = TestRepository::new_reftable("reftable private ref");
        let linked = repository.add_worktree("reftable-ref", "reftable-ref-topic");
        let git_dir = PathBuf::from(
            repository
                .git_output_at(&linked, ["rev-parse", "--absolute-git-dir"])
                .trim(),
        );
        repository.git_at(&linked, ["checkout", "--detach"]);
        repository.git_at(&linked, ["commit", "--allow-empty", "-m", "private ref"]);
        let commit = repository
            .git_output_at(&linked, ["rev-parse", "HEAD"])
            .trim()
            .to_owned();
        repository.git_at(&linked, ["checkout", "reftable-ref-topic"]);
        let update = Command::new("git")
            .arg("--git-dir")
            .arg(&git_dir)
            .args(["update-ref", "refs/worktree/private", &commit])
            .output()
            .unwrap();
        assert!(update.status.success());
        let drop_reflog = Command::new("git")
            .arg("--git-dir")
            .arg(&git_dir)
            .args(["reflog", "drop", "HEAD"])
            .output()
            .unwrap();
        assert!(drop_reflog.status.success());

        let (repositories, errors) = inspect_repositories(
            std::slice::from_ref(&repository.main),
            &repository.main,
            std::slice::from_ref(&repository.root),
        );
        let item = repositories[0]
            .worktrees
            .iter()
            .find(|item| item.path == linked)
            .unwrap();

        assert!(errors.is_empty());
        assert_eq!(item.unreferenced_private_commits, vec![commit]);
    }

    #[test]
    fn finds_commit_in_old_side_of_private_reflog_entry() {
        let repository = TestRepository::new("old reflog object");
        let linked = repository.add_worktree("old-reflog", "old-reflog-topic");
        let git_dir = PathBuf::from(
            repository
                .git_output_at(&linked, ["rev-parse", "--absolute-git-dir"])
                .trim(),
        );
        repository.git_at(&linked, ["checkout", "--detach"]);
        repository.git_at(&linked, ["commit", "--allow-empty", "-m", "old object"]);
        let commit = repository
            .git_output_at(&linked, ["rev-parse", "HEAD"])
            .trim()
            .to_owned();
        repository.git_at(&linked, ["checkout", "old-reflog-topic"]);
        let current = repository
            .git_output_at(&linked, ["rev-parse", "HEAD"])
            .trim()
            .to_owned();
        fs::write(
            git_dir.join("logs/HEAD"),
            format!("{commit} {current} Chop Test <chop@example.invalid> 1 +0000\told only\n"),
        )
        .unwrap();

        let (repositories, errors) = inspect_repositories(
            std::slice::from_ref(&repository.main),
            &repository.main,
            std::slice::from_ref(&repository.root),
        );
        let item = repositories[0]
            .worktrees
            .iter()
            .find(|item| item.path == linked)
            .unwrap();

        assert!(errors.is_empty());
        assert_eq!(item.unreferenced_private_commits, vec![commit]);
    }

    #[test]
    fn finds_commit_retained_only_by_private_pseudoref() {
        let repository = TestRepository::new("private pseudoref");
        let linked = repository.add_worktree("pseudoref", "pseudoref-topic");
        let git_dir = PathBuf::from(
            repository
                .git_output_at(&linked, ["rev-parse", "--absolute-git-dir"])
                .trim(),
        );
        repository.git_at(&linked, ["checkout", "--detach"]);
        fs::write(linked.join("orphan.txt"), "private pseudoref\n").unwrap();
        repository.git_at(&linked, ["add", "orphan.txt"]);
        repository.git_at(&linked, ["commit", "-m", "pseudoref commit"]);
        let commit = repository.git_output_at(&linked, ["rev-parse", "HEAD"]);
        repository.git_at(&linked, ["checkout", "pseudoref-topic"]);
        fs::remove_dir_all(git_dir.join("logs")).unwrap();
        fs::write(git_dir.join("ORIG_HEAD"), commit).unwrap();

        let (repositories, errors) = inspect_repositories(
            std::slice::from_ref(&repository.main),
            &repository.main,
            std::slice::from_ref(&repository.root),
        );
        let item = repositories[0]
            .worktrees
            .iter()
            .find(|item| item.path == linked)
            .unwrap();

        assert!(errors.is_empty());
        assert_eq!(item.unreferenced_private_commits.len(), 1);
        assert!(matches!(item.state(), State::NeedsChoice(_)));
    }

    #[test]
    fn arbitrary_git_metadata_is_not_parsed_as_a_pseudoref() {
        let repository = TestRepository::new("metadata is not pseudoref");
        let linked = repository.add_worktree("linked", "metadata-topic");
        let git_dir = PathBuf::from(
            repository
                .git_output_at(&linked, ["rev-parse", "--absolute-git-dir"])
                .trim(),
        );
        fs::write(
            git_dir.join("COMMIT_EDITMSG"),
            "ffffffffffffffffffffffffffffffffffffffff not a reference\n",
        )
        .unwrap();

        let (repositories, errors) = inspect_repositories(
            std::slice::from_ref(&repository.main),
            &repository.main,
            std::slice::from_ref(&repository.root),
        );
        let item = repositories[0]
            .worktrees
            .iter()
            .find(|item| item.path == linked)
            .unwrap();

        assert!(errors.is_empty());
        assert!(item.unreferenced_private_commits.is_empty());
        assert_eq!(item.state(), State::Ready);
    }

    #[test]
    fn inspection_keeps_worktrees_outside_requested_roots_out_of_scope() {
        let repository = TestRepository::new("root scope");
        let included = repository.add_worktree("included", "included-topic");
        let _excluded = repository.add_worktree("excluded", "excluded-topic");

        let (repositories, errors) = inspect_repositories(
            std::slice::from_ref(&included),
            &repository.main,
            std::slice::from_ref(&included),
        );

        assert!(errors.is_empty());
        assert_eq!(repositories[0].worktrees.len(), 1);
        assert_eq!(repositories[0].worktrees[0].path, included);
    }

    #[test]
    fn inspection_keeps_skipped_worktrees_out_of_scope_unless_exhaustive() {
        let repository = TestRepository::new("skipped listed worktree");
        fs::create_dir(repository.root.join("target")).unwrap();
        let skipped = repository.add_worktree("target/skipped", "skipped-topic");

        let (repositories, errors) = inspect_repositories(
            std::slice::from_ref(&repository.main),
            &repository.main,
            std::slice::from_ref(&repository.root),
        );
        assert!(errors.is_empty());
        assert!(
            repositories[0]
                .worktrees
                .iter()
                .all(|item| item.path != skipped)
        );

        let (repositories, errors) = inspect_repositories_with_options(
            std::slice::from_ref(&repository.main),
            &repository.main,
            std::slice::from_ref(&repository.root),
            true,
        );
        assert!(errors.is_empty());
        assert!(
            repositories[0]
                .worktrees
                .iter()
                .any(|item| item.path == skipped)
        );
    }

    #[test]
    fn missing_worktree_needs_a_choice() {
        let repository = TestRepository::new("missing choice");
        let linked = repository.add_worktree("missing", "missing-topic");
        fs::remove_dir_all(&linked).unwrap();

        let (repositories, errors) = inspect_repositories(
            std::slice::from_ref(&repository.main),
            &repository.main,
            std::slice::from_ref(&repository.root),
        );
        let item = repositories[0]
            .worktrees
            .iter()
            .find(|item| item.path == linked)
            .unwrap();

        assert!(errors.is_empty());
        assert!(matches!(item.state(), State::NeedsChoice(_)));
        force_remove(item).unwrap();
        assert!(repository.git_success(["show-ref", "--verify", "refs/heads/missing-topic"]));
        let worktrees = repository.git_output(["worktree", "list", "--porcelain"]);
        assert!(!worktrees.contains(linked.to_string_lossy().as_ref()));
    }

    #[test]
    fn preserve_blocks_missing_worktree_with_private_submodule_repository() {
        let repository = TestRepository::new("missing submodule");
        let linked = repository.add_worktree("missing-submodule", "submodule-topic");
        let git_dir = PathBuf::from(
            repository
                .git_output_at(&linked, ["rev-parse", "--absolute-git-dir"])
                .trim(),
        );
        fs::create_dir_all(git_dir.join("modules/private")).unwrap();
        fs::remove_dir_all(&linked).unwrap();

        let (repositories, errors) = inspect_repositories(
            std::slice::from_ref(&repository.main),
            &repository.main,
            std::slice::from_ref(&repository.root),
        );
        let item = repositories[0]
            .worktrees
            .iter()
            .find(|item| item.path == linked)
            .unwrap();

        assert!(errors.is_empty());
        assert!(item.has_initialized_submodules);
        let error = preserve_and_remove(item).unwrap_err();
        assert!(error.message.contains("submodule"));
        assert!(git_dir.exists());
    }

    #[test]
    fn preserve_blocks_missing_worktree_with_operation_state() {
        let repository = TestRepository::new("missing operation");
        let linked = repository.add_worktree("missing-operation", "operation-topic");
        let git_dir = PathBuf::from(
            repository
                .git_output_at(&linked, ["rev-parse", "--absolute-git-dir"])
                .trim(),
        );
        fs::create_dir(git_dir.join("rebase-merge")).unwrap();
        fs::remove_dir_all(&linked).unwrap();

        let (repositories, errors) = inspect_repositories(
            std::slice::from_ref(&repository.main),
            &repository.main,
            std::slice::from_ref(&repository.root),
        );
        let item = repositories[0]
            .worktrees
            .iter()
            .find(|item| item.path == linked)
            .unwrap();

        assert!(errors.is_empty());
        assert_eq!(item.operations, vec!["rebase"]);
        let error = preserve_and_remove(item).unwrap_err();
        assert!(error.message.contains("operation"));
        assert!(git_dir.exists());
    }

    #[test]
    fn preserve_blocks_missing_worktree_with_staged_index_data() {
        let repository = TestRepository::new("missing index data");
        let linked = repository.add_worktree("missing-index", "index-topic");
        let git_dir = PathBuf::from(
            repository
                .git_output_at(&linked, ["rev-parse", "--absolute-git-dir"])
                .trim(),
        );
        fs::write(linked.join("tracked.txt"), "staged change\n").unwrap();
        repository.git_at(&linked, ["add", "tracked.txt"]);
        fs::remove_dir_all(&linked).unwrap();

        let (repositories, errors) = inspect_repositories(
            std::slice::from_ref(&repository.main),
            &repository.main,
            std::slice::from_ref(&repository.root),
        );
        let item = repositories[0]
            .worktrees
            .iter()
            .find(|item| item.path == linked)
            .unwrap();

        assert!(errors.is_empty());
        assert_eq!(item.local_entries, vec!["tracked.txt"]);
        let error = preserve_and_remove(item).unwrap_err();
        assert!(error.message.contains("staged index data"));
        assert!(git_dir.exists());
    }

    #[test]
    fn preserve_blocks_missing_worktree_with_hidden_index_flags() {
        let repository = TestRepository::new("missing hidden index");
        let linked = repository.add_worktree("missing-hidden", "hidden-index-topic");
        let git_dir = PathBuf::from(
            repository
                .git_output_at(&linked, ["rev-parse", "--absolute-git-dir"])
                .trim(),
        );
        repository.git_at(
            &linked,
            ["update-index", "--assume-unchanged", "tracked.txt"],
        );
        fs::remove_dir_all(&linked).unwrap();

        let (repositories, errors) = inspect_repositories(
            std::slice::from_ref(&repository.main),
            &repository.main,
            std::slice::from_ref(&repository.root),
        );
        let item = repositories[0]
            .worktrees
            .iter()
            .find(|item| item.path == linked)
            .unwrap();

        assert!(errors.is_empty());
        assert!(item.has_hidden_index_flags);
        let error = preserve_and_remove(item).unwrap_err();
        assert!(error.message.contains("index flags"));
        assert!(git_dir.exists());
    }

    #[test]
    fn successful_no_op_stash_does_not_claim_a_backup() {
        let repository = TestRepository::new("no-op stash");
        let linked = repository.add_worktree("clean", "clean-topic");

        assert_eq!(stash_ref(&linked).unwrap(), None);
        git(&linked, ["stash", "push", "--all", "--message", "no-op"]).unwrap();
        assert_eq!(stash_ref(&linked).unwrap(), None);
    }

    #[test]
    fn preserve_blocks_unborn_worktree() {
        let repository = TestRepository::new("unborn");
        let linked = repository.root.join("unborn-worktree");
        let output = Command::new("git")
            .arg("-C")
            .arg(&repository.main)
            .args(["worktree", "add", "--orphan"])
            .arg(&linked)
            .output()
            .unwrap();
        assert!(output.status.success());
        let linked = linked.canonicalize().unwrap();
        fs::write(linked.join("local.txt"), "keep me\n").unwrap();

        let (repositories, errors) = inspect_repositories(
            std::slice::from_ref(&repository.main),
            &repository.main,
            std::slice::from_ref(&repository.root),
        );
        let item = repositories[0]
            .worktrees
            .iter()
            .find(|item| item.path == linked)
            .unwrap();

        assert!(errors.is_empty());
        assert!(item.is_unborn());
        let error = preserve_and_remove(item).unwrap_err();
        assert!(error.message.contains("no commit"));
        assert!(linked.join("local.txt").exists());
    }

    #[test]
    fn assume_unchanged_entry_needs_a_choice() {
        let repository = TestRepository::new("assume unchanged");
        let linked = repository.add_worktree("hidden change", "hidden-topic");
        let output = Command::new("git")
            .arg("-C")
            .arg(&linked)
            .args(["update-index", "--assume-unchanged", "tracked.txt"])
            .output()
            .unwrap();
        assert!(output.status.success());
        fs::write(linked.join("tracked.txt"), "hidden change\n").unwrap();

        let (repositories, errors) = inspect_repositories(
            std::slice::from_ref(&repository.main),
            &repository.main,
            std::slice::from_ref(&repository.root),
        );
        let item = repositories[0]
            .worktrees
            .iter()
            .find(|item| item.path == linked)
            .unwrap();

        assert!(errors.is_empty());
        assert!(item.has_hidden_index_flags);
        assert!(matches!(item.state(), State::NeedsChoice(_)));
        let error = preserve_and_remove(item).unwrap_err();
        assert!(error.saved.is_empty());
        assert!(error.message.contains("index flags"));
    }

    #[test]
    fn removes_missing_relative_path_record() {
        let repository = TestRepository::new("relative metadata");
        let linked = repository.add_relative_worktree("relative", "relative-topic");
        fs::remove_dir_all(&linked).unwrap();
        let (repositories, errors) = inspect_repositories(
            std::slice::from_ref(&repository.main),
            &repository.main,
            std::slice::from_ref(&repository.root),
        );
        let item = repositories[0]
            .worktrees
            .iter()
            .find(|item| item.path == linked)
            .unwrap();

        assert!(errors.is_empty());
        force_remove(item).unwrap();
    }

    #[test]
    fn rescues_unreferenced_detached_head_from_missing_worktree() {
        let repository = TestRepository::new("missing detached");
        repository.git(["branch", "chop"]);
        let linked = repository.add_detached_worktree("detached");
        fs::write(linked.join("detached.txt"), "new commit\n").unwrap();
        repository.git_at(&linked, ["add", "detached.txt"]);
        repository.git_at(&linked, ["commit", "-m", "detached commit"]);
        fs::remove_dir_all(&linked).unwrap();
        let (repositories, errors) = inspect_repositories(
            std::slice::from_ref(&repository.main),
            &repository.main,
            std::slice::from_ref(&repository.root),
        );
        let item = repositories[0]
            .worktrees
            .iter()
            .find(|item| item.path == linked)
            .unwrap();

        assert!(errors.is_empty());
        assert!(item.branch.is_none());
        assert!(item.detached_references.is_empty());
        let saved = preserve_and_remove(item).unwrap();
        let branch = saved[0].strip_prefix("branch ").unwrap();
        assert!(branch.starts_with("chop-rescue-"));
        assert!(repository.git_success(["show-ref", "--verify", &format!("refs/heads/{branch}")]));
    }

    struct TestRepository {
        root: PathBuf,
        main: PathBuf,
    }

    impl TestRepository {
        fn new(name: &str) -> Self {
            Self::new_with_ref_format(name, None)
        }

        fn new_reftable(name: &str) -> Self {
            Self::new_with_ref_format(name, Some("reftable"))
        }

        fn new_with_ref_format(name: &str, ref_format: Option<&str>) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "chop-git-test-{}-{unique}-{name}",
                std::process::id()
            ));
            let main = root.join("main repository");
            fs::create_dir_all(&main).unwrap();
            let root = root.canonicalize().unwrap();
            let main = root.join("main repository");
            let repository = Self { root, main };
            let mut init = Command::new("git");
            init.arg("-C").arg(&repository.main).arg("init");
            if let Some(ref_format) = ref_format {
                init.arg(format!("--ref-format={ref_format}"));
            }
            let output = init.arg("--initial-branch=main").output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            repository.git(["config", "user.name", "Chop Test"]);
            repository.git(["config", "user.email", "chop@example.invalid"]);
            fs::write(repository.main.join("tracked.txt"), "base\n").unwrap();
            repository.git(["add", "tracked.txt"]);
            repository.git(["commit", "-m", "initial"]);
            repository
        }

        fn add_worktree(&self, name: &str, branch: &str) -> PathBuf {
            let path = self.root.join(name);
            let output = Command::new("git")
                .arg("-C")
                .arg(&self.main)
                .args(["worktree", "add", "-b", branch])
                .arg(&path)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            path.canonicalize().unwrap()
        }

        fn add_relative_worktree(&self, name: &str, branch: &str) -> PathBuf {
            let path = self.root.join(name);
            let output = Command::new("git")
                .arg("-C")
                .arg(&self.main)
                .args(["worktree", "add", "--relative-paths", "-b", branch])
                .arg(&path)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            path.canonicalize().unwrap()
        }

        fn add_detached_worktree(&self, name: &str) -> PathBuf {
            let path = self.root.join(name);
            let output = Command::new("git")
                .arg("-C")
                .arg(&self.main)
                .args(["worktree", "add", "--detach"])
                .arg(&path)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            path.canonicalize().unwrap()
        }

        fn git_at<const N: usize>(&self, path: &Path, arguments: [&str; N]) {
            let success = Command::new("git")
                .arg("-C")
                .arg(path)
                .args(arguments)
                .status()
                .unwrap()
                .success();
            assert!(success);
        }

        fn git<const N: usize>(&self, arguments: [&str; N]) {
            assert!(self.git_success(arguments));
        }

        fn git_success<const N: usize>(&self, arguments: [&str; N]) -> bool {
            Command::new("git")
                .arg("-C")
                .arg(&self.main)
                .args(arguments)
                .status()
                .unwrap()
                .success()
        }

        fn git_output<const N: usize>(&self, arguments: [&str; N]) -> String {
            self.git_output_at(&self.main, arguments)
        }

        fn git_output_at<const N: usize>(&self, path: &Path, arguments: [&str; N]) -> String {
            let output = Command::new("git")
                .arg("-C")
                .arg(path)
                .args(arguments)
                .output()
                .unwrap();
            assert!(output.status.success());
            String::from_utf8(output.stdout).unwrap()
        }
    }

    impl Drop for TestRepository {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}
