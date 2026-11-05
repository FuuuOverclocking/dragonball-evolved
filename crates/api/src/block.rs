use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AddDisk(pub BlockDevConfig);

/// Block device configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BlockDevConfig {
    #[cfg_attr(feature = "serde", serde(default))]
    pub id: Option<String>,
    pub path: PathBuf,
    #[cfg_attr(feature = "serde", serde(default))]
    pub read_only: bool,
    #[cfg_attr(feature = "serde", serde(default))]
    pub root: bool,
}

#[cfg(all(test, feature = "serde"))]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn disk_path_is_required_and_optional_fields_use_defaults() {
        let disk: BlockDevConfig = serde_json::from_value(json!({ "path": "/test/disk" })).unwrap();
        assert_eq!(
            disk,
            BlockDevConfig {
                id: None,
                path: "/test/disk".into(),
                read_only: false,
                root: false,
            }
        );
        assert_eq!(
            serde_json::to_value(disk).unwrap(),
            json!({ "id": null, "path": "/test/disk", "read_only": false, "root": false })
        );
    }

    #[test]
    fn disk_object_round_trips_all_fields() {
        let value = json!({
            "id": "rootfs", "path": "/test/disk", "read_only": true, "root": true
        });
        let disk: BlockDevConfig = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(disk.id.as_deref(), Some("rootfs"));
        assert_eq!(disk.path, PathBuf::from("/test/disk"));
        assert!(disk.read_only);
        assert!(disk.root);
        let operation: AddDisk = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(operation.0, disk);
        assert_eq!(serde_json::to_value(operation).unwrap(), value);
        assert_eq!(serde_json::to_value(disk).unwrap(), value);
    }

    #[test]
    fn disk_rejects_missing_paths_and_unknown_fields() {
        for value in [
            json!({}),
            json!({ "id": "rootfs" }),
            json!({ "path": null }),
            json!({ "path": "/test/disk", "unknown": true }),
        ] {
            assert!(serde_json::from_value::<BlockDevConfig>(value.clone()).is_err());
            assert!(serde_json::from_value::<AddDisk>(value).is_err());
        }
    }

    #[cfg(feature = "schema")]
    #[test]
    fn disk_schema_requires_only_path_and_rejects_unknown_fields() {
        let schema = schemars::schema_for!(BlockDevConfig).to_value();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"], json!(["path"]));
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["path"]["type"], "string");
        assert_eq!(
            schema["properties"]["id"]["type"],
            json!(["string", "null"])
        );
        assert_eq!(schema["properties"]["id"]["default"], json!(null));
        for field in ["read_only", "root"] {
            assert_eq!(schema["properties"][field]["type"], "boolean");
            assert_eq!(schema["properties"][field]["default"], false);
        }
    }
}
