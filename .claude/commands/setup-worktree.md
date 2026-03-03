---
description: Create a git worktree for working on a GitHub issue
allowed-tools: Bash(gh:*), Bash(git:*), Read, Glob
argument-hint: "<issue# or URL>"
---

Create a git worktree using Gru's filesystem structure for working on an issue.

**Issue:** $ARGUMENTS

## Steps

### 1. Fetch Issue Details
- Use `gh issue view $ARGUMENTS` to get the issue title and number
- Extract the issue number for naming the branch and worktree

### 2. Determine Repository Info
- Get the repository owner and name from git remote
- Parse from `git remote get-url origin`

### 3. Setup Gru Filesystem Structure
- Ensure bare repo exists at `~/.gru/repos/owner/repo.git/`
  - If not, create it: `git clone --bare <remote-url> ~/.gru/repos/owner/repo.git`
- Create worktree from bare repo:
  ```
  cd ~/.gru/repos/owner/repo.git
  git worktree add ~/.gru/work/owner/repo/issue-<issue#> -b issue-<issue#>
  ```

### 4. Check if Worktree Already Exists
- Look in `~/.gru/work/owner/repo/` for existing worktree for this issue
- If found, inform user and provide path
- If not found, create new worktree

### 5. Next Steps
Inform the user:
```
✓ Worktree created at ~/.gru/work/owner/repo/issue-<issue#>
✓ Branch: issue-<issue#>

Next steps:
  cd ~/.gru/work/owner/repo/issue-<issue#> && gru do <issue#>
```
