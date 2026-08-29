use std::fmt;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

use crate::VERSION_LONG;

/// Launch a dragonball VMM.
///
/// When boot sources provided, the VM will start directly. Alternatively,
/// a Unix Domain Socket can be created using `--api-sock`, allowing the VM to
/// be controlled through a RESTful API.
#[derive(Debug, Parser)]
#[command(version(VERSION_LONG))]
pub struct Cli {
    /// The ID of the dragonball instance, default to a random string.
    #[arg(long)]
    pub id: Option<String>,

    /// Launch an API server and specify the path to the api socket.
    #[arg(long)]
    pub api_sock: Option<PathBuf>,

    /// Run the minimal KVM + async virtio-block proof of concept with this disk.
    #[arg(long)]
    pub poc_disk: Option<PathBuf>,

    /// Optional subcommand.
    #[command(subcommand)]
    pub command: Option<SubCommand>,
}

/// Subcommands for the dragonball CLI.
#[derive(Debug, Subcommand)]
pub enum SubCommand {}

impl Cli {
    /// Parse command-line arguments and customize defaults.
    ///
    /// This function parses command-line arguments and applies customizations
    /// such as generating a random ID if none was provided.
    pub fn parse_custom() -> impl CliOps + fmt::Debug {
        let mut cli = Self::parse();
        cli.customize();

        cli
    }

    fn customize(&mut self) {
        if self.id.is_none() {
            let random_id = names::Generator::default().next().unwrap();
            self.id = Some(random_id);
        }
    }
}

/// Operations that can be performed on CLI options.
///
/// This trait provides an interface for accessing various configuration options
/// that can be specified via command-line arguments.
pub trait CliOps {
    /// Get the ID of the dragonball sandbox.
    fn id(&self) -> &str;

    /// Get the path to the API socket file.
    fn api_sock(&self) -> Option<&Path>;

    /// Get the disk used by the KVM proof of concept.
    fn poc_disk(&self) -> Option<&Path>;

    /// Get the active subcommand, if any.
    fn subcommand(&self) -> Option<&SubCommand>;
}

impl CliOps for Cli {
    fn id(&self) -> &str {
        // Safe to unwrap, because `customize()` ensures `id` is always set.
        self.id.as_ref().unwrap()
    }

    fn api_sock(&self) -> Option<&Path> {
        self.api_sock.as_deref()
    }

    fn poc_disk(&self) -> Option<&Path> {
        self.poc_disk.as_deref()
    }

    fn subcommand(&self) -> Option<&SubCommand> {
        self.command.as_ref()
    }
}
