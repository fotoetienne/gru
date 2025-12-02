# Project Manager Skill - Project Management Assistant

A Claude Code skill that provides conversational project management for the Gru project.

## What This Skill Does

The Project Manager skill helps you:
- 📊 Understand project status and progress
- 🎯 Prioritize what to work on next
- ⚡ Understand the critical path
- ⛔ See what's blocking issues
- 🔀 Find parallel work opportunities

## Usage

The skill activates automatically when you ask project management questions. Just ask naturally:

- "How's the project going?"
- "What should I work on?"
- "Show me the critical path"
- "Why is issue #15 blocked?"
- "We have 2 developers - how should we split work?"

## Skill Structure

```
project-manager/
├── SKILL.md      # Skill definition and instructions
├── pm.py         # Python CLI tool for analyzing issues
└── README.md     # This file
```

## How It Works

The skill uses `pm.py` to:
1. Fetch all issues from GitHub (via `gh` CLI)
2. Parse dependencies from issue bodies ("Blocked by: #X")
3. Calculate critical path (longest dependency chain)
4. Identify ready vs blocked issues
5. Provide intelligent recommendations

## Dependencies

- Python 3.7+
- GitHub CLI (`gh`) installed and authenticated
- Issues formatted with: `**Blocked by:** #X, #Y` in body

## Example Conversation

```
You: What's our status?

Claude: [Runs analysis]
        We're at 15% complete (3/20 issues). Phase 1 is done ✅!
        You have 2 issues ready to work on right now.

You: What should I work on?

Claude: I recommend #7 (Integrate workspace management) ⚡
        - It's on the critical path
        - Will unblock #8 and #13 when complete
        - Medium complexity (~3 hours)

        Want to start? Use `/fix 7`
```

## Key Features

### Dependency-Aware
Understands issue relationships and what blocks what.

### Critical Path Analysis
Identifies the minimum sequential work needed (currently 13 issues for Gru).

### Smart Prioritization
Prioritizes by:
1. Critical path issues first (⚡)
2. What unblocks the most work
3. Phase ordering

### Conversational
Natural language interaction - just ask questions!

### Context-Aware
Remembers your conversation and provides personalized suggestions.

## Commands Used by Skill

The skill runs these internally:

```bash
# Overall status
python3 .claude/skills/project-manager/pm.py status

# What's ready to work on
python3 .claude/skills/project-manager/pm.py next

# What's blocked
python3 .claude/skills/project-manager/pm.py blocked

# Critical path
python3 .claude/skills/project-manager/pm.py critical-path

# Dependency graph
python3 .claude/skills/project-manager/pm.py graph
```

You can also run these directly if you prefer CLI access!

## Skill vs Direct CLI

| Use Skill When | Use CLI When |
|----------------|--------------|
| Planning & strategizing | Scripts & automation |
| Want explanations | Quick status checks |
| Need recommendations | Reports |
| Having a conversation | Non-interactive |

## Customization

To modify the skill:

1. Edit `SKILL.md` for behavior changes
2. Edit `pm.py` for analysis logic changes
3. Test by asking project management questions

## Tips

- Ask project management questions naturally
- Use `/fix <number>` to implement issues
- Ask "what's ready?" frequently as you complete work
- The skill celebrates milestone completions! 🎉

## Learn More

- [Skill Documentation](SKILL.md) - Full skill instructions
- [CLI Tool](pm.py) - The underlying Python tool
- [Claude Skills Docs](https://code.claude.com/docs/en/skills.md)

---

Happy project managing! 🚀
