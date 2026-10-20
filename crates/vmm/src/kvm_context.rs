use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result};

pub struct KvmContext {
    kvm_fd: File,
}

impl KvmContext {
    pub fn from_dev_path(path: impl AsRef<Path>) -> Result<Self> {
        let kvm_fd = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(path.as_ref())
            .context("open kvm dev")?;
        Ok(Self { kvm_fd })
    }
}
