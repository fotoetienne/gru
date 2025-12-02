---
description: Review a GitHub pull request and provide feedback
allowed-tools: Bash(gh:*), Bash(git:*), Read, Glob, Grep, Task, Edit
argument-hint: "<pr# or URL>"
---

Review a GitHub pull request: fetch details, analyze changes, and provide feedback.

**Pull Request:** $ARGUMENTS

**Workflow:**

## 1. Fetch PR Details
- Use `gh pr view $ARGUMENTS --json headRefName` to get the branch name
- Use `gh pr view $ARGUMENTS` to get the PR title, body, and metadata
- Use `gh pr view $ARGUMENTS --json files` to get a quick overview of changed files
- **Do NOT use `git checkout`. Always use worktrees:**
  1. Check if a worktree exists: `ls ../worktrees/` for a directory matching the branch
  2. If found, read files from that worktree path
  3. If not found, create one: `git worktree add ../worktrees/<branch-name> <branch-name>`
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
- Summarize what the PR does
- List any issues found (bugs, security concerns, style problems)
- Suggest improvements if applicable
- Give an overall assessment (approve, request changes, or needs discussion)

## 4. Submit Review
- Ask the user: "Would you like me to submit this review as comments on the PR?"
- If yes, use `gh pr review $ARGUMENTS` with:
  - `--comment -b "review content"` for general feedback (default)
  - `--approve -b "review content"` if explicitly approving
  - `--request-changes -b "review content"` if changes are required
- Use a HEREDOC for multi-line review content to preserve formatting
- Note: You cannot use `--request-changes` or `--approve` on your own PR; use `--comment` instead

## 5. Merge (if appropriate)
- If the PR looks ready to merge (CI passing, no blocking issues), ask: "Would you like me to merge this PR?"
- If yes, use `gh pr merge $ARGUMENTS` with appropriate merge strategy (--squash, --merge, or --rebase)
- Note: Do NOT use `--delete-branch` if a local worktree exists for this branch - it will fail. Clean up worktrees manually.
- After merging, run `git pull` on main to sync locally.
