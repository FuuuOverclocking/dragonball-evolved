use kvm_bindings::kvm_msr_entry;

const MSR_IA32_TSC: u32 = 0x10;
const MSR_IA32_SYSENTER_CS: u32 = 0x174;
const MSR_IA32_SYSENTER_ESP: u32 = 0x175;
const MSR_IA32_SYSENTER_EIP: u32 = 0x176;
const MSR_IA32_MISC_ENABLE: u32 = 0x1a0;
const MSR_MTRR_DEF_TYPE: u32 = 0x2ff;
const IA32_TSC_DEADLINE: u32 = 0x6E0;
const MSR_EFER: u32 = 0xC000_0080;
const MSR_STAR: u32 = 0xc000_0081;
const MSR_LSTAR: u32 = 0xc000_0082;
const MSR_CSTAR: u32 = 0xc000_0083;
const MSR_SYSCALL_MASK: u32 = 0xc000_0084;
const IA32_FS_BASE: u32 = 0xC000_0100;
const IA32_GS_BASE: u32 = 0xC000_0101;
const MSR_KERNEL_GS_BASE: u32 = 0xc000_0102;
const MSR_TSC_AUX: u32 = 0xC000_0103;

/// Returns the MSR indices that must be saved and restored.
///
/// The set covers `boot_msrs` plus additional registers required for faithful
/// vCPU state round-tripping (segment bases, EFER, TSC helpers, etc.).
pub fn msr_indices() -> Vec<u32> {
    let mut indices: Vec<u32> = boot_msrs().into_iter().map(|e| e.index).collect();
    for &extra in &[
        MSR_IA32_TSC,
        IA32_FS_BASE,
        IA32_GS_BASE,
        MSR_EFER,
        MSR_TSC_AUX,
        IA32_TSC_DEADLINE,
    ] {
        if !indices.contains(&extra) {
            indices.push(extra);
        }
    }
    indices
}

pub fn boot_msrs() -> Vec<kvm_msr_entry> {
    let zero = |index| kvm_msr_entry {
        index,
        ..Default::default()
    };
    vec![
        zero(MSR_IA32_SYSENTER_CS),
        zero(MSR_IA32_SYSENTER_ESP),
        zero(MSR_IA32_SYSENTER_EIP),
        zero(MSR_STAR),
        zero(MSR_CSTAR),
        zero(MSR_KERNEL_GS_BASE),
        zero(MSR_SYSCALL_MASK),
        zero(MSR_LSTAR),
        zero(MSR_IA32_TSC),
        kvm_msr_entry {
            index: MSR_IA32_MISC_ENABLE,
            data: 1,
            ..Default::default()
        },
        kvm_msr_entry {
            index: MSR_MTRR_DEF_TYPE,
            data: (1 << 11) | 6,
            ..Default::default()
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_msr_values_match_the_linux_entry_contract() {
        let msrs = boot_msrs();
        assert_eq!(msrs.len(), 11);
        assert_eq!(msrs[9].index, MSR_IA32_MISC_ENABLE);
        assert_eq!(msrs[9].data, 1);
        assert_eq!(msrs[10].index, MSR_MTRR_DEF_TYPE);
        assert_eq!(msrs[10].data, 0x806);
        assert!(msrs[..9].iter().all(|entry| entry.data == 0));
    }
}
