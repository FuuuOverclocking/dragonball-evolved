use std::path::PathBuf;

use api_macros::api_type;

#[api_type]
#[derive(Debug, Clone, PartialEq, Eq)]
#[serde(transparent)]
pub struct AddDisk(pub DiskConfig);

/// A virtio-blk disk backed by a host file.
#[api_type]
#[derive(Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DiskConfig {
    /// Unique disk identifier.
    pub id: String,
    /// Path to the backing file.
    pub path: PathBuf,
    /// Expose the disk to the guest as read-only.
    #[serde(default)]
    pub read_only: bool,
    /// Use the disk as the guest root device.
    #[serde(default)]
    pub root: bool,
}

#[cfg(all(test, feature = "serde"))]
mod tests {
    use super::*;

    #[test]
    fn optional_flags_default_to_false_and_unknown_fields_are_rejected() {
        let disk: AddDisk =
            serde_json::from_value(serde_json::json!({ "id": "rootfs", "path": "/rootfs" }))
                .unwrap();
        assert_eq!(
            disk.0,
            DiskConfig {
                id: "rootfs".into(),
                path: "/rootfs".into(),
                read_only: false,
                root: false,
            }
        );
        assert!(
            serde_json::from_value::<AddDisk>(
                serde_json::json!({ "id": "a", "path": "/a", "unknown": 1 })
            )
            .is_err()
        );
        assert!(serde_json::from_value::<AddDisk>(serde_json::json!({ "id": "a" })).is_err());
    }
}
