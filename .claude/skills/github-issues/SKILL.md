---
name: github-issues
description: "Manages GitHub issues: creating, editing, setting dependencies (blocked-by), bulk filing, and labeling. ALWAYS activate when creating GitHub issues, filing issues in bulk, setting issue dependencies, or adding blocked-by relationships. Also activate when using gh issue or gh api for issue management."
allowed-tools: [Bash(gh issue:*), Bash(gh api:*), Read, Glob, Grep]
---

You are an expert at managing GitHub issues for the Gru project. Follow these conventions exactly.

## Creating Issues

Use `gh issue create` with a heredoc body:

```bash
gh issue create --title "Clear, specific title" --body "$(cat <<'EOF'
## Description
What this issue addresses.

## Acceptance Criteria
- [ ] Criterion 1
- [ ] Criterion 2

**Blocked by:** #X, #Y
EOF
)"
```

- Keep titles concise and actionable
- Include acceptance criteria when possible
- Add `**Blocked by:** #X, #Y` in the body if the issue has dependencies (see Dependencies below)
- Add labels with `--label "gru:todo"` or other appropriate labels

## Dependencies (CRITICAL)

Gru uses a **two-layer dependency system**. Always set dependencies **both** ways:

### Layer 1: Body text (ALWAYS do this — works everywhere including GHES)

Include `**Blocked by:** #X, #Y` in the issue body. This must be:
- On a single line (only the first line after the marker is parsed)
- Using `#` prefix for issue numbers (bare numbers are ignored)
- Same-repo only (cross-repo references like `owner/repo#123` are skipped)

When editing an existing issue to add blockers, prepend the line to the body:

```bash
# Get current body, prepend blocked-by line
CURRENT_BODY=$(gh issue view 123 --json body --jq .body)
gh issue edit 123 --body "**Blocked by:** #10, #20

$CURRENT_BODY"
```

### Layer 2: Native GitHub Dependencies REST API (always attempt after body text)

The native API requires the blocker's internal `id` (not the issue number):

```bash
# 1. Get the internal id of the blocking issue
BLOCKER_ID=$(gh api /repos/OWNER/REPO/issues/BLOCKER_NUMBER --jq .id)

# 2. Set the dependency: BLOCKED_ISSUE is blocked by BLOCKER
gh api /repos/OWNER/REPO/issues/BLOCKED_NUMBER/dependencies/blocked_by \
  -f issue_id="$BLOCKER_ID"
```

### Error handling for native API

After each `gh api` POST, check the exit status:
- **404 / "Not Found"**: GHES without native dependency support — log a warning and continue. Body-text `**Blocked by:**` is the universal fallback.
- **Other errors** (422, 500, etc.): Surface the error to the user so dependency setting doesn't silently fail.

### ⚠️ NEVER use sub-issues API for dependencies

**NEVER use `addSubIssue` or the sub-issues GraphQL mutation to set dependencies.** Sub-issues represent parent/child decomposition, NOT blocked-by relationships. Gru's dependency system (`src/dependencies.rs`) only reads:
1. The native dependencies REST API (`/issues/{number}/dependencies/blocked_by`)
2. Body-text `**Blocked by:** #X, #Y`

Using `addSubIssue` will create a parent-child relationship that Gru **cannot see** as a dependency.

### Full example: create an issue blocked by #42

```bash
# Get repo info
OWNER="fotoetienne"
REPO="gru"

# Create the issue with body-text deps
NEW_URL=$(gh issue create --title "Implement feature Y" --body "$(cat <<'EOF'
## Description
Feature Y builds on the foundation from #42.

**Blocked by:** #42

## Acceptance Criteria
- [ ] Feature Y works
EOF
)")

# Extract issue number from URL
NEW_NUMBER=$(echo "$NEW_URL" | grep -o '[0-9]*$')

# Set native dependency
BLOCKER_ID=$(gh api /repos/$OWNER/$REPO/issues/42 --jq .id)
gh api /repos/$OWNER/$REPO/issues/$NEW_NUMBER/dependencies/blocked_by \
  -f issue_id="$BLOCKER_ID"
```

## Bulk Filing

When creating multiple related issues:

1. Create all issues first, capturing their numbers
2. Then set dependencies between them in a second pass
3. Use parallel execution where possible for speed

```bash
# Create issues, capture numbers
URL1=$(gh issue create --title "Step 1: Foundation" --body "...")
URL2=$(gh issue create --title "Step 2: Build on step 1" --body $'**Blocked by:** #STEP1\n...')
URL3=$(gh issue create --title "Step 3: Build on step 2" --body $'**Blocked by:** #STEP2\n...')

# Extract numbers
N1=$(echo "$URL1" | grep -o '[0-9]*$')
N2=$(echo "$URL2" | grep -o '[0-9]*$')
N3=$(echo "$URL3" | grep -o '[0-9]*$')

# Set native dependencies
ID1=$(gh api /repos/OWNER/REPO/issues/$N1 --jq .id)
gh api /repos/OWNER/REPO/issues/$N2/dependencies/blocked_by -f issue_id="$ID1"

ID2=$(gh api /repos/OWNER/REPO/issues/$N2 --jq .id)
gh api /repos/OWNER/REPO/issues/$N3/dependencies/blocked_by -f issue_id="$ID2"
```

## Labels

Gru uses these labels (defined in `src/labels.rs`):

### Issue lifecycle
| Label | Purpose |
|---|---|
| `gru:todo` | Issue ready for a Minion to claim |
| `gru:in-progress` | Minion actively working on issue |
| `gru:done` | Minion completed successfully |
| `gru:failed` | Minion encountered failure |
| `gru:blocked` | Needs human intervention |

### PR labels
| Label | Purpose |
|---|---|
| `gru:ready-to-merge` | All merge-readiness checks pass |
| `gru:auto-merge` | Auto-merge when checks pass |
| `gru:needs-human-review` | LLM judge escalated for human review |

Apply labels when creating issues:
```bash
gh issue create --title "..." --body "..." --label "gru:todo"
```

## Editing Issues

```bash
# Edit title
gh issue edit 123 --title "New title"

# Edit body
gh issue edit 123 --body "New body"

# Add labels
gh issue edit 123 --add-label "gru:todo"

# Remove labels
gh issue edit 123 --remove-label "gru:failed"
```
