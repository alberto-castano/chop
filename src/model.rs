use std::path::PathBuf;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorktreeFlags {
    pub bare: bool,
    pub locked: Option<String>,
    pub prunable: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Worktree {
    pub common_dir: PathBuf,
    pub path: PathBuf,
    pub head: String,
    pub branch: Option<String>,
    pub flags: WorktreeFlags,
    pub is_main: bool,
    pub exists: bool,
    pub contains_current_dir: bool,
    pub local_entries: Vec<String>,
    pub operations: Vec<String>,
    pub has_initialized_submodules: bool,
    pub has_nested_repositories: bool,
    pub has_hidden_index_flags: bool,
    pub has_executable_filters: bool,
    pub detached_references: Vec<String>,
    pub unreferenced_private_commits: Vec<String>,
    pub inspection_error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum State {
    Ready,
    Protected(Vec<String>),
    NeedsChoice(Vec<String>),
}

impl Worktree {
    pub fn is_unborn(&self) -> bool {
        !self.head.is_empty() && self.head.bytes().all(|byte| byte == b'0')
    }

    pub fn state(&self) -> State {
        let mut protected = Vec::new();
        if self.is_main {
            protected.push("This is the main worktree. Chop never removes it.".to_owned());
        }
        if self.flags.bare {
            protected.push(
                "This is a bare repository. It has no linked directory to remove.".to_owned(),
            );
        }
        if self.contains_current_dir {
            protected.push("This worktree contains your current directory.".to_owned());
        }
        if !protected.is_empty() {
            return State::Protected(protected);
        }

        let mut reasons = Vec::new();
        if !self.exists {
            reasons.push("The directory is missing. Only Git's record remains.".to_owned());
            if let Some(reason) = &self.flags.prunable {
                reasons.push(format!(
                    "Git says this stale record can be removed: {reason}"
                ));
            }
        }
        if self.is_unborn() {
            reasons.push("This branch has no commit. Git stash cannot preserve it.".to_owned());
        }
        if let Some(reason) = &self.flags.locked {
            let detail = if reason.is_empty() {
                "no reason"
            } else {
                reason
            };
            reasons.push(format!("Git locked this worktree: {detail}."));
        }
        if !self.local_entries.is_empty() {
            let (noun, verb) = if self.local_entries.len() == 1 {
                ("path", "needs")
            } else {
                ("paths", "need")
            };
            reasons.push(format!(
                "{} changed, untracked, or ignored {noun} {verb} a decision.",
                self.local_entries.len(),
            ));
        }
        if !self.operations.is_empty() {
            reasons.push(format!(
                "A Git operation is in progress: {}.",
                self.operations.join(", ")
            ));
        }
        if self.has_initialized_submodules {
            reasons.push("A submodule directory contains data. Review it separately.".to_owned());
        }
        if self.has_nested_repositories {
            reasons.push(
                "This worktree contains a nested Git repository. Review it separately.".to_owned(),
            );
        }
        if self.has_hidden_index_flags {
            reasons.push("Git hides some index changes. Clear the index flags first.".to_owned());
        }
        if self.has_executable_filters {
            reasons.push(
                "Repository filters can run commands. Remove them before preservation.".to_owned(),
            );
        }
        if !self.unreferenced_private_commits.is_empty() {
            reasons.push(format!(
                "{} commit(s) exist only in this worktree's private Git history.",
                self.unreferenced_private_commits.len()
            ));
        }
        if self.branch.is_none() && self.detached_references.is_empty() {
            reasons.push("Detached HEAD has no branch, tag, or remote reference.".to_owned());
        }
        if let Some(error) = &self.inspection_error {
            reasons.push(format!("Chop could not inspect this safely: {error}"));
        }

        if reasons.is_empty() {
            State::Ready
        } else {
            State::NeedsChoice(reasons)
        }
    }
}

#[derive(Clone, Debug)]
pub struct Repository {
    pub common_dir: PathBuf,
    pub worktrees: Vec<Worktree>,
}

#[cfg(test)]
mod tests {
    use super::{State, Worktree, WorktreeFlags};
    use std::path::PathBuf;

    fn worktree() -> Worktree {
        Worktree {
            common_dir: PathBuf::from("/repo/.git"),
            path: PathBuf::from("/worktree"),
            head: "0123456789".to_owned(),
            branch: Some("refs/heads/topic".to_owned()),
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
    fn clean_linked_worktree_is_ready() {
        assert_eq!(worktree().state(), State::Ready);
    }

    #[test]
    fn unborn_worktree_needs_a_choice() {
        let mut item = worktree();
        item.head = "0000000000000000000000000000000000000000".to_owned();

        assert!(item.is_unborn());
        assert!(matches!(item.state(), State::NeedsChoice(_)));
    }

    #[test]
    fn main_worktree_is_always_protected() {
        let mut item = worktree();
        item.is_main = true;
        item.local_entries.push(" M file".to_owned());

        assert_eq!(
            item.state(),
            State::Protected(vec![
                "This is the main worktree. Chop never removes it.".to_owned()
            ])
        );
    }

    #[test]
    fn unreferenced_detached_head_needs_a_choice() {
        let mut item = worktree();
        item.branch = None;

        assert!(matches!(item.state(), State::NeedsChoice(_)));
    }

    #[test]
    fn referenced_detached_head_is_ready() {
        let mut item = worktree();
        item.branch = None;
        item.detached_references.push("refs/tags/saved".to_owned());

        assert_eq!(item.state(), State::Ready);
    }

    #[test]
    fn initialized_submodule_needs_a_choice() {
        let mut item = worktree();
        item.has_initialized_submodules = true;

        assert!(matches!(item.state(), State::NeedsChoice(_)));
    }

    #[test]
    fn hidden_index_flags_need_a_choice() {
        let mut item = worktree();
        item.has_hidden_index_flags = true;

        assert!(matches!(item.state(), State::NeedsChoice(_)));
    }
}
