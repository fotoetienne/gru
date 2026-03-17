---
description: Break a large GitHub issue into smaller, actionable sub-issues
allowed-tools: Bash(gh issue:*), Bash(gh api:*), Read, Glob, Grep, Task, TodoWrite, AskUserQuestion, SlashCommand
argument-hint: "<issue# or URL>"
---

Break a complex GitHub issue into smaller, manageable sub-issues.

**Issue:** $ARGUMENTS

**Workflow:**

## 1. Fetch & Analyze the Issue
- Use `gh issue view $ARGUMENTS` to get the full issue details
- Read the issue title, body, labels, and any comments
- Understand the full scope of what needs to be done

## 2. Assess Complexity
Analyze whether the issue should be decomposed:
- Does it involve multiple distinct components or systems?
- Does it have multiple acceptance criteria or requirements?
- Would it take more than a few hours to complete in one go?
- Does it mix different types of work (e.g., backend + frontend + docs)?

If the issue is straightforward and focused, inform the user that decomposition isn't needed.

## 3. Explore the Codebase
- Use the Task tool with `subagent_type='Explore'` to understand:
  - What code/files are affected by this issue
  - Existing architecture and patterns
  - Dependencies between components
- This helps identify natural boundaries for breaking up the work

## 4. Propose Decomposition Plan
- Create a list of 2-5 smaller sub-issues that:
  - Are independently implementable (minimize dependencies)
  - Each have clear, testable outcomes
  - Follow a logical implementation order
- Use TodoWrite to present the proposed breakdown
- Each todo should describe a potential sub-issue with:
  - Clear title (what it does)
  - Brief description (why it's separate)
  - Dependencies (if any)

## 5. Get User Approval
- Use AskUserQuestion to confirm:
  - Should we proceed with decomposition?
  - Does the proposed breakdown make sense?
  - Any adjustments needed?

## 6. Create Sub-Issues
For each approved sub-issue:
- Use `gh issue create` with:
  - Clear, specific title
  - Body that includes:
    - **Description**: What this sub-issue addresses
    - **Parent Issue**: "Part of #<original-issue>"
    - **Blocked by:** #N (links to prerequisite sub-issues if order matters, using the `**Blocked by:** #X, #Y` convention)
    - **Code Areas**: File/module references
  - Labels: Copy from parent + add `subtask` if available
- Capture the new issue numbers

## 7. Set Native GitHub Dependencies
After all sub-issues are created, set native dependencies via the GitHub API for any sub-issue that depends on another:

For each dependency relationship (where issue B is blocked by issue A):
1. Resolve the blocker's internal ID:
   ```bash
   BLOCKER_ID=$(gh api /repos/OWNER/REPO/issues/A_NUMBER --jq .id)
   ```
2. Set the dependency:
   ```bash
   gh api /repos/OWNER/REPO/issues/B_NUMBER/dependencies/blocked_by \
     -f issue_id="$BLOCKER_ID"
   ```

**Graceful degradation:** If the POST returns a 404 (e.g., on GHES without native dependency support), log a warning but continue — the body-text `**Blocked by:**` convention is the universal fallback.

Also set native dependencies between sub-issues and the parent issue's blockers (if any were listed in the parent).

## 8. Update Parent Issue
- Add a comment to the parent issue with:
  - A checklist of the sub-issues created
  - Links to each sub-issue
- Use `gh issue comment $ARGUMENTS --body "..."`
- Example format:
  ```
  This issue has been broken down into smaller tasks:

  - [ ] #101 - [First sub-issue title]
  - [ ] #102 - [Second sub-issue title]
  - [ ] #103 - [Third sub-issue title]

  Each sub-issue can be worked on independently.
  ```

## 9. Summarize
- Report the sub-issues created with their numbers
- Suggest: "Run `/do <first-sub-issue>` to start implementation"
- Note: Don't close the parent issue - it will close automatically when all sub-issues are done

## Notes
- Keep sub-issues focused and atomic
- Each should be completable in a single PR
- Avoid creating too many tiny issues - aim for meaningful chunks
- Consider implementation order (foundation → features → polish)
