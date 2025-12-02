use clap::{Parser, Subcommand};
use anyhow::Result;

mod github;
mod git;
mod tmux;
mod minion;
mod cli;

#[derive(Parser)]
#[command(name = "gru", version, about = "Kick off AI Minions to fix issues")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Kick off a Minion to fix an issue
    Fix {
        /// GitHub issue URL
        issue_url: String,
    },
    /// Attach to a running Minion
    Attach {
        /// Minion ID
        minion_id: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Fix { issue_url } => cli::fix::run(issue_url).await,
        Commands::Attach { minion_id } => cli::attach::run(minion_id).await,
    }
}
