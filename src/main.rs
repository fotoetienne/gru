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
}

fn handle_fix(issue: &str) -> ! {
    let result = Command::new("claude")
        .arg("/fix")
        .arg(issue)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status();

    match result {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
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
    }
}
