use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(about = "Export API documents or validate a configuration without starting a VMM")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Export {
        #[arg(long, default_value = "dist/schema")]
        output_dir: PathBuf,
    },
    Check {
        path: PathBuf,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Export { output_dir } => {
            let documents = dragonball_evolved::schema::generate()?;
            let schema = serde_json::to_string_pretty(&documents.schema)? + "\n";
            let openapi = serde_yaml::to_string(&documents.openapi)?;
            fs::create_dir_all(&output_dir)
                .with_context(|| format!("create {}", output_dir.display()))?;
            for (name, content) in [("vm.schema.json", schema), ("openapi.yaml", openapi)] {
                let path = output_dir.join(name);
                fs::write(&path, content).with_context(|| format!("write {}", path.display()))?;
                println!("{}", path.display());
            }
        }
        Command::Check { path } => {
            let config = dragonball_evolved::config::load(&path)?;
            println!("{}", serde_json::to_string_pretty(&config)?);
        }
    }
    Ok(())
}
