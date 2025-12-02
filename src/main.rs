use clap::{Parser, Subcommand};

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
        #[arg(help = "Issue number to fix")]
        issue: u32,
    },
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Fix { issue: _ } => {
            println!("Not implemented");
        }
    }
}
