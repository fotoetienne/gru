use clap::{Parser, Subcommand};
use std::process::Command;

#[derive(Parser)]
#[command(name = "gru")]
#[command(version)]
#[command(about = "Local-First LLM Agent Orchestrator", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

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
}

fn handle_fix(issue: &str) -> ! {
    let result = Command::new("claude")
        .arg(format!("/fix {}", issue))
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status();

    match result {
        Ok(status) => std::process::exit(status.code().unwrap_or(128)),
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                eprintln!("Error: 'claude' command not found. Please install Claude CLI first.");
            } else {
                eprintln!("Error launching claude: {}", e);
            }
            std::process::exit(1);
        }
    }
}

fn handle_review(pr: &str) -> ! {
    let result = Command::new("claude")
        .arg(format!("/pr_review {}", pr))
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status();

    match result {
        Ok(status) => std::process::exit(status.code().unwrap_or(128)),
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                eprintln!("Error: 'claude' command not found. Please install Claude CLI first.");
            } else {
                eprintln!("Error launching claude: {}", e);
            }
            std::process::exit(1);
        }
    }
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Fix { issue } => handle_fix(&issue),
        Commands::Review { pr } => handle_review(&pr),
    }
}
