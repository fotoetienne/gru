# Git Worktrees Skill

A Claude Code skill that helps manage git worktrees in the context of the Gru project's filesystem structure.

## What This Skill Does

The git-worktrees skill helps you:
- 🌳 Understand Gru's worktree-based filesystem structure
- 📁 Create and manage worktrees for Minion workspaces
- 🧹 Clean up completed or abandoned worktrees
- 🔧 Troubleshoot worktree issues
- 🔄 Recover worktrees after Lab restarts

## Usage

In any Claude Code session in this project:

```
skill: git-worktrees
```

Then ask naturally:
- "How do worktrees work in Gru?"
- "Create a worktree for Minion M42"
- "List all active worktrees"
- "Clean up completed Minions"
- "I have a worktree error, help me fix it"

## Gru's Filesystem Structure

Gru uses git worktrees to provide isolated workspaces for each Minion:

```
~/.gru/
├── repos/owner/repo.git/        # Bare repository (shared)
└── work/owner/repo/
    ├── M42/                     # Minion M42's worktree
    └── M43/                     # Minion M43's worktree
```

**Benefits:**
- **Isolation**: Each Minion has its own working directory
- **Efficiency**: All worktrees share the same git objects
- **Flexibility**: Minions can work on different branches simultaneously
- **Clean separation**: No workspace conflicts between Minions

## Key Concepts

### Bare Repository
- Located at `~/.gru/repos/owner/repo.git/`
- Contains only git objects, no working directory
- Shared by all worktrees for a repository
- All git operations start here

### Worktrees
- Individual working directories at `~/.gru/work/owner/repo/{minion-id}/`
- Each has its own `.git` file pointing to the bare repo
- Each can be on a different branch
- Shares objects with bare repo (space efficient)

### Branch Naming
- Convention: `gru/{minion-id}` or `gru/issue-{number}`
- One branch per Minion
- Branch tracks Minion's work on specific issue

## Common Operations

### Create a Worktree

```bash
cd ~/.gru/repos/owner/repo.git
git worktree add ~/.gru/work/owner/repo/M42 -b gru/M42
```

### List Worktrees

```bash
cd ~/.gru/repos/owner/repo.git
git worktree list
```

### Remove a Worktree

```bash
cd ~/.gru/repos/owner/repo.git
git worktree remove ~/.gru/work/owner/repo/M42
```

### Clean Up Stale Worktrees

```bash
cd ~/.gru/repos/owner/repo.git
git worktree prune
```

## Integration with Gru

### Minion Lifecycle

1. **Creation**: Lab creates bare repo (if needed) and worktree
2. **Active**: Minion works in its worktree on its branch
3. **Completion**: Lab merges work, removes worktree
4. **Cleanup**: Worktree and branch are deleted

### Lab Recovery

After restart, Lab:
1. Enumerates tmux sessions to find active Minions
2. Lists worktrees to match against Minions
3. Prunes any orphaned worktrees
4. Recreates in-memory Minion state

## Troubleshooting

The skill helps debug common issues:

- **Worktree already exists**: Directory conflicts
- **Branch already exists**: Name collision from previous Minion
- **Orphaned worktrees**: Filesystem and git state mismatch
- **Corrupted metadata**: `.git` file issues

## Best Practices

1. ✅ Always create worktrees from the bare repo directory
2. ✅ Use consistent branch naming: `gru/{minion-id}`
3. ✅ Remove worktrees promptly after Minion completion
4. ✅ Lock worktrees for active Minions
5. ✅ Run `git worktree prune` during recovery
6. ❌ Don't manually delete worktree directories

## Example Conversations

### Understanding the Structure

```
You: skill: git-worktrees

Claude: I can help you manage git worktrees in the Gru project!

You: How do worktrees work in Gru?

Claude: Gru uses git worktrees to give each Minion its own isolated workspace:

        ~/.gru/
        ├── repos/owner/repo.git/  ← Bare repo (shared)
        └── work/owner/repo/
            ├── M42/  ← Minion M42's worktree
            └── M43/  ← Minion M43's worktree

        Benefits:
        • Each Minion has its own working directory
        • All worktrees share git objects (space efficient)
        • Minions can work on different branches simultaneously
        • No workspace conflicts!
```

### Creating a Worktree

```
You: Create a worktree for Minion M42 working on issue #7

Claude: Here's how to create a worktree for M42:

        cd ~/.gru/repos/owner/repo.git
        git worktree add ~/.gru/work/owner/repo/M42 -b gru/M42

        This will:
        1. Create worktree at ~/.gru/work/owner/repo/M42
        2. Create and checkout branch gru/M42
        3. Set up .git metadata linking to bare repo

        Verify with:
        git worktree list
```

### Cleanup

```
You: Show me all active worktrees and help clean up completed Minions

Claude: Let me check the worktrees:

        cd ~/.gru/repos/owner/repo.git
        git worktree list

        [Shows output]

        I see worktrees for M40, M41, and M42.

        To remove a completed Minion's worktree:

        git worktree remove ~/.gru/work/owner/repo/M40

        Want me to help clean up stale entries?
```

## When to Use This Skill

| Use Skill For | Use Direct Commands For |
|---------------|-------------------------|
| Understanding structure | Quick status checks |
| Setting up new Minions | Automated scripts |
| Debugging issues | Batch operations |
| Learning git worktrees | Routine maintenance |

## Related Commands

- `/fix <issue>` - Creates Minion with worktree automatically
- `gh issue view <number>` - View issue before creating Minion

## Learn More

- [Skill Instructions](SKILL.md) - Full skill definition
- [Git Worktree Docs](https://git-scm.com/docs/git-worktree)
- [Gru Design](../../docs/DESIGN.md) - Filesystem layout details

---

Master git worktrees for efficient Minion workspace management!
