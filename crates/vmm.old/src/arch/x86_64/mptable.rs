use thiserror::Error;

use super::layout::{IOAPIC_START, LAPIC_START, SYSTEM_MEM_END, SYSTEM_MEM_START};
use super::pvh::GuestBlob;

const MAX_CPUS: u8 = 254;
const APIC_VERSION: u8 = 0x14;
const CPU_STEPPING: u32 = 0x600;
const CPU_FEATURE_APIC: u32 = 0x200;
const CPU_FEATURE_FPU: u32 = 0x001;
const LEGACY_IRQS: u8 = 24;
const HEADER_LEN: usize = 44;
const FLOATING_POINTER_LEN: usize = 16;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum MpTableError {
    #[error("MP table needs at least one vCPU")]
    NoCpus,
    #[error("MP table supports at most 254 vCPUs")]
    TooManyCpus,
    #[error("MP table does not fit in the system data area")]
    TooLarge,
}

pub fn build(num_cpus: u8) -> Result<GuestBlob, MpTableError> {
    if num_cpus == 0 {
        return Err(MpTableError::NoCpus);
    }
    if num_cpus > MAX_CPUS {
        return Err(MpTableError::TooManyCpus);
    }

    let ioapic_id = num_cpus + 1;
    let mut table = vec![0; HEADER_LEN];
    for cpu in 0..num_cpus {
        table.push(0);
        table.push(cpu);
        table.push(APIC_VERSION);
        table.push(1 | (u8::from(cpu == 0) * 2));
        push_u32(&mut table, CPU_STEPPING);
        push_u32(&mut table, CPU_FEATURE_APIC | CPU_FEATURE_FPU);
        table.extend_from_slice(&[0; 8]);
    }
    table.extend_from_slice(&[1, 0]);
    table.extend_from_slice(b"ISA   ");
    table.extend_from_slice(&[2, ioapic_id, APIC_VERSION, 1]);
    push_u32(&mut table, IOAPIC_START as u32);
    for irq in 0..LEGACY_IRQS {
        table.extend_from_slice(&[3, 0]);
        push_u16(&mut table, 0);
        table.extend_from_slice(&[0, irq, ioapic_id, irq]);
    }
    table.extend_from_slice(&[4, 3]);
    push_u16(&mut table, 0);
    table.extend_from_slice(&[0, 0, 0, 0]);
    table.extend_from_slice(&[4, 1]);
    push_u16(&mut table, 0);
    table.extend_from_slice(&[0, 0, 0xff, 1]);

    table[..4].copy_from_slice(b"PCMP");
    let table_len = u16::try_from(table.len()).map_err(|_| MpTableError::TooLarge)?;
    table[4..6].copy_from_slice(&table_len.to_le_bytes());
    table[6] = 4;
    table[8..16].copy_from_slice(b"DBE     ");
    table[16..28].copy_from_slice(b"MICROVM     ");
    let entry_count = u16::from(num_cpus) + 1 + 1 + u16::from(LEGACY_IRQS) + 2;
    table[34..36].copy_from_slice(&entry_count.to_le_bytes());
    table[36..40].copy_from_slice(&(LAPIC_START as u32).to_le_bytes());
    table[7] = checksum(&table);

    let table_address = SYSTEM_MEM_START + FLOATING_POINTER_LEN as u64;
    let mut floating = Vec::with_capacity(FLOATING_POINTER_LEN);
    floating.extend_from_slice(b"_MP_");
    push_u32(&mut floating, table_address as u32);
    floating.extend_from_slice(&[1, 4, 0, 0, 0, 0, 0, 0]);
    floating[10] = checksum(&floating);

    floating.extend_from_slice(&table);
    if SYSTEM_MEM_START + floating.len() as u64 > SYSTEM_MEM_END {
        return Err(MpTableError::TooLarge);
    }
    Ok(GuestBlob {
        address: SYSTEM_MEM_START,
        bytes: floating,
    })
}

fn push_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn checksum(bytes: &[u8]) -> u8 {
    0u8.wrapping_sub(bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_has_valid_headers_counts_and_checksums() {
        let blob = build(4).unwrap();
        assert_eq!(blob.address, SYSTEM_MEM_START);
        let floating = &blob.bytes[..FLOATING_POINTER_LEN];
        let table = &blob.bytes[FLOATING_POINTER_LEN..];
        assert_eq!(&floating[..4], b"_MP_");
        assert_eq!(
            u32::from_le_bytes(floating[4..8].try_into().unwrap()),
            0x9_fc10
        );
        assert_eq!(
            floating
                .iter()
                .fold(0u8, |sum, byte| sum.wrapping_add(*byte)),
            0
        );
        assert_eq!(&table[..4], b"PCMP");
        assert_eq!(
            u16::from_le_bytes(table[4..6].try_into().unwrap()) as usize,
            table.len()
        );
        assert_eq!(u16::from_le_bytes(table[34..36].try_into().unwrap()), 32);
        assert_eq!(
            table.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)),
            0
        );
    }

    #[test]
    fn every_cpu_is_present_and_only_zero_is_the_bsp() {
        let table = build(3).unwrap().bytes;
        let entries = &table[FLOATING_POINTER_LEN + HEADER_LEN..];
        for cpu in 0..3usize {
            let entry = &entries[cpu * 20..(cpu + 1) * 20];
            assert_eq!(entry[0], 0);
            assert_eq!(entry[1], cpu as u8);
            assert_eq!(entry[3], if cpu == 0 { 3 } else { 1 });
        }
    }

    #[test]
    fn invalid_cpu_counts_are_rejected() {
        assert_eq!(build(0), Err(MpTableError::NoCpus));
        assert_eq!(build(255), Err(MpTableError::TooManyCpus));
    }
}
