#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SetCpus(pub VcpuCount);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VcpuCount(pub u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SetMemory(pub MemorySizeMib);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MemorySizeMib(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SetMemoryHotplug(pub MemoryHotplugConfig);

/// Memory hotplug capacity and optional block/chunk geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MemoryHotplugConfig {
    pub size_mib: u32,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub block_size_mib: Option<u32>,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub chunk_size_mib: Option<u32>,
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for MemoryHotplugConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            size_mib: u32,
            block_size_mib: Option<u32>,
            chunk_size_mib: Option<u32>,
        }

        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = MemoryHotplugConfig;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a memory hotplug configuration object")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> Result<Self::Value, A::Error> {
                let fields = <Fields as serde::Deserialize>::deserialize(
                    serde::de::value::MapAccessDeserializer::new(map),
                )?;
                Ok(MemoryHotplugConfig {
                    size_mib: fields.size_mib,
                    block_size_mib: fields.block_size_mib,
                    chunk_size_mib: fields.chunk_size_mib,
                })
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ResizeMemory(pub HotplugSizeMib);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HotplugSizeMib(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SetMemoryBacking(pub MemoryBacking);

/// What backs guest RAM on the host: `anonymous` private memory, or `shared`
/// memory with a file descriptor that other processes, such as vhost-user
/// backends, can map.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum MemoryBacking {
    #[default]
    Anonymous,
    Shared,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SetConsole(pub ConsoleConfig);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ConsoleConfig {
    #[default]
    Stdio,
    None,
}

#[cfg(all(test, feature = "serde"))]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn operation_wrappers_preserve_scalar_wire_shapes() {
        assert_eq!(
            serde_json::to_value(SetCpus(VcpuCount(2))).unwrap(),
            json!(2)
        );
        assert_eq!(
            serde_json::from_value::<SetCpus>(json!(2)).unwrap(),
            SetCpus(VcpuCount(2))
        );
        assert_eq!(
            serde_json::to_value(SetMemory(MemorySizeMib(1024))).unwrap(),
            json!(1024)
        );
        assert_eq!(
            serde_json::from_value::<SetMemory>(json!(1024)).unwrap(),
            SetMemory(MemorySizeMib(1024))
        );
        for size in [0, 128, u32::MAX] {
            assert_eq!(
                serde_json::to_value(ResizeMemory(HotplugSizeMib(size))).unwrap(),
                json!(size)
            );
            assert_eq!(
                serde_json::from_value::<ResizeMemory>(json!(size)).unwrap(),
                ResizeMemory(HotplugSizeMib(size))
            );
        }
        assert_eq!(
            serde_json::to_value(SetMemoryBacking(MemoryBacking::Shared)).unwrap(),
            json!("shared")
        );
        assert_eq!(
            serde_json::from_value::<SetMemoryBacking>(json!("anonymous")).unwrap(),
            SetMemoryBacking(MemoryBacking::Anonymous)
        );
        assert_eq!(
            serde_json::to_value(SetConsole(ConsoleConfig::Stdio)).unwrap(),
            json!("stdio")
        );
        assert_eq!(
            serde_json::from_value::<SetConsole>(json!("none")).unwrap(),
            SetConsole(ConsoleConfig::None)
        );
    }

    #[test]
    fn scalar_payloads_reject_other_shapes() {
        for value in [json!({ "cpus": 2 }), json!(-1), json!(256), json!("2")] {
            assert!(serde_json::from_value::<SetCpus>(value).is_err());
        }
        for value in [
            json!({ "memory_mib": 1024 }),
            json!(-1),
            json!(4_294_967_296_u64),
            json!("1024"),
        ] {
            assert!(serde_json::from_value::<SetMemory>(value).is_err());
        }
        for value in [
            json!({ "size_mib": 128 }),
            json!(-1),
            json!(4_294_967_296_u64),
            json!("128"),
            json!(1.5),
            json!(true),
            json!(null),
            json!([128]),
        ] {
            assert!(serde_json::from_value::<HotplugSizeMib>(value.clone()).is_err());
            assert!(serde_json::from_value::<ResizeMemory>(value).is_err());
        }
        for value in [json!("Stdio"), json!("None"), json!({}), json!(null)] {
            assert!(serde_json::from_value::<SetConsole>(value).is_err());
        }
        for value in [json!("Shared"), json!("memfd"), json!({}), json!(null)] {
            assert!(serde_json::from_value::<SetMemoryBacking>(value).is_err());
        }
    }

    #[test]
    fn memory_hotplug_size_is_required_and_optional_fields_are_omitted() {
        let value = json!({ "size_mib": 256 });
        let config = MemoryHotplugConfig {
            size_mib: 256,
            block_size_mib: None,
            chunk_size_mib: None,
        };
        assert_eq!(
            serde_json::from_value::<MemoryHotplugConfig>(value.clone()).unwrap(),
            config
        );
        assert_eq!(
            serde_json::from_value::<SetMemoryHotplug>(value.clone()).unwrap(),
            SetMemoryHotplug(config)
        );
        assert_eq!(serde_json::to_value(config).unwrap(), value);
        assert_eq!(
            serde_json::to_value(SetMemoryHotplug(config)).unwrap(),
            value
        );
    }

    #[test]
    fn memory_hotplug_object_round_trips_all_fields() {
        let value = json!({ "size_mib": 256, "block_size_mib": 2, "chunk_size_mib": 128 });
        let config = MemoryHotplugConfig {
            size_mib: 256,
            block_size_mib: Some(2),
            chunk_size_mib: Some(128),
        };
        assert_eq!(
            serde_json::from_value::<MemoryHotplugConfig>(value.clone()).unwrap(),
            config
        );
        assert_eq!(
            serde_json::from_value::<SetMemoryHotplug>(value.clone()).unwrap(),
            SetMemoryHotplug(config)
        );
        assert_eq!(serde_json::to_value(config).unwrap(), value);
        assert_eq!(
            serde_json::to_value(SetMemoryHotplug(config)).unwrap(),
            value
        );
    }

    #[test]
    fn memory_hotplug_rejects_missing_unknown_and_wrong_fields() {
        for value in [
            json!([]),
            json!([256]),
            json!([256, 2, 128]),
            json!({}),
            json!({ "block_size_mib": 2 }),
            json!({ "memory_mib": 256 }),
            json!({ "size_mib": 256, "unknown": true }),
            json!({ "size_mib": null }),
            json!({ "size_mib": "256" }),
            json!({ "size_mib": -1 }),
            json!({ "size_mib": 4_294_967_296_u64 }),
            json!({ "size_mib": 256, "block_size_mib": "2" }),
            json!({ "size_mib": 256, "chunk_size_mib": true }),
            json!({ "size_mib": 256, "block_size_mib": -1 }),
            json!({ "size_mib": 256, "chunk_size_mib": 4_294_967_296_u64 }),
            json!(256),
            json!(null),
        ] {
            assert!(serde_json::from_value::<MemoryHotplugConfig>(value.clone()).is_err());
            assert!(serde_json::from_value::<SetMemoryHotplug>(value).is_err());
        }
    }

    #[cfg(feature = "schema")]
    #[test]
    fn memory_hotplug_schema_requires_only_size_and_rejects_unknown_fields() {
        for schema in [
            schemars::schema_for!(MemoryHotplugConfig).to_value(),
            schemars::schema_for!(SetMemoryHotplug).to_value(),
        ] {
            assert_eq!(schema["type"], "object");
            assert_eq!(schema["required"], json!(["size_mib"]));
            assert_eq!(schema["additionalProperties"], false);
            assert_eq!(schema["properties"]["size_mib"]["type"], "integer");
            assert_eq!(schema["properties"]["size_mib"]["minimum"], 0);
            for field in ["block_size_mib", "chunk_size_mib"] {
                assert_eq!(
                    schema["properties"][field]["type"],
                    json!(["integer", "null"])
                );
            }
        }
    }

    #[cfg(feature = "schema")]
    #[test]
    fn operation_wrappers_preserve_scalar_schema_shapes() {
        let cpus = schemars::schema_for!(SetCpus).to_value();
        assert_eq!(cpus["type"], "integer");
        assert_eq!(cpus["minimum"], 0);
        assert_eq!(cpus["maximum"], u8::MAX);
        let memory = schemars::schema_for!(SetMemory).to_value();
        assert_eq!(memory["type"], "integer");
        assert_eq!(memory["minimum"], 0);
        for resize in [
            schemars::schema_for!(HotplugSizeMib).to_value(),
            schemars::schema_for!(ResizeMemory).to_value(),
        ] {
            assert_eq!(resize["type"], "integer");
            assert_eq!(resize["minimum"], 0);
        }
        let console = schemars::schema_for!(SetConsole).to_value();
        assert_eq!(console["type"], "string");
        assert_eq!(console["enum"], json!(["stdio", "none"]));
        let backing = schemars::schema_for!(SetMemoryBacking).to_value();
        assert_eq!(backing["type"], "string");
        assert_eq!(backing["enum"], json!(["anonymous", "shared"]));
    }
}
