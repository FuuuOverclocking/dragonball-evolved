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
        for value in [json!("Stdio"), json!("None"), json!({}), json!(null)] {
            assert!(serde_json::from_value::<SetConsole>(value).is_err());
        }
        for value in [json!("Shared"), json!("memfd"), json!({}), json!(null)] {
            assert!(serde_json::from_value::<SetMemoryBacking>(value).is_err());
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
        let console = schemars::schema_for!(SetConsole).to_value();
        assert_eq!(console["type"], "string");
        assert_eq!(console["enum"], json!(["stdio", "none"]));
        let backing = schemars::schema_for!(SetMemoryBacking).to_value();
        assert_eq!(backing["type"], "string");
        assert_eq!(backing["enum"], json!(["anonymous", "shared"]));
    }
}
