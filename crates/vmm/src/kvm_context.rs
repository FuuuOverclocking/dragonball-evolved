use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use kvm_bindings::KVM_API_VERSION;
use kvm_ioctls::Kvm;

pub struct KvmContext {
    pub fd: Kvm,
}

impl KvmContext {
    pub fn from_dev_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let c_path =
            CString::new(path.as_os_str().as_bytes()).context("invalid KVM device path")?;
        let fd = Kvm::new_with_path(&c_path)
            .with_context(|| format!("open KVM device {}", path.display()))?;
        let api_version = fd.get_api_version();
        ensure!(
            api_version == KVM_API_VERSION as i32,
            "unsupported KVM API version {api_version}, expected {KVM_API_VERSION}"
        );
        Ok(Self { fd })
    }
}

#[cfg(test)]
mod tests {
    use super::KvmContext;

    #[test]
    fn rejects_path_with_nul() {
        let error = KvmContext::from_dev_path("/dev/kvm\0").err().unwrap();
        assert_eq!(error.to_string(), "invalid KVM device path");
    }

    #[test]
    fn rejects_non_kvm_device() {
        let error = KvmContext::from_dev_path("/dev/null").err().unwrap();
        assert!(error.to_string().contains("unsupported KVM API version"));
    }
}
