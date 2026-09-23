use std::collections::BTreeMap;

use anyhow::{Error, Result, anyhow};
use schemars::{Schema, SchemaGenerator, json_schema};
use serde_json::{Map, Value};

pub(crate) struct Binding<C> {
    pub path: &'static str,
    pub parse: fn(Value) -> Result<C>,
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

pub(crate) struct Config<C> {
    root: Node,
    entries: Vec<Entry<C>>,
}

impl<C> Config<C> {
    pub(crate) fn new(bindings: impl IntoIterator<Item = Binding<C>>) -> Result<Self> {
        let mut config = Self {
            root: Node::default(),
            entries: Vec::new(),
        };
        for binding in bindings {
            let path = binding.path.strip_suffix("[]").unwrap_or(binding.path);
            let many = path != binding.path;
            let segments = path
                .strip_prefix('.')
                .ok_or_else(|| anyhow!("config binding must start with '.': {path}"))?;
            let mut node = &mut config.root;
            for segment in segments.split('.') {
                if segment.is_empty()
                    || !segment
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                {
                    return Err(Error::msg(format!(
                        "invalid config binding: {}",
                        binding.path
                    )));
                }
                if node.binding.is_some() {
                    return Err(Error::msg(format!("overlapping config binding: {path}")));
                }
                node = node.children.entry(segment).or_default();
            }
            if node.binding.is_some() || !node.children.is_empty() {
                return Err(Error::msg(format!(
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

    pub(crate) fn parse(&self, value: Value) -> Result<Vec<C>> {
        let mut values = vec![None; self.entries.len()];
        self.root.collect(value, "", &mut values)?;

        let mut commands = Vec::new();
        for (entry, value) in self.entries.iter().zip(values) {
            let Some(value) = value else {
                continue;
            };
            if entry.many {
                let Value::Array(items) = value else {
                    return Err(Error::msg(format!("{}: expected an array", entry.path)));
                };
                for (index, item) in items.into_iter().enumerate() {
                    commands.push((entry.binding.parse)(item).map_err(|error| {
                        Error::msg(format!("{}[{index}]: {error}", entry.path))
                    })?);
                }
            } else {
                commands.push(
                    (entry.binding.parse)(value)
                        .map_err(|error| Error::msg(format!("{}: {error}", entry.path)))?,
                );
            }
        }
        Ok(commands)
    }

    pub(crate) fn schema(&self, generator: &mut SchemaGenerator) -> Schema {
        self.node_schema(&self.root, generator)
    }

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
    fn collect(&self, value: Value, path: &str, values: &mut [Option<Value>]) -> Result<()> {
        if let Some(index) = self.binding {
            values[index] = Some(value);
            return Ok(());
        }
        let Value::Object(object) = value else {
            return Err(Error::msg(format!(
                "{}: expected an object",
                if path.is_empty() { "." } else { path },
            )));
        };
        for (key, value) in object {
            let path = format!("{path}.{key}");
            let child = self
                .children
                .get(key.as_str())
                .ok_or_else(|| Error::msg(format!("unknown config field: {path}")))?;
            child.collect(value, &path, values)?;
        }
        Ok(())
    }
}
