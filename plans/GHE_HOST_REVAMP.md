# Plan: Generic GitHub Enterprise Host Configuration

## Context

`gru init` fails for Netflix GHE repos because octocrab can't authenticate — the GHE API requires mTLS (client certs) which only the `gh` CLI handles transparently. The codebase also has Netflix-specific heuristics hardcoded (`infer_github_host` checks for "netflix" substring, `gh_command_for_host` returns `"ghe"` binary).

**Key discovery:** Netflix GHE has two relevant hostnames:
- `git.netflix.net` — git proxy and API (port 7004). `GH_HOST=git.netflix.net` works for all `gh` CLI operations. This is the host in git remote URLs.
- `github.netflix.net` — web UI only. Links for humans (PR URLs, issue URLs) should use this host.

The existing `~/prj/ghe-cli` wrapper uses `GH_HOST=github.netflix.net`, but testing confirmed `GH_HOST=git.netflix.net` also works for all `gh` CLI operations (auth, issues, PRs, labels). The plan uses `git.netflix.net` as `host` since it matches git remotes, with `web_url` pointing to `github.netflix.net` for human-readable links.

**Important:** `GH_HOST=git.netflix.net` works because the Netflix `gh` fork handles metatron/mTLS auth transparently. Other GHE installations may require their own `gh` forks or auth plugins. Gru treats `gh` as a black box — it just sets `GH_HOST` and expects `gh` to handle authentication. No GHE-vendor-specific code should exist in gru.

**Goal:** Remove octocrab entirely. Use `gh` CLI for all GitHub operations. Add config-driven host mapping. Remove all Netflix-specific code.

## Config Design

```toml
# ~/.gru/config.toml

[github_hosts.netflix]
host = "git.netflix.net"              # For GH_HOST, gh --hostname, git remotes
web_url = "https://github.netflix.net"  # Optional: web UI URL (for links in comments/output)

[daemon]
repos = ["netflix:corp/ste-slackdgs"]   # "netflix:" references [github_hosts.netflix]
```

**Field semantics:**
- `host` (required) — The hostname for everything: `GH_HOST` env var, `gh --hostname`, and matching git remote URLs.
- `web_url` (optional) — Where web links point (PR URLs in output, progress comments). Defaults to `https://{host}`. Only needed when the web UI is on a different domain than the git/API host.

**Repo entry formats:**
- `"owner/repo"` → github.com (unchanged, zero config needed)
- `"netflix:owner/repo"` → looks up `[github_hosts.netflix]` section
- Legacy `"host.com/owner/repo"` → host.com for everything (backwards compat)

## Implementation

### Phase 1: Config layer (`src/config.rs`)

1. **Add `GhHostConfig` struct:**
   ```rust
   #[derive(Debug, Clone, Serialize, Deserialize)]
   pub struct GhHostConfig {
       pub host: String,
       pub web_url: Option<String>,  // defaults to https://{host}
   }
   ```

2. **Add to `LabConfig`:**
   ```rust
   #[serde(default)]
   pub github_hosts: HashMap<String, GhHostConfig>,
   ```

3. **Add `HostRegistry`** — built from config, provides lookups:
   - `from_config(config: &LabConfig) -> Self` — includes hosts from `[github_hosts.*]` sections AND `daemon.repos` legacy entries
   - `host_for_name(name: &str) -> Option<&str>` — resolve config name (e.g. "netflix") to its host
   - `all_hosts() -> Vec<&str>` — for matching git remote URLs (includes github.com always)
   - `web_url_for(host: &str) -> String` — for building web links
   - Always includes implicit github.com entry

4. **Update `parse_repo_entry`** to support `"name:owner/repo"` format. Needs access to the `github_hosts` config map to resolve the name to a host. Return `RepoEntry { host, owner, repo }`.

5. **Replace `load_github_hosts() -> Vec<String>`** with `load_host_registry() -> HostRegistry`

6. **Update validation** — `"netflix:owner/repo"` requires `[github_hosts.netflix]` to exist. Add check in `validate()` that all config name references in `daemon.repos` have a corresponding `[github_hosts.*]` entry. Reject empty prefix (`:owner/repo`).

7. **Update `default_config_toml`** template with commented example

8. **Add `gh` availability check** — verify `gh` binary is on PATH, called early in `gru init` and `gru lab` startup with a clear error message if missing. If a GHE host is configured and `gh auth status --hostname <host>` fails, surface the error clearly (the user's `gh` binary may not support their GHE's auth method).

### Phase 2: CLI replacements for octocrab (`src/github.rs`)

**New CLI functions** (all take `host: &str`, use `gh_cli_command(host)`):

| Octocrab method | CLI replacement |
|------|-------------|
| `post_comment(owner, repo, issue, body)` | `gh issue comment <num> --repo o/r --body <body>` |
| `add_label + remove_label` | `gh issue edit <num> --repo o/r --add-label x --remove-label y` (combine in single call) |
| `create_label(owner, repo, name, color, desc)` | `gh label create <name> --repo o/r --color <color> -d <desc> --force` |
| `get_authenticated_user()` | `gh auth status --hostname <host>` (exit code check) |
| `get_issue()` | Already exists: `get_issue_via_cli()` |
| `get_pr()` | Already exists: `get_pr_via_cli()` |
| `list_issues_with_label()` | Already exists: `list_ready_issues_via_cli()` |
| `claim_issue` | `gh issue edit <num> --repo o/r --add-label gru:in-progress --remove-label gru:todo` |
| `mark_issue_done` | `gh issue edit <num> --repo o/r --add-label gru:done --remove-label gru:in-progress` |
| `mark_issue_failed` | `gh issue edit <num> --repo o/r --add-label gru:failed --remove-label gru:in-progress` |
| `mark_issue_blocked` | `gh issue edit <num> --repo o/r --add-label gru:blocked --remove-label gru:in-progress` |

**Composite operations** (`claim_issue`, `mark_issue_done`, etc.) rewritten using combined `--add-label`/`--remove-label` in single `gh issue edit` calls to reduce subprocesses and race windows.

**`gh_cli_command` simplified:**
```rust
pub fn gh_cli_command(host: &str) -> Command {
    let mut cmd = Command::new("gh");
    if host != "github.com" {
        cmd.env("GH_HOST", host);
    }
    cmd
}
```
Always uses `"gh"`, never `"ghe"`.

**Add tests** for each new CLI replacement function before proceeding to Phase 3.

### Phase 3: Remove old code + migrate callers

**Delete from `src/github.rs`:**
- `GitHubClient` struct + all methods
- `infer_github_host()` — Netflix substring heuristic
- `gh_command_for_host()` — `"ghe"` binary selection
- `gh_command_for_repo()` — wraps above
- `get_github_token_for_host()` / `try_get_token_from_cli_for_host()` — token extraction for octocrab

**Migrate `build_issue_url_with_host`** to use `web_url_for()` from the registry so GHE web links point to `github.netflix.net` instead of `git.netflix.net`.

**Migrate callers** — thread `host` through, use CLI functions:

#### Phase 3a: Simple `gh_command_for_repo` → `gh_cli_command(host)` swaps

| File | Call sites | Change |
|------|-----------|--------|
| `src/ci.rs` | 4 | Replace `gh_command_for_repo` with `gh_cli_command(host)` |
| `src/merge_judge.rs` | 6 | Replace `gh_command_for_repo` with `gh_cli_command(host)` |
| `src/pr_monitor.rs` | 2 | Replace `gh_command_for_repo` |
| `src/merge_readiness.rs` | 1 | Replace `gh_command_for_repo` with `gh_cli_command(host)` |
| `src/worktree_scanner.rs` | 3 | Replace `gh_command_for_repo`, add `host` to `WorktreeInfo` |

#### Phase 3b: `GitHubClient` removal + `load_host_registry` migration

| File | Call sites | Change |
|------|-----------|--------|
| `src/commands/init.rs` | 3 | `HostRegistry` for host lookup, CLI for auth + labels |
| `src/commands/fix.rs` | 7 | Replace `GitHubClient` with CLI, `load_host_registry()` |
| `src/commands/lab.rs` | 3 | Replace `GitHubClient` with CLI |
| `src/commands/prompt.rs` | 4 | Replace `try_from_env_with_host` fallback pattern with direct CLI |
| `src/commands/review.rs` | 5 | Replace `try_from_env_with_host` with CLI |
| `src/commands/resume.rs` | 2 | Replace `GitHubClient` + `infer_github_host` with registry |
| `src/commands/rebase.rs` | 2 | Use `load_host_registry()` |
| `src/commands/chat.rs` | 1 | Use `load_host_registry()` |
| `src/url_utils.rs` | 1 | Use `load_host_registry()` |
| `src/minion_resolver.rs` | 2 | Use `load_host_registry()` |

**Pattern for callers that detect repo from git remote:**
```rust
let registry = load_host_registry();
let (host, owner, repo) = parse_github_remote(&url, &registry.all_hosts())?;
// host is already the right value for GH_HOST since it matches git remote
```

### Phase 4: Remove octocrab dependency

1. Remove `octocrab` from `Cargo.toml`
2. Remove all `use octocrab::...` imports
3. Replace octocrab model types with existing CLI structs (`IssueInfo`, `PrInfo`) + simple `AuthInfo { login: String }`
4. Clean up remaining `"ghe"` or Netflix references (search codebase)
5. Update `CLAUDE.md`
6. `just check`

## Key Design Decisions

- **Always `gh` binary, never `ghe`** — gru sets `GH_HOST` itself
- **Single `host` field** — the host in git remotes IS the GH_HOST value (Netflix: `git.netflix.net` for both)
- **Optional `web_url`** — only for when web UI is on a different domain than git/API host. Essential for Netflix where `git.netflix.net` ≠ `github.netflix.net`.
- **`gh label create --force`** — idempotent label creation (verified: flag exists)
- **Combined label ops** — `gh issue edit --add-label x --remove-label y` in single call
- **Backwards compat** — `"host.com/owner/repo"` in daemon.repos still works
- **github.com implicit** — always in the registry, zero config needed
- **`gru do 42` from GHE repo** — works as long as `[github_hosts.*]` section defines the host; doesn't require a daemon.repos entry

## Risks

1. **Performance** — CLI subprocess per API call is slower than octocrab HTTP. Mitigated by combining label ops into single calls. Acceptable for now.

2. **Error handling** — CLI gives stderr strings, not typed errors. Mitigated by `--force` for labels. Other errors wrapped with context.

3. **`gh` required** — If `gh` isn't installed, nothing works. Mitigated by availability check in `gru init` and `gru lab` with clear error message.

4. **GHE auth is the user's responsibility** — Gru sets `GH_HOST` and calls `gh`. If the user's `gh` binary can't authenticate to their GHE instance (e.g. needs a vendor fork, auth plugin, or special config), that's outside gru's scope. Gru should surface auth failures clearly so users can diagnose their `gh` setup.

5. **Org/repo mapping** — Some GHE installations remap org/repo names between git remotes and the API. Gru uses the owner/repo from the git remote as-is when calling `gh`. If a GHE instance requires mapping, this would need a config extension (not in scope for this plan).

## Verified CLI Commands (2026-03-13)

All commands tested against `corp/ste-slackdgs` with `GH_HOST=git.netflix.net` (using a `gh` fork with GHE auth support):

| Command | Status |
|---------|--------|
| `gh auth status --hostname git.netflix.net` | PASS — logged in via keyring |
| `gh issue list --repo corp/ste-slackdgs` | PASS |
| `gh issue view <num> --json title,body,labels,state` | PASS |
| `gh pr list --repo corp/ste-slackdgs` | PASS |
| `gh label list --repo corp/ste-slackdgs` | PASS |
| `gh label create --force` (flag check) | PASS — flag exists |
| `gh issue edit --add-label / --remove-label` (flag check) | PASS — flags exist |
| `gh issue comment --body` (flag check) | PASS — flag exists |

## Verification

1. `just check` passes (fmt + lint + test + build)
2. `gru init` in `~/prj/slackdgs` with `[github_hosts.netflix]` config works
3. `gru init` in a github.com repo works with zero config
4. `gru do <issue>` from inside a GHE repo works (host discovered from registry)
5. Existing tests updated/passing
6. No references to `"ghe"` binary, `"netflix"` heuristic, or `octocrab` remain
7. Web links (PR URLs, issue URLs in comments) use `web_url` from registry
