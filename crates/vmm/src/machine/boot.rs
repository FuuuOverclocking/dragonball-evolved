//! The data a PVH guest boots from, computed from the plan and from where the
//! kernel and the initrd landed. Nothing here touches guest memory.

use crate::align::GuestPageSize;
use crate::arch::x86_64::layout::{CMDLINE_START, HIMEM_START};
use crate::arch::x86_64::mptable::{self, MpTableError};
use crate::arch::x86_64::pvh::{self, GuestBlob, PvhError};
use crate::machine::plan::MachinePlan;
use crate::memory::layout::{MemoryLayout, WindowKind};
use crate::space::{Gpa, Range, Space};

#[derive(Debug, Clone, Copy, PartialEq, Eq, displaydoc::Display, thiserror::Error)]
pub enum BootDataError {
    /// guest memory has no RAM at address 0
    NoLowRam,
    /// PVH entry {0} lies outside low guest RAM above 1 MiB
    EntryOutsideLowRam(Gpa),
    /// initrd is empty
    EmptyInitrd,
    /// initrd of {size:#x} bytes does not fit in low RAM above the kernel, which ends at {kernel_end}
    InitrdTooLarge { size: u64, kernel_end: Gpa },
    /// describe guest memory for PVH
    Pvh(#[from] PvhError),
    /// build the MP table
    MpTable(#[from] MpTableError),
}

/// The boot RAM of `layout`, in guest-physical order.
pub(super) fn boot_ram(layout: &MemoryLayout) -> Vec<Range<Gpa>> {
    let mut ram: Vec<_> = layout
        .windows()
        .filter(|(_, window)| window.kind() == WindowKind::BootRam)
        .map(|(_, window)| window.placement())
        .collect();
    ram.sort_unstable_by_key(|range| range.start());
    ram
}

/// The boot RAM from address 0, below the 32-bit MMIO hole.
pub(super) fn low_ram(layout: &MemoryLayout) -> Result<Range<Gpa>, BootDataError> {
    boot_ram(layout)
        .first()
        .copied()
        .filter(|range| range.start() == Gpa(0))
        .ok_or(BootDataError::NoLowRam)
}

/// Checks that a 32-bit PVH entry can run where the kernel put it.
pub(super) fn check_entry(low_ram: Range<Gpa>, entry: Gpa) -> Result<(), BootDataError> {
    if entry >= HIMEM_START && low_ram.contains(entry) {
        Ok(())
    } else {
        Err(BootDataError::EntryOutsideLowRam(entry))
    }
}

/// Places `size` bytes of initrd at the top of low RAM, on a guest page, and
/// above both the kernel and the first MiB.
pub(super) fn place_initrd(
    low_ram: Range<Gpa>,
    kernel_end: Gpa,
    size: u64,
) -> Result<Range<Gpa>, BootDataError> {
    if size == 0 {
        return Err(BootDataError::EmptyInitrd);
    }
    let too_large = BootDataError::InitrdTooLarge { size, kernel_end };
    let top = low_ram.end().ok_or(too_large)?;
    let start = top
        .0
        .checked_sub(size)
        .map(|start| Gpa(start).align_down(GuestPageSize::X86_4K.alignment()))
        .filter(|&start| start >= kernel_end.max(HIMEM_START))
        .ok_or(too_large)?;
    Range::new(start, size).ok_or(too_large)
}

/// Everything to write before the first vCPU runs, except the kernel, the
/// initrd and the per-vCPU descriptor tables: the command line, the PVH start
/// info with its memory map and module list, and the MP table.
pub(super) fn boot_data(
    plan: &MachinePlan,
    initrd: Option<Range<Gpa>>,
) -> Result<Vec<GuestBlob>, BootDataError> {
    let mut blobs = vec![GuestBlob {
        address: CMDLINE_START,
        bytes: plan.cmdline().to_bytes_with_nul(),
    }];
    blobs.extend(pvh::boot_data(
        &boot_ram(plan.memory()),
        CMDLINE_START,
        initrd,
    )?);
    blobs.push(mptable::build(plan.vcpus())?);
    Ok(blobs)
}

#[cfg(test)]
mod tests {
    use api::RawVmmOp;
    use api::boot::{KernelConfig, SetKernel};
    use api::machine::{MemorySizeMib, SetCpus, SetMemory, VcpuCount};
    use linux_loader::loader::elf::start_info::{hvm_modlist_entry, hvm_start_info};
    use vm_memory::ByteValued;

    use super::*;
    use crate::arch::x86_64::layout::{MODLIST_START, PVH_INFO_START, SYSTEM_MEM};
    use crate::kvm_context::HostCapabilities;
    use crate::spec::MachineSpec;

    const LOW_128M: Range<Gpa> = Range::<Gpa>::const_new(Gpa(0), 128 << 20).unwrap();

    fn plan(cpus: u8, memory_mib: u32) -> MachinePlan {
        let spec = MachineSpec::default()
            .apply(
                SetKernel(KernelConfig {
                    path: "/test/vmlinux".into(),
                    cmdline: Some("console=ttyS0".into()),
                    initrd: None,
                })
                .into_vmm_command(),
            )
            .and_then(|spec| spec.apply(SetCpus(VcpuCount(cpus)).into_vmm_command()))
            .and_then(|spec| spec.apply(SetMemory(MemorySizeMib(memory_mib)).into_vmm_command()))
            .unwrap();
        let mut host = HostCapabilities::fake(46, 8, 4096);
        host.max_vcpus = 254;
        MachinePlan::new(&spec, &host).unwrap().0
    }

    #[test]
    fn low_ram_is_the_boot_window_at_zero() {
        let plan = plan(1, 4096);
        assert_eq!(
            low_ram(plan.memory()),
            Ok(Range::new(Gpa(0), 3 << 30).unwrap())
        );
        assert_eq!(boot_ram(plan.memory()).len(), 2);
    }

    #[test]
    fn the_entry_must_be_in_low_ram_above_one_mib() {
        assert_eq!(check_entry(LOW_128M, HIMEM_START), Ok(()));
        assert_eq!(check_entry(LOW_128M, Gpa((128 << 20) - 1)), Ok(()));
        for entry in [Gpa(0x7000), Gpa(128 << 20), Gpa(4 << 30)] {
            assert_eq!(
                check_entry(LOW_128M, entry),
                Err(BootDataError::EntryOutsideLowRam(entry))
            );
        }
    }

    #[test]
    fn the_initrd_sits_page_aligned_at_the_top_of_low_ram() {
        let initrd = place_initrd(LOW_128M, Gpa(0x20_0000), 0x1800).unwrap();
        assert_eq!(
            initrd,
            Range::new(Gpa((128 << 20) - 0x2000), 0x1800).unwrap()
        );
        // An initrd may end right where low RAM does, and start right at the kernel's end.
        let whole = (128 << 20) - 0x20_0000;
        assert_eq!(
            place_initrd(LOW_128M, Gpa(0x20_0000), whole).unwrap(),
            Range::new(Gpa(0x20_0000), whole).unwrap()
        );
    }

    #[test]
    fn an_initrd_that_would_overlap_the_kernel_is_refused() {
        let kernel_end = Gpa(0x20_0000);
        let too_large = (128 << 20) - 0x20_0000 + 1;
        assert_eq!(
            place_initrd(LOW_128M, kernel_end, too_large),
            Err(BootDataError::InitrdTooLarge {
                size: too_large,
                kernel_end
            })
        );
        assert_eq!(
            place_initrd(LOW_128M, kernel_end, u64::MAX),
            Err(BootDataError::InitrdTooLarge {
                size: u64::MAX,
                kernel_end
            })
        );
        // Nor may it reach below 1 MiB when the kernel ends lower.
        assert!(place_initrd(LOW_128M, Gpa(0), (128 << 20) - 0x8_0000).is_err());
        assert_eq!(
            place_initrd(LOW_128M, kernel_end, 0),
            Err(BootDataError::EmptyInitrd)
        );
    }

    #[test]
    fn boot_data_holds_the_cmdline_start_info_and_mp_table() {
        let plan = plan(2, 128);
        let initrd = Range::new(Gpa(120 << 20), 0x1000).unwrap();
        let blobs = boot_data(&plan, Some(initrd)).unwrap();
        let addresses: Vec<_> = blobs.iter().map(|blob| blob.address).collect();
        assert_eq!(
            addresses,
            [
                CMDLINE_START,
                PVH_INFO_START,
                crate::arch::x86_64::layout::MEMMAP_START,
                MODLIST_START,
                SYSTEM_MEM.start(),
            ]
        );
        assert_eq!(blobs[0].bytes, b"console=ttyS0\0");
        let info = hvm_start_info::from_slice(&blobs[1].bytes).unwrap();
        assert_eq!(info.cmdline_paddr, CMDLINE_START.0);
        assert_eq!(info.nr_modules, 1);
        let module = hvm_modlist_entry::from_slice(&blobs[3].bytes).unwrap();
        assert_eq!((module.paddr, module.size), (120 << 20, 0x1000));
        assert_eq!(blobs[4], mptable::build(2).unwrap());

        let without = boot_data(&plan, None).unwrap();
        assert_eq!(without.len(), 4);
    }
}
