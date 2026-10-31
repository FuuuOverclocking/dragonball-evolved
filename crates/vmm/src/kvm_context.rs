use std::arch::x86_64::{__cpuid, __cpuid_count};
use std::ffi::{CString, NulError};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};

use kvm_bindings::{CpuId, KVM_API_VERSION, KVM_MAX_CPUID_ENTRIES};
use kvm_ioctls::{Cap, Kvm};

use crate::align::HostPageSize;
use crate::arch::x86_64::cpuid::{CpuVendor, CpuidRegisters, HostCpuid};
use crate::space::Gpa;

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
    pub page_size: HostPageSize,
}

impl HostCapabilities {
    /// The highest guest-physical address KVM supports.
    pub fn max_gpa(&self) -> Gpa {
        let bits = self
            .supported_cpuid
            .as_slice()
            .iter()
            .find(|entry| entry.function == 0x8000_0008)
            .map_or(64, |leaf| leaf.eax & 0xff);
        Gpa(1u64.checked_shl(bits).map_or(u64::MAX, |limit| limit - 1))
    }
}

#[cfg(test)]
impl HostCapabilities {
    /// A host described without KVM: `physical_bits` of guest-physical
    /// address, `max_memslots` memslots and `page` byte base pages.
    pub(crate) fn fake(physical_bits: u32, max_memslots: usize, page: u64) -> Self {
        let leaf = kvm_bindings::kvm_cpuid_entry2 {
            function: 0x8000_0008,
            eax: physical_bits,
            ..Default::default()
        };
        Self {
            supported_cpuid: CpuId::from_entries(&[leaf]).unwrap(),
            host_cpuid: HostCpuid {
                vendor: CpuVendor::Intel,
                extended_l1: CpuidRegisters::default(),
                extended_l2: CpuidRegisters::default(),
                amd_cache_topology: Vec::new(),
                amd_processor_topology: CpuidRegisters::default(),
            },
            max_vcpus: 1,
            max_memslots,
            page_size: HostPageSize::new(page).unwrap(),
        }
    }
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum KvmContextError {
    /// stat KVM device {path}
    Stat {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// KVM device {path} is not a character device
    NotCharDevice { path: PathBuf },
    /// encode KVM device path {path}
    EncodePath {
        path: PathBuf,
        #[source]
        source: NulError,
    },
    /// open KVM device {path}
    Open {
        path: PathBuf,
        #[source]
        source: kvm_ioctls::Error,
    },
    /// KVM device {path} reports unsupported API version {version}
    ApiVersion { path: PathBuf, version: i32 },
    /// KVM device {path} lacks required capability {capability:?}
    MissingCapability { path: PathBuf, capability: Cap },
    /// query supported CPUID of KVM device {path}
    SupportedCpuid {
        path: PathBuf,
        #[source]
        source: kvm_ioctls::Error,
    },
    /// host reports invalid page size {0}
    PageSize(libc::c_long),
    /// host CPU vendor {0:?} is not supported
    CpuVendor(String),
    /// host CPUID leaf 0x8000001d has no terminating subleaf
    CacheTopology,
}

pub struct KvmContext {
    kvm: Kvm,
    path: PathBuf,
    capabilities: HostCapabilities,
}

impl KvmContext {
    pub fn from_dev_path(path: impl AsRef<Path>) -> Result<Self, KvmContextError> {
        let path = path.as_ref();
        let kvm = open(path)?;

        let version = kvm.get_api_version();
        if version != KVM_API_VERSION as i32 {
            return Err(KvmContextError::ApiVersion {
                path: path.to_owned(),
                version,
            });
        }
        if let Some(&capability) = REQUIRED_CAPABILITIES
            .iter()
            .find(|&&capability| !kvm.check_extension(capability))
        {
            return Err(KvmContextError::MissingCapability {
                path: path.to_owned(),
                capability,
            });
        }

        let supported_cpuid = kvm
            .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
            .map_err(|source| KvmContextError::SupportedCpuid {
                path: path.to_owned(),
                source,
            })?;
        let max_vcpus = usize::try_from(kvm.check_extension_int(Cap::MaxVcpus))
            .ok()
            .filter(|&count| count > 0)
            .unwrap_or_else(|| kvm.get_nr_vcpus());
        let capabilities = HostCapabilities {
            supported_cpuid,
            host_cpuid: capture_host_cpuid()?,
            max_vcpus,
            max_memslots: kvm.get_nr_memslots(),
            page_size: host_page_size()?,
        };
        Ok(Self {
            kvm,
            path: path.to_owned(),
            capabilities,
        })
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

fn open(path: &Path) -> Result<Kvm, KvmContextError> {
    let metadata = path.metadata().map_err(|source| KvmContextError::Stat {
        path: path.to_owned(),
        source,
    })?;
    if !metadata.file_type().is_char_device() {
        return Err(KvmContextError::NotCharDevice {
            path: path.to_owned(),
        });
    }
    let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|source| {
        KvmContextError::EncodePath {
            path: path.to_owned(),
            source,
        }
    })?;
    Kvm::new_with_path(&c_path).map_err(|source| KvmContextError::Open {
        path: path.to_owned(),
        source,
    })
}

fn host_page_size() -> Result<HostPageSize, KvmContextError> {
    // SAFETY: sysconf takes no pointers and has no preconditions.
    let bytes = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    u64::try_from(bytes)
        .ok()
        .and_then(HostPageSize::new)
        .ok_or(KvmContextError::PageSize(bytes))
}

fn capture_host_cpuid() -> Result<HostCpuid, KvmContextError> {
    let leaf = __cpuid(0);
    let mut vendor = [0u8; 12];
    vendor[..4].copy_from_slice(&leaf.ebx.to_le_bytes());
    vendor[4..8].copy_from_slice(&leaf.edx.to_le_bytes());
    vendor[8..].copy_from_slice(&leaf.ecx.to_le_bytes());
    let vendor = match &vendor {
        b"GenuineIntel" => CpuVendor::Intel,
        b"AuthenticAMD" => CpuVendor::Amd,
        other => {
            return Err(KvmContextError::CpuVendor(
                String::from_utf8_lossy(other).into_owned(),
            ));
        }
    };

    let mut amd_cache_topology = Vec::new();
    if vendor == CpuVendor::Amd {
        for index in 0..32 {
            let leaf = __cpuid_count(0x8000_001d, index);
            if leaf.eax & 0x1f == 0 {
                break;
            }
            amd_cache_topology.push(registers(leaf));
        }
        if amd_cache_topology.len() == 32 {
            return Err(KvmContextError::CacheTopology);
        }
    }
    Ok(HostCpuid {
        vendor,
        extended_l1: registers(__cpuid(0x8000_0005)),
        extended_l2: registers(__cpuid(0x8000_0006)),
        amd_cache_topology,
        amd_processor_topology: registers(__cpuid(0x8000_001e)),
    })
}

fn registers(leaf: std::arch::x86_64::CpuidResult) -> CpuidRegisters {
    CpuidRegisters {
        eax: leaf.eax,
        ebx: leaf.ebx,
        ecx: leaf.ecx,
        edx: leaf.edx,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use kvm_bindings::kvm_cpuid_entry2;

    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("dbe-kvm-context-{}-{name}", std::process::id()))
    }

    #[test]
    fn ordinary_files_are_rejected_before_ioctl() {
        let path = temp_path("file");
        fs::write(&path, b"not kvm").unwrap();
        let result = KvmContext::from_dev_path(&path);
        fs::remove_file(&path).unwrap();
        assert!(matches!(
            result,
            Err(KvmContextError::NotCharDevice { path: rejected }) if rejected == path
        ));
    }

    #[test]
    fn missing_devices_report_the_stat_failure() {
        let path = temp_path("missing");
        let Err(KvmContextError::Stat { source, .. }) = KvmContext::from_dev_path(&path) else {
            panic!("a missing device was not reported as a stat failure");
        };
        assert_eq!(source.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn host_page_size_is_at_least_4_kib() {
        assert!(host_page_size().unwrap().alignment().get() >= 4096);
    }

    #[test]
    fn host_cpuid_is_captured_without_kvm() {
        let host = capture_host_cpuid().unwrap();
        if host.vendor == CpuVendor::Intel {
            assert!(host.amd_cache_topology.is_empty());
        }
    }

    #[test]
    fn max_gpa_follows_leaf_0x8000_0008() {
        let host = |entries: &[kvm_cpuid_entry2]| HostCapabilities {
            supported_cpuid: CpuId::from_entries(entries).unwrap(),
            host_cpuid: HostCpuid {
                vendor: CpuVendor::Intel,
                extended_l1: CpuidRegisters::default(),
                extended_l2: CpuidRegisters::default(),
                amd_cache_topology: Vec::new(),
                amd_processor_topology: CpuidRegisters::default(),
            },
            max_vcpus: 1,
            max_memslots: 1,
            page_size: HostPageSize::new(4096).unwrap(),
        };
        let leaf = |bits: u32| kvm_cpuid_entry2 {
            function: 0x8000_0008,
            eax: 0x3000 | bits,
            ..Default::default()
        };
        assert_eq!(host(&[leaf(0x27)]).max_gpa(), Gpa((1 << 39) - 1));
        assert_eq!(host(&[leaf(64)]).max_gpa(), Gpa(u64::MAX));
        assert_eq!(host(&[]).max_gpa(), Gpa(u64::MAX));
    }

    #[test]
    fn opens_the_host_kvm_device_when_available() {
        if !Path::new("/dev/kvm").exists() {
            return;
        }
        let context = KvmContext::from_dev_path("/dev/kvm").unwrap();
        let capabilities = context.capabilities();
        assert_eq!(context.path(), Path::new("/dev/kvm"));
        assert!(capabilities.max_vcpus > 0);
        assert!(capabilities.max_memslots > 0);
        assert!(!capabilities.supported_cpuid.as_slice().is_empty());
        assert_eq!(capabilities.page_size, host_page_size().unwrap());
    }
}
