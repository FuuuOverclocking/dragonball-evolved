use std::fmt;
use std::path::{Path, PathBuf};

use clap::{ArgAction, Parser, Subcommand};
use logger::LevelFilter;
use logger_backend::{Config, CrashTarget, Format, Target};

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
    id: Option<String>,

    /// Launch an API server and specify the path to the api socket.
    #[arg(long, value_name = "PATH")]
    api_sock: Option<PathBuf>,

    /// Specify the path to the KVM device.
    #[arg(long, value_name = "PATH")]
    kvm_dev: Option<PathBuf>,

    /// The lowest level a record has to reach to be logged: `off`, `error`,
    /// `warn`, `info`, `debug` or `trace`.
    #[arg(long, value_name = "LEVEL", default_value = "info")]
    log_level: LevelFilter,

    /// Where log records go: `stderr` or `file=<path>`.
    #[arg(long, value_name = "TARGET", default_value = "stderr")]
    log_target: Target,

    /// Where crash records go: `same_as_log`, `stderr` or `file=<path>`.
    ///
    /// Defaults to following the log target: under a supervisor stderr is often
    /// `/dev/null`, and a crash record that lands there is one nobody reads.
    #[arg(long, value_name = "TARGET", default_value = "same_as_log")]
    log_crash_target: CrashTarget,

    /// How a log line is laid out: `text` or `json`.
    #[arg(long, value_name = "FORMAT", default_value = "text")]
    log_format: Format,

    /// Whether a log line carries the thread id.
    #[arg(long, value_name = "BOOL", default_value_t = true, action = ArgAction::Set)]
    log_show_tid: bool,

    /// Whether a log line carries the thread name.
    #[arg(long, value_name = "BOOL", default_value_t = true, action = ArgAction::Set)]
    log_show_thread_name: bool,

    /// Whether a log line carries the target, i.e. the module that logged it.
    #[arg(long, value_name = "BOOL", default_value_t = true, action = ArgAction::Set)]
    log_show_target: bool,

    /// Whether a log line carries the source file and line.
    #[arg(long, value_name = "BOOL", default_value_t = true, action = ArgAction::Set)]
    log_show_file_line: bool,

    /// Whether a log line carries the instance id.
    #[arg(long, value_name = "BOOL", default_value_t = true, action = ArgAction::Set)]
    log_show_id: bool,

    /// What the `--log-*` arguments add up to, assembled by `customize`.
    #[arg(skip)]
    logger_config: Config,

    /// Optional subcommand.
    #[command(subcommand)]
    command: Option<SubCommand>,
}

/// Subcommands for the dragonball CLI.
#[derive(Debug, Subcommand)]
pub enum SubCommand {}

impl Cli {
    /// Parse command-line arguments and customize defaults.
    ///
    /// This function parses command-line arguments and applies customizations
    /// such as generating a random ID if none was provided.
    pub fn parse_custom() -> Self {
        let mut cli = Self::parse();
        cli.customize();

        cli
    }

    fn customize(&mut self) {
        if self.id.is_none() {
            let random_id = names::Generator::default().next().unwrap();
            self.id = Some(random_id);
        }

        // The logger takes one value per setting, so every `--log-*` argument
        // ends up as a `Some` here; the defaults come from clap, not from the
        // logger.
        self.logger_config = Config {
            id: Some(self.id().to_owned()),
            level: Some(self.log_level),
            target: Some(self.log_target.clone()),
            crash_target: Some(self.log_crash_target.clone()),
            format: Some(self.log_format),
            show_tid: Some(self.log_show_tid),
            show_thread_name: Some(self.log_show_thread_name),
            show_target: Some(self.log_show_target),
            show_file_line: Some(self.log_show_file_line),
            show_id: Some(self.log_show_id),
        };
    }
}

/// Methods for accessing CLI arguments.
impl Cli {
    pub fn id(&self) -> &str {
        // Safe to unwrap, because `customize()` ensures `id` is always set.
        self.id.as_ref().unwrap()
    }

    pub fn api_sock(&self) -> Option<&Path> {
        self.api_sock.as_deref()
    }

    pub fn kvm_dev(&self) -> &Path {
        self.kvm_dev.as_deref().unwrap_or(Path::new("/dev/kvm"))
    }

    pub fn logger_config(&self) -> &Config {
        &self.logger_config
    }

    pub fn subcommand(&self) -> Option<&SubCommand> {
        self.command.as_ref()
    }
}

impl fmt::Display for Cli {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Cli {{ id: {}, api_sock: {:?}, kvm_dev: {}, logger_config: {:?} }}",
            self.id(),
            self.api_sock(),
            self.kvm_dev().display(),
            self.logger_config()
        )
    }
}
