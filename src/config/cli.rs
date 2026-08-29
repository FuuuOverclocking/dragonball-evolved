use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::VERSION_LONG;
use crate::config::DragonballConfig;

/// Start a Dragonball VMM instance.
///
/// If boot sources are provided, the virtual machine will boot directly.
/// Alternatively, you can use `--api-sock` to create a Unix domain socket,
/// enabling control of the VM via a RESTful API.
#[derive(Debug, Parser)]
#[command(version(VERSION_LONG))]
pub(super) struct Cli {
    /// Specify the path to the config file. Could be json or toml, recognized by extension.
    /// The settings can be overwritten by CLI arguments or env vars.
    #[arg(short = 'c', long, value_name = "PATH")]
    pub(super) config: Option<PathBuf>,

    #[command(flatten)]
    pub(super) dragonball: DragonballConfig,

    #[command(subcommand)]
    pub(super) command: Option<SubCommand>,
}

/// Subcommands for the dragonball CLI.
#[derive(Debug, Subcommand)]
pub(crate) enum SubCommand {}
