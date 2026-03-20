# Security

This document describes Gru's threat model, permission model, and security guidance for operators.

## Trust Model

Gru orchestrates autonomous coding agents (Minions) that have **full filesystem and shell access** within their worktree. This is by design — Minions need to read code, write files, run tests, and interact with build tools.

**Gru trusts the agent backend.** When you run Gru, you are granting the underlying LLM agent (e.g., Claude Code) the same level of access a developer would have in a terminal session.

## What Minions Can Do

Each Minion runs with `--dangerously-skip-permissions`, which means it can:

- **Read and write files** anywhere accessible to the OS user running Gru
- **Execute arbitrary shell commands** (build tools, tests, scripts, network calls)
- **Make API calls** to GitHub (via `gh` CLI) and any other reachable services
- **Install packages** if build tools permit it
- **Access environment variables** available to the parent process

## Isolation and Guardrails

### Worktree Isolation

Each Minion works in a dedicated git worktree under `~/.gru/work/`. Worktrees are created from bare repository mirrors, so Minions cannot interfere with your main checkout or each other's working directories.

- Path traversal is validated — `..` and backslash sequences are rejected
- Branch names are validated against git's rules (no `..`, `@{`, null bytes, etc.)
- Worktree paths are verified to stay within the `~/.gru/work/` directory

### Ephemeral Processes

Minions are short-lived processes, not persistent daemons. Each Minion:

- Runs as a child process of the Gru CLI
- Exits when the task completes, times out, or is stopped
- Has no background persistence after termination
- Can be resumed (max 3 attempts) if interrupted, then marked as failed

### Timeout and Stuck Detection

Multiple timeout layers prevent runaway processes:

| Timeout | Default | Purpose |
|---------|---------|---------|
| Stream timeout | 5 minutes | No stdout output from the agent |
| Inactivity warning | 5 minutes | No stream events received |
| Stuck threshold | 15 minutes | Agent considered stuck; process terminated |
| Task timeout | None (opt-in) | Overall task duration limit via `--timeout` flag |
| CI fix timeout | 20 minutes | Per CI auto-fix attempt |
| CI completion timeout | 30 minutes | Waiting for CI checks to finish |

### CI Auto-Fix Limits

Minions can attempt to fix CI failures automatically, but this is bounded:

- Maximum 2 auto-fix attempts per PR
- After 2 failures, the PR is labeled `gru:blocked` and escalated to a human

### Merge Controls

PRs created by Minions are **not merged automatically** unless you explicitly configure auto-merge. The default flow requires human review:

1. **Deterministic checks** run first: CI status, review approvals, no merge conflicts, not a draft
2. **Confidence threshold**: An LLM judge evaluates merge readiness (default threshold: 8/10)
3. **Escalation**: If confidence is below threshold, the PR is labeled `gru:needs-human-review`

### GitHub Labels as State

Gru uses GitHub labels (`gru:todo`, `gru:in-progress`, `gru:done`, `gru:failed`, `gru:blocked`) to track Minion state. Labels are reversible — you can re-label an issue at any time to change its state.

### Credential Handling

- GitHub tokens are passed via `GIT_ASKPASS` scripts with `0700` permissions, never in command arguments
- Credentials are redacted from log output
- Token environment variables are only visible to the same OS user

## Known Risks

### No Filesystem Sandboxing

Minions run as your OS user. There is no container, chroot, or seccomp sandbox. A Minion can access anything your user account can access, including:

- Files outside the worktree (SSH keys, credentials, other repos)
- Network services reachable from your machine
- Other processes running as the same user

### No Network Restrictions

There are no firewall rules or network policies applied to Minion processes. A Minion can make outbound HTTP requests, DNS lookups, and connect to any reachable host.

### LLM Prompt Injection

Malicious content in issues, PRs, or code comments could influence Minion behavior. If an attacker can write to a GitHub issue that Gru processes, they could potentially steer the agent to execute unintended commands.

### Secrets in Environment

If secrets are present in environment variables or accessible files (`.env`, credentials), Minions can read them. Gru does not filter or restrict environment variable access.

## Guidance for Cautious Adopters

1. **Review PRs before merging.** This is the single most effective guardrail. Do not enable auto-merge until you trust the setup.

2. **Restrict repository access.** Run Gru against repositories where the blast radius is acceptable. Avoid running it against repos containing production secrets or sensitive infrastructure code.

3. **Use a dedicated machine or user account.** Isolate Gru from your personal credentials and SSH keys by running it under a service account with minimal permissions.

4. **Limit the GitHub token scope.** Use a fine-grained personal access token scoped to only the repositories Gru needs.

5. **Set task timeouts.** Use `gru do <issue> --timeout 30m` to cap how long a Minion can run.

6. **Monitor Minion activity.** Stream events are logged to `events.jsonl` in each Minion's directory. Use `gru tail` or `gru logs` to observe what a Minion is doing.

7. **Start with low-risk issues.** Begin with documentation, test additions, or simple bug fixes before graduating to larger changes.

8. **Limit daemon concurrency.** In lab mode, set `max_slots` to a low value (default is 2) to limit how many Minions run simultaneously.

## Reporting Security Vulnerabilities

If you discover a security vulnerability in Gru, please report it responsibly:

1. **Do not open a public GitHub issue** for security vulnerabilities
2. Email the maintainers at the address listed in the repository's contact information
3. Include a description of the vulnerability, steps to reproduce, and potential impact
4. We will acknowledge receipt within 48 hours and provide a timeline for a fix

For non-security bugs, please use [GitHub Issues](https://github.com/fotoetienne/gru/issues).
