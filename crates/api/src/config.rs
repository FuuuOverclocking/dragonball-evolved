use std::path::PathBuf;

use crate::VmmCommand;
use crate::logger::UpdateLogger;

#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct Config {
    pub dragonball: ProcessConfig,
    pub commands: Vec<VmmCommand>,
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Config {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        use serde_json::{Map, Value};

        let mut object = Map::<String, Value>::deserialize(deserializer)?;
        let dragonball = match object.remove("dragonball") {
            Some(value) => serde_json::from_value(value).map_err(|error| {
                D::Error::custom(format_args!("parse process config (.dragonball): {error}"))
            })?,
            None => ProcessConfig::default(),
        };
        let commands = crate::metadata::CONFIG_BINDINGS
            .parse(Value::Object(object))
            .map_err(D::Error::custom)?;
        Ok(Self {
            dragonball,
            commands,
        })
    }
}

#[cfg(feature = "schema")]
pub fn schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    crate::metadata::CONFIG_BINDINGS.schema(generator)
}

/// Process-level config.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default, deny_unknown_fields))]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProcessConfig {
    /// The ID of the dragonball instance, default to a random string.
    pub id: Option<String>,

    /// Launch an API server and specify the path to the api socket.
    pub api_sock: Option<PathBuf>,

    /// Specify the path to the KVM device.
    pub kvm_dev: Option<PathBuf>,

    /// Initial logger configuration. Omitted fields use the process defaults.
    pub logger: UpdateLogger,
}
