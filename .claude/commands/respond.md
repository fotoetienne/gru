---
description: Respond to all comments and reviews on a PR
allowed-tools: Bash(gh:*), Bash(git:*)
argument-hint: "<pr# or URL>"
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
- Ask the user: "Would you like me to post these responses to the PR?"
- If yes, use `gh pr comment $ARGUMENTS -b "response content"` for each response
