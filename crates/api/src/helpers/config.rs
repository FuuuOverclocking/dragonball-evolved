#[cfg(feature = "schema")]
use schemars::{Schema, SchemaGenerator, json_schema};
use serde::de::Error as _;
use serde::de::value::Error;
#[cfg(feature = "schema")]
use serde_json::Map;
use serde_json::Value;

use super::metadata::{ConfigPath, Op, path_prefix, str_eq};

const NONE: usize = usize::MAX;

#[derive(Clone, Copy)]
struct Node {
    name: &'static str,
    path: &'static str,
    binding: usize,
    child: usize,
    sibling: usize,
}

impl Node {
    const EMPTY: Self = Self {
        name: "",
        path: "",
        binding: NONE,
        child: NONE,
        sibling: NONE,
    };
}

pub(crate) const fn node_count<C>(operations: &[Op<C>]) -> usize {
    let mut count = 1;
    let mut index = 0;
    while index < operations.len() {
        if let Some(path) = operations[index].config {
            let path = ConfigPath::new(path).path;
            let mut end = 1;
            while end <= path.len() {
                if end == path.len() || path.as_bytes()[end] == b'.' {
                    let prefix = path.split_at(end).0;
                    let mut shared = false;
                    let mut previous = 0;
                    while previous < index {
                        if let Some(other) = operations[previous].config
                            && path_prefix(prefix, ConfigPath::new(other).path)
                        {
                            shared = true;
                            break;
                        }
                        previous += 1;
                    }
                    if !shared {
                        count += 1;
                    }
                }
                end += 1;
            }
        }
        index += 1;
    }
    count
}

pub(crate) struct ConfigBindings<C: 'static, const N: usize> {
    nodes: [Node; N],
    operations: &'static [Op<C>],
}

impl<C, const N: usize> ConfigBindings<C, N> {
    pub(crate) const fn new(operations: &'static [Op<C>]) -> Self {
        assert!(N > 0, "config tree needs a root");
        let mut nodes = [Node::EMPTY; N];
        let mut used = 1;
        let mut index = 0;
        while index < operations.len() {
            if let Some(path) = operations[index].config {
                let path = ConfigPath::new(path).path;
                let mut parent = 0;
                let mut start = 1;
                while start < path.len() {
                    assert!(nodes[parent].binding == NONE, "overlapping config binding");
                    let mut end = start;
                    while end < path.len() && path.as_bytes()[end] != b'.' {
                        end += 1;
                    }
                    let name = path.split_at(end).0.split_at(start).1;
                    let mut child = nodes[parent].child;
                    while child != NONE && !str_eq(nodes[child].name, name) {
                        child = nodes[child].sibling;
                    }
                    if child == NONE {
                        assert!(used < N, "config node count is too small");
                        child = used;
                        nodes[child] = Node {
                            name,
                            path: path.split_at(end).0,
                            sibling: nodes[parent].child,
                            ..Node::EMPTY
                        };
                        nodes[parent].child = child;
                        used += 1;
                    }
                    parent = child;
                    start = end + 1;
                }
                assert!(
                    nodes[parent].binding == NONE && nodes[parent].child == NONE,
                    "duplicate or overlapping config binding"
                );
                nodes[parent].binding = index;
            }
            index += 1;
        }
        assert!(used == N, "config node count is too large");
        Self { nodes, operations }
    }

    pub(crate) fn parse(&self, value: Value) -> Result<Vec<C>, Error> {
        let mut values = vec![None; self.operations.len()];
        self.collect(0, value, &mut values)?;

        let mut commands = Vec::new();
        for (op, value) in self.operations.iter().zip(values) {
            let (Some(path), Some(value)) = (op.config, value) else {
                continue;
            };
            let ConfigPath { path, many } = ConfigPath::new(path);
            if many {
                let Value::Array(items) = value else {
                    return Err(Error::custom(format_args!("{path}: expected an array")));
                };
                for (index, item) in items.into_iter().enumerate() {
                    commands.push((op.parse)(item.into()).map_err(|error| {
                        Error::custom(format_args!("{path}[{index}]: {error}"))
                    })?);
                }
            } else {
                commands.push(
                    (op.parse)(value.into())
                        .map_err(|error| Error::custom(format_args!("{path}: {error}")))?,
                );
            }
        }
        Ok(commands)
    }

    fn collect(
        &self,
        index: usize,
        value: Value,
        values: &mut [Option<Value>],
    ) -> Result<(), Error> {
        let node = &self.nodes[index];
        if node.binding != NONE {
            values[node.binding] = Some(value);
            return Ok(());
        }
        let Value::Object(object) = value else {
            return Err(Error::custom(format_args!(
                "{}: expected an object",
                if node.path.is_empty() { "." } else { node.path },
            )));
        };
        for (key, value) in object {
            let mut child = node.child;
            while child != NONE && self.nodes[child].name != key {
                child = self.nodes[child].sibling;
            }
            if child == NONE {
                return Err(Error::custom(format_args!(
                    "unknown config field: {}.{key}",
                    node.path
                )));
            }
            self.collect(child, value, values)?;
        }
        Ok(())
    }

    #[cfg(feature = "schema")]
    pub(crate) fn schema(&self, generator: &mut SchemaGenerator) -> Schema {
        self.node_schema(0, generator)
    }

    #[cfg(feature = "schema")]
    fn node_schema(&self, index: usize, generator: &mut SchemaGenerator) -> Schema {
        let node = &self.nodes[index];
        if node.binding != NONE {
            let op = &self.operations[node.binding];
            let schema = (op.request)(generator);
            return if ConfigPath::new(op.config.unwrap()).many {
                json_schema!({ "type": "array", "items": schema })
            } else {
                schema
            };
        }

        let mut children = Vec::new();
        let mut child = node.child;
        while child != NONE {
            children.push(child);
            child = self.nodes[child].sibling;
        }
        children.sort_unstable_by_key(|&child| self.nodes[child].name);
        let properties: Map<String, Value> = children
            .into_iter()
            .map(|child| {
                (
                    self.nodes[child].name.into(),
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const fn op(path: &'static str) -> Op<u64> {
        Op {
            name: path,
            config: Some(path),
            route: None,
            parse: |input| input.deserialize().map_err(Error::custom),
            #[cfg(feature = "schema")]
            request: |generator| generator.subschema_for::<u64>(),
            #[cfg(feature = "schema")]
            response: |generator| generator.subschema_for::<()>(),
        }
    }

    #[test]
    fn parsing_preserves_binding_order_and_array_order() {
        static OPERATIONS: &[Op<u64>] = &[
            op(".z"),
            Op {
                config: None,
                ..op(".unbound")
            },
            op(".machine.disks[]"),
        ];
        static CONFIG: ConfigBindings<u64, { node_count(OPERATIONS) }> =
            ConfigBindings::new(OPERATIONS);
        assert!(std::ptr::eq(CONFIG.operations, OPERATIONS));
        assert_eq!(CONFIG.nodes.len(), 4);
        assert_eq!(
            CONFIG
                .parse(json!({ "machine": { "disks": [2, 1] }, "z": 3 }))
                .unwrap(),
            [3, 2, 1]
        );
        assert!(CONFIG.parse(json!({})).unwrap().is_empty());
        assert!(
            CONFIG
                .parse(json!({ "machine": { "disks": [] } }))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn invalid_configuration_reports_the_field_path() {
        static CONFIG: ConfigBindings<u64, 4> =
            ConfigBindings::new(&[op(".machine.disks[]"), op(".z")]);
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
            assert_eq!(CONFIG.parse(value).unwrap_err().to_string(), message);
        }
    }

    #[cfg(feature = "schema")]
    #[test]
    fn schema_preserves_object_and_array_shapes() {
        static CONFIG: ConfigBindings<u64, 4> =
            ConfigBindings::new(&[op(".machine.disks[]"), op(".z")]);
        let mut generator = SchemaGenerator::default();
        let schema = CONFIG.schema(&mut generator).to_value();
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
