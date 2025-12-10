mod commands;
mod git;
mod github;
mod logger;
mod minion;
mod minion_resolver;
mod pr_state;
mod progress;
mod progress_comments;
mod stream;
mod text_buffer;
mod url_utils;
mod workspace;
mod worktree_scanner;

use clap::{Parser, Subcommand};
use commands::{clean, fix, path, resume, review, status};

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
    },
    #[command(about = "Review a GitHub pull request")]
    Review {
        #[arg(help = "PR number or URL to review")]
        pr: String,
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
    #[command(about = "Resume a Minion's Claude session")]
    Resume {
        #[arg(help = "Minion ID, issue number, or PR number (e.g., M0tk, 42)")]
        id: String,
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
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Fix { issue, timeout } => fix::handle_fix(&issue, timeout, cli.quiet).await,
        Commands::Review { pr } => review::handle_review(&pr).await,
        Commands::Path { id, issue, pr } => path::handle_path(id, issue, pr).await,
        Commands::Resume { id } => resume::handle_resume(id).await,
        Commands::Clean {
            dry_run,
            force,
            base_branch,
        } => clean::handle_clean(dry_run, force, &base_branch).await,
        Commands::Status { id } => status::handle_status(id).await,
    };

    // Handle any errors that occurred
    match result {
        Ok(exit_code) => std::process::exit(exit_code),
        Err(e) => {
            eprintln!("Error: {:#}", e);
            std::process::exit(1);
        }
    }
}
