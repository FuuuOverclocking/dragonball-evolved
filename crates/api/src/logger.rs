use std::path::PathBuf;

use log::LevelFilter;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Update the logger configuration. Omitted fields leave their current values unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct UpdateLogger {
    /// Lowest enabled log level: off, error, warn, info, debug or trace (case-insensitive).
    #[schemars(
        with = "Option<String>",
        regex(
            pattern = "^([Oo][Ff][Ff]|[Ee][Rr][Rr][Oo][Rr]|[Ww][Aa][Rr][Nn]|[Ii][Nn][Ff][Oo]|[Dd][Ee][Bb][Uu][Gg]|[Tt][Rr][Aa][Cc][Ee])$"
        )
    )]
    pub level: Option<LevelFilter>,
    /// Where normal log records are written.
    pub target: Option<Target>,
    /// Where crash records are written.
    pub crash_target: Option<CrashTarget>,
    /// How each log line is formatted.
    pub format: Option<Format>,
    /// Include the thread id in log records.
    pub show_tid: Option<bool>,
    /// Include the thread name in log records.
    pub show_thread_name: Option<bool>,
    /// Include the logging module in log records.
    pub show_target: Option<bool>,
    /// Include the source file and line in log records.
    pub show_file_line: Option<bool>,
    /// Include the instance id in log records.
    pub show_id: Option<bool>,
}

/// How a line is laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    #[default]
    Text,
    Json,
}

/// Where lines go: `stderr` or `file=<path>`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(try_from = "String")]
#[schemars(with = "String", extend("pattern" = r"^(stderr|file=[\s\S]+)$"))]
pub enum Target {
    Stderr,
    File(PathBuf),
}

impl TryFrom<String> for Target {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "stderr" => Ok(Self::Stderr),
            value => value
                .strip_prefix("file=")
                .filter(|path| !path.is_empty())
                .map(|path| Self::File(path.into()))
                .ok_or("expected `stderr` or `file=<path>`"),
        }
    }
}

impl Serialize for Target {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Stderr => serializer.serialize_str("stderr"),
            Self::File(path) => {
                let path = path
                    .to_str()
                    .filter(|path| !path.is_empty())
                    .ok_or_else(|| serde::ser::Error::custom("log path must be nonempty UTF-8"))?;
                serializer.collect_str(&format_args!("file={path}"))
            }
        }
    }
}

/// Where crash records go: `same_as_log`, `stderr` or `file=<path>`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(try_from = "String")]
#[schemars(
    with = "String",
    extend("pattern" = r"^(same_as_log|stderr|file=[\s\S]+)$")
)]
pub enum CrashTarget {
    /// Track whatever the normal log target is.
    #[default]
    SameAsLog,
    /// A target independent of the normal log.
    Own(Target),
}

impl TryFrom<String> for CrashTarget {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value == "same_as_log" {
            Ok(Self::SameAsLog)
        } else {
            Target::try_from(value)
                .map(Self::Own)
                .map_err(|_| "expected `same_as_log`, `stderr` or `file=<path>`")
        }
    }
}

impl Serialize for CrashTarget {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::SameAsLog => serializer.serialize_str("same_as_log"),
            Self::Own(target) => target.serialize(serializer),
        }
    }
}
