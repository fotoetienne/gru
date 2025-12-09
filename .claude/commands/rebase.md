---
description: Rebase current branch onto default branch with intelligent conflict resolution
allowed-tools: Bash(git:*), Bash(gh:*), Read, Write, Edit, Glob, Grep, TodoWrite, Skill
---

Rebase the current working branch onto the default branch (usually `main`) with intelligent conflict resolution.

This command uses the **git-rebase** skill to:
1. Fetch the latest default branch from origin
2. Rebase your current branch onto it
3. Intelligently resolve merge conflicts by examining PR and issue context
4. Report any conflicts that require your input with detailed analysis

## Prerequisites

- You must be on a branch (not the default branch)
- Your working directory should be clean (commit or stash changes first)
- You're in a git repository

## Workflow

Invoke the git-rebase skill to handle the entire rebase process:

```
skill: git-rebase
```

The skill will:
- ✅ Verify your git state
- ✅ Fetch the latest default branch
- ✅ Start the rebase process
- ✅ Resolve straightforward conflicts automatically
- ✅ Pause and ask for input on ambiguous conflicts
- ✅ Provide detailed context from your PR and issue
- ✅ Complete the rebase or report what needs your decision

## What Happens During Rebase

### Automatically Resolved Conflicts

The skill handles these confidently:
- Independent changes in different code sections
- Import additions (merges and sorts them)
- Refactoring on default branch (adapts your code)
- Both branches adding tests/config (merges both)

### Conflicts Requiring Your Input

The skill will pause for:
- Logic conflicts with different approaches
- Security or permission changes
- Architectural decisions
- Configuration value conflicts
- Anything ambiguous or uncertain

When paused, you'll receive:
- Clear explanation of each conflict
- Context from your PR/issue
- Why the skill is uncertain
- Resolution options
- How to relaunch with your decision

## After Rebase Completes

1. **Review the changes**: `git diff origin/main`
2. **Run tests**: Ensure nothing broke
3. **Force push** (if needed): `git push --force-with-lease`

## If Something Goes Wrong

You can always abort the rebase:
```bash
git rebase --abort
```

This returns you to the state before the rebase started.

---

**Ready to rebase?** The git-rebase skill will guide you through the process!
