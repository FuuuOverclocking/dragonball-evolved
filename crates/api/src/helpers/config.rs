use std::collections::BTreeMap;

#[cfg(feature = "schema")]
use schemars::{Schema, SchemaGenerator, json_schema};
use serde::de::Error as _;
use serde::de::value::Error;
#[cfg(feature = "schema")]
use serde_json::Map;
use serde_json::Value;

pub(crate) struct Binding<C> {
    pub path: &'static str,
    pub parse: fn(Value) -> Result<C, Error>,
    #[cfg(feature = "schema")]
    pub schema: fn(&mut SchemaGenerator) -> Schema,
}

struct Entry<C> {
    binding: Binding<C>,
    path: &'static str,
    many: bool,
}

#[derive(Default)]
struct Node {
    binding: Option<usize>,
    children: BTreeMap<&'static str, Node>,
}

pub(crate) struct ConfigBindings<C> {
    root: Node,
    entries: Vec<Entry<C>>,
}

impl<C> ConfigBindings<C> {
    pub(crate) fn new(bindings: impl IntoIterator<Item = Binding<C>>) -> Result<Self, Error> {
        let mut config = Self {
            root: Node::default(),
            entries: Vec::new(),
        };
        for binding in bindings {
            let path = binding.path.strip_suffix("[]").unwrap_or(binding.path);
            let many = path != binding.path;
            let segments = path.strip_prefix('.').ok_or_else(|| {
                Error::custom(format_args!("config binding must start with '.': {path}"))
            })?;
            let mut node = &mut config.root;
            for segment in segments.split('.') {
                if segment.is_empty()
                    || !segment
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                {
                    return Err(Error::custom(format_args!(
                        "invalid config binding: {}",
                        binding.path
                    )));
                }
                if node.binding.is_some() {
                    return Err(Error::custom(format_args!(
                        "overlapping config binding: {path}"
                    )));
                }
                node = node.children.entry(segment).or_default();
            }
            if node.binding.is_some() || !node.children.is_empty() {
                return Err(Error::custom(format_args!(
                    "duplicate or overlapping config binding: {path}"
                )));
            }
            node.binding = Some(config.entries.len());
            config.entries.push(Entry {
                binding,
                path,
                many,
            });
        }
        Ok(config)
    }

    pub(crate) fn parse(&self, value: Value) -> Result<Vec<C>, Error> {
        let mut values = vec![None; self.entries.len()];
        self.root.collect(value, "", &mut values)?;

        let mut commands = Vec::new();
        for (entry, value) in self.entries.iter().zip(values) {
            let Some(value) = value else {
                continue;
            };
            if entry.many {
                let Value::Array(items) = value else {
                    return Err(Error::custom(format_args!(
                        "{}: expected an array",
                        entry.path
                    )));
                };
                for (index, item) in items.into_iter().enumerate() {
                    commands.push((entry.binding.parse)(item).map_err(|error| {
                        Error::custom(format_args!("{}[{index}]: {error}", entry.path))
                    })?);
                }
            } else {
                commands.push(
                    (entry.binding.parse)(value)
                        .map_err(|error| Error::custom(format_args!("{}: {error}", entry.path)))?,
                );
            }
        }
        Ok(commands)
    }

    #[cfg(feature = "schema")]
    pub(crate) fn schema(&self, generator: &mut SchemaGenerator) -> Schema {
        self.node_schema(&self.root, generator)
    }

    #[cfg(feature = "schema")]
    fn node_schema(&self, node: &Node, generator: &mut SchemaGenerator) -> Schema {
        if let Some(index) = node.binding {
            let entry = &self.entries[index];
            let schema = (entry.binding.schema)(generator);
            return if entry.many {
                json_schema!({ "type": "array", "items": schema })
            } else {
                schema
            };
        }

        let properties: Map<String, Value> = node
            .children
            .iter()
            .map(|(name, child)| {
                (
                    (*name).into(),
                    self.node_schema(child, generator).to_value(),
                )
            })
            .collect();
        json_schema!({
            "type": "object",
            "properties": properties,
            "additionalProperties": false,
        })
    }
}

impl Node {
    fn collect(&self, value: Value, path: &str, values: &mut [Option<Value>]) -> Result<(), Error> {
        if let Some(index) = self.binding {
            values[index] = Some(value);
            return Ok(());
        }
        let Value::Object(object) = value else {
            return Err(Error::custom(format_args!(
                "{}: expected an object",
                if path.is_empty() { "." } else { path },
            )));
        };
        for (key, value) in object {
            let path = format!("{path}.{key}");
            let child = self
                .children
                .get(key.as_str())
                .ok_or_else(|| Error::custom(format_args!("unknown config field: {path}")))?;
            child.collect(value, &path, values)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn binding(path: &'static str) -> Binding<u64> {
        Binding {
            path,
            parse: |value| serde_json::from_value(value).map_err(Error::custom),
            #[cfg(feature = "schema")]
            schema: |generator| generator.subschema_for::<u64>(),
        }
    }

    #[test]
    fn parsing_preserves_binding_order_and_array_order() {
        let config = ConfigBindings::new([binding(".z"), binding(".machine.disks[]")]).unwrap();
        assert_eq!(
            config
                .parse(json!({ "machine": { "disks": [2, 1] }, "z": 3 }))
                .unwrap(),
            [3, 2, 1]
        );
        assert!(config.parse(json!({})).unwrap().is_empty());
        assert!(
            config
                .parse(json!({ "machine": { "disks": [] } }))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn invalid_configuration_reports_the_field_path() {
        let config = ConfigBindings::new([binding(".machine.disks[]"), binding(".z")]).unwrap();
        for (value, message) in [
            (json!(null), ".: expected an object"),
            (json!({ "machine": null }), ".machine: expected an object"),
            (
                json!({ "machine": { "unknown": 1 } }),
                "unknown config field: .machine.unknown",
            ),
            (
                json!({ "machine": { "disks": {} } }),
                ".machine.disks: expected an array",
            ),
            (
                json!({ "machine": { "disks": ["invalid"] } }),
                ".machine.disks[0]: invalid type: string \"invalid\", expected u64",
            ),
            (
                json!({ "z": "invalid" }),
                ".z: invalid type: string \"invalid\", expected u64",
            ),
        ] {
            assert_eq!(config.parse(value).unwrap_err().to_string(), message);
        }
    }

    #[test]
    fn invalid_bindings_are_rejected() {
        for paths in [
            vec!["machine.disks"],
            vec![".machine..disks"],
            vec![".machine.disks", ".machine.disks"],
            vec![".machine", ".machine.disks"],
            vec![".machine.disks", ".machine"],
        ] {
            assert!(ConfigBindings::new(paths.into_iter().map(binding)).is_err());
        }
    }

    #[cfg(feature = "schema")]
    #[test]
    fn schema_preserves_object_and_array_shapes() {
        let config = ConfigBindings::new([binding(".machine.disks[]"), binding(".z")]).unwrap();
        let mut generator = SchemaGenerator::default();
        let schema = config.schema(&mut generator).to_value();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
        let machine = &schema["properties"]["machine"];
        assert_eq!(machine["additionalProperties"], false);
        assert_eq!(machine["properties"]["disks"]["type"], "array");
        let integer = generator.subschema_for::<u64>().to_value();
        assert_eq!(machine["properties"]["disks"]["items"], integer);
        assert_eq!(schema["properties"]["z"], integer);
    }
}
