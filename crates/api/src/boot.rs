use std::path::PathBuf;

use api_macros::api_type;

#[api_type]
#[derive(Debug, Clone, PartialEq, Eq)]
#[serde(transparent)]
pub struct SetBoot(pub BootConfig);

/// Direct kernel boot source.
#[api_type]
#[derive(Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BootConfig {
    /// Path to the guest kernel image.
    pub kernel: PathBuf,
    /// Kernel command line.
    #[serde(default)]
    pub cmdline: Option<String>,
    /// Path to the initial ramdisk.
    #[serde(default)]
    pub initrd: Option<PathBuf>,
}

#[cfg(all(test, feature = "serde"))]
mod tests {
    use super::*;

    #[test]
    fn only_the_kernel_is_required() {
        let parse = |value| serde_json::from_value::<SetBoot>(value).map(|boot| boot.0);
        assert_eq!(
            parse(serde_json::json!({ "kernel": "/vmlinux" })).unwrap(),
            BootConfig {
                kernel: "/vmlinux".into(),
                cmdline: None,
                initrd: None,
            }
        );
        assert_eq!(
            parse(serde_json::json!({
                "kernel": "/vmlinux",
                "cmdline": "console=ttyS0",
                "initrd": "/initrd.img",
            }))
            .unwrap(),
            BootConfig {
                kernel: "/vmlinux".into(),
                cmdline: Some("console=ttyS0".into()),
                initrd: Some("/initrd.img".into()),
            }
        );
        assert!(parse(serde_json::json!({ "cmdline": "console=ttyS0" })).is_err());
        assert!(parse(serde_json::json!({ "kernel": "/vmlinux", "initramfs": "/i" })).is_err());
    }
}
