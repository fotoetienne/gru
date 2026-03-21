# GitHub Enterprise Server (GHES) Setup

This guide walks you through configuring Gru to work with a GitHub Enterprise Server instance. If you're using `github.com`, see [GETTING_STARTED.md](GETTING_STARTED.md) instead.

## Prerequisites

Complete the basic [Getting Started](GETTING_STARTED.md) prerequisites first (Rust, Claude Code). Then come back here for GHES-specific setup.

## Step 1: Authenticate `gh` CLI with your GHES host

Gru delegates all GitHub API calls to the `gh` CLI, so you need `gh` authenticated against your GHES instance.

```bash
gh auth login --hostname ghe.example.com
```

Follow the prompts to authenticate. When asked about protocol, HTTPS is recommended for GHES.

Verify it worked:

```bash
gh auth status --hostname ghe.example.com
```

You should see output like:

```
ghe.example.com
  ✓ Logged in to ghe.example.com account yourname
```

### Multi-host authentication

The `gh` CLI supports being logged into multiple hosts simultaneously. If you use both `github.com` and a GHES instance:

```bash
# Authenticate to both
gh auth login                                  # github.com
gh auth login --hostname ghe.example.com       # GHES

# Verify both
gh auth status
```

Gru sets `GH_HOST` on every `gh` CLI invocation, so the correct host is always targeted — you don't need to worry about which is the "default".

### Token scope requirements

Your token needs the following scopes:

| Scope | Why |
|-------|-----|
| `repo` | Read/write access to repositories, issues, and PRs |
| `read:org` | Discover repos and check org membership |

If you authenticate interactively with `gh auth login`, the default scopes are usually sufficient. If you're using a personal access token, make sure it includes the scopes above.

To check your token's scopes:

```bash
gh auth status --hostname ghe.example.com --show-token
```

## Step 2: Configure `~/.gru/config.toml`

Create or edit `~/.gru/config.toml` and add a named host entry:

```toml
[github_hosts.netflix]
host = "github.netflix.com"
```

The name (`netflix` in this example) is your shorthand — you'll use it when referencing repos.

### Optional: `web_url`

If the GHES web UI lives on a different domain than the API/git host (uncommon), set `web_url`. Note that `host` is a bare hostname while `web_url` is a full URL including scheme:

```toml
[github_hosts.netflix]
host = "github.netflix.com"
web_url = "https://github-web.netflix.com"
```

Most GHES installations don't need this.

## Step 3: Reference repos using the named host

In your config's `daemon.repos`, reference GHES repos with the `name:owner/repo` format (this shorthand is for config files only, not CLI arguments):

```toml
[daemon]
repos = [
    "netflix:myteam/myapp",
    "netflix:myteam/mylib",
]
```

This tells Gru that `myteam/myapp` lives on the host defined in `[github_hosts.netflix]`.

### Repo format reference

| Format | Example | Resolves to |
|--------|---------|-------------|
| `owner/repo` | `myorg/app` | `github.com/myorg/app` |
| `name:owner/repo` | `netflix:myteam/myapp` | Uses `[github_hosts.netflix]` |
| `host/owner/repo` | `ghe.example.com/org/svc` | Legacy explicit-host format |

The `name:owner/repo` format is recommended for GHES.

## Step 4: Initialize and run

Initialize your GHES repo using `--host` to specify the GHES hostname:

```bash
gru init myteam/myapp --host github.netflix.com
```

Then use Gru normally:

```bash
# Work on a single issue (use the full URL for GHES issues)
gru do https://github.netflix.com/myteam/myapp/issues/42

# Or from within the worktree, just use the issue number
gru do 42

# Run lab mode to poll for gru:todo issues
gru lab
```

## Full config example

Here's a complete config for a team using GHES with two repos:

```toml
[github_hosts.netflix]
host = "github.netflix.com"

[daemon]
repos = [
    "netflix:myteam/api-gateway",
    "netflix:myteam/web-ui",
]
poll_interval_secs = 60
max_slots = 4

[agent]
default = "claude"

[merge]
confidence_threshold = 9
```

See [config.example.toml](config.example.toml) for all available options with explanations.

## Troubleshooting

### `gh auth status` fails for GHES host

```
ghe.example.com
  X Not logged in to ghe.example.com
```

**Fix:** Run `gh auth login --hostname ghe.example.com` and authenticate.

### "Unknown host name" error

```
Unknown host name 'netflix' in repo 'netflix:myteam/myapp'.
Add a [github_hosts.netflix] section to config.toml
```

**Fix:** Add the missing host entry to `~/.gru/config.toml`:

```toml
[github_hosts.netflix]
host = "github.netflix.com"
```

### 403 Forbidden on API calls

Your token likely lacks required scopes. Re-authenticate with appropriate scopes:

```bash
gh auth login --hostname ghe.example.com --scopes repo,read:org
```

Or generate a new personal access token in your GHES settings with the scopes listed in [Step 1](#token-scope-requirements).

### SSL/TLS certificate errors

If your GHES instance uses a self-signed or internal CA certificate:

```bash
# Tell gh to trust your corporate CA bundle
export GH_CACERT=/path/to/ca-bundle.crt
```

Or configure git:

```bash
git config --global http.https://ghe.example.com/.sslCAInfo /path/to/ca-bundle.crt
```

### `gru init` hangs or times out

Check network connectivity to your GHES host:

```bash
gh api --hostname ghe.example.com /meta
```

If this fails, the issue is network-level (VPN, firewall, DNS).

### Issue comments or labels not appearing

Verify your token has `repo` scope and that you have write access to the repository:

```bash
gh api --hostname ghe.example.com repos/myteam/myapp
```

If you get a 404, you either don't have access or the repo path is wrong.
