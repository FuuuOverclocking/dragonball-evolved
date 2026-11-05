use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SetKernel(pub KernelConfig);

/// Kernel image and boot parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct KernelConfig {
    pub path: PathBuf,
    #[cfg_attr(feature = "serde", serde(default))]
    pub cmdline: Option<String>,
    #[cfg_attr(feature = "serde", serde(default))]
    pub initrd: Option<PathBuf>,
}

#[cfg(all(test, feature = "serde"))]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn kernel_path_is_required_and_optional_fields_default_to_none() {
        let kernel: KernelConfig =
            serde_json::from_value(json!({ "path": "/test/kernel" })).unwrap();
        assert_eq!(
            kernel,
            KernelConfig {
                path: "/test/kernel".into(),
                cmdline: None,
                initrd: None,
            }
        );
        assert_eq!(
            serde_json::to_value(kernel).unwrap(),
            json!({ "path": "/test/kernel", "cmdline": null, "initrd": null })
        );
    }

    #[test]
    fn kernel_object_round_trips_all_fields() {
        let value = json!({
            "path": "/test/kernel",
            "cmdline": "console=ttyS0",
            "initrd": "/test/initrd"
        });
        let kernel: KernelConfig = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(kernel.path, PathBuf::from("/test/kernel"));
        assert_eq!(kernel.cmdline.as_deref(), Some("console=ttyS0"));
        assert_eq!(kernel.initrd, Some(PathBuf::from("/test/initrd")));
        let operation: SetKernel = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(operation.0, kernel);
        assert_eq!(serde_json::to_value(operation).unwrap(), value);
        assert_eq!(serde_json::to_value(kernel).unwrap(), value);
    }

    #[test]
    fn kernel_rejects_missing_paths_and_unknown_fields() {
        for value in [
            json!({}),
            json!({ "cmdline": "console=ttyS0" }),
            json!({ "path": null }),
            json!({ "path": "/test/kernel", "unknown": true }),
        ] {
            assert!(serde_json::from_value::<KernelConfig>(value.clone()).is_err());
            assert!(serde_json::from_value::<SetKernel>(value).is_err());
        }
    }

    #[cfg(feature = "schema")]
    #[test]
    fn kernel_schema_requires_only_path_and_rejects_unknown_fields() {
        let schema = schemars::schema_for!(KernelConfig).to_value();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"], json!(["path"]));
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["path"]["type"], "string");
        for field in ["cmdline", "initrd"] {
            assert_eq!(
                schema["properties"][field]["type"],
                json!(["string", "null"])
            );
            assert_eq!(schema["properties"][field]["default"], json!(null));
        }
    }
}
