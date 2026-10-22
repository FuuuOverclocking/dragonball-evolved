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
        for (name, config, path) in [
            ("SetCpus", Some(".machine.cpus"), "/v1/machine/cpus"),
            (
                "SetMemory",
                Some(".machine.memory_mib"),
                "/v1/machine/memory-mib",
            ),
            (
                "SetConsole",
                Some(".machine.console"),
                "/v1/machine/console",
            ),
            ("SetKernel", Some(".machine.kernel"), "/v1/machine/kernel"),
            ("AddDisk", Some(".machine.disks[]"), "/v1/disks"),
            ("Start", None, "/v1/actions/start"),
            ("Pause", None, "/v1/actions/pause"),
            ("Resume", None, "/v1/actions/resume"),
            ("Shutdown", None, "/v1/actions/shutdown"),
        ] {
            let op = metadata::OPERATIONS
                .iter()
                .find(|op| op.name == name)
                .unwrap();
            assert_eq!(op.config, config);
            assert_eq!(op.route, Some(("PUT", path).into()));
        }
    }

    #[cfg(feature = "serde")]
    #[test]
    fn configuration_parses_scalars_and_objects_in_operation_order() {
        let config = config::Config::parse(json!({
            "machine": {
                "cpus": 2,
                "memory_mib": 1024,
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
    fn invalid_configuration_reports_missing_paths_and_unknown_fields() {
        for (machine, path, reason) in [
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
            ("console", generator.subschema_for::<machine::SetConsole>()),
            ("kernel", generator.subschema_for::<boot::SetKernel>()),
        ] {
            assert_eq!(properties[field], expected.to_value());
        }
        assert_eq!(properties["disks"]["type"], "array");
        assert_eq!(
            properties["disks"]["items"],
            generator.subschema_for::<block::AddDisk>().to_value()
        );
    }
}
