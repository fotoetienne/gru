mod commands;
mod git;
mod github;
mod logger;
mod minion;
mod progress;
mod stream;
mod url_utils;
mod workspace;
mod worktree_scanner;

use clap::{Parser, Subcommand};
use commands::{clean, fix, path, review};

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
    },
    #[command(about = "Review a GitHub pull request")]
    Review {
        #[arg(help = "PR number or URL to review")]
        pr: String,
    },
    #[command(about = "Get the filesystem path to a Minion's worktree")]
    Path {
        #[arg(help = "Minion ID (e.g., M42 or 42)", conflicts_with_all = ["issue", "pr"])]
        minion_id: Option<String>,

        #[arg(long, help = "Resolve from issue number", conflicts_with_all = ["minion_id", "pr"])]
        issue: Option<u64>,

        #[arg(long, help = "Resolve from PR number", conflicts_with_all = ["minion_id", "issue"])]
        pr: Option<u64>,
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
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Fix { issue } => fix::handle_fix(&issue, cli.quiet).await,
        Commands::Review { pr } => review::handle_review(&pr).await,
        Commands::Path {
            minion_id,
            issue,
            pr,
        } => path::handle_path(minion_id, issue, pr).await,
        Commands::Clean {
            dry_run,
            force,
            base_branch,
        } => clean::handle_clean(dry_run, force, &base_branch).await,
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
