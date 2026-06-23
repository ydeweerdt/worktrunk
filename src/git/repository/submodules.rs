//! Submodule utilities for DWIM branch resolution across worktrees.
//!
//! Provides functions for reading `.gitmodules` from a specific tree,
//! resolving the DWIM branch for a submodule, and gitlink commit lookup.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, bail};

use crate::shell_exec::Cmd;

use super::{Repository, WorkingTree};

/// A parsed submodule record from `.gitmodules`.
#[derive(Debug, Clone)]
pub struct SubmoduleRecord {
    /// The submodule name as it appears in `.gitmodules`.
    pub name: String,
    /// Checkout path relative to the parent worktree root.
    pub path: String,
    /// Clone URL.
    pub url: String,
    /// Optional branch field from `.gitmodules`.
    pub branch: Option<String>,
}

/// The result of DWIM branch resolution for a submodule.
#[derive(Debug, Clone)]
pub enum DwimResult {
    /// Rule 1: local branch exists, just switch to it.
    CheckoutLocal(String),
    /// Rule 2: no local branch, but a local remote-tracking ref exists;
    /// create a local branch tracking the remote ref, then check it out.
    CreateFromRemote(String, String),
    /// Rule 3: no branch exists anywhere; create a new branch from the
    /// parent's gitlink commit.
    CreateFromGitlink(String, String),
}

impl DwimResult {
    /// Get the branch name regardless of which variant.
    pub fn branch_name(&self) -> &str {
        match self {
            DwimResult::CheckoutLocal(b) => b,
            DwimResult::CreateFromRemote(b, _) => b,
            DwimResult::CreateFromGitlink(b, _) => b,
        }
    }
}

/// Read `.gitmodules` from a specific tree in the parent repository.
///
/// Returns an empty vec if the tree has no `.gitmodules` file or if
/// the file contains no submodule entries.
pub fn read_gitmodules(repo: &Repository, treeish: &str) -> anyhow::Result<Vec<SubmoduleRecord>> {
    let raw = match repo.run_command(&["show", &format!("{}:.gitmodules", treeish)]) {
        Ok(s) => s,
        Err(_) => return Ok(vec![]), // No .gitmodules at this tree
    };

    if raw.trim().is_empty() {
        return Ok(vec![]);
    }

    let output = Cmd::new("git")
        .args(["config", "--file", "-", "--list"])
        .stdin_bytes(raw.as_bytes())
        .run()
        .context("Failed to parse .gitmodules")?;

    if !output.status.success() {
        return Ok(vec![]);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_gitmodules_config_list(&stdout))
}

/// Parse `git config --file - --list` output from a `.gitmodules` file.
///
/// Input lines are of the form:
/// ```text
/// submodule.<name>.path=lib/foo
/// submodule.<name>.url=https://...
/// submodule.<name>.branch=main
/// ```
fn parse_gitmodules_config_list(output: &str) -> Vec<SubmoduleRecord> {
    // Collect entries keyed by submodule name
    let mut modules: BTreeMap<String, SubmoduleRecord> = BTreeMap::new();

    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Split on first '='
        let eq_pos = match line.find('=') {
            Some(pos) => pos,
            None => continue,
        };
        let key = &line[..eq_pos];
        let value = &line[eq_pos + 1..];

        // Parse key: submodule.<name>.<field>
        let parts: Vec<&str> = key.splitn(3, '.').collect();
        if parts.len() != 3 || parts[0] != "submodule" {
            continue;
        }
        let name = parts[1];
        let field = parts[2];

        let entry = modules
            .entry(name.to_string())
            .or_insert_with(|| SubmoduleRecord {
                name: name.to_string(),
                path: String::new(),
                url: String::new(),
                branch: None,
            });

        match field {
            "path" => entry.path = value.to_string(),
            "url" => entry.url = value.to_string(),
            "branch" => entry.branch = Some(value.to_string()),
            _ => {}
        }
    }

    modules.into_values().collect()
}

/// Read the gitlink commit for a submodule path from a specific tree.
///
/// The submodule appears as a tree entry with mode `160000` and type `commit`.
pub fn gitlink_commit(repo: &Repository, treeish: &str, sub_path: &str) -> anyhow::Result<String> {
    let output = repo.run_command(&["ls-tree", treeish, "--", sub_path])?;

    let line = output.trim();
    if line.is_empty() {
        bail!(
            "Submodule path '{}' not found in tree '{}'",
            sub_path,
            treeish
        );
    }

    // Format: 160000 commit <sha>\t<path>
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() >= 3 && parts[0] == "160000" && parts[1] == "commit" {
        Ok(parts[2].to_string())
    } else {
        // Treat as regular file/directory, not a submodule
        bail!(
            "Path '{}' is not a submodule in tree '{}'",
            sub_path,
            treeish
        );
    }
}

/// Resolve the DWIM branch for a submodule.
///
/// Applies the three-rule priority against the submodule's **shared** gitdir
/// (`.git/modules/<name>/`), so the main worktree's checkout is never touched:
/// 1. If a local branch `<parent_branch>` exists → checkout as-is.
/// 2. If a local remote-tracking ref `origin/<parent_branch>` exists → create
///    local branch tracking the remote ref.
/// 3. Otherwise → create a new branch from the gitlink commit.
///
/// All checks use local refs only — no network access.
pub fn resolve_dwim(
    repo: &Repository,
    sub_name: &str,
    parent_branch: &str,
    gitlink_hash: &str,
) -> anyhow::Result<DwimResult> {
    let modules_gitdir = repo.git_common_dir().join("modules").join(sub_name);
    let gitdir_str = modules_gitdir.to_string_lossy();

    // Rule 1: local branch exists
    let local_exists = Cmd::new("git")
        .args([
            "--git-dir",
            gitdir_str.as_ref(),
            "rev-parse",
            "--verify",
            &format!("refs/heads/{}", parent_branch),
        ])
        .run()
        .is_ok();

    if local_exists {
        return Ok(DwimResult::CheckoutLocal(parent_branch.to_string()));
    }

    // Rule 2: remote-tracking ref exists locally
    let remote_ref = format!("refs/remotes/origin/{}", parent_branch);
    let remote_exists = Cmd::new("git")
        .args([
            "--git-dir",
            gitdir_str.as_ref(),
            "rev-parse",
            "--verify",
            &remote_ref,
        ])
        .run()
        .is_ok();

    if remote_exists {
        return Ok(DwimResult::CreateFromRemote(
            parent_branch.to_string(),
            format!("origin/{}", parent_branch),
        ));
    }

    // Rule 3: create from gitlink
    Ok(DwimResult::CreateFromGitlink(
        parent_branch.to_string(),
        gitlink_hash.to_string(),
    ))
}

/// Pre-flight check before applying submodule DWIM.
///
/// Verifies the gitlink commit exists in the submodule's object store.
/// The submodule's `.git` file will be created by `git worktree add`
/// in the apply step — we only validate that the object is reachable.
pub fn preflight_check(
    repo: &Repository,
    sub_name: &str,
    sub_path: &str,
    gitlink_hash: &str,
) -> anyhow::Result<()> {
    let modules_gitdir = repo.git_common_dir().join("modules").join(sub_name);
    let gitdir_str = modules_gitdir.to_string_lossy();
    if Cmd::new("git")
        .args([
            "--git-dir",
            gitdir_str.as_ref(),
            "cat-file",
            "-t",
            gitlink_hash,
        ])
        .run()
        .is_err()
    {
        bail!(
            "Cannot create worktree. Submodule '{}' is missing commit '{}' locally. \
             Run 'git fetch --recurse-submodules' first.",
            sub_path,
            gitlink_hash
        );
    }

    Ok(())
}

/// Get the list of initialized submodule names from a worktree.
///
/// Reads `.gitmodules` from the worktree's HEAD and filters to only
/// submodules that are initialized (have a local clone).
pub fn initialized_submodule_names(
    repo: &Repository,
    wt: &WorkingTree<'_>,
) -> anyhow::Result<Vec<String>> {
    // Use the worktree's current HEAD
    let head = wt.run_command(&["rev-parse", "HEAD"])?;
    let treeish = head.trim();

    let records = read_gitmodules(repo, treeish)?;
    let mut result = Vec::new();

    for record in &records {
        let submodule_dir = wt.path().join(&record.path);
        if submodule_dir.join(".git").exists() {
            result.push(record.name.clone());
        }
    }

    Ok(result)
}

/// If the submodule's HEAD currently points to `branch`, detach it so
/// `git worktree add branch` doesn't refuse ("branch is already checked out").
fn detach_if_current(gitdir_str: &str, branch: &str) {
    let current_head = Cmd::new("git")
        .args([
            "--git-dir",
            gitdir_str,
            "symbolic-ref",
            "--short",
            "HEAD",
        ])
        .run()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string());
    if current_head.as_deref() == Some(branch) {
        let _ = Cmd::new("git")
            .args(["--git-dir", gitdir_str, "checkout", "--detach"])
            .run();
    }
}

/// Apply a DWIM result by creating a linked worktree in the submodule.
///
/// Uses `git --git-dir=<modules>/<name> worktree add <path> <branch>` so the
/// main worktree's submodule checkout is never touched — the new parent
/// worktree gets its own submodule worktree linked back to the shared gitdir.
pub fn apply_dwim(
    repo: &Repository,
    sub_name: &str,
    worktree_path: &Path,
    result: &DwimResult,
) -> anyhow::Result<String> {
    let modules_gitdir = repo.git_common_dir().join("modules").join(sub_name);
    let gitdir_str = modules_gitdir.to_string_lossy();
    let wt_path_str = worktree_path.to_string_lossy();

    // The submodule directory was populated as a gitlink tree entry by the
    // parent `git worktree add`. Remove it so `git worktree add` can create a
    // fresh submodule checkout without --force.
    if worktree_path.exists() {
        std::fs::remove_dir_all(worktree_path)
            .with_context(|| format!("Failed to clean submodule directory '{}'", sub_name))?;
    }

    match result {
        DwimResult::CheckoutLocal(branch) => {
            detach_if_current(&gitdir_str, branch);
            Cmd::new("git")
                .args([
                    "--git-dir",
                    gitdir_str.as_ref(),
                    "worktree",
                    "add",
                    wt_path_str.as_ref(),
                    branch,
                ])
                .run()
                .with_context(|| {
                    format!(
                        "Failed to add submodule worktree '{}' for branch '{}'",
                        sub_name, branch
                    )
                })?;
        }
        DwimResult::CreateFromRemote(branch, remote_ref) => {
            Cmd::new("git")
                .args([
                    "--git-dir",
                    gitdir_str.as_ref(),
                    "branch",
                    branch,
                    remote_ref,
                ])
                .run()
                .with_context(|| {
                    format!(
                        "Failed to create branch '{}' in submodule '{}'",
                        branch, sub_name
                    )
                })?;
            detach_if_current(&gitdir_str, branch);
            Cmd::new("git")
                .args([
                    "--git-dir",
                    gitdir_str.as_ref(),
                    "worktree",
                    "add",
                    wt_path_str.as_ref(),
                    branch,
                ])
                .run()
                .with_context(|| {
                    format!(
                        "Failed to add submodule worktree '{}' for branch '{}'",
                        sub_name, branch
                    )
                })?;
        }
        DwimResult::CreateFromGitlink(branch, commit) => {
            Cmd::new("git")
                .args([
                    "--git-dir",
                    gitdir_str.as_ref(),
                    "branch",
                    branch,
                    commit,
                ])
                .run()
                .with_context(|| {
                    format!(
                        "Failed to create branch '{}' in submodule '{}'",
                        branch, sub_name
                    )
                })?;
            detach_if_current(&gitdir_str, branch);
            Cmd::new("git")
                .args([
                    "--git-dir",
                    gitdir_str.as_ref(),
                    "worktree",
                    "add",
                    wt_path_str.as_ref(),
                    branch,
                ])
                .run()
                .with_context(|| {
                    format!(
                        "Failed to add submodule worktree '{}' for branch '{}'",
                        sub_name, branch
                    )
                })?;
        }
    }

    Ok(String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_empty_gitmodules() {
        let records = parse_gitmodules_config_list("");
        assert!(records.is_empty());
    }

    #[test]
    fn test_parse_single_submodule() {
        let input = "\
submodule.auth.path=lib/auth
submodule.auth.url=https://github.com/example/auth.git
submodule.auth.branch=main
";
        let records = parse_gitmodules_config_list(input);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].name, "auth");
        assert_eq!(records[0].path, "lib/auth");
        assert_eq!(records[0].url, "https://github.com/example/auth.git");
        assert_eq!(records[0].branch.as_deref(), Some("main"));
    }

    #[test]
    fn test_parse_multiple_submodules() {
        let input = "\
submodule.front.path=frontend
submodule.front.url=https://github.com/example/front.git
submodule.back.path=backend
submodule.back.url=https://github.com/example/back.git
submodule.back.branch=stable
";
        let records = parse_gitmodules_config_list(input);
        assert_eq!(records.len(), 2);

        let front = records.iter().find(|r| r.name == "front").unwrap();
        assert_eq!(front.path, "frontend");
        assert_eq!(front.branch, None);

        let back = records.iter().find(|r| r.name == "back").unwrap();
        assert_eq!(back.path, "backend");
        assert_eq!(back.branch.as_deref(), Some("stable"));
    }

    #[test]
    fn test_parse_no_branch_field() {
        let input = "\
submodule.foo.path=foo
submodule.foo.url=https://example.com/foo.git
";
        let records = parse_gitmodules_config_list(input);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].branch, None);
    }

    #[test]
    fn test_dwim_branch_name() {
        let r1 = DwimResult::CheckoutLocal("feat".to_string());
        assert_eq!(r1.branch_name(), "feat");

        let r2 = DwimResult::CreateFromRemote("feat".to_string(), "origin/feat".to_string());
        assert_eq!(r2.branch_name(), "feat");

        let r3 = DwimResult::CreateFromGitlink("feat".to_string(), "abc123".to_string());
        assert_eq!(r3.branch_name(), "feat");
    }
}
