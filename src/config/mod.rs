mod cli;
mod parse;
mod serde_helper;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub(crate) use self::cli::SubCommand;
pub(crate) use self::parse::parse_cli;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Config {
    pub(crate) dragonball: DragonballConfig,
    pub(crate) machine: MachineConfig,
}

/// Process-level config.
#[derive(clap::Args, Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct DragonballConfig {
    /// The ID of the dragonball instance, default to a random string.
    #[arg(long, env = "DRAGONBALL_ID")]
    pub(crate) id: Option<String>,

    /// Launch an API server and specify the path to the api socket.
    #[arg(long, value_name = "PATH", env)]
    pub(crate) api_sock: Option<PathBuf>,

    /// Specify the path to the KVM device.
    #[arg(long, value_name = "PATH", env)]
    pub(crate) kvm_dev: Option<PathBuf>,

    #[command(flatten)]
    pub(crate) logger: LoggerConfig,
}

impl DragonballConfig {
    pub(crate) fn id(&self) -> &str {
        self.id.as_deref().unwrap()
    }

    pub(crate) fn api_sock(&self) -> Option<&Path> {
        self.api_sock.as_deref()
    }

    pub(crate) fn kvm_dev(&self) -> &Path {
        self.kvm_dev.as_deref().unwrap()
    }

    pub(crate) fn logger(&self) -> &LoggerConfig {
        &self.logger
    }
}

#[derive(clap::Args, Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct LoggerConfig {
    /// The lowest level a record has to reach to be logged: `off`, `error`,
    /// `warn`, `info`, `debug` or `trace`. Default: `info`.
    #[arg(long = "log-level", value_name = "LEVEL", env = "LOG_LEVEL")]
    level: Option<logger::LevelFilter>,

    /// Where log records go: `stderr` or `file=<path>`. Default: `stderr`.
    #[arg(long = "log-target", value_name = "TARGET", env = "LOG_TARGET")]
    #[serde(with = "serde_helper::optional_str")]
    target: Option<logger_backend::Target>,

    /// Where crash records go: `same_as_log`, `stderr` or `file=<path>`.
    ///
    /// Defaults to following the log target: under a supervisor stderr is often
    /// `/dev/null`, and a crash record that lands there is one nobody reads.
    #[arg(
        long = "log-crash-target",
        value_name = "TARGET",
        env = "LOG_CRASH_TARGET"
    )]
    #[serde(with = "serde_helper::optional_str")]
    crash_target: Option<logger_backend::CrashTarget>,

    /// How a log line is laid out: `text` or `json`. Default: `text`.
    #[arg(long = "log-format", value_name = "FORMAT", env = "LOG_FORMAT")]
    #[serde(with = "serde_helper::optional_str")]
    format: Option<logger_backend::Format>,

    /// Whether a log line carries the thread id. Default: true.
    #[arg(long = "log-show-tid", value_name = "BOOL", env = "LOG_SHOW_TID")]
    show_tid: Option<bool>,

    /// Whether a log line carries the thread name. Default: true.
    #[arg(
        long = "log-show-thread-name",
        value_name = "BOOL",
        env = "LOG_SHOW_THREAD_NAME"
    )]
    show_thread_name: Option<bool>,

    /// Whether a log line carries the target, i.e. the module that logged it. Default: true.
    #[arg(long = "log-show-target", value_name = "BOOL", env = "LOG_SHOW_TARGET")]
    show_target: Option<bool>,

    /// Whether a log line carries the source file and line. Default: true.
    #[arg(
        long = "log-show-file-line",
        value_name = "BOOL",
        env = "LOG_SHOW_FILE_LINE"
    )]
    show_file_line: Option<bool>,

    /// Whether a log line carries the instance id. Default: true.
    #[arg(long = "log-show-id", value_name = "BOOL", env = "LOG_SHOW_ID")]
    show_id: Option<bool>,
}

impl From<LoggerConfig> for logger_backend::Config {
    fn from(config: LoggerConfig) -> Self {
        let LoggerConfig {
            level,
            target,
            crash_target,
            format,
            show_tid,
            show_thread_name,
            show_target,
            show_file_line,
            show_id,
        } = config;

        Self {
            id: None,
            level,
            target,
            crash_target,
            format,
            show_tid,
            show_thread_name,
            show_target,
            show_file_line,
            show_id,
        }
    }
}

/// 暂时放这儿.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MachineConfig {}
