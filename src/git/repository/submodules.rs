//! Submodule utilities for DWIM branch resolution across worktrees.
//!
//! Provides functions for reading `.gitmodules` from a specific tree,
//! resolving the DWIM branch for a submodule, and gitlink commit lookup.

use std::collections::BTreeMap;

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

/// Resolve the DWIM branch for a submodule within a worktree.
///
/// Applies the three-rule priority:
/// 1. If a local branch `<parent_branch>` exists → checkout as-is.
/// 2. If a local remote-tracking ref `origin/<parent_branch>` exists → create
///    local branch tracking the remote ref.
/// 3. Otherwise → create a new branch from the gitlink commit.
///
/// All checks use local refs only — no network access.
pub fn resolve_dwim(
    wt: &WorkingTree<'_>,
    sub_path: &str,
    parent_branch: &str,
    gitlink_hash: &str,
) -> anyhow::Result<DwimResult> {
    // Rule 1: local branch exists
    let local_exists = wt
        .run_command_in_submodule(
            sub_path,
            &[
                "rev-parse",
                "--verify",
                &format!("refs/heads/{}", parent_branch),
            ],
        )
        .is_ok();

    if local_exists {
        return Ok(DwimResult::CheckoutLocal(parent_branch.to_string()));
    }

    // Rule 2: remote-tracking ref exists locally
    let remote_ref = format!("refs/remotes/origin/{}", parent_branch);
    let remote_exists = wt
        .run_command_in_submodule(sub_path, &["rev-parse", "--verify", &remote_ref])
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
/// Returns an error describing the first blocking condition found.
/// Checks are ordered by likelihood and cost:
/// 1. Uninitialized submodule (repo dir doesn't exist locally)
/// 2. Missing gitlink commit in submodule (for Rule 3 candidates)
/// 3. Branch already active / checkout would fail
/// 4. Directory path conflicts
pub fn preflight_check(
    wt: &WorkingTree<'_>,
    sub_path: &str,
    _parent_branch: &str,
    gitlink_hash: &str,
    allow_init: bool,
) -> anyhow::Result<()> {
    let submodule_dir = wt.path().join(sub_path);

    // Check 1: submodule is initialized
    if !submodule_dir.join(".git").exists() {
        if allow_init {
            wt.run_command(&["submodule", "init", "--", sub_path])
                .with_context(|| {
                    format!("Failed to initialize submodule '{}'", sub_path)
                })?;
        } else {
            bail!(
                "Submodule '{}' is not initialized. Use --init to auto-initialize, \
                 or run 'git submodule init' first.",
                sub_path
            );
        }
    }

    // Check 2: missing gitlink commit (relevant for Rule 3)
    // Fast check: `git cat-file -t <sha>` succeeds if the object exists
    if wt
        .run_command_in_submodule(sub_path, &["cat-file", "-t", gitlink_hash])
        .is_err()
    {
        bail!(
            "Cannot create worktree. Submodule '{}' is missing commit '{}' locally. \
             Run 'git fetch --recurse-submodules' first.",
            sub_path,
            gitlink_hash
        );
    }

    // Check 3: verify the branch switch would work
    // Quick sanity: check if the submodule has a clean working tree
    if wt
        .run_command_in_submodule(sub_path, &["status", "--porcelain"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
    {
        bail!(
            "Submodule '{}' has uncommitted changes. Commit or stash them first.",
            sub_path
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

/// Apply a DWIM result to a submodule within a worktree.
///
/// Runs the appropriate git command in the submodule directory.
/// Returns the previous HEAD commit SHA for rollback purposes.
pub fn apply_dwim(
    wt: &WorkingTree<'_>,
    sub_path: &str,
    result: &DwimResult,
) -> anyhow::Result<String> {
    // Snapshot current HEAD before any changes
    let prev_head = wt
        .run_command_in_submodule(sub_path, &["rev-parse", "HEAD"])
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    match result {
        DwimResult::CheckoutLocal(branch) => {
            wt.run_command_in_submodule(sub_path, &["switch", branch])?;
        }
        DwimResult::CreateFromRemote(branch, remote_ref) => {
            wt.run_command_in_submodule(
                sub_path,
                &["switch", "-c", branch, remote_ref],
            )?;
            // Unset upstream to prevent accidental pushes to the remote branch
            let _ = wt.run_command_in_submodule(
                sub_path,
                &["branch", "--unset-upstream", "--", branch],
            );
        }
        DwimResult::CreateFromGitlink(branch, commit) => {
            wt.run_command_in_submodule(
                sub_path,
                &["switch", "-c", branch, commit],
            )?;
        }
    }

    Ok(prev_head)
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
