use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const ENTRY: u32 = 0x10_0000;
static NEXT: AtomicU64 = AtomicU64::new(0);

pub struct TempFile(PathBuf);

impl TempFile {
    pub fn new(contents: &[u8]) -> Self {
        let path = std::env::temp_dir().join(format!(
            "dbe-guest-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, contents).unwrap();
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub fn image(code: &[u8], pvh: bool) -> Vec<u8> {
    let mut elf = vec![0u8; 0x200];
    elf[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    put16(&mut elf, 16, 2);
    put16(&mut elf, 18, 62);
    put32(&mut elf, 20, 1);
    put64(&mut elf, 24, u64::from(ENTRY));
    put64(&mut elf, 32, 64);
    put16(&mut elf, 52, 64);
    put16(&mut elf, 54, 56);
    put16(&mut elf, 56, 2);
    put32(&mut elf, 64, 1);
    put32(&mut elf, 68, 7);
    put64(&mut elf, 72, 0x200);
    put64(&mut elf, 80, u64::from(ENTRY));
    put64(&mut elf, 88, u64::from(ENTRY));
    put64(&mut elf, 96, code.len() as u64);
    put64(&mut elf, 104, code.len() as u64);
    put64(&mut elf, 112, 1);
    put32(&mut elf, 120, 4);
    put64(&mut elf, 128, 0x100);
    put64(&mut elf, 152, 20);
    put64(&mut elf, 160, 20);
    put64(&mut elf, 168, 4);
    put32(&mut elf, 0x100, 4);
    put32(&mut elf, 0x104, 4);
    put32(&mut elf, 0x108, if pvh { 0x12 } else { 0 });
    elf[0x10c..0x110].copy_from_slice(b"Xen\0");
    put32(&mut elf, 0x110, ENTRY);
    elf.extend_from_slice(code);
    elf
}

pub fn serial_reset_payload(block: bool) -> Vec<u8> {
    let mut code = Vec::new();
    if block {
        for (address, value) in [
            (0x1000, 0x4000),
            (0x1004, 0),
            (0x1008, 16),
            (0x100c, 0x1_0001),
            (0x1010, 0x5000),
            (0x1014, 0),
            (0x1018, 512),
            (0x101c, 0x2_0003),
            (0x1020, 0x8000),
            (0x1024, 0),
            (0x1028, 1),
            (0x102c, 2),
            (0x2000, 0x1_0000),
            (0x2004, 0),
            (0xc000_0070, 1),
            (0xc000_0070, 3),
            (0xc000_0024, 1),
            (0xc000_0020, 1),
            (0xc000_0070, 11),
            (0xc000_0030, 0),
            (0xc000_0038, 16),
            (0xc000_0080, 0x1000),
            (0xc000_0090, 0x2000),
            (0xc000_00a0, 0x3000),
            (0xc000_0044, 1),
            (0xc000_0070, 15),
            (0xc000_0050, 0),
        ] {
            mov32(&mut code, address, value);
        }
        code.extend_from_slice(&[0x66, 0x83, 0x3d]);
        code.extend_from_slice(&0x3002u32.to_le_bytes());
        code.extend_from_slice(&[1, 0x75, 0xf6]);
        code.extend_from_slice(&[0x80, 0x3d]);
        code.extend_from_slice(&0x5000u32.to_le_bytes());
        code.extend_from_slice(&[0xab, 0x74, 2, 0x0f, 0x0b]);
        code.extend_from_slice(&[0x80, 0x3d]);
        code.extend_from_slice(&0x8000u32.to_le_bytes());
        code.extend_from_slice(&[0, 0x74, 2, 0x0f, 0x0b]);
    }
    code.extend_from_slice(&[
        0x8b, 0x03, 0x3d, 0x78, 0xc5, 0x6e, 0x33, 0x74, 2, 0x0f, 0x0b,
    ]);
    code.extend_from_slice(&[0x66, 0xba, 0xf8, 0x03]);
    for byte in b"DBE_BOOT_OK\n" {
        code.extend_from_slice(&[0xb0, *byte, 0xee]);
    }
    code.extend_from_slice(&[0x66, 0xba, 0x64, 0, 0xb0, 0xfe, 0xee, 0xf4, 0xeb, 0xfd]);
    code
}

fn mov32(code: &mut Vec<u8>, address: u32, value: u32) {
    code.extend_from_slice(&[0xc7, 0x05]);
    code.extend_from_slice(&address.to_le_bytes());
    code.extend_from_slice(&value.to_le_bytes());
}

fn put16(buf: &mut [u8], at: usize, value: u16) {
    buf[at..at + 2].copy_from_slice(&value.to_le_bytes());
}
fn put32(buf: &mut [u8], at: usize, value: u32) {
    buf[at..at + 4].copy_from_slice(&value.to_le_bytes());
}
fn put64(buf: &mut [u8], at: usize, value: u64) {
    buf[at..at + 8].copy_from_slice(&value.to_le_bytes());
}
