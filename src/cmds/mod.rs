use anyhow::Result;

use crate::cli::SubCommand;
use crate::config;

pub async fn run_subcommand(cmd: SubCommand) -> Result<()> {
    match cmd {
        SubCommand::CheckConfig { path } => {
            let config = config::load(&path)?;
            println!("{}", serde_json::to_string_pretty(&config)?);
        }
    }
    Ok(())
}
