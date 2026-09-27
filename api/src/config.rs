use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result, bail};
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Serialize};

use crate::VmmCommand;
use crate::logger::UpdateLogger;

static BINDINGS: LazyLock<crate::helpers::config::Config<VmmCommand>> = LazyLock::new(|| {
    crate::metadata::config_bindings().expect("Failed making bindings for api config")
});

/// Parse merged operation configuration, with process settings already removed.
fn parse_vmm_commands(value: serde_json::Value) -> Result<Vec<VmmCommand>> {
    BINDINGS.parse(value)
}

pub fn schema(generator: &mut SchemaGenerator) -> Schema {
    BINDINGS.schema(generator)
}

#[derive(Debug, Default, Serialize)]
pub struct Config {
    pub dragonball: ProcessConfig,
    pub commands: Vec<VmmCommand>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let value = parse_config_internal(path, &mut Default::default())?;
        serde_json::from_value(value)
            .with_context(|| format!("interpret config fields from {}", path.display()))
    }
}

impl<'de> Deserialize<'de> for Config {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        use serde_json::{Map, Value};

        let mut object = Map::<String, Value>::deserialize(deserializer)?;
        let dragonball = match object.remove("dragonball") {
            Some(value) => serde_json::from_value(value).map_err(D::Error::custom)?,
            None => ProcessConfig::default(),
        };
        let commands = crate::config::parse_vmm_commands(Value::Object(object)).map_err(D::Error::custom)?;
        Ok(Self {
            dragonball,
            commands,
        })
    }
}

/// Process-level config.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
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

impl ProcessConfig {
    pub fn id(&self) -> &str {
        self.id.as_deref().unwrap()
    }
}

fn parse_config_internal(path: &Path, visited: &mut HashSet<PathBuf>) -> Result<serde_json::Value> {
    let path = path
        .canonicalize()
        .with_context(|| format!("canonicalize config path {}", path.display()))?;
    if !visited.insert(path.clone()) {
        bail!(
            "circular inheritance detected in config file: {}",
            path.display()
        );
    }

    let bytes = fs::read(&path).with_context(|| format!("read config file {}", path.display()))?;

    let mut value: serde_json::Value = match path.extension() {
        Some(s) if s.eq_ignore_ascii_case("toml") => toml::from_slice(&bytes)
            .with_context(|| format!("parse toml config {}", path.display()))?,
        _ => serde_json::from_slice(&bytes)
            .with_context(|| format!("parse json config {}", path.display()))?,
    };

    let serde_json::Value::Object(object) = &mut value else {
        bail!("expected a config object in {}", path.display());
    };
    if let Some(schema) = object.remove("$schema")
        && !schema.is_string()
    {
        bail!("invalid `$schema` field type in {}", path.display());
    }

    if let Some(extends_val) = object.remove("extends") {
        let Some(extends_str) = extends_val.as_str().filter(|path| !path.is_empty()) else {
            bail!("expected a nonempty `extends` path in {}", path.display());
        };

        let parent_dir = path.parent().unwrap_or(Path::new(""));
        let base_path = parent_dir.join(extends_str);
        let mut base_value = parse_config_internal(&base_path, visited).with_context(|| {
            format!(
                "parse base config {} extended by {}",
                base_path.display(),
                path.display()
            )
        })?;

        merge_value(&mut base_value, value);

        visited.remove(&path);
        return Ok(base_value);
    }

    visited.remove(&path);
    Ok(value)
}

fn merge_value(base: &mut serde_json::Value, overlay: serde_json::Value) {
    match (base, overlay) {
        (serde_json::Value::Object(base), serde_json::Value::Object(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(base_value) => merge_value(base_value, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        // Anything else -- scalars, nulls, type changes and arrays -- replaces what
        // it lands on. Extending an inherited array instead would leave no way to
        // shorten one, and no way to clear it.
        (base, overlay) => *base = overlay,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use serde_json::json;

    use super::*;

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let sequence = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "dragonball-config-test-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn join(&self, path: impl AsRef<Path>) -> PathBuf {
            self.0.join(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn config_preserves_dragonball_key_and_commands() {
        let config: Config = serde_json::from_value(json!({
            "dragonball": { "id": "from-file", "logger": { "show_tid": false } },
            "machine": { "disks": [{}] }
        }))
        .unwrap();

        assert_eq!(config.dragonball.id(), "from-file");
        assert_eq!(config.dragonball.logger.show_tid, Some(false));
        assert!(config.dragonball.api_sock.is_none());
        assert!(config.dragonball.kvm_dev.is_none());
        assert!(config.dragonball.logger.level.is_none());
        assert!(matches!(
            config.commands.as_slice(),
            [VmmCommand::AddDisk(_, ())]
        ));

        let serialized = serde_json::to_value(config).unwrap();
        assert_eq!(serialized["dragonball"]["id"], "from-file");
        assert!(serialized.get("process").is_none());
        assert_eq!(serialized["commands"], json!([{ "add-disk": {} }]));
    }

    #[test]
    fn config_defaults_do_not_apply_runtime_settings() {
        let config: Config = serde_json::from_value(json!({})).unwrap();
        assert!(config.dragonball.id.is_none());
        assert!(config.dragonball.kvm_dev.is_none());
        assert!(config.dragonball.logger.level.is_none());
        assert!(config.commands.is_empty());
    }

    #[test]
    fn config_rejects_unknown_and_invalid_process_settings() {
        for value in [
            json!({ "process": {} }),
            json!({ "dragonball": { "unknown": true } }),
            json!({ "dragonball": { "id": 1 } }),
            json!({ "dragonball": { "logger": { "target": "invalid" } } }),
        ] {
            assert!(serde_json::from_value::<Config>(value).is_err());
        }
    }

    #[test]
    fn config_load_preserves_inheritance_and_metadata() {
        let dir = TestDir::new();
        fs::write(
            dir.join("base.toml"),
            "[dragonball]\nid = 'parent'\nkvm_dev = '/dev/custom-kvm'\n[[machine.disks]]\n",
        )
        .unwrap();
        let child = dir.join("child.json");
        fs::write(
            &child,
            r#"{
                "$schema": "unused.schema.json",
                "extends": "base.toml",
                "dragonball": {"id": "child"},
                "machine": {"disks": []}
            }"#,
        )
        .unwrap();

        let config = Config::load(&child).unwrap();
        assert_eq!(config.dragonball.id(), "child");
        assert_eq!(
            config.dragonball.kvm_dev.as_deref(),
            Some(Path::new("/dev/custom-kvm"))
        );
        assert!(config.commands.is_empty());
    }

    #[test]
    fn merge_value_applies_config_inheritance_semantics() {
        let mut base = json!({
            "object": {
                "inherited": 1,
                "overridden": "parent",
                "nested": {
                    "kept": true,
                    "kept_array": [{ "deep": true }, 2],
                    "replaced": [1]
                }
            },
            "array": [1, { "parent": true }],
            // A parent array no overlay mentions survives whole, nested or not.
            "untouched_array": ["parent", 0],
            "emptied": ["parent"],
            "cleared": {
                "inherited": true
            },
            "replaced_type": {
                "old": true
            }
        });
        let overlay = json!({
            "object": {
                "overridden": "child",
                "added": 2,
                "nested": {
                    "replaced": [2, 3]
                }
            },
            "array": [2, { "child": true }],
            "emptied": [],
            "cleared": null,
            "replaced_type": ["new"]
        });

        merge_value(&mut base, overlay);

        assert_eq!(
            base,
            json!({
                "object": {
                    "inherited": 1,
                    "overridden": "child",
                    "added": 2,
                    "nested": {
                        "kept": true,
                        "kept_array": [{ "deep": true }, 2],
                        "replaced": [2, 3]
                    }
                },
                "array": [2, { "child": true }],
                "untouched_array": ["parent", 0],
                "emptied": [],
                "cleared": null,
                "replaced_type": ["new"]
            })
        );
    }

    #[test]
    fn parse_config_resolves_and_merges_relative_parent() {
        let dir = TestDir::new();
        let parent = dir.join("parent.toml");
        let child = dir.join("child.json");
        fs::write(
            &parent,
            r#"
array = [1]
cleared = "parent"

[object]
inherited = 1
overridden = "parent"
"#,
        )
        .unwrap();
        fs::write(
            &child,
            r#"{
                "extends": "parent.toml",
                "object": {"overridden": "child", "added": 2},
                "array": [2],
                "cleared": null
            }"#,
        )
        .unwrap();

        let value = parse_config_internal(&child, &mut Default::default()).unwrap();

        assert_eq!(
            value,
            json!({
                "object": {
                    "inherited": 1,
                    "overridden": "child",
                    "added": 2
                },
                "array": [2],
                "cleared": null
            })
        );
    }

    #[test]
    fn parse_config_merges_through_several_generations() {
        let dir = TestDir::new();
        fs::write(
            dir.join("grandparent.toml"),
            r#"
drives = ["grandparent"]
nics = ["grandparent-a", "grandparent-b"]
scalar = 1

[object]
from_grandparent = true
shared = "grandparent"
ports = [1, 2]
"#,
        )
        .unwrap();
        fs::write(
            dir.join("parent.toml"),
            r#"
extends = "grandparent.toml"
drives = ["parent"]
scalar = 2

[object]
from_parent = true
shared = "parent"
"#,
        )
        .unwrap();
        fs::write(
            dir.join("child.json"),
            r#"{
                "extends": "parent.toml",
                "drives": [],
                "object": {"shared": "child"}
            }"#,
        )
        .unwrap();

        let value =
            parse_config_internal(&dir.join("child.json"), &mut Default::default()).unwrap();

        // Each generation replaces the list whole, so the last one can empty it,
        // while objects keep merging and keys nobody mentions survive.
        assert_eq!(
            value,
            json!({
                "drives": [],
                "nics": ["grandparent-a", "grandparent-b"],
                "scalar": 2,
                "object": {
                    "from_grandparent": true,
                    "from_parent": true,
                    "shared": "child",
                    "ports": [1, 2]
                }
            })
        );
    }

    #[test]
    fn parse_config_errors_identify_relevant_files() {
        let dir = TestDir::new();
        let malformed = dir.join("malformed.json");
        fs::write(&malformed, "{").unwrap();

        let error = parse_config_internal(&malformed, &mut Default::default()).unwrap_err();
        assert!(format!("{error:#}").contains(malformed.to_str().unwrap()));

        let child = dir.join("child.json");
        let missing_parent = dir.join("missing.json");
        fs::write(&child, r#"{"extends": "missing.json"}"#).unwrap();

        let error = parse_config_internal(&child, &mut Default::default()).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains(child.to_str().unwrap()));
        assert!(message.contains(missing_parent.to_str().unwrap()));
    }
}
