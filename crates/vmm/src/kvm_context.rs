use std::ffi::CString;
use std::os::unix::{ffi::OsStrExt, fs::FileTypeExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use kvm_bindings::{CpuId, KVM_API_VERSION, KVM_MAX_CPUID_ENTRIES};
use kvm_ioctls::{Cap, Kvm};

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

pub struct KvmContext {
    kvm: Kvm,
    path: PathBuf,
    capabilities: HostCapabilities,
}

pub struct HostCapabilities {
    pub supported_cpuid: CpuId,
    pub host_cpuid: HostCpuId,
    pub max_vcpus: usize,
    pub max_memslots: usize,
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

        for &cap in REQUIRED_CAPABILITIES {
            if !kvm.check_extension(cap) {
                bail!(
                    "KVM device {} does not support required capability {cap:?}",
                    path.display()
                );
            }
        }

        let supported_cpuid = kvm
            .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
            .context("get kvm supported cpuid")?;

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
            .with_context(|| format!("stat kvm device {}", path.display()))?;
        if metadata.file_type().is_char_device() {
            bail!("kvm device {} is not a char dev", path.display());
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

fn capture_host_cpuid() -> Result<HostCpuId> {
    todo!()
}
