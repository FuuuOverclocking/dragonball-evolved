use kvm_bindings::{CpuId, kvm_msr_entry};

const MSR_IA32_TSC: u32 = 0x10;
const MSR_IA32_SYSENTER_CS: u32 = 0x174;
const MSR_IA32_SYSENTER_ESP: u32 = 0x175;
const MSR_IA32_SYSENTER_EIP: u32 = 0x176;
const MSR_IA32_MISC_ENABLE: u32 = 0x1a0;
const MSR_IA32_XFD: u32 = 0x1c4;
const MSR_IA32_XFD_ERR: u32 = 0x1c5;
const MSR_MTRR_DEF_TYPE: u32 = 0x2ff;
const MSR_IA32_U_CET: u32 = 0x6a0;
const MSR_IA32_S_CET: u32 = 0x6a2;
const MSR_IA32_PL0_SSP: u32 = 0x6a4;
const MSR_IA32_PL1_SSP: u32 = 0x6a5;
const MSR_IA32_PL2_SSP: u32 = 0x6a6;
const MSR_IA32_PL3_SSP: u32 = 0x6a7;
const MSR_IA32_INT_SSP_TAB: u32 = 0x6a8;
const MSR_IA32_XSS: u32 = 0xda0;
const XFEATURE_MASK_CET: u64 = (1 << 11) | (1 << 12);
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

// Linux arch/x86/include/uapi/asm/kvm_para.h and arch/x86/kvm/msrs.c.
pub(crate) const MSR_KVM_WALL_CLOCK: u32 = 0x11;
pub(crate) const MSR_KVM_SYSTEM_TIME: u32 = 0x12;
pub(crate) const MSR_KVM_WALL_CLOCK_NEW: u32 = 0x4b56_4d00;
pub(crate) const MSR_KVM_SYSTEM_TIME_NEW: u32 = 0x4b56_4d01;
const MSR_KVM_ASYNC_PF_EN: u32 = 0x4b56_4d02;
const MSR_KVM_STEAL_TIME: u32 = 0x4b56_4d03;
const MSR_KVM_PV_EOI_EN: u32 = 0x4b56_4d04;
const MSR_KVM_ASYNC_PF_INT: u32 = 0x4b56_4d06;
const KVM_CPUID_FEATURES: u32 = 0x4000_0001;
const KVM_FEATURE_CLOCKSOURCE: u32 = 0;
const KVM_FEATURE_CLOCKSOURCE2: u32 = 3;
const KVM_FEATURE_ASYNC_PF: u32 = 4;
const KVM_FEATURE_STEAL_TIME: u32 = 5;
const KVM_FEATURE_PV_EOI: u32 = 6;
const KVM_FEATURE_ASYNC_PF_INT: u32 = 14;
const KVM_MSR_ENABLED: u64 = 1;

/// Returns the fixed architectural MSR indices saved alongside KVM registrations.
///
/// The set covers `boot_msrs` plus segment bases, EFER and TSC helpers.
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

/// Required MSR ABI for CPUID's xstate controls, CET and KVM registrations; active SSP uses ONE_REG.
pub(crate) fn snapshot_msr_indices(cpuid: &CpuId) -> Vec<u32> {
    let features = cpuid
        .as_slice()
        .iter()
        .find(|entry| entry.function == KVM_CPUID_FEATURES && entry.index == 0)
        .map_or(0, |entry| entry.eax);
    let mut indices = msr_indices();
    let xstate = cpuid_leaf(cpuid, 0xd, 1);
    if xstate.is_some_and(|leaf| leaf.eax & (1 << 3) != 0) {
        indices.push(MSR_IA32_XSS);
    }
    if xstate.is_some_and(|leaf| leaf.eax & (1 << 4) != 0) {
        indices.extend([MSR_IA32_XFD, MSR_IA32_XFD_ERR]);
    }
    let features7 = cpuid_leaf(cpuid, 7, 0);
    let shstk = has_shadow_stack(cpuid);
    let ibt = features7.is_some_and(|leaf| leaf.edx & (1 << 20) != 0);
    if shstk || ibt {
        indices.extend([MSR_IA32_U_CET, MSR_IA32_S_CET]);
    }
    if shstk {
        indices.extend([
            MSR_IA32_PL0_SSP,
            MSR_IA32_PL1_SSP,
            MSR_IA32_PL2_SSP,
            MSR_IA32_PL3_SSP,
        ]);
        if cpuid_leaf(cpuid, 0x8000_0001, 0).is_some_and(|leaf| leaf.edx & (1 << 29) != 0) {
            indices.push(MSR_IA32_INT_SSP_TAB);
        }
    }
    for (feature, index) in [
        (KVM_FEATURE_CLOCKSOURCE, MSR_KVM_SYSTEM_TIME),
        (KVM_FEATURE_CLOCKSOURCE, MSR_KVM_WALL_CLOCK),
        (KVM_FEATURE_CLOCKSOURCE2, MSR_KVM_SYSTEM_TIME_NEW),
        (KVM_FEATURE_CLOCKSOURCE2, MSR_KVM_WALL_CLOCK_NEW),
        (KVM_FEATURE_STEAL_TIME, MSR_KVM_STEAL_TIME),
        // Install the interrupt vector before enabling async PF delivery.
        (KVM_FEATURE_ASYNC_PF_INT, MSR_KVM_ASYNC_PF_INT),
        (KVM_FEATURE_ASYNC_PF, MSR_KVM_ASYNC_PF_EN),
        (KVM_FEATURE_PV_EOI, MSR_KVM_PV_EOI_EN),
    ] {
        if features & (1 << feature) != 0 {
            indices.push(index);
        }
    }
    // ASYNC_PF_ACK is an action, not a registration (GET always returns zero).
    indices
}

pub(crate) fn has_shadow_stack(cpuid: &CpuId) -> bool {
    cpuid_leaf(cpuid, 7, 0).is_some_and(|leaf| leaf.ecx & (1 << 7) != 0)
}

fn cpuid_leaf(cpuid: &CpuId, function: u32, index: u32) -> Option<&kvm_bindings::kvm_cpuid_entry2> {
    cpuid
        .as_slice()
        .iter()
        .find(|entry| entry.function == function && entry.index == index)
}

/// These controls must precede SET_XSAVE, particularly XFD: KVM clears disabled
/// components' XSTATE_BV bits on SET rather than reporting an error.
pub(crate) fn is_xstate_control(index: u32) -> bool {
    matches!(index, MSR_IA32_XSS | MSR_IA32_XFD | MSR_IA32_XFD_ERR)
}

/// Reject supervisor state without a known MSR representation. Never assume
/// GET_XSAVE2 exports it, or silently lose dynamic state that SET would clear.
pub(crate) fn snapshot_xstate_supported(
    cpuid: &CpuId,
    entries: &[kvm_msr_entry],
    xstate_bv: u64,
) -> bool {
    let value = |index| {
        entries
            .iter()
            .find(|entry| entry.index == index)
            .map_or(0, |entry| entry.data)
    };
    let supported_xss = cpuid_leaf(cpuid, 0xd, 1)
        .map_or(0, |leaf| u64::from(leaf.ecx) | (u64::from(leaf.edx) << 32));
    let supported_xcr0 = cpuid_leaf(cpuid, 0xd, 0)
        .map_or(0, |leaf| u64::from(leaf.eax) | (u64::from(leaf.edx) << 32));
    value(MSR_IA32_XSS) & !(supported_xss & XFEATURE_MASK_CET) == 0
        && xstate_bv & !(supported_xcr0 | 3) == 0
        && xstate_bv & value(MSR_IA32_XFD) == 0
}

/// Both clock namespaces alias the same KVM state, even if only one was written.
pub(crate) fn snapshot_msrs_match(cpuid: &CpuId, entries: &[kvm_msr_entry]) -> bool {
    let aliases_match = |old, new| {
        let value = |index| {
            entries
                .iter()
                .find(|entry| entry.index == index)
                .map(|entry| entry.data)
        };
        match (value(old), value(new)) {
            (Some(old), Some(new)) => old == new,
            _ => true,
        }
    };
    entries
        .iter()
        .map(|entry| entry.index)
        .eq(snapshot_msr_indices(cpuid))
        && aliases_match(MSR_KVM_SYSTEM_TIME, MSR_KVM_SYSTEM_TIME_NEW)
        && aliases_match(MSR_KVM_WALL_CLOCK, MSR_KVM_WALL_CLOCK_NEW)
}

/// Only system-time bit 0 enables pvclock; a wall-clock address does not.
pub(crate) fn pvclock_enabled(entries: &[kvm_msr_entry]) -> bool {
    entries
        .iter()
        .rev()
        .find(|entry| matches!(entry.index, MSR_KVM_SYSTEM_TIME | MSR_KVM_SYSTEM_TIME_NEW))
        .is_some_and(|entry| entry.data & KVM_MSR_ENABLED != 0)
}

pub(crate) fn wall_clock_msrs(
    entries: &[kvm_msr_entry],
) -> impl Iterator<Item = kvm_msr_entry> + '_ {
    entries
        .iter()
        .copied()
        .filter(|entry| matches!(entry.index, MSR_KVM_WALL_CLOCK | MSR_KVM_WALL_CLOCK_NEW))
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

    fn kvm_cpuid(features: u32) -> CpuId {
        CpuId::from_entries(&[kvm_bindings::kvm_cpuid_entry2 {
            function: KVM_CPUID_FEATURES,
            eax: features,
            ..Default::default()
        }])
        .unwrap()
    }

    #[test]
    fn snapshot_registrations_follow_kvm_feature_support() {
        assert_eq!(snapshot_msr_indices(&kvm_cpuid(0)), msr_indices());
        for (feature, system, wall) in [
            (
                KVM_FEATURE_CLOCKSOURCE,
                MSR_KVM_SYSTEM_TIME,
                MSR_KVM_WALL_CLOCK,
            ),
            (
                KVM_FEATURE_CLOCKSOURCE2,
                MSR_KVM_SYSTEM_TIME_NEW,
                MSR_KVM_WALL_CLOCK_NEW,
            ),
        ] {
            let indices = snapshot_msr_indices(&kvm_cpuid(1 << feature));
            assert_eq!(&indices[msr_indices().len()..], &[system, wall]);
        }
        let indices = snapshot_msr_indices(&kvm_cpuid(
            (1 << KVM_FEATURE_STEAL_TIME)
                | (1 << KVM_FEATURE_ASYNC_PF)
                | (1 << KVM_FEATURE_ASYNC_PF_INT)
                | (1 << KVM_FEATURE_PV_EOI),
        ));
        assert_eq!(
            &indices[msr_indices().len()..],
            &[
                MSR_KVM_STEAL_TIME,
                MSR_KVM_ASYNC_PF_INT,
                MSR_KVM_ASYNC_PF_EN,
                MSR_KVM_PV_EOI_EN
            ]
        );
    }

    #[test]
    fn cet_snapshot_msrs_exclude_the_non_abi_active_ssp_selector() {
        let cpuid = CpuId::from_entries(&[
            kvm_bindings::kvm_cpuid_entry2 {
                function: 7,
                ecx: 1 << 7,
                ..Default::default()
            },
            kvm_bindings::kvm_cpuid_entry2 {
                function: 0x8000_0001,
                edx: 1 << 29,
                ..Default::default()
            },
        ])
        .unwrap();
        assert!(has_shadow_stack(&cpuid));
        let indices = snapshot_msr_indices(&cpuid);
        assert_eq!(
            &indices[msr_indices().len()..],
            &[
                MSR_IA32_U_CET,
                MSR_IA32_S_CET,
                MSR_IA32_PL0_SSP,
                MSR_IA32_PL1_SSP,
                MSR_IA32_PL2_SSP,
                MSR_IA32_PL3_SSP,
                MSR_IA32_INT_SSP_TAB,
            ]
        );
    }

    #[test]
    fn pvclock_requires_system_time_enable_not_cpuid_or_a_wall_clock_address() {
        assert!(!pvclock_enabled(&[]));
        for system in [MSR_KVM_SYSTEM_TIME, MSR_KVM_SYSTEM_TIME_NEW] {
            for data in [0, 0x2000, 0x2001] {
                let entries = [
                    kvm_msr_entry {
                        index: system,
                        data,
                        ..Default::default()
                    },
                    kvm_msr_entry {
                        index: MSR_KVM_WALL_CLOCK_NEW,
                        data: 0x3001,
                        ..Default::default()
                    },
                ];
                assert_eq!(pvclock_enabled(&entries), data & KVM_MSR_ENABLED != 0);
            }
        }
    }

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
