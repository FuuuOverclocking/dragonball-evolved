use std::arch::x86_64::{__cpuid, __cpuid_count};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use kvm_bindings::{CpuId, KVM_API_VERSION, KVM_MAX_CPUID_ENTRIES};
use kvm_ioctls::{Cap, Kvm};

use crate::arch::x86_64::cpuid::{CpuVendor, CpuidRegisters, HostCpuid};

const REQUIRED_CAPABILITIES: &[Cap] = &[
    Cap::Irqchip,
    Cap::UserMemory,
    Cap::SetTssAddr,
    Cap::ExtCpuid,
    Cap::MpState,
    Cap::Irqfd,
    Cap::IrqfdResample,
    Cap::ImmediateExit,
    Cap::Pit2,
    Cap::PitState2,
    Cap::Ioeventfd,
];

#[derive(Debug, Clone)]
pub struct HostCapabilities {
    pub supported_cpuid: CpuId,
    pub host_cpuid: HostCpuid,
    pub max_vcpus: usize,
    pub max_memslots: usize,
}

pub struct KvmContext {
    kvm: Kvm,
    path: PathBuf,
    capabilities: HostCapabilities,
}

impl KvmContext {
    pub fn from_dev_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let kvm = Self::check_and_open(path)?;

        let api_version = kvm.get_api_version();
        if api_version != KVM_API_VERSION as i32 {
            bail!(
                "KVM device {} reports API version {api_version}, expected {KVM_API_VERSION}",
                path.display()
            );
        }
        for &capability in REQUIRED_CAPABILITIES {
            if !kvm.check_extension(capability) {
                bail!(
                    "KVM device {} does not support required capability {capability:?}",
                    path.display()
                );
            }
        }

        let supported_cpuid = kvm
            .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
            .context("query KVM supported CPUID")?;
        let max_vcpus = kvm.check_extension_int(Cap::MaxVcpus);
        let max_vcpus = if max_vcpus > 0 {
            max_vcpus as usize
        } else {
            kvm.get_nr_vcpus()
        };
        let capabilities = HostCapabilities {
            supported_cpuid,
            host_cpuid: capture_host_cpuid()?,
            max_vcpus,
            max_memslots: kvm.get_nr_memslots(),
        };
        Ok(Self {
            kvm,
            path: path.to_path_buf(),
            capabilities,
        })
    }

    fn check_and_open(path: &Path) -> Result<Kvm> {
        let metadata = path
            .metadata()
            .with_context(|| format!("stat KVM device {}", path.display()))?;
        if !metadata.file_type().is_char_device() {
            bail!("KVM device {} is not a character device", path.display());
        }
        let c_path = CString::new(path.as_os_str().as_bytes())
            .with_context(|| format!("KVM device path contains NUL: {}", path.display()))?;
        Kvm::new_with_path(&c_path).with_context(|| format!("open KVM device {}", path.display()))
    }

    pub fn kvm(&self) -> &Kvm {
        &self.kvm
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn capabilities(&self) -> &HostCapabilities {
        &self.capabilities
    }
}

fn capture_host_cpuid() -> Result<HostCpuid> {
    let vendor = __cpuid(0);
    let mut vendor_id = [0u8; 12];
    vendor_id[..4].copy_from_slice(&vendor.ebx.to_le_bytes());
    vendor_id[4..8].copy_from_slice(&vendor.edx.to_le_bytes());
    vendor_id[8..].copy_from_slice(&vendor.ecx.to_le_bytes());
    let vendor = match &vendor_id {
        b"GenuineIntel" => CpuVendor::Intel,
        b"AuthenticAMD" => CpuVendor::Amd,
        other => bail!(
            "unsupported x86 CPU vendor {}",
            String::from_utf8_lossy(other)
        ),
    };

    let extended_l1 = __cpuid(0x8000_0005).into();
    let extended_l2 = __cpuid(0x8000_0006).into();
    let mut amd_cache_topology = Vec::new();
    if vendor == CpuVendor::Amd {
        for index in 0..32 {
            let leaf = __cpuid_count(0x8000_001d, index);
            if leaf.eax & 0x1f == 0 {
                break;
            }
            amd_cache_topology.push(leaf.into());
        }
        if amd_cache_topology.len() == 32 {
            bail!("AMD CPUID cache topology has no terminating subleaf");
        }
    }
    let amd_processor_topology = __cpuid(0x8000_001e).into();
    Ok(HostCpuid {
        vendor,
        extended_l1,
        extended_l2,
        amd_cache_topology,
        amd_processor_topology,
    })
}

impl From<std::arch::x86_64::CpuidResult> for CpuidRegisters {
    fn from(value: std::arch::x86_64::CpuidResult) -> Self {
        Self {
            eax: value.eax,
            ebx: value.ebx,
            ecx: value.ecx,
            edx: value.edx,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn ordinary_files_are_rejected_before_ioctl() {
        let path = std::env::temp_dir().join(format!("dbe-kvm-context-{}", std::process::id()));
        fs::write(&path, b"not kvm").unwrap();
        let error = match KvmContext::from_dev_path(&path) {
            Ok(_) => panic!("ordinary file was accepted as KVM"),
            Err(error) => error,
        };
        fs::remove_file(&path).unwrap();
        assert!(error.to_string().contains("is not a character device"));
    }

    #[test]
    fn opens_the_host_kvm_device_when_available() {
        if !Path::new("/dev/kvm").exists() {
            return;
        }
        let context = KvmContext::from_dev_path("/dev/kvm").unwrap();
        assert_eq!(context.path(), Path::new("/dev/kvm"));
        assert!(context.capabilities().max_vcpus > 0);
        assert!(context.capabilities().max_memslots > 0);
        assert!(!context.capabilities().supported_cpuid.as_slice().is_empty());
    }
}
