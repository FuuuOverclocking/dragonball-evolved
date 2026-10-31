use kvm_bindings::{CpuId, KVM_CPUID_FLAG_SIGNIFCANT_INDEX, kvm_cpuid_entry2};
use thiserror::Error;
use vmm_sys_util::fam;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuVendor {
    Intel,
    Amd,
}

impl CpuVendor {
    pub fn id(self) -> [u8; 12] {
        match self {
            Self::Intel => *b"GenuineIntel",
            Self::Amd => *b"AuthenticAMD",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuidRegisters {
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostCpuid {
    pub vendor: CpuVendor,
    pub extended_l1: CpuidRegisters,
    pub extended_l2: CpuidRegisters,
    pub amd_cache_topology: Vec<CpuidRegisters>,
    pub amd_processor_topology: CpuidRegisters,
}

#[derive(Debug, Error)]
pub enum CpuidError {
    #[error("CPUID configuration needs 1 to 254 vCPUs, got {0}")]
    InvalidVcpuCount(u8),
    #[error("vCPU index {index} is outside a {count}-vCPU topology")]
    InvalidVcpuIndex { index: u8, count: u8 },
    #[error("required CPUID leaf {function:#x}, subleaf {index:#x} is missing")]
    Missing { function: u32, index: u32 },
    #[error("failed to construct KVM CPUID: {0}")]
    Build(#[from] fam::Error),
}

pub fn normalize(
    base: &CpuId,
    host: &HostCpuid,
    vcpu_count: u8,
    vcpu_index: u8,
) -> Result<CpuId, CpuidError> {
    if !(1..=254).contains(&vcpu_count) {
        return Err(CpuidError::InvalidVcpuCount(vcpu_count));
    }
    if vcpu_index >= vcpu_count {
        return Err(CpuidError::InvalidVcpuIndex {
            index: vcpu_index,
            count: vcpu_count,
        });
    }

    let mut entries = base.as_slice().to_vec();
    normalize_common(&mut entries, host, vcpu_count, vcpu_index)?;
    match host.vendor {
        CpuVendor::Intel => normalize_intel(&mut entries, vcpu_count)?,
        CpuVendor::Amd => normalize_amd(&mut entries, host, vcpu_count, vcpu_index)?,
    }
    Ok(CpuId::from_entries(&entries)?)
}

fn normalize_common(
    entries: &mut Vec<kvm_cpuid_entry2>,
    host: &HostCpuid,
    vcpu_count: u8,
    vcpu_index: u8,
) -> Result<(), CpuidError> {
    let vendor = host.vendor.id();
    let leaf0 = required(entries, 0, 0)?;
    leaf0.ebx = u32::from_le_bytes(vendor[0..4].try_into().unwrap());
    leaf0.edx = u32::from_le_bytes(vendor[4..8].try_into().unwrap());
    leaf0.ecx = u32::from_le_bytes(vendor[8..12].try_into().unwrap());

    let leaf1 = required(entries, 1, 0)?;
    set_range(&mut leaf1.ebx, 8, 8, 8);
    set_range(&mut leaf1.ebx, 16, 8, u32::from(vcpu_count));
    set_range(&mut leaf1.ebx, 24, 8, u32::from(vcpu_index));
    set_bit(&mut leaf1.ecx, 15, false);
    set_bit(&mut leaf1.ecx, 24, true);
    set_bit(&mut leaf1.ecx, 31, true);
    set_bit(&mut leaf1.edx, 28, vcpu_count > 1);

    entries.retain(|entry| entry.function != 0xb || entry.index <= 1);
    let width = apic_id_width(vcpu_count);
    let thread = upsert(entries, 0xb, 0);
    thread.flags = KVM_CPUID_FLAG_SIGNIFCANT_INDEX;
    thread.eax = 0;
    thread.ebx = 1;
    thread.ecx = 0x100;
    thread.edx = u32::from(vcpu_index);
    let core = upsert(entries, 0xb, 1);
    core.flags = KVM_CPUID_FLAG_SIGNIFCANT_INDEX;
    core.eax = u32::from(width);
    core.ebx = u32::from(vcpu_count);
    core.ecx = 0x201;
    core.edx = u32::from(vcpu_index);

    apply(required(entries, 0x8000_0005, 0)?, host.extended_l1);
    let leaf = required(entries, 0x8000_0006, 0)?;
    apply(leaf, host.extended_l2);
    leaf.edx &= !0x0003_0000;
    Ok(())
}

fn normalize_intel(entries: &mut Vec<kvm_cpuid_entry2>, vcpu_count: u8) -> Result<(), CpuidError> {
    for entry in entries.iter_mut().filter(|entry| entry.function == 4) {
        if entry.eax & 0x1f == 0 {
            continue;
        }
        let level = (entry.eax >> 5) & 0x7;
        let sharing = if level == 3 {
            u32::from(vcpu_count - 1)
        } else {
            0
        };
        set_range(&mut entry.eax, 14, 12, sharing);
        set_range(&mut entry.eax, 26, 6, u32::from(vcpu_count - 1).min(0x3f));
    }

    let leaf6 = required(entries, 6, 0)?;
    set_bit(&mut leaf6.eax, 1, false);
    set_bit(&mut leaf6.ecx, 3, false);
    let leaf7 = required(entries, 7, 0)?;
    set_bit(&mut leaf7.ebx, 6, true);
    set_bit(&mut leaf7.ebx, 13, true);
    set_bit(&mut leaf7.ecx, 5, false);
    let leaf_a = required(entries, 0xa, 0)?;
    leaf_a.eax = 0;
    leaf_a.ebx = 0;
    leaf_a.ecx = 0;
    leaf_a.edx = 0;

    if entries.iter().any(|entry| entry.function == 0x1f) {
        let topology: Vec<_> = entries
            .iter()
            .filter(|entry| entry.function == 0xb)
            .cloned()
            .map(|mut entry| {
                entry.function = 0x1f;
                entry
            })
            .collect();
        entries.retain(|entry| entry.function != 0x1f);
        entries.extend(topology);
    }
    Ok(())
}

fn normalize_amd(
    entries: &mut Vec<kvm_cpuid_entry2>,
    host: &HostCpuid,
    vcpu_count: u8,
    vcpu_index: u8,
) -> Result<(), CpuidError> {
    let leaf7 = required(entries, 7, 0)?;
    set_bit(&mut leaf7.edx, 29, false);
    let leaf = required(entries, 0x8000_0001, 0)?;
    set_bit(&mut leaf.ecx, 22, true);
    let leaf = required(entries, 0x8000_0008, 0)?;
    set_range(&mut leaf.ecx, 12, 4, u32::from(apic_id_width(vcpu_count)));
    set_range(&mut leaf.ecx, 0, 8, u32::from(vcpu_count - 1));

    entries.retain(|entry| entry.function != 0x8000_001d);
    for (index, registers) in host.amd_cache_topology.iter().copied().enumerate() {
        let mut entry = kvm_cpuid_entry2 {
            function: 0x8000_001d,
            index: index as u32,
            flags: KVM_CPUID_FLAG_SIGNIFCANT_INDEX,
            ..Default::default()
        };
        apply(&mut entry, registers);
        let level = (entry.eax >> 5) & 0x7;
        let sharing = if level == 3 {
            u32::from(vcpu_count - 1)
        } else {
            0
        };
        set_range(&mut entry.eax, 14, 12, sharing);
        entries.push(entry);
    }

    let topology = upsert(entries, 0x8000_001e, 0);
    apply(topology, host.amd_processor_topology);
    topology.eax = u32::from(vcpu_index);
    set_range(&mut topology.ebx, 0, 8, u32::from(vcpu_index));
    set_range(&mut topology.ebx, 8, 8, 0);
    set_range(&mut topology.ecx, 0, 11, 0);
    Ok(())
}

fn required(
    entries: &mut [kvm_cpuid_entry2],
    function: u32,
    index: u32,
) -> Result<&mut kvm_cpuid_entry2, CpuidError> {
    entries
        .iter_mut()
        .find(|entry| entry.function == function && entry.index == index)
        .ok_or(CpuidError::Missing { function, index })
}

fn upsert(entries: &mut Vec<kvm_cpuid_entry2>, function: u32, index: u32) -> &mut kvm_cpuid_entry2 {
    if let Some(position) = entries
        .iter()
        .position(|entry| entry.function == function && entry.index == index)
    {
        return &mut entries[position];
    }
    entries.push(kvm_cpuid_entry2 {
        function,
        index,
        ..Default::default()
    });
    entries.last_mut().unwrap()
}

fn apply(entry: &mut kvm_cpuid_entry2, registers: CpuidRegisters) {
    entry.eax = registers.eax;
    entry.ebx = registers.ebx;
    entry.ecx = registers.ecx;
    entry.edx = registers.edx;
}

fn set_bit(value: &mut u32, bit: u32, enabled: bool) {
    if enabled {
        *value |= 1 << bit;
    } else {
        *value &= !(1 << bit);
    }
}

fn set_range(value: &mut u32, offset: u32, width: u32, replacement: u32) {
    let mask = ((1u64 << width) - 1) as u32;
    *value = (*value & !(mask << offset)) | ((replacement & mask) << offset);
}

fn apic_id_width(vcpu_count: u8) -> u8 {
    if vcpu_count <= 1 {
        0
    } else {
        (u8::BITS - (vcpu_count - 1).leading_zeros()) as u8
    }
}

#[cfg(test)]
mod tests {
    use vmm_sys_util::fam::FamStruct;

    use super::*;

    fn entry(function: u32, index: u32) -> kvm_cpuid_entry2 {
        kvm_cpuid_entry2 {
            function,
            index,
            ..Default::default()
        }
    }

    fn base(vendor: CpuVendor) -> CpuId {
        let mut entries: Vec<_> = [
            0,
            1,
            4,
            6,
            7,
            0xa,
            0xb,
            0x1f,
            0x8000_0001,
            0x8000_0005,
            0x8000_0006,
            0x8000_0008,
            0x8000_001e,
        ]
        .into_iter()
        .map(|function| entry(function, 0))
        .collect();
        entries[2].eax = 3 | (3 << 5);
        let vendor = vendor.id();
        entries[0].ebx = u32::from_le_bytes(vendor[0..4].try_into().unwrap());
        entries[0].edx = u32::from_le_bytes(vendor[4..8].try_into().unwrap());
        entries[0].ecx = u32::from_le_bytes(vendor[8..12].try_into().unwrap());
        CpuId::from_entries(&entries).unwrap()
    }

    fn host(vendor: CpuVendor) -> HostCpuid {
        HostCpuid {
            vendor,
            extended_l1: CpuidRegisters {
                eax: 1,
                ebx: 2,
                ecx: 3,
                edx: 4,
            },
            extended_l2: CpuidRegisters {
                eax: 5,
                ebx: 6,
                ecx: 7,
                edx: 0x3_0008,
            },
            amd_cache_topology: vec![CpuidRegisters {
                eax: 3 | (3 << 5) | (0xfff << 14),
                ..Default::default()
            }],
            amd_processor_topology: CpuidRegisters {
                eax: 0xffff,
                ebx: 0xffff,
                ecx: 0xffff,
                edx: 0xffff,
            },
        }
    }

    fn find(cpuid: &CpuId, function: u32, index: u32) -> kvm_cpuid_entry2 {
        *cpuid
            .as_slice()
            .iter()
            .find(|entry| entry.function == function && entry.index == index)
            .unwrap()
    }

    #[test]
    fn common_topology_is_per_vcpu_and_supports_the_xapic_limit() {
        let cpuid = normalize(&base(CpuVendor::Amd), &host(CpuVendor::Amd), 254, 253).unwrap();
        let leaf1 = find(&cpuid, 1, 0);
        assert_eq!((leaf1.ebx >> 16) & 0xff, 254);
        assert_eq!(leaf1.ebx >> 24, 253);
        assert_ne!(leaf1.ecx & (1 << 31), 0);
        assert_ne!(leaf1.edx & (1 << 28), 0);
        let core = find(&cpuid, 0xb, 1);
        assert_eq!(core.eax, 8);
        assert_eq!(core.ebx, 254);
        assert_eq!(core.edx, 253);
    }

    #[test]
    fn amd_topology_and_cache_sharing_are_rewritten() {
        let cpuid = normalize(&base(CpuVendor::Amd), &host(CpuVendor::Amd), 8, 5).unwrap();
        let ext = find(&cpuid, 0x8000_0008, 0);
        assert_eq!(ext.ecx & 0xff, 7);
        assert_eq!((ext.ecx >> 12) & 0xf, 3);
        let cache = find(&cpuid, 0x8000_001d, 0);
        assert_eq!((cache.eax >> 14) & 0xfff, 7);
        let topology = find(&cpuid, 0x8000_001e, 0);
        assert_eq!(topology.eax, 5);
        assert_eq!(topology.ebx & 0xff, 5);
        assert_eq!((topology.ebx >> 8) & 0xff, 0);
        assert_eq!(topology.ecx & 0x7ff, 0);
        let leaf7 = find(&cpuid, 7, 0);
        assert_eq!(leaf7.edx & (1 << 29), 0);
    }

    #[test]
    fn intel_hides_power_and_pmu_features_and_copies_leaf_1f() {
        let mut base = base(CpuVendor::Intel);
        let entries = unsafe { base.as_mut_fam_struct().as_mut_slice() };
        find_mut(entries, 6, 0).eax = u32::MAX;
        find_mut(entries, 6, 0).ecx = u32::MAX;
        find_mut(entries, 7, 0).ecx = u32::MAX;
        find_mut(entries, 0xa, 0).eax = u32::MAX;
        let cpuid = normalize(&base, &host(CpuVendor::Intel), 4, 2).unwrap();
        assert_eq!(find(&cpuid, 6, 0).eax & 2, 0);
        assert_eq!(find(&cpuid, 6, 0).ecx & 8, 0);
        assert_eq!(find(&cpuid, 7, 0).ecx & (1 << 5), 0);
        assert_eq!(find(&cpuid, 0xa, 0).eax, 0);
        assert_eq!(find(&cpuid, 0x1f, 1).edx, 2);
    }

    fn find_mut(
        entries: &mut [kvm_cpuid_entry2],
        function: u32,
        index: u32,
    ) -> &mut kvm_cpuid_entry2 {
        entries
            .iter_mut()
            .find(|entry| entry.function == function && entry.index == index)
            .unwrap()
    }

    #[test]
    fn invalid_topologies_are_rejected() {
        let base = base(CpuVendor::Amd);
        let host = host(CpuVendor::Amd);
        assert!(matches!(
            normalize(&base, &host, 0, 0),
            Err(CpuidError::InvalidVcpuCount(0))
        ));
        assert!(matches!(
            normalize(&base, &host, 2, 2),
            Err(CpuidError::InvalidVcpuIndex { .. })
        ));
    }
}
