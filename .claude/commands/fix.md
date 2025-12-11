---
description: Implement a fix for a GitHub issue (run from worktree)
allowed-tools: Bash(gh:*), Bash(gh pr checks:*), Bash(git:*), Bash(cargo:*), Bash(just:*), Read, Glob, Grep, Edit, Write, Task, TodoWrite, WebFetch
argument-hint: "<issue# or URL>"
---

Implement a fix for a GitHub issue.

**Prerequisites:** Run `/setup-worktree $ARGUMENTS` first, then launch Claude in that directory.

**Issue:** $ARGUMENTS

## When Running Under Gru

This command is being orchestrated by Gru. GitHub operations (claiming issue, creating PR, posting updates) are handled automatically. Focus on:

1. understanding the issue (details provided by Gru)
2. planning the implementation
3. writing clean code
4. testing thoroughly
5. committing your changes

Gru will handle:
- Fetching issue details
- Creating pull requests
- Posting status updates
- Merging and cleanup

**Workflow:**

## 1. Check if Decomposition is Needed
- Assess the issue's complexity:
  - Does it involve multiple distinct components or systems?
  - Does it have multiple acceptance criteria?
  - Would it take more than a few hours to complete?
  - Does it mix different types of work (backend + frontend + docs)?

- **If the issue is complex and should be broken down:**
  - Recommend to the user: "This issue seems complex. Run `/decompose $ARGUMENTS` to break it into smaller sub-issues first."
  - Stop the fix workflow here - wait for user to decompose

- **If the issue is focused and ready to fix:**
  - Proceed to the next step

## 2. Verify Worktree Setup
- Confirm current directory is a git worktree with `git rev-parse --git-dir`
- Check current branch matches expected pattern `minion/issue-<number>-<minion-id>`
- If not in correct worktree, remind user to run `/setup-worktree` first

## 3. Plan the Fix
- Explore the codebase to understand the relevant code
- Create a detailed plan using TodoWrite with specific steps to fix the issue
- Consider tests that need to be added or updated

## 4. Implement the Fix
- Work through each todo item
- Write clean, minimal code changes
- Add or update tests as needed
- Check CLAUDE.md for project-specific build/test commands
- Run tests to verify the fix

## 5. Code Review
- Make a commit with the changes
- Use the Task tool with `subagent_type='code-reviewer'` to perform an autonomous code review
- The code-reviewer agent will analyze the changes for:
  - Code correctness and logic errors
  - Security vulnerabilities
  - Error handling gaps
  - Edge cases
  - Adherence to project conventions (check CLAUDE.md)
  - Test coverage
- Address any issues raised by the code-reviewer before proceeding
- If the review identifies significant problems, iterate on the implementation

## 6. Commit Changes
- Commit the changes with a descriptive message
- Push the branch to the remote repository
- Gru will automatically create a pull request

## 7. Iterate on Feedback
- Look at CI check results
- Address any issues raised by the CI checks
- Read review comments
- Determine which comments require changes. Sometimes reviewers are wrong!
- Make the necessary changes
- For any comments that you've determined don't require changes, acknowledge them
- Make a reply that addresses each comment and includes a summary of the changes made
- Repeat until the PR is ready to merge
