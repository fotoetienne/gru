mod agent;
mod agent_registry;
mod agent_runner;
mod ci;
mod claude_backend;
mod claude_runner;
mod codex_backend;
mod commands;
mod config;
mod dependencies;
mod display_utils;
mod file_lock;
mod git;
mod github;
mod labels;
mod log_viewer;
mod mcp;
mod merge_judge;
mod merge_readiness;
mod minion;
mod minion_registry;
mod minion_resolver;
mod pr_monitor;
mod pr_state;
mod progress;
mod progress_comments;
mod prompt_loader;
mod prompt_renderer;
mod prompt_utils;
mod retry_queue;
mod session_claim;
mod shared_rate_limit;
mod stream;
mod text_buffer;
mod tmux;
mod url_utils;
mod version_check;
mod workspace;
mod worktree_scanner;

use clap::{CommandFactory, Parser, Subcommand};
use commands::{
    attach, chat, clean, fix, init, lab, logs, path, pm, prompt, prompts, rebase, resume, review,
    status, stop, tail,
};

/// Version string including git hash (e.g., "0.1.0 (abc1234)")
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GRU_GIT_HASH"), ")");

/// CLI structure for the Gru agent orchestrator
#[derive(Parser)]
#[command(name = "gru")]
#[command(version = VERSION)]
#[command(about = "Local-First LLM Agent Orchestrator", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Run in quiet mode (only show errors)
    #[arg(short, long, global = true)]
    quiet: bool,
}

/// Available commands for Gru
#[derive(Subcommand)]
enum Commands {
    #[command(about = "Initialize a repository for use with Gru")]
    Init {
        #[arg(
            help = "Repository to initialize: 'owner/repo' for GitHub, '.' for current directory, or a path. Defaults to current directory."
        )]
        repo: Option<String>,

        #[arg(
            long,
            help = "GitHub Enterprise Server hostname (e.g., ghe.example.com)"
        )]
        host: Option<String>,
    },
    #[command(about = "Start an interactive project-aware chat session")]
    Chat {
        #[arg(long, help = "Repository context as 'owner/repo'")]
        repo: Option<String>,

        #[arg(short, long, help = "Show additional context information")]
        verbose: bool,
    },
    #[command(about = "Start an interactive product manager session")]
    Pm {
        #[arg(help = "Optional prompt to start the session with")]
        prompt: Option<String>,

        #[arg(short, long, help = "Show additional context information")]
        verbose: bool,
    },
    #[command(about = "Start an interactive technical project manager session")]
    Tpm {
        #[arg(help = "Optional prompt to start the session with")]
        prompt: Option<String>,

        #[arg(short, long, help = "Show additional context information")]
        verbose: bool,
    },
    #[command(about = "Work on a GitHub issue", alias = "fix")]
    Do {
        #[arg(help = "Issue number or URL")]
        issue: String,

        #[arg(
            short,
            long,
            help = "Maximum duration for the task (e.g., '10s', '5m', '1h'). Exits with error if exceeded."
        )]
        timeout: Option<String>,

        #[arg(
            long,
            help = "Timeout for the automated PR review subprocess (e.g., '30m', '1h'). No timeout by default."
        )]
        review_timeout: Option<String>,

        #[arg(
            long,
            help = "Maximum duration for PR monitoring (e.g., '30m', '2h', '24h'). Defaults to 24 hours."
        )]
        monitor_timeout: Option<String>,

        #[arg(
            long,
            help = "Create a new Minion even if one already exists for this issue"
        )]
        force_new: bool,

        #[arg(
            long,
            help = "Agent backend to use (claude, codex). Defaults to claude."
        )]
        agent: Option<String>,

        #[arg(
            long,
            conflicts_with = "auto_merge",
            help = "Skip PR lifecycle monitoring after PR creation (fire-and-forget mode)"
        )]
        no_watch: bool,

        #[arg(
            long,
            conflicts_with = "no_watch",
            help = "Auto-merge PR when all readiness checks pass (adds gru:auto-merge label). Requires lifecycle monitoring (incompatible with --no-watch)."
        )]
        auto_merge: bool,

        #[arg(
            short = 'd',
            long,
            help = "Detach immediately after spawning the background worker (don't follow logs)"
        )]
        detach: bool,

        #[arg(
            long,
            conflicts_with = "detach",
            help = "Pause to review the assembled prompt before launching the agent"
        )]
        discuss: bool,

        #[arg(long, help = "Skip dependency checking entirely")]
        ignore_deps: bool,

        #[arg(
            long,
            hide = true,
            help = "Internal flag: run as background worker process for a previously-registered minion"
        )]
        worker: Option<String>,
    },
    #[command(about = "View logs from a Minion's event stream", alias = "lg")]
    Logs {
        #[arg(help = "Minion ID, issue number, or PR number (e.g., M001, 42)")]
        id: String,

        #[arg(
            short = 'f',
            long = "follow",
            conflicts_with = "no_follow",
            help = "Force follow mode (replay + stream live events)"
        )]
        follow: bool,

        #[arg(
            long = "no-follow",
            conflicts_with = "follow",
            help = "Replay history only, don't follow live events"
        )]
        no_follow: bool,

        #[arg(long, help = "Output raw JSONL for piping/scripting")]
        raw: bool,

        #[arg(
            short = 'n',
            long = "lines",
            help = "Number of events to show (default: all for stopped, 20 before following for running)"
        )]
        lines: Option<usize>,
    },
    #[command(about = "Stream a Minion's event log (alias for 'logs')", hide = true)]
    Tail {
        #[arg(help = "Minion ID, issue number, or PR number (e.g., M0tk, 42)")]
        id: String,

        #[arg(
            long = "no-follow",
            help = "Don't follow live events, just replay history"
        )]
        no_follow: bool,

        #[arg(long, help = "Output raw JSONL for piping/scripting")]
        raw: bool,

        #[arg(
            short = 'n',
            long = "lines",
            help = "Number of events to show (default: all for stopped, 20 before following for running)"
        )]
        lines: Option<usize>,
    },
    #[command(about = "Review a GitHub pull request")]
    Review {
        #[arg(
            help = "PR number, URL, Minion ID, or issue number. Auto-detects from current worktree if omitted."
        )]
        pr: Option<String>,

        #[arg(
            long,
            help = "Agent backend to use (e.g., 'claude'). Overrides config.toml default."
        )]
        agent: Option<String>,
    },
    #[command(about = "Rebase a Minion's branch onto the latest base branch")]
    Rebase {
        #[arg(
            help = "Issue number, PR number, Minion ID, or URL. Auto-detects from current worktree if omitted."
        )]
        target: Option<String>,

        #[arg(
            long,
            help = "Force-push the rebased branch to origin after a successful rebase"
        )]
        push: bool,

        #[arg(
            short,
            long,
            requires = "push",
            help = "Skip confirmation prompt before force-pushing (requires --push)"
        )]
        yes: bool,

        #[arg(
            short,
            long,
            help = "Maximum duration for Claude conflict resolution (e.g., '10m', '30m', '1h'). Defaults to 30 minutes."
        )]
        timeout: Option<String>,
    },
    #[command(about = "Get the filesystem path to a Minion's worktree")]
    Path {
        #[arg(help = "Minion ID, issue number, or PR number (e.g., M42, 42)")]
        id: String,
    },
    #[command(about = "Attach to a Minion's Claude session", alias = "at")]
    Attach {
        #[arg(help = "Minion ID, issue number, or PR number (e.g., M0tk, 42)")]
        id: String,

        #[arg(
            short = 'y',
            long,
            help = "Skip permission prompts (adds --dangerously-skip-permissions)"
        )]
        yolo: bool,

        #[arg(
            long,
            help = "Skip auto-resume prompt after exiting interactive session"
        )]
        no_auto_resume: bool,
    },
    #[command(about = "Resume a Minion in autonomous mode (stream monitoring + auto-PR)")]
    Resume {
        #[arg(help = "Minion ID, issue number, or PR number (e.g., M0tk, 42)")]
        id: String,

        #[arg(long, help = "Additional instructions to pass to Claude when resuming")]
        prompt: Option<String>,

        #[arg(
            short,
            long,
            help = "Maximum duration for the task (e.g., '10s', '5m', '1h'). Exits with error if exceeded."
        )]
        timeout: Option<String>,
    },
    #[command(about = "Clean up merged/closed worktrees")]
    Clean {
        #[arg(long, help = "Show what would be cleaned without removing")]
        dry_run: bool,
        #[arg(long, help = "Force removal without confirmation")]
        force: bool,
        #[arg(long, default_value = "main", help = "Base branch to check for merges")]
        base_branch: String,
    },
    #[command(about = "List active Minions", alias = "st")]
    Status {
        #[arg(help = "Optional ID to filter by (minion ID, issue number, or PR number)")]
        id: Option<String>,

        #[arg(short, long, help = "Show session ID, PID, and worktree path details")]
        verbose: bool,

        #[arg(short, long, help = "Show all entries including archived")]
        all: bool,
    },
    #[command(about = "Stop a running Minion")]
    Stop {
        #[arg(help = "Minion ID, issue number, or PR number (e.g., M0tk, 42)")]
        id: String,

        #[arg(long, help = "Force kill (SIGKILL instead of SIGTERM)")]
        force: bool,
    },
    #[command(about = "Run an ad-hoc prompt with an agent")]
    Prompt {
        #[arg(help = "Prompt text or prompt name to send to the agent")]
        prompt: String,

        #[arg(
            long,
            help = "Show detailed information about this prompt (description, parameters, source)",
            conflicts_with_all = ["issue", "pr", "no_worktree", "worktree", "params", "timeout", "agent"]
        )]
        info: bool,

        #[arg(
            short,
            long,
            help = "GitHub issue number or URL for context (populates {{ issue_number }}, {{ issue_title }}, {{ issue_body }})"
        )]
        issue: Option<String>,

        #[arg(
            long,
            conflicts_with = "worktree",
            help = "Skip automatic worktree setup when --issue or --pr is provided; run in CWD instead"
        )]
        no_worktree: bool,

        #[arg(
            short = 'w',
            long,
            conflicts_with = "no_worktree",
            value_name = "PATH",
            help = "Use an explicit worktree path instead of auto-creating one"
        )]
        worktree: Option<String>,

        #[arg(
            short,
            long,
            help = "GitHub PR number or URL for context (populates {{ pr_number }}, {{ pr_title }}, {{ pr_body }})"
        )]
        pr: Option<String>,

        #[arg(
            short = 'P',
            long = "param",
            help = "Custom parameter as key=value (can be repeated)",
            value_name = "KEY=VALUE"
        )]
        params: Vec<String>,

        #[arg(
            short,
            long,
            help = "Maximum duration for the task (e.g., '10s', '5m', '1h'). Exits with error if exceeded."
        )]
        timeout: Option<String>,

        #[arg(
            long,
            help = "Agent backend to use (e.g., 'claude'). Overrides config.toml default."
        )]
        agent: Option<String>,
    },
    #[command(about = "List available prompts")]
    Prompts,
    #[command(about = "Start Gru as an MCP server")]
    Mcp {
        #[command(subcommand)]
        action: Option<McpAction>,
    },
    #[command(about = "Run Gru Lab in daemon mode to automatically work on issues")]
    Lab {
        #[arg(long, help = "Path to config file (default: ~/.gru/config.toml)")]
        config: Option<std::path::PathBuf>,

        #[arg(
            long,
            help = "Repositories to monitor (comma-separated, e.g., owner/repo1,owner/repo2)",
            value_delimiter = ',',
            num_args = 1..
        )]
        repos: Option<Vec<String>>,

        #[arg(long, help = "Polling interval in seconds (overrides config)")]
        poll_interval: Option<u64>,

        #[arg(long, help = "Maximum concurrent Minion slots (overrides config)")]
        slots: Option<usize>,

        #[arg(long, help = "Disable auto-resuming interrupted Minions")]
        no_resume: bool,

        #[arg(
            long,
            help = "Kill running Minions on shutdown (default: detach and let them continue)"
        )]
        stop_minions: bool,
    },
    #[command(about = "Generate shell completions", hide = true)]
    Completions { shell: clap_complete::Shell },
}

/// MCP server subcommands
#[derive(Subcommand)]
enum McpAction {
    #[command(about = "Register Gru as an MCP server in ~/.claude.json")]
    Install,
    #[command(about = "Remove Gru MCP server from ~/.claude.json")]
    Uninstall,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Initialize logger based on quiet flag
    let log_filter = if cli.quiet { "error" } else { "warn" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(log_filter))
        .format_timestamp(None)
        .init();

    let result = match cli.command {
        Commands::Init { repo, host } => {
            init::handle_init(repo.unwrap_or_else(|| ".".to_string()), host).await
        }
        Commands::Chat { repo, verbose } => chat::handle_chat(repo, verbose).await,
        Commands::Pm { prompt, verbose } => pm::handle_pm(prompt, verbose).await,
        Commands::Tpm { prompt, verbose } => pm::handle_tpm(prompt, verbose).await,
        Commands::Do {
            issue,
            timeout,
            review_timeout,
            monitor_timeout,
            force_new,
            agent,
            no_watch,
            auto_merge,
            detach,
            discuss,
            ignore_deps,
            worker,
        } => {
            let agent_name = agent.unwrap_or_else(|| agent_registry::DEFAULT_AGENT.to_string());
            fix::handle_fix(
                &issue,
                fix::FixOptions {
                    timeout,
                    review_timeout,
                    monitor_timeout,
                    quiet: cli.quiet,
                    force_new,
                    agent_name,
                    no_watch,
                    auto_merge,
                    detach,
                    discuss,
                    ignore_deps,
                    worker,
                },
            )
            .await
        }
        Commands::Logs {
            id,
            follow,
            no_follow,
            raw,
            lines,
        } => logs::handle_logs(id, follow, no_follow, raw, lines, cli.quiet).await,
        Commands::Tail {
            id,
            no_follow,
            raw,
            lines,
        } => tail::handle_tail(id, no_follow, raw, lines, cli.quiet).await,
        Commands::Review { pr, agent } => {
            let agent_name = agent.unwrap_or_else(|| agent_registry::DEFAULT_AGENT.to_string());
            review::handle_review(pr, &agent_name).await
        }
        Commands::Rebase {
            target,
            push,
            yes,
            timeout,
        } => rebase::handle_rebase(target, push, yes, timeout.as_deref()).await,
        Commands::Path { id } => path::handle_path(id).await,
        Commands::Attach {
            id,
            yolo,
            no_auto_resume,
        } => attach::handle_attach(id, yolo, no_auto_resume, cli.quiet).await,
        Commands::Resume {
            id,
            prompt,
            timeout,
        } => resume::handle_resume(id, prompt, timeout, cli.quiet).await,
        Commands::Clean {
            dry_run,
            force,
            base_branch,
        } => clean::handle_clean(dry_run, force, &base_branch).await,
        Commands::Status { id, verbose, all } => status::handle_status(id, verbose, all).await,
        Commands::Stop { id, force } => stop::handle_stop(id, force).await,
        Commands::Prompt {
            prompt,
            info,
            issue,
            pr,
            no_worktree,
            worktree,
            params,
            timeout,
            agent,
        } => {
            if info {
                prompt::handle_prompt_info(&prompt).await
            } else {
                let agent_name = agent.unwrap_or_else(|| agent_registry::DEFAULT_AGENT.to_string());
                prompt::handle_prompt(
                    &prompt,
                    prompt::PromptOptions {
                        issue,
                        pr,
                        no_worktree,
                        worktree,
                        params,
                        timeout,
                        quiet: cli.quiet,
                        agent_name,
                    },
                )
                .await
            }
        }
        Commands::Prompts => prompts::handle_prompts().await,
        Commands::Mcp { action } => match action {
            None => commands::mcp::handle_mcp_server().await,
            Some(McpAction::Install) => commands::mcp::handle_mcp_install().await,
            Some(McpAction::Uninstall) => commands::mcp::handle_mcp_uninstall().await,
        },
        Commands::Lab {
            config,
            repos,
            poll_interval,
            slots,
            no_resume,
            stop_minions,
        } => lab::handle_lab(config, repos, poll_interval, slots, no_resume, stop_minions).await,
        Commands::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "gru", &mut std::io::stdout());
            Ok(0)
        }
    };

    // Handle any errors that occurred
    match result {
        Ok(exit_code) => std::process::exit(exit_code),
        Err(e) => {
            log::error!("{:#}", e);
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readme_mentions_all_cli_subcommands() {
        let readme = include_str!("../README.md");
        let cmd = Cli::command();

        let mut missing = Vec::new();
        for sub in cmd.get_subcommands() {
            if sub.is_hide_set() {
                continue;
            }
            let name = sub.get_name();
            // Check for "gru <name>" followed by a non-alphanumeric character
            // (word boundary) to avoid "gru prompt" matching "gru prompts"
            let pattern = format!("gru {name}");
            let found = readme.match_indices(&pattern).any(|(i, _)| {
                let after = i + pattern.len();
                after >= readme.len() || !readme.as_bytes()[after].is_ascii_alphanumeric()
            });
            if !found {
                missing.push(name.to_string());
            }
        }

        assert!(
            missing.is_empty(),
            "README.md is missing documentation for these CLI subcommands: {:?}",
            missing
        );
    }

    #[test]
    fn completions_generates_nonempty_output() {
        let mut buf = Vec::new();
        clap_complete::generate(
            clap_complete::Shell::Bash,
            &mut Cli::command(),
            "gru",
            &mut buf,
        );
        assert!(
            !buf.is_empty(),
            "bash completions output should not be empty"
        );
    }
}
