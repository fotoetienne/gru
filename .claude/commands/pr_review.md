---
description: Review a GitHub pull request and provide feedback
allowed-tools: Bash(gh:*), Bash(gh pr view:*"), Bash(gh pr review:*"), Bash(gh pr checks:*), Bash(git:*), Read, Glob, Grep, Task, Edit
argument-hint: "<pr# or URL>"
---

Review a GitHub pull request: fetch details, analyze changes, and provide feedback.

**Pull Request:** $ARGUMENTS

**Workflow:**

## 1. Fetch PR Details
- Use `gh pr view $ARGUMENTS --json headRefName` to get the branch name
- Use `gh pr view $ARGUMENTS` to get the PR title, body, and metadata
- Use `gh pr view $ARGUMENTS --json files` to get a quick overview of changed files
- Fetch existing review comments: `gh api repos/{owner}/{repo}/pulls/{pr#}/comments`
- Understand the scope and intent of the PR, and any prior discussion
- The code should already be checked out in the current directory
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
