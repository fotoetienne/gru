mod ci;
mod claude_runner;
mod commands;
mod config;
mod git;
mod github;
mod minion;
mod minion_registry;
mod minion_resolver;
mod pr_monitor;
mod pr_state;
mod progress;
mod progress_comments;
mod prompt_loader;
mod prompt_renderer;
mod reserved_commands;
mod stream;
mod text_buffer;
mod url_utils;
mod workspace;
mod worktree_scanner;

use clap::{Parser, Subcommand};
use commands::{
    attach, clean, fix, init, lab, path, prompt, prompts, resume, review, status, stop,
};

/// CLI structure for the Gru agent orchestrator
#[derive(Parser)]
#[command(name = "gru")]
#[command(version)]
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
            help = "Repository to initialize: 'owner/repo' for GitHub, '.' for current directory, or a path"
        )]
        repo: String,
    },
    #[command(about = "Fix a GitHub issue")]
    Fix {
        #[arg(help = "Issue number or URL to fix")]
        issue: String,

        #[arg(
            short,
            long,
            help = "Maximum duration for the task (e.g., '10s', '5m', '1h'). Exits with error if exceeded."
        )]
        timeout: Option<String>,

        #[arg(
            long,
            help = "Create a new Minion even if one already exists for this issue"
        )]
        force_new: bool,
    },
    #[command(about = "Review a GitHub pull request")]
    Review {
        #[arg(
            help = "PR number, URL, Minion ID, or issue number. Auto-detects from current worktree if omitted."
        )]
        pr: Option<String>,
    },
    #[command(about = "Get the filesystem path to a Minion's worktree")]
    Path {
        #[arg(help = "Minion ID, issue number, or PR number (e.g., M42, 42)")]
        id: String,

        #[arg(long, help = "[DEPRECATED] Resolve from issue number")]
        issue: Option<u64>,

        #[arg(long, help = "[DEPRECATED] Resolve from PR number")]
        pr: Option<u64>,
    },
    #[command(about = "Attach to a Minion's Claude session")]
    Attach {
        #[arg(help = "Minion ID, issue number, or PR number (e.g., M0tk, 42)")]
        id: String,

        #[arg(
            long,
            help = "Skip permission prompts (adds --dangerously-skip-permissions)"
        )]
        yolo: bool,
    },
    #[command(about = "Resume a Minion's Claude session")]
    Resume {
        #[arg(help = "Minion ID, issue number, or PR number (e.g., M0tk, 42)")]
        id: String,

        #[arg(
            long,
            help = "Skip permission prompts (adds --dangerously-skip-permissions)"
        )]
        yolo: bool,
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
    #[command(about = "List active Minions")]
    Status {
        #[arg(help = "Optional ID to filter by (minion ID, issue number, or PR number)")]
        id: Option<String>,
    },
    #[command(about = "Stop a running Minion")]
    Stop {
        #[arg(help = "Minion ID, issue number, or PR number (e.g., M0tk, 42)")]
        id: String,
    },
    #[command(about = "Run an ad-hoc prompt with Claude")]
    Prompt {
        #[arg(help = "Prompt text to send to Claude")]
        prompt: String,

        #[arg(
            short,
            long,
            help = "GitHub issue number or URL for context (populates {{ issue_number }}, {{ issue_title }}, {{ issue_body }})"
        )]
        issue: Option<String>,

        #[arg(
            long,
            help = "Skip automatic worktree creation when --issue is provided"
        )]
        no_worktree: bool,

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
    },
    #[command(about = "List available prompts")]
    Prompts,
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
    },
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
        Commands::Init { repo } => init::handle_init(repo).await,
        Commands::Fix {
            issue,
            timeout,
            force_new,
        } => fix::handle_fix(&issue, timeout, cli.quiet, force_new).await,
        Commands::Review { pr } => review::handle_review(pr).await,
        Commands::Path { id, issue, pr } => path::handle_path(id, issue, pr).await,
        Commands::Attach { id, yolo } => attach::handle_attach(id, yolo).await,
        Commands::Resume { id, yolo } => resume::handle_resume(id, yolo).await,
        Commands::Clean {
            dry_run,
            force,
            base_branch,
        } => clean::handle_clean(dry_run, force, &base_branch).await,
        Commands::Status { id } => status::handle_status(id).await,
        Commands::Stop { id } => stop::handle_stop(id).await,
        Commands::Prompt {
            prompt,
            issue,
            no_worktree,
            params,
            timeout,
        } => prompt::handle_prompt(&prompt, issue, no_worktree, params, timeout, cli.quiet).await,
        Commands::Prompts => prompts::handle_prompts().await,
        Commands::Lab {
            config,
            repos,
            poll_interval,
            slots,
        } => lab::handle_lab(config, repos, poll_interval, slots).await,
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
