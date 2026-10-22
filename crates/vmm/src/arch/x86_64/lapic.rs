use kvm_bindings::kvm_lapic_state;

const APIC_LVT0: usize = 0x350;
const APIC_LVT1: usize = 0x360;
const APIC_MODE_NMI: u32 = 0x4;
const APIC_MODE_EXTINT: u32 = 0x7;

pub fn configure_lint(mut lapic: kvm_lapic_state) -> kvm_lapic_state {
    set_delivery_mode(&mut lapic, APIC_LVT0, APIC_MODE_EXTINT);
    set_delivery_mode(&mut lapic, APIC_LVT1, APIC_MODE_NMI);
    lapic
}

fn set_delivery_mode(lapic: &mut kvm_lapic_state, offset: usize, mode: u32) {
    let current = read_u32(lapic, offset);
    write_u32(lapic, offset, (current & !0x700) | (mode << 8));
}

fn read_u32(lapic: &kvm_lapic_state, offset: usize) -> u32 {
    let mut bytes = [0u8; 4];
    for (target, source) in bytes.iter_mut().zip(&lapic.regs[offset..offset + 4]) {
        *target = *source as u8;
    }
    u32::from_le_bytes(bytes)
}

fn write_u32(lapic: &mut kvm_lapic_state, offset: usize, value: u32) {
    for (slot, byte) in lapic.regs[offset..offset + 4]
        .iter_mut()
        .zip(value.to_le_bytes())
    {
        *slot = byte as i8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lint_delivery_modes_are_set_without_touching_other_bits() {
        let mut lapic = kvm_lapic_state::default();
        write_u32(&mut lapic, APIC_LVT0, 0x1_ffff);
        write_u32(&mut lapic, APIC_LVT1, 0x2_ffff);
        let lapic = configure_lint(lapic);
        assert_eq!(read_u32(&lapic, APIC_LVT0), (0x1_ffff & !0x700) | 0x700);
        assert_eq!(read_u32(&lapic, APIC_LVT1), (0x2_ffff & !0x700) | 0x400);
    }
}
