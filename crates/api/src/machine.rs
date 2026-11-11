use std::path::PathBuf;

use api_macros::api_type;

#[api_type]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SetCpus(pub u32);

#[api_type]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SetMemoryMib(pub u32);

#[api_type]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SetConsole(pub ConsoleConfig);

/// Where the guest console is connected: `stdio` or `unix=<path>`.
#[api_type(serialize = false)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[serde(try_from = "String")]
#[schemars(with = "String", extend("pattern" = r"^(stdio|unix=[\s\S]+)$"))]
pub enum ConsoleConfig {
    #[default]
    Stdio,
    /// The VMM listens on this Unix socket and serves one client at a time.
    Unix(PathBuf),
}

impl TryFrom<String> for ConsoleConfig {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "stdio" => Ok(Self::Stdio),
            value => value
                .strip_prefix("unix=")
                .filter(|path| !path.is_empty())
                .map(|path| Self::Unix(path.into()))
                .ok_or("expected `stdio` or `unix=<path>`"),
        }
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for ConsoleConfig {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Stdio => serializer.serialize_str("stdio"),
            Self::Unix(path) => {
                let path = path
                    .to_str()
                    .filter(|path| !path.is_empty())
                    .ok_or_else(|| {
                        serde::ser::Error::custom("console socket path must be nonempty UTF-8")
                    })?;
                serializer.collect_str(&format_args!("unix={path}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_accepts_stdio_and_nonempty_unix_paths() {
        assert_eq!(
            ConsoleConfig::try_from("stdio".to_owned()),
            Ok(ConsoleConfig::Stdio)
        );
        assert_eq!(
            ConsoleConfig::try_from("unix=/run/vm/console.sock".to_owned()),
            Ok(ConsoleConfig::Unix("/run/vm/console.sock".into()))
        );
        for value in ["", "unix=", "unix", "Stdio", "file=/tmp/console"] {
            assert!(ConsoleConfig::try_from(value.to_owned()).is_err());
        }
    }

    #[cfg(feature = "serde")]
    #[test]
    fn console_roundtrips_through_its_string_form() {
        for (config, value) in [
            (ConsoleConfig::Stdio, "stdio"),
            (
                ConsoleConfig::Unix("/run/vm/console.sock".into()),
                "unix=/run/vm/console.sock",
            ),
        ] {
            assert_eq!(serde_json::to_value(&config).unwrap(), value);
            assert_eq!(
                serde_json::from_value::<SetConsole>(value.into()).unwrap(),
                SetConsole(config)
            );
        }
    }
}
