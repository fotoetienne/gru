---
description: Respond to all comments and reviews on a PR
allowed-tools: Bash(gh:*), Bash(gh pr checks:*), Bash(git:*), Bash(just:*), Read, Glob, Grep, Edit, Write, Task, TodoWrite, WebFetch
---

Respond to all comments and reviews on a GitHub pull request.

**Pull Request:** $ARGUMENTS

**Workflow:**

## 1. Fetch Comments and Reviews
- Use `gh pr view $ARGUMENTS` to get PR details
- Use `gh api repos/{owner}/{repo}/pulls/{pr#}/comments` to fetch all review comments
- Use `gh api repos/{owner}/{repo}/issues/{pr#}/comments` to fetch all issue comments

## 2. Review All Feedback
- Read through all comments and reviews
- Identify questions, concerns, or suggestions that need responses
- Group related comments together

## 3. Draft Responses
- For each comment or review, draft a thoughtful response
- Address concerns directly
- Provide clarifications where needed
- Acknowledge suggestions and indicate if/how they'll be addressed

## 4. Post Responses
- Use `gh pr comment $ARGUMENTS -b "response content"` for each response

## 5. Update Code
- For each response, update the code accordingly
- Commit changes with a descriptive message prefixed with your Minion ID (from the branch name), e.g. `[M042] Address review feedback`
- Push changes to the branch

## 6. Post follow-up
- After commiting updates, use `gh pr comment $ARGUMENTS -b "update content"` to post a follow-up comment
