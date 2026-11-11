use std::path::PathBuf;

use api_macros::api_type;

use crate::VmmCommand;
use crate::logger::LoggerConfig;

#[api_type(deserialize = false, schema = false)]
#[derive(Debug, Default)]
pub struct Config {
    pub dragonball: ProcessConfig,
    pub commands: Vec<VmmCommand>,
}

#[cfg(feature = "serde")]
impl Config {
    pub fn parse(value: serde_json::Value) -> Result<Self, serde::de::value::Error> {
        use serde::de::Error as _;
        use serde::de::value::Error;
        use serde_json::Value;

        let Value::Object(mut object) = value else {
            return Err(Error::custom("expected a config object"));
        };
        let dragonball = match object.remove("dragonball") {
            Some(value) => serde_json::from_value(value).map_err(|error| {
                Error::custom(format_args!("parse process config (.dragonball): {error}"))
            })?,
            None => ProcessConfig::default(),
        };
        let commands = crate::metadata::CONFIG_BINDINGS.parse(Value::Object(object))?;
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
#[api_type]
#[derive(Debug, Clone, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ProcessConfig {
    /// The ID of the dragonball instance, default to a random string.
    pub id: Option<String>,

    /// Launch an API server and specify the path to the api socket.
    pub api_sock: Option<PathBuf>,

    /// Specify the path to the KVM device.
    pub kvm_dev: Option<PathBuf>,

    /// Initial logger configuration. Omitted fields use the process defaults.
    pub logger: LoggerConfig,
}

#[cfg(all(test, feature = "serde"))]
mod tests {
    use super::*;
    use crate::boot::{BootConfig, SetBoot};
    use crate::disk::{AddDisk, DiskConfig};
    use crate::machine::{ConsoleConfig, SetConsole, SetCpus, SetMemoryMib};

    #[test]
    fn example_machine_section_becomes_commands() {
        let config = Config::parse(serde_json::json!({
            "machine": {
                "cpus": 2,
                "memory_mib": 1024,
                "console": "stdio",
                "boot": { "kernel": "/path/to/pvh-vmlinux" },
                "disks": [{
                    "id": "rootfs",
                    "path": "/path/to/rootfs.ext4",
                    "read_only": true,
                    "root": true,
                }],
            },
        }))
        .unwrap();
        let commands: Vec<_> = config
            .commands
            .into_iter()
            .map(|command| format!("{command:?}"))
            .collect();
        let expected = [
            format!("{:?}", VmmCommand::SetCpus(SetCpus(2), ())),
            format!("{:?}", VmmCommand::SetMemoryMib(SetMemoryMib(1024), ())),
            format!(
                "{:?}",
                VmmCommand::SetConsole(SetConsole(ConsoleConfig::Stdio), ())
            ),
            format!(
                "{:?}",
                VmmCommand::SetBoot(
                    SetBoot(BootConfig {
                        kernel: "/path/to/pvh-vmlinux".into(),
                        cmdline: None,
                        initrd: None,
                    }),
                    (),
                )
            ),
            format!(
                "{:?}",
                VmmCommand::AddDisk(
                    AddDisk(DiskConfig {
                        id: "rootfs".into(),
                        path: "/path/to/rootfs.ext4".into(),
                        read_only: true,
                        root: true,
                    }),
                    (),
                )
            ),
        ];
        assert_eq!(commands, expected);
    }
}
