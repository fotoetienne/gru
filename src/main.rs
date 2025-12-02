use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::process::Command;

/// CLI structure for the Gru agent orchestrator
#[derive(Parser)]
#[command(name = "gru")]
#[command(version)]
#[command(about = "Local-First LLM Agent Orchestrator", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

/// Available commands for Gru
#[derive(Subcommand)]
enum Commands {
    #[command(about = "Fix a GitHub issue")]
    Fix {
        #[arg(help = "Issue number or URL to fix")]
        issue: String,
    },
}

/// Validates that the issue argument is either a number or a valid GitHub URL
fn validate_issue_format(issue: &str) -> Result<()> {
    // Check if it's a number
    if issue.parse::<u32>().is_ok() {
        return Ok(());
    }

    // Check if it's a GitHub URL
    if issue.starts_with("https://github.com/") && issue.contains("/issues/") {
        return Ok(());
    }

    anyhow::bail!(
        "Invalid issue format. Expected: <number> or <github-url>\n\
         Examples:\n\
         - gru fix 42\n\
         - gru fix https://github.com/owner/repo/issues/42"
    );
}

/// Handles the fix command by delegating to the Claude CLI
fn handle_fix(issue: &str) -> Result<()> {
    // Validate the issue format before proceeding
    validate_issue_format(issue)?;

    // Execute the claude CLI with the /fix command
    let status = Command::new("claude")
        .arg(format!("/fix {}", issue))
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .context(
            "claude command not found. Install from: https://github.com/anthropics/claude-code"
        )?;

    // Exit with the same code as the claude process
    std::process::exit(status.code().unwrap_or(1));
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Fix { issue } => handle_fix(&issue),
    };

    // Handle any errors that occurred
    if let Err(e) = result {
        eprintln!("Error: {:#}", e);
        std::process::exit(1);
    }
}
