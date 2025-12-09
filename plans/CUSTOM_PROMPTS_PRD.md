# Feature: Custom Minion Prompts

## Problem Statement
**Who**: Developers using Gru for autonomous coding tasks
**Pain**: Currently must create GitHub issues to start minions. Need flexibility for:
- Ad-hoc tasks that don't warrant issues ("analyze this error", "explain this code")
- Reusable workflows without copy-pasting prompts
- Organized agent workspace instead of raw Claude CLI chaos
- Easy switching between multiple active minions
- Notifications when minions need attention

**Impact**: Without this, Gru is limited to issue-driven workflows. Users fall back to raw `claude` CLI for one-off tasks, losing the benefits of organized session management, Tower UI, and minion persistence.

## Proposed Solution
Extend Gru CLI to support flexible prompt execution:
1. **Ad-hoc prompts**: `gru "explain this error"` starts a persistent session
2. **Reusable prompts**: Files in `.gru/prompts/*.md` define templated instructions
3. **Built-in prompts**: `fix`, `review`, `rebase` are standard prompts (overridable by repo/personal)
4. **Unified minion system**: All minions (issue-driven or ad-hoc) get IDs, appear in `gru status`, support `gru attach`
5. **Customization via override**: Teams/individuals can override built-in prompts with custom versions

## User Stories

1. As a developer, I want to start an agent session with a quick prompt so that I can get help without creating a GitHub issue
2. As a developer, I want to define reusable prompts in `.gru/prompts/` so that my team can standardize agent workflows
3. As a developer, I want all my minions to appear in `gru status` so that I can see what's running and switch between them
4. As a developer, I want prompts to accept parameters (`--issue`, `--param`) so that I can customize behavior per invocation
5. As a developer, I want to discover available prompts with `gru prompts` so that I know what's available
6. As a team lead, I want to check reusable prompts into version control so that the team shares consistent workflows
7. As a developer, I want to override built-in prompts (like `fix`) with custom versions so that I can adapt Gru's behavior to my team's workflow

## Core Principles Check
✅ **Local-first**: Prompts are local files (`.gru/prompts/`), no cloud dependency
✅ **One binary**: Just extending `gru` CLI with flexible prompt resolution
✅ **GitHub as state**: Prompts can work with/without issues; worktrees auto-created when needed
✅ **Stateless Tower**: Minions run in Lab, Tower just proxies attach sessions

## Design Details

### Command Syntax

```bash
# Ad-hoc prompt (quoted = literal)
gru "analyze the error in src/main.rs"

# Built-in prompts (unquoted = prompt lookup)
gru fix --issue 123
gru review --pr 456
gru rebase

# Custom prompts from .gru/prompts/
gru analyze-deps
gru refactor-auth --param module=auth

# List available prompts
gru prompts

# Get help for specific prompt
gru fix --help
```

**Resolution order**:
1. **Reserved system commands** (hard-coded, cannot be overridden):
   - `status`, `attach`, `stop`, `lab`, `tower`, `up`, `prompts`, `help`, `version`
2. **If argument is quoted** → treat as literal ad-hoc prompt
3. **If unquoted** → look up prompt file (first match wins):
   - Repo-specific: `.gru/prompts/<name>.md`
   - Built-in prompts: `fix`, `review`, `rebase`
   - Global: `~/.gru/prompts/<name>.md`
   - Error if not found

**Override behavior**: Repo prompts can override built-ins (e.g., `.gru/prompts/fix.md` replaces default `fix`), enabling team/personal customization.

### Customization via Override

Teams and individuals can customize Gru's behavior by overriding built-in prompts:

**Example: Team-specific fix workflow**
```bash
# Default behavior - uses built-in fix
gru fix --issue 123

# Create custom fix in repo
cat > .gru/prompts/fix.md <<'EOF'
---
description: Team-specific fix with security checks
requires: [issue]
---
You are fixing issue #{{ issue_number }}: {{ issue_title }}

IMPORTANT: Our team requires:
1. Security review before implementation
2. Performance impact analysis
3. Tests with >90% coverage
4. Documentation updates

{{ issue_body }}
EOF

# Now `gru fix` uses team version automatically
gru fix --issue 123  # Uses .gru/prompts/fix.md

# Commit to repo so team shares this workflow
git add .gru/prompts/fix.md
git commit -m "Add team-specific fix workflow"
```

This enables:
- **Team standards**: Check in `.gru/prompts/` to enforce workflow conventions
- **Personal preferences**: Use `~/.gru/prompts/` for your own defaults across all repos
- **Zero configuration**: Overrides work automatically, no flags needed

### Prompt File Format

**Simple prompt** (`.gru/prompts/analyze-deps.md`):
```markdown
---
description: Analyze dependency graph and identify issues
---
Analyze the dependency graph for this repository and identify:
- Unused dependencies
- Version conflicts
- Security vulnerabilities

Provide actionable recommendations.
```

**Templated prompt** (`.gru/prompts/fix.md`):
```markdown
---
description: Fix a GitHub issue with tests and PR
requires: [issue]
params:
  - name: target
    description: Specific file or module to focus on
    required: false
---
You are fixing issue #{{ issue_number }}: {{ issue_title }}

Worktree: {{ worktree_path }}
Branch: {{ branch_name }}

Issue description:
{{ issue_body }}

Please:
1. Read the issue and understand the requirements
2. Implement a fix with tests
3. Create a PR targeting {{ base_branch }}

{{#target}}
Focus specifically on: {{ target }}
{{/target}}
```

### Standard Template Variables

```rust
struct PromptContext {
    // GitHub context (when --issue or --pr provided)
    issue_number: Option<u64>,
    issue_title: Option<String>,
    issue_body: Option<String>,
    pr_number: Option<u64>,
    pr_title: Option<String>,

    // Git context
    worktree_path: PathBuf,
    branch_name: String,
    base_branch: String,
    repo_owner: String,
    repo_name: String,

    // Environment
    cwd: PathBuf,

    // Custom params from --param key=value
    params: HashMap<String, String>,
}
```

### Worktree Behavior (Smart Defaults)

| Command | Worktree Behavior |
|---------|-------------------|
| `gru "analyze"` | Runs in CWD (no worktree) |
| `gru fix --issue 123` | Auto-creates worktree for issue |
| `gru review --pr 456` | Uses existing PR worktree or CWD |
| `gru analyze --no-worktree` | Force CWD even if issue context |
| `gru refactor --worktree /path` | Explicit worktree path |

### Minion Lifecycle (Always Persistent)

All minions are persistent by default:
- Get unique IDs (M42, M43, etc.)
- Appear in `gru status`
- Attachable via `gru attach M42` or Tower UI
- Run until explicitly stopped: `gru minion stop M42`
- Survive Lab restarts (session IDs persisted)

```bash
$ gru status

ACTIVE MINIONS:
  M42  fix --issue 123           [waiting for review]  2h ago
  M43  "analyze auth module"     [running]             5m ago
  M44  review --pr 456           [blocked: CI failed]  30m ago
```

### Prompt Discovery

```bash
$ gru prompts

BUILT-IN PROMPTS:
  fix       Fix a GitHub issue with tests and PR
  review    Review and respond to PR comments
  rebase    Rebase branch with intelligent conflict resolution

CUSTOM PROMPTS (.gru/prompts/):
  analyze-deps     Analyze dependency graph and versions
  refactor-auth    Refactor authentication module
  fix              [OVERRIDES BUILT-IN] Team-customized fix workflow

GLOBAL PROMPTS (~/.gru/prompts/):
  explain          Explain complex code sections

$ gru fix --help
Prompt: fix
Fix a GitHub issue with tests and PR

Required parameters:
  --issue <number>    GitHub issue number to fix

Optional parameters:
  --param target=<path>    Specific file/module to focus on

Template location: .gru/prompts/fix.md (overrides built-in)
```

## MVP Scope

### Phase 1: Ad-hoc Prompts (Core)
- [ ] `gru "<prompt>"` starts persistent minion in CWD
- [ ] Auto-generated minion IDs (M42, M43, etc.)
- [ ] Appears in `gru status` alongside issue-driven minions
- [ ] `gru attach <id>` works for ad-hoc minions
- [ ] `gru stop <id>` terminates minion
- [ ] Session persistence (survives Lab restart)
- [ ] Reserved system commands list (status, attach, stop, etc.)

### Phase 2: Prompt Files
- [ ] Load prompts from `.gru/prompts/*.md`
- [ ] Simple variable substitution (`{{ variable }}`)
- [ ] Frontmatter for metadata (description, requires, params)
- [ ] `gru prompts` shows available prompts with override indicators
- [ ] `gru <prompt-name> --help` shows prompt help
- [ ] Resolution order: system → repo → built-in → global

### Phase 3: Context & Parameters
- [ ] `--issue <number>` flag populates issue context
- [ ] `--pr <number>` flag populates PR context
- [ ] `--param key=value` for custom parameters
- [ ] Auto-create worktree when `--issue` provided
- [ ] `--no-worktree` flag forces CWD
- [ ] `--worktree <path>` for explicit worktree
- [ ] Validate required parameters from frontmatter

### Phase 4: Built-in Prompts as Templates
- [ ] Implement `fix`, `review`, `rebase` as built-in prompts
- [ ] Allow overriding built-ins with `.gru/prompts/fix.md`
- [ ] Show override status in `gru prompts` output
- [ ] `gru <prompt> --help` shows which version is active (built-in vs override)

### Out of Scope (Future)
- [ ] Full Mustache logic (conditionals, loops)
- [ ] Interactive prompt picker TUI
- [ ] Prompt templates from remote URLs
- [ ] Minion-to-minion handoffs
- [ ] Scheduled/cron minions
- [ ] Prompt marketplace or sharing

## Success Metrics

- **Adoption**: % of minions started via custom prompts vs issues (target: 30%+)
- **Reusability**: Avg # times each custom prompt is used (target: 5+)
- **Session management**: % of users with >3 concurrent minions (target: 40%+)
- **Team adoption**: % of repos with checked-in `.gru/prompts/` (target: 60%+)

## Open Questions

1. **Minion ID persistence**: Do IDs persist across Lab restarts or reset? → Persist (store in `~/.gru/state/minions.json`)
2. **Tower UI**: How do ad-hoc minions appear differently from issue-driven ones? → Same UI, just show prompt text instead of issue title
3. **Cleanup**: Should stopped minions auto-archive after N days? → Future enhancement
4. **Reserved command conflicts**: What happens if someone tries to name a prompt `status` or `attach`? → Error with helpful message listing reserved names

## Acceptance Criteria

**Given** I have no GitHub issue,
**When** I run `gru "explain src/main.rs"`,
**Then** a persistent minion starts in CWD with a unique ID

**Given** I create `.gru/prompts/analyze.md`,
**When** I run `gru analyze`,
**Then** the prompt file is loaded and rendered

**Given** a prompt declares `requires: [issue]`,
**When** I run it without `--issue`,
**Then** I see an error: "Prompt 'fix' requires --issue <number>"

**Given** I run `gru fix --issue 123`,
**When** the command executes,
**Then** a worktree is auto-created at `~/.gru/work/owner/repo/gru/issue-123/`

**Given** multiple minions are running,
**When** I run `gru status`,
**Then** I see all minions (ad-hoc and issue-driven) with IDs and status

**Given** a minion is running,
**When** I run `gru attach M42` or click in Tower UI,
**Then** I see the minion's terminal output and can interact

**Given** I create `.gru/prompts/fix.md` to override the built-in,
**When** I run `gru fix --issue 123`,
**Then** the repo-specific version is used instead of the built-in

**Given** I run `gru prompts`,
**When** a custom prompt overrides a built-in,
**Then** I see `[OVERRIDES BUILT-IN]` indicator next to it

---

## Next Steps

Ready to break this into GitHub issues? Or do you want to refine anything first?
