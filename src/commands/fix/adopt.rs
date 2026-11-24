//! `gru adopt`: hand an existing PR to a Minion to shepherd to merge.
//!
//! An adopted Minion skips implementation and PR creation. It checks out the
//! PR's own head branch, runs a short read-only orientation session (so later
//! review/CI rounds can resume a session that knows the PR), then enters the
//! same PR monitoring loop `gru do` uses: CI fixes, review responses and
//! rebases on conflict.
//!
//! The registry entry is keyed by the PR's linked issue (like `gru do`), with
//! `command = "adopt"`, the PR's head branch stored in `branch`, and the PR
//! number in `pr`. The Minion directory uses the usual
//! `minion/issue-<n>-<id>` layout even though the git branch has another name.

use super::agent::{resume_agent_session, run_agent_session_with_prompt};
use super::helpers::update_orchestration_phase;
use super::resolve::claim_issue;
use super::types::{IssueContext, WorktreeContext};
use super::worker;
use super::worktree::format_branch_name;
use super::{spawn_worker, FixOptions};
use crate::agent::AgentBackend;
use crate::agent_registry;
use crate::agent_runner::{parse_timeout, EXIT_CODE_SIGNAL_TERMINATED};
use crate::config::HostRegistry;
use crate::git;
use crate::github;
use crate::minion;
use crate::minion_registry::{with_registry, MinionInfo, MinionMode, OrchestrationPhase};
use crate::pr_monitor;
use crate::pr_state::PrState;
use crate::tmux::TmuxGuard;
use crate::url_utils::{parse_github_url, GitHubResourceType};
use crate::workspace;
use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde::Deserialize;
use tokio::time::Duration;
use uuid::Uuid;

/// Registry `command` value for adopted Minions.
pub(crate) const ADOPT_COMMAND: &str = "adopt";

/// Options for `gru adopt`.
pub struct AdoptOptions {
    pub timeout: Option<String>,
    pub review_timeout: Option<String>,
    pub monitor_timeout: Option<String>,
    pub quiet: bool,
    pub agent_name: String,
    pub auto_merge: bool,
    pub detach: bool,
    /// Allow adopting a PR authored by someone other than the current `gh` user.
    pub any_author: bool,
}

#[derive(Debug, Deserialize)]
struct Login {
    login: String,
}

#[derive(Debug, Deserialize)]
struct RepoRef {
    name: String,
    owner: Login,
}

#[derive(Debug, Deserialize)]
struct ClosingIssue {
    number: u64,
    #[serde(default)]
    repository: Option<RepoRef>,
}

/// The PR fields `gru adopt` needs, as returned by `gh pr view --json`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrMeta {
    number: u64,
    title: String,
    #[serde(default)]
    body: String,
    state: String,
    url: String,
    head_ref_name: String,
    is_cross_repository: bool,
    #[serde(default)]
    author: Option<Login>,
    #[serde(default)]
    closing_issues_references: Vec<ClosingIssue>,
    #[serde(default)]
    commits: Vec<PrCommit>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrCommit {
    committed_date: chrono::DateTime<Utc>,
}

const PR_META_FIELDS: &str = "number,title,body,state,url,headRefName,isCrossRepository,author,closingIssuesReferences,commits";

/// Review baseline for a newly adopted PR: the commit date of its latest
/// commit.
///
/// Reviews and comments posted after the author's latest commit are probably
/// still unaddressed, so the Minion picks them up; earlier ones are assumed
/// handled. This is commit time, not push time: a commit made before a
/// review but pushed after it makes that review look new. That errs toward
/// re-reading feedback rather than skipping it. `None` (no commits) falls
/// back to "from now on".
fn initial_review_baseline(meta: &PrMeta) -> Option<chrono::DateTime<Utc>> {
    meta.commits.iter().map(|c| c.committed_date).max()
}

/// Finds the issue the PR is linked to in the same repository.
///
/// GitHub's `closingIssuesReferences` is checked first (covers both keyword
/// links and links made in the sidebar). The PR body is the fallback, for
/// hosts that don't populate the field.
fn find_linked_issue(meta: &PrMeta, owner: &str, repo: &str) -> Option<u64> {
    let same_repo = |issue: &&ClosingIssue| match &issue.repository {
        Some(r) => r.owner.login.eq_ignore_ascii_case(owner) && r.name.eq_ignore_ascii_case(repo),
        None => true,
    };
    meta.closing_issues_references
        .iter()
        .find(same_repo)
        .map(|i| i.number)
        .or_else(|| crate::minion_resolver::parse_linked_issue(&meta.body))
}

/// Checks that a PR can be adopted and returns its linked issue number.
///
/// Refuses closed/merged PRs, fork PRs (we can't push to them), PRs authored
/// by someone else unless `any_author` is set, and PRs with no linked issue.
fn check_adoptable(
    meta: &PrMeta,
    owner: &str,
    repo: &str,
    current_user: &str,
    any_author: bool,
) -> Result<u64> {
    if !meta.state.eq_ignore_ascii_case("OPEN") {
        bail!(
            "PR #{} is {}. Only open PRs can be adopted.",
            meta.number,
            meta.state.to_lowercase()
        );
    }

    if meta.is_cross_repository {
        bail!(
            "PR #{} comes from a fork. Gru can't push to fork branches, so fork PRs \
             can't be adopted.",
            meta.number
        );
    }

    if !any_author {
        let author = meta.author.as_ref().map(|a| a.login.as_str()).unwrap_or("");
        if !author.eq_ignore_ascii_case(current_user) {
            bail!(
                "PR #{} was opened by {}, not by you ({}). Gru only adopts your own PRs \
                 by default. Pass --any-author to adopt it anyway.",
                meta.number,
                if author.is_empty() {
                    "an unknown user"
                } else {
                    author
                },
                current_user
            );
        }
    }

    find_linked_issue(meta, owner, repo).with_context(|| {
        format!(
            "PR #{} has no linked issue. Gru tracks Minions by issue, so add a line like \
             `Fixes #123` to the PR description (or link an issue from the PR sidebar), \
             then run `gru adopt` again.",
            meta.number
        )
    })
}

/// Resolves a PR argument (number or URL) to `(host, owner, repo, number)` of
/// the base repository.
///
/// Unlike `url_utils::parse_pr_info`, this returns the base repo rather than
/// the head repo, so fork PRs resolve to the repo they target.
async fn resolve_pr_target(
    arg: &str,
    host_registry: &HostRegistry,
) -> Result<(String, String, String, u64)> {
    if let Ok(num) = arg.parse::<u64>() {
        git::detect_git_repo().await.context(
            "Not in a git repository. Run from inside the repo, or pass the full PR URL.",
        )?;
        let remote_url = git::get_github_remote(host_registry)
            .await
            .context("Failed to get GitHub remote")?;
        let (host, owner, repo) = git::parse_github_remote(&remote_url, host_registry)
            .context("Failed to parse GitHub remote URL")?;
        return Ok((host, owner, repo, num));
    }

    match parse_github_url(arg, host_registry) {
        Some(parsed) if parsed.resource_type == GitHubResourceType::Pull => Ok((
            parsed.host,
            parsed.owner,
            parsed.repo,
            u64::from(parsed.number),
        )),
        Some(_) => bail!(
            "Expected a pull request URL, but got an issue URL. \
             Use `gru do` to work on an issue."
        ),
        None => bail!(
            "Invalid PR format. Expected a PR number or URL, e.g.\n  \
             gru adopt 42\n  \
             gru adopt https://github.com/owner/repo/pull/42"
        ),
    }
}

async fn fetch_pr_meta(host: &str, owner: &str, repo: &str, pr: u64) -> Result<PrMeta> {
    let repo_full = github::repo_slug(owner, repo);
    let pr_str = pr.to_string();
    let stdout = github::run_gh(
        host,
        &[
            "pr",
            "view",
            &pr_str,
            "--repo",
            &repo_full,
            "--json",
            PR_META_FIELDS,
        ],
    )
    .await
    .with_context(|| format!("Failed to fetch PR #{} from {}", pr, repo_full))?;
    serde_json::from_str(&stdout).context("Failed to parse gh pr view output")
}

async fn current_gh_user(host: &str) -> Result<String> {
    let login = github::run_gh(host, &["api", "user", "--jq", ".login"])
        .await
        .context("Failed to look up the current gh user (is `gh auth login` done?)")?;
    let login = login.trim().to_string();
    if login.is_empty() {
        bail!("gh returned an empty login for the current user");
    }
    Ok(login)
}

/// Refuses to adopt when a live (non-terminal) Minion already owns the issue
/// or the PR, so two Minions never push to the same branch.
fn check_no_live_minion(existing: &[(String, MinionInfo)], issue_num: u64, pr: u64) -> Result<()> {
    let pr_str = pr.to_string();
    // An entry for this same PR blocks unless it failed: a Completed one
    // (e.g. its monitor timed out) can be resumed, and the PR is still open
    // or adopt would have refused it. Other entries for the issue block only
    // while non-terminal.
    if let Some((id, info)) = existing.iter().find(|(_, info)| {
        let same_pr = info.pr.as_deref() == Some(pr_str.as_str());
        (same_pr && info.orchestration_phase != OrchestrationPhase::Failed)
            || (!info.orchestration_phase.is_terminal() && info.issue == Some(issue_num))
    }) {
        let state = if info.is_running() {
            "running"
        } else {
            "stopped"
        };
        bail!(
            "Minion {} ({}) is already working on issue #{} / PR #{}.\n\
             Resume it with `gru resume {}`, or stop it with `gru stop {}` and clean it up \
             with `gru clean` before adopting.",
            id,
            state,
            issue_num,
            pr,
            id,
            id
        );
    }
    Ok(())
}

/// Builds the prompt for the adopted Minion's first (orientation) session.
fn build_orientation_prompt(
    issue_ctx: &IssueContext,
    wt_ctx: &WorktreeContext,
    pr: u64,
    pr_title: &str,
    pr_url: &str,
) -> String {
    let issue_num = issue_ctx
        .issue_num
        .map_or("?".to_string(), |n| n.to_string());
    let issue_section = match &issue_ctx.details {
        Some(d) => format!(
            "## Linked issue #{}: {}\n\n{}\n",
            issue_num, d.title, d.body
        ),
        None => format!("## Linked issue #{}\n", issue_num),
    };
    let branch = &wt_ctx.branch_name;
    let repo_full = github::repo_slug(&issue_ctx.owner, &issue_ctx.repo);
    format!(
        "# Adopted PR #{pr}: {pr_title}\n\n\
You are Minion {minion_id}. A developer wrote this pull request and has handed it to you to \
see through to merge. The implementation already exists.\n\n\
- PR: {pr_url}\n\
- Branch: `{branch}` (checked out in the current directory)\n\n\
{issue_section}\n\
## How this works\n\n\
Gru will watch the PR and resume this session when CI fails, when reviewers leave comments, or \
when the branch needs a rebase. When that happens, make the smallest change that addresses it.\n\n\
## Rules for this PR\n\n\
- Do not re-implement the issue, restructure the PR, or edit the PR title or description.\n\
- The author may keep pushing to `{branch}`. Commit your changes but do not push: when your \
session ends, Gru integrates the author's new commits and pushes yours itself.\n\
- Never force-push. Never rewrite, squash or drop commits you did not author.\n\
- Do not create or switch branches, and do not write PR_DESCRIPTION.md.\n\n\
## Right now\n\n\
Get familiar with the change: read the PR with `gh pr view {pr} --repo {repo_full}` and the \
commits on this branch (`git log`, `git diff`). Do not edit files, commit or push in this step. \
Finish with a short summary of what the PR does.\n",
        minion_id = wt_ctx.minion_id,
    )
}

/// Continuation prompt for an orientation session that was interrupted.
fn orientation_resume_prompt(pr: &str) -> String {
    format!(
        "Continue getting familiar with PR #{pr}. Do not edit files, commit or push yet; \
         Gru will resume you when CI or reviewers need something. Finish with a short summary \
         of what the PR does."
    )
}

/// Foreground entry point for `gru adopt <pr>`.
///
/// Validates the PR, creates a worktree on its head branch, registers the
/// Minion at the PR monitoring stage, and spawns a background worker.
pub(crate) async fn handle_adopt(pr_arg: &str, opts: AdoptOptions) -> Result<i32> {
    // Fail fast on unknown agents and bad durations, before any side effects.
    agent_registry::resolve_backend(&opts.agent_name)?;
    for (flag, value) in [
        ("--timeout", &opts.timeout),
        ("--review-timeout", &opts.review_timeout),
        ("--monitor-timeout", &opts.monitor_timeout),
    ] {
        if let Some(v) = value {
            parse_timeout(v).with_context(|| format!("Invalid {} value", flag))?;
        }
    }

    let host_registry = crate::config::load_host_registry();
    let (host, owner, repo, pr) = resolve_pr_target(pr_arg, &host_registry).await?;
    let repo_name = github::repo_slug(&owner, &repo);

    println!("🔍 Checking PR {}#{}...", repo_name, pr);
    let meta = fetch_pr_meta(&host, &owner, &repo, pr).await?;
    let current_user = if opts.any_author {
        String::new()
    } else {
        current_gh_user(&host).await?
    };
    let issue_num = check_adoptable(&meta, &owner, &repo, &current_user, opts.any_author)?;
    let branch = meta.head_ref_name.clone();
    println!(
        "✅ PR #{} (branch `{}`) is linked to issue #{}",
        pr, branch, issue_num
    );

    let repo_for_check = repo_name.clone();
    let existing = with_registry(move |reg| {
        Ok(reg
            .list()
            .into_iter()
            .filter(|(_, info)| info.repo == repo_for_check)
            .collect::<Vec<_>>())
    })
    .await?;
    check_no_live_minion(&existing, issue_num, pr)?;

    let _tmux_guard = TmuxGuard::new(&format!("gru:adopt:#{}", pr));

    // Workspace setup
    let minion_id = minion::generate_minion_id().context("Failed to generate Minion ID")?;
    println!("📋 Generated Minion ID: {}", minion_id);

    let ws = workspace::Workspace::new().context("Failed to initialize Gru workspace")?;
    let bare_path = ws.repos().join(&owner).join(format!("{}.git", repo));
    let git_repo = git::GitRepo::new(&owner, &repo, &host, bare_path);

    println!("📦 Ensuring repository is cloned...");
    git_repo
        .ensure_bare_clone()
        .await
        .context("Failed to clone or update repository")?;

    if let Some(path) = git_repo
        .find_worktree_for_branch(&branch)
        .await
        .context("Failed to check for an existing worktree")?
    {
        bail!(
            "Branch `{}` is already checked out at {}.\n\
             Remove that worktree (e.g. `gru clean`, or `git worktree remove`) and try again.",
            branch,
            path.display()
        );
    }

    println!("🔄 Fetching PR branch `{}`...", branch);
    git_repo
        .fetch_branch(&branch)
        .await
        .with_context(|| format!("Failed to fetch PR branch '{}'", branch))?;

    // Directory layout matches `gru do` (minion/issue-<n>-<id>/checkout) so
    // path-based tooling (clean, resolver) treats it like any other Minion.
    let minion_dir = ws
        .work_dir(&repo_name, &format_branch_name(Some(issue_num), &minion_id))
        .context("Failed to compute minion directory path")?;
    let checkout_path = minion_dir.join("checkout");
    tokio::fs::create_dir_all(&minion_dir)
        .await
        .context("Failed to create minion directory")?;

    println!("🌿 Creating worktree on branch `{}`...", branch);
    git_repo
        .checkout_worktree(&branch, &checkout_path)
        .await
        .with_context(|| format!("Failed to create worktree for PR #{}", pr))?;

    // Populate refs/remotes/origin/<branch> (used by --force-with-lease and
    // `git pull`) and set it as upstream. Best effort: the monitor syncs
    // again before every push.
    if let Err(e) = git::sync_with_remote_branch(&checkout_path, &branch).await {
        log::warn!("⚠️  Could not sync with origin/{}: {:#}", branch, e);
    }
    let upstream = format!("origin/{}", branch);
    match tokio::process::Command::new("git")
        .arg("-C")
        .arg(&checkout_path)
        .args(["branch", "--set-upstream-to", &upstream])
        .output()
        .await
    {
        Ok(out) if out.status.success() => {}
        Ok(out) => log::warn!(
            "⚠️  Could not set upstream to {}: {}",
            upstream,
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(e) => log::warn!("⚠️  Could not set upstream to {}: {}", upstream, e),
    }

    println!("📂 Workspace created at: {}", checkout_path.display());

    if let Err(e) = PrState::new(pr.to_string(), issue_num.to_string()).save(&minion_dir) {
        log::warn!("⚠️  Failed to write PR state: {:#}", e);
    }

    let now = Utc::now();
    let timeout_deadline = opts
        .timeout
        .as_deref()
        .map(parse_timeout)
        .transpose()?
        .and_then(|d| chrono::TimeDelta::from_std(d).ok())
        .map(|delta| now + delta);
    let info = MinionInfo {
        repo: repo_name.clone(),
        issue: Some(issue_num),
        command: ADOPT_COMMAND.to_string(),
        prompt: format!("/adopt {}", pr),
        started_at: now,
        branch: branch.clone(),
        worktree: minion_dir.clone(),
        status: "active".to_string(),
        pr: Some(pr.to_string()),
        session_id: Uuid::new_v4().to_string(),
        pid: None,
        pid_start_time: None,
        mode: MinionMode::Autonomous,
        last_activity: now,
        orchestration_phase: OrchestrationPhase::Setup,
        token_usage: None,
        agent_name: opts.agent_name.clone(),
        timeout_deadline,
        attempt_count: 1,
        no_watch: false,
        last_review_check_time: initial_review_baseline(&meta),
        wake_reason: None,
        archived_at: None,
        pending_review_sha: None,
    };
    let mid = minion_id.clone();
    let repo_for_register = repo_name.clone();
    // Re-check under the registry lock: two `gru adopt` runs for the same
    // issue on different branches could both pass the earlier check. (Two
    // runs for the same branch are already stopped by git's worktree lock.)
    if let Err(e) = with_registry(move |reg| {
        let existing: Vec<_> = reg
            .list()
            .into_iter()
            .filter(|(_, i)| i.repo == repo_for_register)
            .collect();
        check_no_live_minion(&existing, issue_num, pr)?;
        reg.register(mid, info)
    })
    .await
    {
        // Without a registry entry nothing would ever clean this worktree up,
        // and it would block the next `gru adopt` of this branch.
        let _ = tokio::process::Command::new("git")
            .arg("-C")
            .arg(ws.repos().join(&owner).join(format!("{}.git", repo)))
            .args(["worktree", "remove", "--force"])
            .arg(&checkout_path)
            .output()
            .await;
        let _ = tokio::fs::remove_dir_all(&minion_dir).await;
        return Err(e.context("Failed to register the adopted Minion"));
    }
    println!("📝 Registered Minion {} in registry", minion_id);

    claim_issue(&host, &owner, &repo, Some(issue_num)).await;

    if opts.auto_merge {
        if let Err(e) = pr_monitor::ensure_auto_merge_label(&host, &owner, &repo).await {
            log::warn!("⚠️  Failed to ensure gru:auto-merge label: {:#}", e);
        }
        match pr_monitor::add_auto_merge_label(&host, &owner, &repo, &pr.to_string()).await {
            Ok(()) => println!("🏷️  Added gru:auto-merge label to PR #{}", pr),
            Err(e) => log::warn!("⚠️  Failed to add gru:auto-merge label: {:#}", e),
        }
    }

    // The worker is `gru do <issue-url> --worker <id>`; `run_worker` sees
    // command == "adopt" in the registry and runs `run_adopted_pipeline`.
    let issue_url = format!("https://{}/{}/{}/issues/{}", host, owner, repo, issue_num);
    let fix_opts = FixOptions {
        timeout: opts.timeout,
        review_timeout: opts.review_timeout,
        monitor_timeout: opts.monitor_timeout,
        quiet: opts.quiet,
        force_new: false,
        agent_name: opts.agent_name,
        no_watch: false,
        auto_merge: false, // label already applied above
        detach: opts.detach,
        discuss: false,
        ignore_deps: true,
        worker: None,
    };
    let worker_pid = spawn_worker(&issue_url, &minion_id, &minion_dir, &fix_opts).await?;
    println!(
        "Minion {} adopted PR #{} (issue #{}, PID: {})",
        minion_id, pr, issue_num, worker_pid
    );

    if opts.detach {
        println!(
            "Detached. Use `gru logs {}` to follow progress, `gru stop {}` to cancel.",
            minion_id, minion_id
        );
        return Ok(0);
    }

    println!(
        "Streaming progress... (Ctrl+C to detach, `gru stop {}` to cancel)\n",
        minion_id
    );
    crate::log_viewer::tail_events(
        minion_dir.join("events.jsonl"),
        &minion_id,
        &issue_num.to_string(),
        opts.quiet,
    )
    .await?;
    Ok(0)
}

/// Runs an adopted Minion from its current phase: orientation session (if not
/// done yet), then PR monitoring. Shared by the background worker and
/// `gru resume`.
///
/// Never runs the `do` prompt, never creates a PR, and never calls the
/// failure-path branch rescue (which force-pushes), so the author's branch is
/// only ever changed by the monitor loop's sync-then-push paths.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_adopted_pipeline(
    backend: &dyn AgentBackend,
    issue_ctx: &IssueContext,
    wt_ctx: &WorktreeContext,
    start_phase: &OrchestrationPhase,
    quiet: bool,
    timeout_opt: Option<&str>,
    review_timeout: Option<Duration>,
    monitor_timeout: Duration,
) -> Result<i32> {
    let mid = wt_ctx.minion_id.clone();
    let pr = with_registry(move |reg| Ok(reg.get(&mid).and_then(|i| i.pr.clone())))
        .await?
        .with_context(|| {
            format!(
                "Adopted Minion {} has no PR in the registry",
                wt_ctx.minion_id
            )
        })?;

    if *start_phase <= OrchestrationPhase::RunningAgent {
        update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::RunningAgent).await;
        // Without an issue number the session runner skips its issue progress
        // comments ("✅ Task completed"), which would be misleading for a
        // read-only orientation.
        let session_ctx = IssueContext {
            owner: issue_ctx.owner.clone(),
            repo: issue_ctx.repo.clone(),
            host: issue_ctx.host.clone(),
            issue_num: None,
            details: None,
        };
        let result = if *start_phase == OrchestrationPhase::Setup {
            let pr_num: u64 = pr.parse().context("Invalid PR number in registry")?;
            let meta = fetch_pr_meta(&issue_ctx.host, &issue_ctx.owner, &issue_ctx.repo, pr_num)
                .await
                .map_err(|e| log::warn!("⚠️  Could not fetch PR #{} details: {:#}", pr, e))
                .ok();
            let (title, url) = meta
                .map(|m| (m.title, m.url))
                .unwrap_or_else(|| (String::new(), String::new()));
            let prompt = build_orientation_prompt(issue_ctx, wt_ctx, pr_num, &title, &url);
            run_agent_session_with_prompt(
                backend,
                &session_ctx,
                wt_ctx,
                &prompt,
                quiet,
                timeout_opt,
            )
            .await
        } else {
            let prompt = orientation_resume_prompt(&pr);
            resume_agent_session(
                backend,
                &session_ctx,
                wt_ctx,
                quiet,
                timeout_opt,
                Some(&prompt),
            )
            .await
        };
        // Orientation only gives later sessions context; the PR still needs
        // watching if it fails, so log and carry on to monitoring.
        match result {
            Ok(r) if r.status.success() => {}
            // Killed by a signal: `gru stop` (or Ctrl-C / shutdown) signals the
            // registered agent child. That's a request to stop, not a failed
            // orientation, so don't go on to monitoring and spawn more agents.
            Ok(r) if r.status.code().is_none() => {
                println!(
                    "🛑 Orientation session was stopped; not monitoring PR #{}",
                    pr
                );
                clear_ownership(&wt_ctx.minion_id).await;
                return Ok(EXIT_CODE_SIGNAL_TERMINATED);
            }
            Ok(r) => log::warn!(
                "⚠️  Orientation session exited with {:?}; continuing to monitor PR #{}",
                r.status.code(),
                pr
            ),
            Err(e) => log::warn!(
                "⚠️  Orientation session failed: {:#}; continuing to monitor PR #{}",
                e,
                pr
            ),
        }
    }

    let monitor_result = worker::monitor_pr_phase(
        backend,
        issue_ctx,
        wt_ctx,
        &Some(pr),
        timeout_opt,
        review_timeout,
        monitor_timeout,
    )
    .await;

    if monitor_result.is_ok() {
        update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::Completed).await;
    }
    clear_ownership(&wt_ctx.minion_id).await;
    Ok(if monitor_result.is_ok() { 0 } else { 1 })
}

/// Clears PID and marks the Minion stopped (end of a worker/resume run).
async fn clear_ownership(minion_id: &str) {
    let mid = minion_id.to_string();
    let _ = with_registry(move |reg| {
        reg.update(&mid, |info| {
            info.clear_pid();
            info.mode = MinionMode::Stopped;
        })
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::super::types::IssueDetails;
    use super::*;

    fn meta(json: &str) -> PrMeta {
        serde_json::from_str(json).expect("valid PrMeta JSON")
    }

    fn own_pr() -> PrMeta {
        meta(
            r#"{
                "number": 7,
                "title": "Add widget",
                "body": "Some text",
                "state": "OPEN",
                "url": "https://github.com/octo/app/pull/7",
                "headRefName": "feature/widget",
                "isCrossRepository": false,
                "author": {"login": "alice"},
                "closingIssuesReferences": [
                    {"number": 3, "repository": {"name": "app", "owner": {"login": "octo"}}}
                ]
            }"#,
        )
    }

    #[test]
    fn test_adoptable_own_pr_returns_linked_issue() {
        assert_eq!(
            check_adoptable(&own_pr(), "octo", "app", "alice", false).unwrap(),
            3
        );
    }

    #[test]
    fn test_author_match_is_case_insensitive() {
        assert!(check_adoptable(&own_pr(), "octo", "app", "Alice", false).is_ok());
    }

    #[test]
    fn test_other_author_refused_without_any_author() {
        let err = check_adoptable(&own_pr(), "octo", "app", "bob", false).unwrap_err();
        let msg = format!("{:#}", err);
        assert!(msg.contains("--any-author"), "{msg}");
        assert!(msg.contains("alice"), "{msg}");
    }

    #[test]
    fn test_other_author_allowed_with_any_author() {
        assert_eq!(
            check_adoptable(&own_pr(), "octo", "app", "", true).unwrap(),
            3
        );
    }

    #[test]
    fn test_missing_author_refused_without_any_author() {
        let mut pr = own_pr();
        pr.author = None;
        assert!(check_adoptable(&pr, "octo", "app", "alice", false).is_err());
    }

    #[test]
    fn test_fork_pr_refused() {
        let mut pr = own_pr();
        pr.is_cross_repository = true;
        let err = check_adoptable(&pr, "octo", "app", "alice", true).unwrap_err();
        assert!(format!("{:#}", err).contains("fork"));
    }

    #[test]
    fn test_closed_pr_refused() {
        let mut pr = own_pr();
        pr.state = "MERGED".to_string();
        let err = check_adoptable(&pr, "octo", "app", "alice", false).unwrap_err();
        assert!(format!("{:#}", err).contains("merged"));
    }

    #[test]
    fn test_no_linked_issue_refused_with_guidance() {
        let mut pr = own_pr();
        pr.closing_issues_references.clear();
        let err = check_adoptable(&pr, "octo", "app", "alice", false).unwrap_err();
        let msg = format!("{:#}", err);
        assert!(msg.contains("no linked issue"), "{msg}");
        assert!(msg.contains("Fixes #123"), "{msg}");
    }

    #[test]
    fn test_linked_issue_falls_back_to_body() {
        let mut pr = own_pr();
        pr.closing_issues_references.clear();
        pr.body = "This change\n\nCloses #42".to_string();
        assert_eq!(find_linked_issue(&pr, "octo", "app"), Some(42));
    }

    #[test]
    fn test_linked_issue_ignores_other_repo_references() {
        let pr = meta(
            r#"{
                "number": 7, "title": "t", "body": "", "state": "OPEN", "url": "u",
                "headRefName": "b", "isCrossRepository": false,
                "closingIssuesReferences": [
                    {"number": 9, "repository": {"name": "other", "owner": {"login": "octo"}}},
                    {"number": 5, "repository": {"name": "app", "owner": {"login": "octo"}}}
                ]
            }"#,
        );
        assert_eq!(find_linked_issue(&pr, "octo", "app"), Some(5));
    }

    #[test]
    fn test_linked_issue_none_when_only_other_repo() {
        let pr = meta(
            r#"{
                "number": 7, "title": "t", "body": "", "state": "OPEN", "url": "u",
                "headRefName": "b", "isCrossRepository": false,
                "closingIssuesReferences": [
                    {"number": 9, "repository": {"name": "other", "owner": {"login": "octo"}}}
                ]
            }"#,
        );
        assert_eq!(find_linked_issue(&pr, "octo", "app"), None);
    }

    #[test]
    fn test_initial_review_baseline_is_latest_commit() {
        let pr = meta(
            r#"{
                "number": 1, "title": "t", "state": "OPEN", "url": "u",
                "headRefName": "b", "isCrossRepository": false,
                "commits": [
                    {"committedDate": "2026-01-02T00:00:00Z"},
                    {"committedDate": "2026-01-05T00:00:00Z"},
                    {"committedDate": "2026-01-03T00:00:00Z"}
                ]
            }"#,
        );
        assert_eq!(
            initial_review_baseline(&pr).unwrap().to_rfc3339(),
            "2026-01-05T00:00:00+00:00"
        );
        assert!(initial_review_baseline(&own_pr()).is_none());
    }

    #[test]
    fn test_pr_meta_tolerates_null_body_fields_missing() {
        // gh omits nothing in practice, but `author` can be null for deleted users.
        let pr = meta(
            r#"{
                "number": 1, "title": "t", "state": "OPEN", "url": "u",
                "headRefName": "b", "isCrossRepository": false, "author": null
            }"#,
        );
        assert!(pr.author.is_none());
        assert!(pr.body.is_empty());
        assert!(pr.closing_issues_references.is_empty());
    }

    fn registry_entry(
        issue: Option<u64>,
        pr: Option<&str>,
        phase: OrchestrationPhase,
    ) -> MinionInfo {
        let now = Utc::now();
        MinionInfo {
            repo: "octo/app".to_string(),
            issue,
            command: "do".to_string(),
            prompt: String::new(),
            started_at: now,
            branch: "minion/issue-3-M001".to_string(),
            worktree: std::path::PathBuf::from("/tmp/none"),
            status: "active".to_string(),
            pr: pr.map(String::from),
            session_id: Uuid::new_v4().to_string(),
            pid: None,
            pid_start_time: None,
            mode: MinionMode::Stopped,
            last_activity: now,
            orchestration_phase: phase,
            token_usage: None,
            agent_name: "claude".to_string(),
            timeout_deadline: None,
            attempt_count: 1,
            no_watch: false,
            last_review_check_time: None,
            wake_reason: None,
            archived_at: None,
            pending_review_sha: None,
        }
    }

    #[test]
    fn test_live_minion_on_issue_blocks_adopt() {
        let existing = vec![(
            "M001".to_string(),
            registry_entry(Some(3), None, OrchestrationPhase::MonitoringPr),
        )];
        let err = check_no_live_minion(&existing, 3, 7).unwrap_err();
        assert!(format!("{:#}", err).contains("gru resume M001"));
    }

    #[test]
    fn test_live_minion_on_pr_blocks_adopt() {
        let existing = vec![(
            "M001".to_string(),
            registry_entry(Some(99), Some("7"), OrchestrationPhase::RunningAgent),
        )];
        assert!(check_no_live_minion(&existing, 3, 7).is_err());
    }

    #[test]
    fn test_completed_minion_on_same_pr_blocks_adopt() {
        let existing = vec![(
            "M001".to_string(),
            registry_entry(Some(3), Some("7"), OrchestrationPhase::Completed),
        )];
        assert!(check_no_live_minion(&existing, 3, 7).is_err());
    }

    #[test]
    fn test_terminal_minion_does_not_block_adopt() {
        let existing = vec![
            (
                "M001".to_string(),
                registry_entry(Some(3), Some("7"), OrchestrationPhase::Failed),
            ),
            (
                "M002".to_string(),
                registry_entry(Some(3), Some("5"), OrchestrationPhase::Completed),
            ),
        ];
        assert!(check_no_live_minion(&existing, 3, 7).is_ok());
    }

    #[test]
    fn test_orientation_prompt_forbids_force_push_and_names_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let wt_ctx = WorktreeContext {
            minion_id: "M00x".to_string(),
            branch_name: "feature/widget".to_string(),
            minion_dir: tmp.path().to_path_buf(),
            checkout_path: tmp.path().join("checkout"),
            session_id: Uuid::new_v4(),
        };
        let issue_ctx = IssueContext {
            owner: "octo".to_string(),
            repo: "app".to_string(),
            host: "github.com".to_string(),
            issue_num: Some(3),
            details: Some(IssueDetails {
                title: "Widgets".to_string(),
                body: "We need widgets".to_string(),
                labels: vec![],
            }),
        };
        let prompt = build_orientation_prompt(
            &issue_ctx,
            &wt_ctx,
            7,
            "Add widget",
            "https://github.com/octo/app/pull/7",
        );
        assert!(prompt.starts_with("# Adopted PR #7: Add widget"));
        assert!(prompt.contains("Linked issue #3: Widgets"));
        assert!(prompt.contains("do not push"));
        assert!(prompt.contains("`feature/widget`"));
        assert!(prompt.contains("Never force-push"));
        assert!(prompt.contains("gh pr view 7 --repo octo/app"));
        assert!(prompt.contains("Do not edit files"));
    }
}
