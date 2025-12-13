---
description: Review a GitHub pull request and provide feedback
allowed-tools: Bash(gh:*), Bash(gh pr view:*"), Bash(gh pr review:*"), Bash(gh pr checks:*), Bash(git:*), Read, Glob, Grep, Task, Edit
argument-hint: "<pr# or URL>"
---

Review a GitHub pull request: fetch details, analyze changes, and provide feedback.

**Pull Request:** $ARGUMENTS

**Note:** This command assumes you're already in the PR's worktree directory. Use `gru review <pr#>` (with a full GitHub URL) to automatically handle workspace setup.

**Workflow:**

## 1. Fetch PR Details
- Use `gh pr view $ARGUMENTS --json headRefName` to get the branch name
- Use `gh pr view $ARGUMENTS` to get the PR title, body, and metadata
- Use `gh pr view $ARGUMENTS --json files` to get a quick overview of changed files
- **Do NOT use `git checkout`. Always use worktrees:**
  - Determine the repository owner and name from the git remote
  - Derive paths:
    - Bare repo: `~/.gru/repos/owner/repo.git/`
    - Worktree: `~/.gru/work/owner/repo/<branch-name>`
  - **Check if branch is already checked out in a worktree:**
    1. Run `git -C ~/.gru/repos/owner/repo.git worktree list --porcelain` to list all worktrees
    2. Parse output to find if `<branch-name>` is checked out (look for line: `branch refs/heads/<branch-name>`)
    3. If found, extract the worktree path (line starting with `worktree `)
  - **If worktree exists:**
    - Use the existing worktree path
    - Optionally run `git -C <worktree-path> pull` to update it
    - Inform user: "Using existing worktree at <worktree-path>"
  - **If no worktree exists:**
    - Ensure bare repo exists at `~/.gru/repos/owner/repo.git/`
      - If not, create it: `git clone --bare <remote-url> ~/.gru/repos/owner/repo.git`
    - Fetch the PR branch: `git -C ~/.gru/repos/owner/repo.git fetch origin <branch-name>:<branch-name>`
    - Create worktree:
      ```
      git -C ~/.gru/repos/owner/repo.git worktree add ~/.gru/work/owner/repo/<branch-name> <branch-name>
      ```
    - Inform user: "Created new worktree at <worktree-path>"
- Fetch existing review comments: `gh api repos/{owner}/{repo}/pulls/{pr#}/comments`
- Understand the scope and intent of the PR, and any prior discussion
- If this PR is addressing an Issue, read Issue details to understand the problem and context. Does this PR address the issue completely? Are there any missing details or assumptions?

## 2. Analyze the Changes
- Review each file changed in the diff
- For complex changes, use Read to examine full file context around the changes
- Check CLAUDE.md for project-specific conventions and patterns
- Consider:
  - Code correctness and logic
  - Error handling
  - Edge cases
  - Security implications
  - Performance considerations
  - Test coverage

## 3. Provide Feedback
- Summarize what the PR does (Be Concise. Don't give unnecessary praise. Focus on the most important points.)
- List any issues found (bugs, security concerns, style problems)
- Suggest improvements if applicable
- Give an overall assessment (approve, request changes, or needs discussion)

## 4. Submit Review
- BEFORE submitting: Check if this is your own PR:
  - Use `gh pr view $ARGUMENTS --json author --jq '.author.login'` to get the PR author
  - Use `gh api user --jq '.login'` to get your GitHub username
  - If they match, you CANNOT use `--approve` or `--request-changes` (GitHub will reject it)
- Use `gh pr review $ARGUMENTS` with:
  - `--comment -b "review content"` for general feedback (use this for your own PRs)
  - `--approve -b "review content"` if explicitly approving AND it's not your own PR
  - `--request-changes -b "review content"` if changes are required AND it's not your own PR
- Use a HEREDOC for multi-line review content to preserve formatting
- **IMPORTANT**: ALWAYS verify the gh command succeeded by checking its output. If it returns empty or fails, inform the user of the issue

## 5. Merge (if appropriate)
- If the PR looks ready to merge (CI passing, no blocking issues), ask: "Would you like me to merge this PR?"
- If yes, use `gh pr merge $ARGUMENTS` with appropriate merge strategy (--squash, --merge, or --rebase)
- **IMPORTANT**: When merging PRs created from worktrees:
  - DO NOT run `gh pr merge` from inside the worktree - it will fail with "fatal: 'main' is already used by worktree"
  - Instead, run the merge command from the main working directory or use `--auto` flag
  - After successful merge, clean up the worktree from the bare repo:
    ```
    cd ~/.gru/repos/owner/repo.git
    git worktree remove ~/.gru/work/owner/repo/<branch-name>
    ```
  - Optionally delete the branch: `git branch -D <branch-name>`
- After merging, run `git pull` on main to sync locally.
