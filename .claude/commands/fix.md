---
description: Create a branch/worktree for an issue and implement a fix
allowed-tools: Bash(gh:*), Bash(git:*), Read, Glob, Grep, Edit, Write, Task, TodoWrite, WebFetch
argument-hint: "<issue# or URL>"
---

Fix a GitHub issue end-to-end: setup workspace, plan, and implement.

**Issue:** $ARGUMENTS

**Workflow:**

## 1. Fetch Issue Details
- Use `gh issue view $ARGUMENTS` to get the issue title, body, and labels
- Understand what needs to be fixed

## 2. Check if Decomposition is Needed
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

## 3. Create Branch & Worktree
- Derive a branch name from the issue: `fix/<issue#>-<short-description>` (e.g., `fix/42-cors-headers`)
- Check if a worktree already exists in `../worktrees/` for this branch
  - If found, use the existing worktree
  - If not, create a git worktree for isolated work:
    ```
    git worktree add ../worktrees/fix-<issue#>-<desc> -b fix/<issue#>-<desc>
    ```
- Inform the user of the worktree location

## 4. Plan the Fix
- Explore the codebase to understand the relevant code
- Create a detailed plan using TodoWrite with specific steps to fix the issue
- Consider tests that need to be added or updated

## 5. Implement the Fix
- Work through each todo item
- Write clean, minimal code changes
- Add or update tests as needed
- Check CLAUDE.md for project-specific build/test commands
- Run tests to verify the fix

## 6. Code Review
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

## 7. Summarize
- Report what was changed
- If confident, go ahead and commit and create a pull request (PR) for review
- If not confident, ask for feedback or help

## 8. Commit & Push
- Commit the changes with a descriptive message
- Push the branch to the remote repository
- Create a pull request (PR) with "Fixes #<issue>" in the body to auto-close the issue

## 9. Iterate
- Look at CI check results
- Address any issues raised by the CI checks
- Read review comments
- Determine which comments require changes. Sometimes reviewers are wrong!
- Make the necessary changes
- For any comments that you've determined don't require changes, acknowledge them
- make a reply that addresses each comment and includes a summary of the changes made
- Repeat until the PR is ready to merge

## 10. Cleanup
- Once PR has been merged, remove the worktree directory
- **IMPORTANT**: When merging PRs created from worktrees:
  - DO NOT run `gh pr merge` from inside the worktree - it will fail with "fatal: 'main' is already used by worktree"
  - Instead, run the merge command from the main working directory or use `--auto` flag
  - After successful merge, manually clean up: `git worktree remove ../worktrees/fix-<issue#>-<desc>`
- After merging, run `git pull` on main to sync locally.
