use std::mem;

use kvm_bindings::{kvm_fpu, kvm_regs, kvm_segment, kvm_sregs};

use super::layout::{GDT_START, IDT_START, PVH_INFO_START};
use super::pvh::GuestBlob;

const CR0_PE: u64 = 1;
const CR0_ET: u64 = 1 << 4;

pub fn boot_regs(entry: u64) -> kvm_regs {
    kvm_regs {
        rip: entry,
        rbx: PVH_INFO_START,
        rflags: 2,
        ..Default::default()
    }
}

pub fn boot_fpu() -> kvm_fpu {
    kvm_fpu {
        fcw: 0x37f,
        mxcsr: 0x1f80,
        ..Default::default()
    }
}

pub fn boot_sregs(mut sregs: kvm_sregs) -> (kvm_sregs, [GuestBlob; 2]) {
    let gdt = [
        gdt_entry(0, 0, 0),
        gdt_entry(0xc09b, 0, u32::MAX),
        gdt_entry(0xc093, 0, u32::MAX),
        gdt_entry(0x008b, 0, 0x67),
    ];
    sregs.gdt.base = GDT_START;
    sregs.gdt.limit = (mem::size_of_val(&gdt) - 1) as u16;
    sregs.idt.base = IDT_START;
    sregs.idt.limit = 7;
    sregs.cs = segment(gdt[1], 1);
    let data = segment(gdt[2], 2);
    sregs.ds = data;
    sregs.es = data;
    sregs.fs = data;
    sregs.gs = data;
    sregs.ss = data;
    sregs.tr = segment(gdt[3], 3);
    sregs.cr0 = CR0_PE | CR0_ET;
    sregs.cr3 = 0;
    sregs.cr4 = 0;
    sregs.efer = 0;

    let gdt_bytes = gdt.into_iter().flat_map(u64::to_le_bytes).collect();
    (
        sregs,
        [
            GuestBlob {
                address: GDT_START,
                bytes: gdt_bytes,
            },
            GuestBlob {
                address: IDT_START,
                bytes: vec![0; 8],
            },
        ],
    )
}

fn gdt_entry(flags: u16, base: u32, limit: u32) -> u64 {
    ((u64::from(base) & 0xff00_0000) << 32)
        | ((u64::from(flags) & 0x0000_f0ff) << 40)
        | ((u64::from(limit) & 0x000f_0000) << 32)
        | ((u64::from(base) & 0x00ff_ffff) << 16)
        | (u64::from(limit) & 0xffff)
}

fn segment(entry: u64, index: u8) -> kvm_segment {
    let granularity = ((entry >> 55) & 1) as u8;
    let limit = (((entry >> 32) & 0xf_0000) | (entry & 0xffff)) as u32;
    kvm_segment {
        base: ((entry >> 16) & 0xff_ffff) | ((entry >> 32) & 0xff00_0000),
        limit: if granularity == 0 {
            limit
        } else {
            (limit << 12) | 0xfff
        },
        selector: u16::from(index) * 8,
        type_: ((entry >> 40) & 0xf) as u8,
        present: ((entry >> 47) & 1) as u8,
        dpl: ((entry >> 45) & 3) as u8,
        db: ((entry >> 54) & 1) as u8,
        s: ((entry >> 44) & 1) as u8,
        l: ((entry >> 53) & 1) as u8,
        g: granularity,
        avl: ((entry >> 52) & 1) as u8,
        unusable: u8::from(((entry >> 47) & 1) == 0),
        padding: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_enter_the_pvh_entry_contract() {
        let regs = boot_regs(0x10_0000);
        assert_eq!(regs.rip, 0x10_0000);
        assert_eq!(regs.rbx, PVH_INFO_START);
        assert_eq!(regs.rflags, 2);
        let fpu = boot_fpu();
        assert_eq!(fpu.fcw, 0x37f);
        assert_eq!(fpu.mxcsr, 0x1f80);
    }

    #[test]
    fn special_registers_enter_flat_32_bit_protected_mode() {
        let (sregs, tables) = boot_sregs(kvm_sregs::default());
        assert_eq!(sregs.cr0, CR0_PE | CR0_ET);
        assert_eq!(sregs.cr3, 0);
        assert_eq!(sregs.cr4, 0);
        assert_eq!(sregs.efer, 0);
        assert_eq!(sregs.cs.selector, 8);
        assert_eq!(sregs.cs.limit, u32::MAX);
        assert_eq!(sregs.cs.db, 1);
        assert_eq!(sregs.cs.l, 0);
        assert_eq!(sregs.ds.selector, 16);
        assert_eq!(sregs.ds.limit, u32::MAX);
        assert_eq!(sregs.tr.selector, 24);
        assert_eq!(sregs.tr.limit, 0x67);
        assert_eq!(tables[0].address, GDT_START);
        assert_eq!(tables[0].bytes.len(), 32);
        assert_eq!(tables[1].address, IDT_START);
    }
}
