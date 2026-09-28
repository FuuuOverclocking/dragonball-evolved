use std::path::PathBuf;

/// Update the logger configuration. Omitted fields leave their current values unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default, deny_unknown_fields))]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UpdateLogger {
    /// Lowest enabled log level: off, error, warn, info, debug or trace (case-insensitive).
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "serde",
    serde(rename_all = "lowercase", try_from = "String")
)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(
    feature = "schema",
    schemars(
        with = "String",
        extend("pattern" = "^([Oo][Ff][Ff]|[Ee][Rr][Rr][Oo][Rr]|[Ww][Aa][Rr][Nn]|[Ii][Nn][Ff][Oo]|[Dd][Ee][Bb][Uu][Gg]|[Tt][Rr][Aa][Cc][Ee])$")
    )
)]
pub enum LevelFilter {
    #[default]
    Off,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl TryFrom<String> for LevelFilter {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, &'static str> {
        match value.as_str() {
            value if value.eq_ignore_ascii_case("off") => Ok(Self::Off),
            value if value.eq_ignore_ascii_case("error") => Ok(Self::Error),
            value if value.eq_ignore_ascii_case("warn") => Ok(Self::Warn),
            value if value.eq_ignore_ascii_case("info") => Ok(Self::Info),
            value if value.eq_ignore_ascii_case("debug") => Ok(Self::Debug),
            value if value.eq_ignore_ascii_case("trace") => Ok(Self::Trace),
            _ => Err("expected `off`, `error`, `warn`, `info`, `debug` or `trace`"),
        }
    }
}

/// How a line is laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum Format {
    #[default]
    Text,
    Json,
}

/// Where lines go: `stderr` or `file=<path>`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "String"))]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(
    feature = "schema",
    schemars(with = "String", extend("pattern" = r"^(stderr|file=[\s\S]+)$"))
)]
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

#[cfg(feature = "serde")]
impl serde::Serialize for Target {
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
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "String"))]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(
    feature = "schema",
    schemars(
        with = "String",
        extend("pattern" = r"^(same_as_log|stderr|file=[\s\S]+)$")
    )
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

#[cfg(feature = "serde")]
impl serde::Serialize for CrashTarget {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::SameAsLog => serializer.serialize_str("same_as_log"),
            Self::Own(target) => target.serialize(serializer),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_accept_all_ascii_casings() {
        for (name, expected) in [
            ("off", LevelFilter::Off),
            ("error", LevelFilter::Error),
            ("warn", LevelFilter::Warn),
            ("info", LevelFilter::Info),
            ("debug", LevelFilter::Debug),
            ("trace", LevelFilter::Trace),
        ] {
            for mask in 0..(1 << name.len()) {
                let spelling: String = name
                    .chars()
                    .enumerate()
                    .map(|(i, c)| {
                        if mask & (1 << i) != 0 {
                            c.to_ascii_uppercase()
                        } else {
                            c
                        }
                    })
                    .collect();
                assert_eq!(LevelFilter::try_from(spelling), Ok(expected));
            }
        }
        assert_eq!(LevelFilter::default(), LevelFilter::Off);
    }

    #[test]
    fn levels_reject_unknown_names_and_whitespace() {
        for value in [
            "", "warning", "fatal", " info", "info ", "info\n", "0", "İnfo",
        ] {
            assert!(LevelFilter::try_from(value.to_owned()).is_err());
        }
    }
}
