# Git Rebase Skill

An intelligent git rebase assistant that helps rebase your working branch onto the default branch with smart conflict resolution.

## What This Skill Does

The git-rebase skill automates the complex process of rebasing with intelligent conflict resolution:
- 🔄 Fetches the latest default branch from origin
- 🔀 Rebases your current branch onto the default branch
- 🧠 Intelligently resolves merge conflicts using context from PRs and issues
- 📊 Reports conflicts it cannot resolve confidently with detailed analysis
- 💬 Asks to be relaunched with additional context when needed

## Usage

In any conversation in this project:

```
skill: git-rebase
```

The skill will automatically:
1. Check your current branch and git status
2. Fetch the latest default branch
3. Start the rebase process
4. Resolve conflicts intelligently
5. Report success or ask for help with ambiguous conflicts

## When to Use This Skill

Use this skill when:
- 🆕 Your branch is behind the default branch and needs updating
- 🔨 You want to resolve merge conflicts intelligently
- 🤔 You need help understanding what conflicts mean
- 📝 You want conflict resolution guided by PR/issue context
- ⚡ You want to speed up the rebase process

## How It Works

### Automatic Conflict Resolution

The skill resolves conflicts automatically when they are straightforward:

**Examples of auto-resolved conflicts:**
- ✅ Independent changes in different parts of a file
- ✅ Both branches add new imports (merges them)
- ✅ Default branch refactored code structure (adapts your changes)
- ✅ Simple additions that don't conflict logically

### Smart Context Gathering

Before resolving conflicts, the skill examines:
- 📋 **Commit messages** - to understand what changed and why
- 🎫 **GitHub Issue** - to understand the problem being solved
- 🔍 **Pull Request** - to understand the intended solution
- 🔬 **Code diff** - to see the exact changes on both sides

### Asking for Help

When conflicts are ambiguous, the skill pauses and provides:
- Clear explanation of each conflict
- Context from issues/PRs
- Why it's uncertain
- Options for resolution
- Instructions for relaunching with your decision

## Example: Successful Automatic Rebase

```
You: skill: git-rebase

Claude: I'll help you rebase your branch onto main. Let me check the current state...

[Checks branch, fetches latest, starts rebase]

I found 3 conflicts during the rebase. Let me resolve them:

1. ✅ src/utils.py - Both branches added imports, merged them
2. ✅ src/config.py - Default branch refactored config structure, adapted your changes
3. ✅ tests/test_api.py - Independent test additions, kept both

Rebase completed successfully! Your branch is now up to date with main.

Summary:
- 5 commits rebased
- 3 conflicts resolved automatically
- No manual intervention needed
```

## Example: Rebase Requiring User Input

```
You: skill: git-rebase

Claude: I'll help you rebase your branch onto main...

[Starts rebase process]

I successfully rebased most of your changes but encountered 1 conflict that requires your input.

### Conflict Requiring Review

#### src/auth.py:45-47

**The Conflict**: Default branch added admin permission check, but your PR removes permission checks.

**Context from Issue #42**: "Simplify authentication per security team decision"

**Context from PR #45**: "Remove complex permission checks"

**Why I'm Uncertain**: The default branch tightened security after your branch was created,
but your changes explicitly simplify auth per team decision. This is a policy conflict.

**Options**:
- A: Keep the admin check (more restrictive, default branch)
- B: Remove the check (follows your PR requirements)
- C: Compromise - keep admin check but update PR

**Current state**: Rebase is paused at this conflict. The file is ready for you to decide.

Please let me know which option you prefer, then relaunch me to complete the rebase.

---

You: Let's go with option B - the security team decision was made after discussion, so we should remove the permission check as planned.

skill: git-rebase

Claude: Got it! Applying option B - removing the permission check as per your PR requirements.

[Resolves conflict, continues rebase]

Rebase completed successfully!
- Resolved the auth.py conflict per your decision
- Your branch is now up to date with main
- All changes preserved as intended
```

## Conflict Resolution Strategy

The skill uses intelligent heuristics to resolve conflicts:

### Automatically Resolved

| Conflict Type | Resolution Strategy |
|--------------|---------------------|
| **Independent changes** | Keeps both changes in appropriate locations |
| **Import additions** | Merges and sorts all imports |
| **Refactoring on default** | Adapts your code to new structure |
| **Both fix same bug** | Keeps default branch version (already tested) |
| **Config additions** | Merges both sets of config |

### Requires User Input

| Conflict Type | Why It's Uncertain |
|--------------|-------------------|
| **Logic conflicts** | Different approaches to same problem |
| **Security changes** | Policy decisions, not technical |
| **Architectural choices** | Requires understanding of project direction |
| **Deletions** | Both sides deleted different code |
| **Config value conflicts** | Same key, different values |

## Best Practices

### Before Using the Skill

1. **Commit your work**: Make sure your working directory is clean
2. **Know your PR/issue**: Having a PR or issue helps the skill understand context
3. **Trust but verify**: Review the resolved conflicts after completion

### After the Skill Completes

1. **Review the changes**: `git diff origin/main`
2. **Run tests**: Ensure nothing broke
3. **Check the commits**: `git log --oneline`

### If You Need to Abort

If something goes wrong:
```bash
git rebase --abort
```

This returns you to the state before the rebase started.

## Relaunching with Context

If the skill asks for your input on conflicts:

1. Read the conflict summary carefully
2. Choose an option or provide additional context
3. Type `skill: git-rebase` to relaunch
4. Provide your decision in the message

**Example:**
```
skill: git-rebase

For the auth.py conflict, use option B. The security team approved
simplifying auth in yesterday's meeting, so we should proceed with
removing the permission checks.
```

## Integration with Gru Workflow

This skill works great with Gru's worktree-based workflow:

```bash
# After working on an issue in a worktree
cd ~/.gru/work/owner/repo/issue-42

# Rebase onto latest main
skill: git-rebase

# Skill handles the rebase automatically
# Then push and create/update PR
```

## Troubleshooting

### "Uncommitted changes detected"

Commit or stash your changes first:
```bash
git add .
git commit -m "WIP: current progress"
# or
git stash
```

### "Cannot determine default branch"

The skill needs to find the default branch. Make sure:
- You have a remote named `origin`
- The remote is properly configured
- You're in a git repository

### "Too many conflicts"

If there are many complex conflicts, the skill will recommend:
1. Manual review of the conflicts
2. Understanding what changed on default branch
3. Providing explicit guidance for each conflict area

## Tips for Best Results

- ✅ Keep your branches short-lived to minimize conflicts
- ✅ Rebase frequently to stay up to date
- ✅ Write clear commit messages (helps the skill understand intent)
- ✅ Link PRs to issues (provides more context)
- ✅ Add good PR descriptions (explains the "why")

## Limitations

The skill cannot:
- ❌ Resolve conflicts that require deep domain knowledge
- ❌ Make security or architectural policy decisions
- ❌ Understand complex business logic without context
- ❌ Force push automatically (you control when to push)

## Security Notes

The skill will:
- ⚠️ Always pause on security-related conflicts
- ⚠️ Ask for confirmation on permission changes
- ⚠️ Explain the implications of different options
- ⚠️ Never blindly apply security changes

## Related Skills

- **git-worktrees** - Manages worktrees for Gru project
- **project-manager** - Helps understand issue dependencies and priorities

## Learn More

- [SKILL.md](SKILL.md) - Full skill implementation details
- [Git Rebase Documentation](https://git-scm.com/docs/git-rebase)
- [Gru Workflow](../../README.md)

---

Keep your branches up to date effortlessly with intelligent rebase assistance!