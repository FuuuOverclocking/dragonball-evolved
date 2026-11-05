pub mod helpers;

pub mod block;
pub mod boot;
pub mod config;
pub mod instance_info;
pub mod lifecycle;
pub mod live_upgrade;
pub mod logger;
pub mod machine;

use api_macros::define_schema;

use crate::helpers::op_mode;
#[cfg(feature = "request")]
pub use crate::helpers::reply::Reply;
pub use crate::helpers::result::{ApiError, ApiResult};

#[cfg(feature = "request")]
pub type VmmRequest = VmmOp<op_mode::Request>;
pub type VmmCommand = VmmOp<op_mode::Command>;

pub trait RawVmmOp {
    type Response;

    #[cfg(feature = "request")]
    fn into_vmm_request(self) -> (VmmRequest, oneshot::Receiver<Self::Response>);
    fn into_vmm_command(self) -> VmmCommand;
}

define_schema! {
    VmmOp {
        instance_info::GetInstanceInfo -> instance_info::InstanceInfo {
            route: GET /v1/;
        }
        logger::UpdateLogger -> ApiResult<()> {
            route: PATCH /v1/logger;
        }
        machine::SetCpus -> ApiResult<()> {
            config: ".machine.cpus";
            route: PUT /v1/machine/cpus;
        }
        machine::SetMemory -> ApiResult<()> {
            config: ".machine.memory_mib";
            route: PUT /v1/machine/memory-mib;
        }
        machine::SetMemoryHotplug -> ApiResult<()> {
            config: ".machine.memory_hotplug";
            route: PUT /v1/machine/memory-hotplug;
        }
        machine::ResizeMemory -> ApiResult<()> {
            route: PATCH /v1/machine/memory-hotplug;
        }
        machine::SetMemoryBacking -> ApiResult<()> {
            config: ".machine.memory_backing";
            route: PUT /v1/machine/memory-backing;
        }
        machine::SetConsole -> ApiResult<()> {
            config: ".machine.console";
            route: PUT /v1/machine/console;
        }
        boot::SetKernel -> ApiResult<()> {
            config: ".machine.kernel";
            route: PUT /v1/machine/kernel;
        }
        block::AddDisk -> ApiResult<()> {
            config: ".machine.disks[]";
            route: PUT /v1/disks;
        }
        lifecycle::Start -> ApiResult<()> {
            route: PUT /v1/actions/start;
        }
        lifecycle::Pause -> ApiResult<()> {
            route: PUT /v1/actions/pause;
        }
        lifecycle::Resume -> ApiResult<()> {
            route: PUT /v1/actions/resume;
        }
        lifecycle::Shutdown -> ApiResult<()> {
            route: PUT /v1/actions/shutdown;
        }
        live_upgrade::HandoverState -> ApiResult<live_upgrade::LiveUpgradeState> {
            route: POST /v1/live-upgrade/handover-state;
            fd_in: resp;
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "serde")]
    use serde_json::json;

    use super::*;

    #[test]
    fn machine_and_lifecycle_bindings_match_the_api() {
        for (name, config, method, path) in [
            ("SetCpus", Some(".machine.cpus"), "PUT", "/v1/machine/cpus"),
            (
                "SetMemory",
                Some(".machine.memory_mib"),
                "PUT",
                "/v1/machine/memory-mib",
            ),
            (
                "SetMemoryHotplug",
                Some(".machine.memory_hotplug"),
                "PUT",
                "/v1/machine/memory-hotplug",
            ),
            ("ResizeMemory", None, "PATCH", "/v1/machine/memory-hotplug"),
            (
                "SetMemoryBacking",
                Some(".machine.memory_backing"),
                "PUT",
                "/v1/machine/memory-backing",
            ),
            (
                "SetConsole",
                Some(".machine.console"),
                "PUT",
                "/v1/machine/console",
            ),
            (
                "SetKernel",
                Some(".machine.kernel"),
                "PUT",
                "/v1/machine/kernel",
            ),
            ("AddDisk", Some(".machine.disks[]"), "PUT", "/v1/disks"),
            ("Start", None, "PUT", "/v1/actions/start"),
            ("Pause", None, "PUT", "/v1/actions/pause"),
            ("Resume", None, "PUT", "/v1/actions/resume"),
            ("Shutdown", None, "PUT", "/v1/actions/shutdown"),
        ] {
            let op = metadata::OPERATIONS
                .iter()
                .find(|op| op.name == name)
                .unwrap();
            assert_eq!(op.config, config);
            assert_eq!(op.route, Some((method, path).into()));
        }
    }

    #[cfg(feature = "serde")]
    #[test]
    fn configuration_parses_scalars_and_objects_in_operation_order() {
        let config = config::Config::parse(json!({
            "machine": {
                "cpus": 2,
                "memory_mib": 1024,
                "memory_hotplug": { "size_mib": 256, "block_size_mib": 2, "chunk_size_mib": 128 },
                "memory_backing": "shared",
                "console": "stdio",
                "kernel": { "path": "/test/kernel" },
                "disks": [
                    { "path": "/test/disk", "root": true },
                    { "id": "data", "path": "/test/disk", "read_only": true }
                ]
            }
        }))
        .unwrap();
        let commands: Vec<_> = config
            .commands
            .into_iter()
            .map(|command| serde_json::to_value(command).unwrap())
            .collect();
        assert_eq!(
            commands,
            vec![
                json!({ "set-cpus": 2 }),
                json!({ "set-memory": 1024 }),
                json!({ "set-memory-hotplug": {
                    "size_mib": 256, "block_size_mib": 2, "chunk_size_mib": 128
                } }),
                json!({ "set-memory-backing": "shared" }),
                json!({ "set-console": "stdio" }),
                json!({ "set-kernel": {
                "path": "/test/kernel", "cmdline": null, "initrd": null
            } }),
                json!({ "add-disk": {
                "id": null, "path": "/test/disk", "read_only": false, "root": true
            } }),
                json!({ "add-disk": {
                "id": "data", "path": "/test/disk", "read_only": true, "root": false
            } }),
            ]
        );
        assert!(
            config::Config::parse(json!({}))
                .unwrap()
                .commands
                .is_empty()
        );
    }

    #[cfg(feature = "serde")]
    #[test]
    fn memory_hotplug_configuration_uses_the_object_payload() {
        let config = config::Config::parse(json!({
            "machine": { "memory_hotplug": { "size_mib": 256 } }
        }))
        .unwrap();
        let commands: Vec<_> = config
            .commands
            .into_iter()
            .map(|command| serde_json::to_value(command).unwrap())
            .collect();
        assert_eq!(
            commands,
            vec![json!({ "set-memory-hotplug": { "size_mib": 256 } })]
        );
    }

    #[cfg(feature = "serde")]
    #[test]
    fn invalid_configuration_reports_missing_paths_and_unknown_fields() {
        for (machine, path, reason) in [
            (
                json!({ "memory_hotplug": {} }),
                ".machine.memory_hotplug",
                "missing field `size_mib`",
            ),
            (
                json!({ "memory_hotplug": { "size_mib": 256, "unknown": true } }),
                ".machine.memory_hotplug",
                "unknown field `unknown`",
            ),
            (
                json!({ "memory_hotplug": { "memory_mib": 256 } }),
                ".machine.memory_hotplug",
                "unknown field `memory_mib`",
            ),
            (
                json!({ "memory_hotplug": { "size_mib": "256" } }),
                ".machine.memory_hotplug",
                "invalid type",
            ),
            (
                json!({ "memory_hotplug": { "size_mib": 256, "block_size_mib": "2" } }),
                ".machine.memory_hotplug",
                "invalid type",
            ),
            (
                json!({ "memory_hotplug": { "size_mib": 256, "chunk_size_mib": true } }),
                ".machine.memory_hotplug",
                "invalid type",
            ),
            (
                json!({ "memory_hotplug": 256 }),
                ".machine.memory_hotplug",
                "invalid type",
            ),
            (
                json!({ "resize_memory": 0 }),
                ".machine.resize_memory",
                "unknown config field",
            ),
            (
                json!({ "kernel": {} }),
                ".machine.kernel",
                "missing field `path`",
            ),
            (
                json!({ "disks": [{}] }),
                ".machine.disks[0]",
                "missing field `path`",
            ),
            (
                json!({ "kernel": { "path": "/test/kernel", "unknown": true } }),
                ".machine.kernel",
                "unknown field `unknown`",
            ),
            (
                json!({ "disks": [{ "path": "/test/disk", "unknown": true }] }),
                ".machine.disks[0]",
                "unknown field `unknown`",
            ),
            (
                json!({ "unknown": true }),
                ".machine.unknown",
                "unknown config field",
            ),
        ] {
            let error = config::Config::parse(json!({ "machine": machine }))
                .unwrap_err()
                .to_string();
            assert!(error.contains(path), "{error}");
            assert!(error.contains(reason), "{error}");
        }
    }

    #[cfg(feature = "schema")]
    #[test]
    fn configuration_schema_binds_scalar_object_and_array_payloads() {
        let mut generator = schemars::SchemaGenerator::default();
        let schema = config::schema(&mut generator).to_value();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
        let machine = &schema["properties"]["machine"];
        assert_eq!(machine["type"], "object");
        assert_eq!(machine["additionalProperties"], false);
        let properties = &machine["properties"];
        for (field, expected) in [
            ("cpus", generator.subschema_for::<machine::SetCpus>()),
            (
                "memory_mib",
                generator.subschema_for::<machine::SetMemory>(),
            ),
            (
                "memory_hotplug",
                generator.subschema_for::<machine::SetMemoryHotplug>(),
            ),
            (
                "memory_backing",
                generator.subschema_for::<machine::SetMemoryBacking>(),
            ),
            ("console", generator.subschema_for::<machine::SetConsole>()),
            ("kernel", generator.subschema_for::<boot::SetKernel>()),
        ] {
            assert_eq!(properties[field], expected.to_value());
        }
        assert!(properties.get("resize_memory").is_none());
        assert_eq!(properties["disks"]["type"], "array");
        assert_eq!(
            properties["disks"]["items"],
            generator.subschema_for::<block::AddDisk>().to_value()
        );
    }
}
