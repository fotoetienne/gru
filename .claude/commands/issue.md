---
description: Create a GitHub issue from description or recent context
allowed-tools: Bash(gh issue:*), Bash(git remote:*), Read, Glob, Grep
argument-hint: "issue description (optional)"
---

Create a GitHub issue using the `gh` CLI.

**Input:** $ARGUMENTS

**Instructions:**
1. If the input above is provided, use it as the basis for the issue
2. If the input is empty, infer an appropriate issue from the recent conversation context (bugs discussed, features requested, problems encountered, etc.)

3. Create a well-structured issue with:
   - A clear, concise title (imperative mood, e.g., "Fix CORS handling" not "CORS is broken")
   - A body with:
     - **Description**: What needs to be done and why
     - **Context**: Relevant background (if applicable)
     - **Code References**: Include `file:line` references if the issue relates to specific code
     - **Acceptance Criteria**: How to know when it's done (if applicable)

4. Infer appropriate labels based on context:
   - `bug` for defects or broken behavior
   - `enhancement` for new features or improvements
   - `documentation` for docs-related issues
   - Use `--label` flag(s) when creating

5. Use `gh issue create` with `--title`, `--body`, and `--label` flags

6. After creating, display the issue URL to the user
