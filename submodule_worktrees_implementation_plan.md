# Implementation Plan

**Execution model:** Standard git submodule layout. Submodule checkouts live inside the parent worktree at their registered paths. Operations use `git checkout`/`git switch`/`git branch` in the submodule directory — no extra linked worktrees for submodules.

---

## Phase 0: Foundation — Submodule Utilities

### New file: `src/git/repository/submodules.rs`

**Data structures:**

```rust
pub struct SubmoduleRecord {
    pub name: String,           // from .gitmodules [submodule "name"]
    pub path: String,           // checkout path relative to parent worktree root
    pub url: String,            // clone URL
    pub branch: Option<String>, // optional branch field from .gitmodules
}

pub enum DwimResult {
    /// Rule 1: local branch exists, just switch to it
    CheckoutLocal(String),
    /// Rule 2: no local branch, but local remote-tracking ref exists; create + track
    CreateFromRemote(String, String),
    /// Rule 3: no branch exists anywhere; create from gitlink commit
    CreateFromGitlink(String, String),
}
```

**Key functions:**

| Function | Signature | Implementation |
|----------|-----------|----------------|
| `read_gitmodules()` | `(repo, treeish) -> Vec<SubmoduleRecord>` | `git show <treeish>:.gitmodules` → pipe through `git config --file - --list` to enumerate sections, parse path/url/branch per section |
| `gitlink_commit()` | `(repo, treeish, sub_path) -> String` | `git ls-tree <treeish> <sub_path>` → parse `160000 commit <sha>\t<path>` line |
| `resolve_dwim()` | `(sub_repo_or_cmd, parent_branch, gitlink_hash) -> DwimResult` | 1) `git rev-parse --verify refs/heads/<parent_branch>` → Rule 1. 2) `git rev-parse --verify refs/remotes/origin/<parent_branch>` → Rule 2. 3) Fall through → Rule 3. |
| `preflight_check()` | `(repo, submodule_records, parent_branch, gitlinks, new_wt_path, init_flag) -> Result<()>` | Checks 1-4 from brainstorm. Returns first failure. |

**Pre-flight checks (consolidated):**
1. **Uninitialized submodules** — Check if `.git/modules/<name>` exists. If not, fail (unless `--init`).
2. **Missing submodule commits** — For Rule 3 candidates, `git cat-file -t <gitlink_sha>` in submodule repo.
3. **Branch already active in another worktree** — For the simple checkout model, this check is less relevant (submodules within linked worktrees have isolated metadata). Still, verify `git checkout <branch>` won't fail.
4. **Directory path conflicts** — The submodule path is within the new parent worktree, which was just created. Unlikely unless `--clobber` clobbered submodule dirs.

**Other foundation changes:**
- `src/git/repository/mod.rs` — Add `pub mod submodules;`
- `WorkingTree::run_command_in_submodule(sub_path, args)` — convenience method runs `Cmd::new("git").args(args).current_dir(wt.path.join(sub_path))`
- `CreatedArtifacts` struct — inline in `switch.rs`, tracks parent worktree/branch creation and submodule snapshot/rollback data

---

## Phase 1: `wt switch` / `wt start` — Submodule DWIM Execution

### CLI Flags (`src/cli/mod.rs`, `SwitchArgs`)

Add after existing flags:

```rust
/// Initialize uninitialized submodules before switching
#[arg(long)]
pub(crate) init: bool,

/// Skip submodule branch resolution
#[arg(long = "no-recurse-submodules")]
pub(crate) no_recurse_submodules: bool,
```

### Flow Change (`src/commands/worktree/switch.rs`)

Insert after `execute_switch()` in `SwitchPipeline::run()`:

1. If `--no-recurse-submodules` → skip all submodule work
2. Read `.gitmodules` from the new worktree's HEAD tree
3. Collect `SubmoduleRecord`s. If empty → skip.
4. Run `preflight_check()` — catches issues before any submodule mutation
5. For each initialized submodule (in order):
   a. **Snapshot**: record current commit via `git rev-parse HEAD`
   b. **Resolve**: call `resolve_dwim()` to determine target branch
   c. **Apply**:
      - Rule 1 (local): `git switch <branch>`
      - Rule 2 (DWIM): `git switch -c <branch> <remote>/<branch>`
      - Rule 3 (create): `git switch -c <branch> <gitlink_sha>`
   d. **Track**: push snapshot + result to `CreatedArtifacts`
6. On any failure → **rollback** (reverse order: restore commits, delete new branches, remove parent worktree)

### Rollback

```
For each submodule in reverse order:
  1. git branch -d <branch>  (safe delete, best-effort)
  2. git checkout --force <original_commit>  (restore snapshot)
Then: existing parent worktree removal + branch deletion
```

Errors are collected (not swallowed). Report summary after all attempts.

### after_long_help Update

Add to Switch command docs:

```
## Submodule management

When switching to a worktree with initialized submodules, wt resolves the
matching branch in each submodule using the same DWIM rules as the parent:

1. **Local branch exists** — checks it out as-is. If the commit disagrees
   with the parent's gitlink, the parent shows the submodule as modified.
2. **Remote-tracking branch exists** — creates a local branch tracking the
   remote ref, then checks it out.
3. **Neither exists** — creates a new branch from the gitlink commit.

Use `--no-recurse-submodules` to skip all submodule operations. Use `--init`
to auto-initialize uninitialized submodules.
```

---

## Phase 2: `wt remove` — Submodule Cleanup Enhancement

### What Already Exists

`src/git/repository/worktrees.rs:remove_worktree()` already detects submodules and auto-adds `--force`. No change needed for the basic removal path.

### Metadata Pruning — `src/output/handlers.rs`

After successful worktree removal (in `handle_remove_output()`), if the removed worktree had initialized submodules, run from the primary worktree:

```rust
Cmd::new("git")
    .args(["submodule", "foreach", "--recursive", "git worktree prune"])
    .current_dir(primary_worktree_path)
    .context("submodule worktree prune")
    .run()?;
```

Best-effort: wrap in `let _ = ...` to avoid failing the removal command.

### Submodule Branch Safe Deletion — `src/git/remove.rs`

New function `delete_submodule_branches_if_safe()`:

Called from `delete_branch_if_safe()` or `remove_worktree_with_cleanup()` after the parent branch is deleted:

```rust
pub fn delete_submodule_branches_if_safe(
    repo: &Repository,
    parent_worktree_path: &Path,
    branch_name: &str,
) -> anyhow::Result<()> {
    let wt = repo.worktree_at(parent_worktree_path);
    if !wt.has_initialized_submodules()? { return Ok(()); }
    let submodules = get_submodule_names(repo, &wt)?;
    for sub_name in submodules {
        let result = wt.run_command_in_submodule(
            &sub_name,
            &["branch", "-d", branch_name]
        );
        if let Err(_) = result {
            warning_message(format!(
                "Submodule '{}' branch '{}' is not fully merged. Skipping branch deletion.",
                sub_name, branch_name
            ));
        }
    }
    Ok(())
}
```

Needs `get_submodule_names()` helper that reads `.gitmodules` from the primary worktree's HEAD.

---

## Phase 3: `wt step prune` — Submodule Pruning

### File: `src/commands/step/prune.rs`

### Metadata Pruning

After the main prune loop completes, add a cleanup step:

```rust
if let Some(primary_path) = repo.primary_worktree().map(|w| w.path) {
    let _ = Cmd::new("git")
        .args(["submodule", "foreach", "--recursive", "git worktree prune"])
        .current_dir(&primary_path)
        .run();
}
```

### Submodule Branch Deletion

When prune removes a merged parent branch, hook into `try_remove()` to also attempt safe deletion of the matching branch in submodules via `delete_submodule_branches_if_safe()`.

---

## Phase 4: `wt merge` — Recursive Submodule Merging

### CLI Flag (`src/cli/mod.rs`, `MergeArgs`)

```rust
/// Recurse into submodules during merge
#[arg(long)]
pub(crate) recurse_submodules: bool,
```

### Implementation (`src/commands/merge.rs`)

Insert after the rebase step, before the merge step:

1. **Identify modified submodules** — Compare `git ls-tree <target> <path>` vs `git ls-tree <source> <path>` for each submodule to find ones with different gitlink commits.
2. **Recursive merge** — For each modified submodule (inside the source worktree):
   - `git switch <target_branch>` (usually default branch)
   - `git merge <feature_branch>`
   - **Conflict guard**: If conflict, abort parent merge, report to user
3. **Local push** — `git push .git/modules/<name> HEAD:<target_branch>`
4. **Update gitlink** — Record new submodule commit hash in parent index

---

## Phase 5: `wt push` — Submodule Push Flag

### CLI Flag (`src/cli/step.rs`)

On the `Push` variant of `StepCommand`:

```rust
/// Control submodule push behavior
#[arg(long, value_name = "MODE")]
pub(crate) recurse_submodules: Option<String>,
```

### Implementation (`src/commands/worktree/push.rs:314`)

```rust
let recurse = args.recurse_submodules.as_deref().unwrap_or("no");
ctx.repo.run_command(&[
    "push",
    &format!("--recurse-submodules={}", recurse),
    "--receive-pack=git -c receive.denyCurrentBranch=updateInstead receive-pack",
    git_common_dir_str.as_ref(),
    &push_target,
])?;
```

---

## Files to Create/Modify Summary

| File | Action | Key Content |
|------|--------|-------------|
| `src/git/repository/submodules.rs` | **CREATE** | `SubmoduleRecord`, `DwimResult`, `read_gitmodules()`, `gitlink_commit()`, `resolve_dwim()`, `preflight_check()` |
| `src/git/repository/mod.rs` | EDIT | Add `pub mod submodules;` |
| `src/git/repository/working_tree.rs` | EDIT | Add `run_command_in_submodule()` convenience method |
| `src/cli/mod.rs` | EDIT | Add `--init`, `--no-recurse-submodules` to `SwitchArgs`; `--recurse-submodules` to `MergeArgs`; update `after_long_help` for Switch |
| `src/cli/step.rs` | EDIT | Add `--recurse-submodules` to `Push` step |
| `src/commands/worktree/switch.rs` | EDIT | Submodule DWIM execution + rollback after `execute_switch()` |
| `src/commands/merge.rs` | EDIT | Recursive submodule merge logic |
| `src/commands/worktree/push.rs` | EDIT | Pass `--recurse-submodules` flag value to git push |
| `src/commands/step/prune.rs` | EDIT | Post-prune submodule metadata cleanup |
| `src/git/remove.rs` | EDIT | Add `delete_submodule_branches_if_safe()` |
| `src/output/handlers.rs` | EDIT | Trigger submodule metadata prune after removal |

---

## Test Plan

| Test | Type | What it verifies |
|------|------|-----------------|
| `test_read_gitmodules_from_tree` | Unit | Parses `.gitmodules` correctly from a known treeish |
| `test_resolve_dwim_rule1` | Unit | Existing local branch → `CheckoutLocal` |
| `test_resolve_dwim_rule2` | Unit | Remote-tracking ref exists → `CreateFromRemote` |
| `test_resolve_dwim_rule3` | Unit | Neither exists → `CreateFromGitlink` |
| `test_preflight_missing_commit` | Unit | Pre-flight fails for missing gitlink commit |
| `test_preflight_uninitialized` | Unit | Pre-flight fails for uninitialized (without `--init`) |
| `test_submodule_switch_and_rollback` | Integration | Full switch with submodule + failure rollback |
| `test_remove_submodule_metadata_prune` | Integration | `wt remove` triggers `git submodule foreach git worktree prune` |
| `test_remove_submodule_branch_safe_delete` | Integration | Safe branch deletion for merged submodule branches |
| `test_prune_submodule_cleanup` | Integration | `wt step prune` cleans submodule metadata |
| `test_merge_submodule_conflict_abort` | Integration | Recursive merge conflict aborts parent merge |
| `test_push_recurse_submodules_flag` | Integration | `wt step push --recurse-submodules=on-demand` passes flag |
