//! Shared helpers for the device integration tests.

#![allow(dead_code)]

pub mod guest;

use std::os::fd::{FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{fs, io};

use tokio::time;

/// A disk image that removes itself.
pub struct TempDisk {
    path: PathBuf,
}

impl TempDisk {
    pub fn new(name: &str, contents: &[u8]) -> Self {
        let path =
            std::env::temp_dir().join(format!("dragonball-poc-{}-{name}", std::process::id()));
        fs::write(&path, contents).expect("write temp disk");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn contents(&self) -> Vec<u8> {
        fs::read(&self.path).expect("read temp disk")
    }
}

impl Drop for TempDisk {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// A pipe standing in for the host side of a console.
pub fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: `pipe2` writes exactly two descriptors into `fds` on success.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both descriptors are freshly created and owned by nothing else.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Give the device task room to run, then check `condition`, until it holds.
///
/// Device tasks share this thread, so the test has to yield for them to make
/// progress at all.
pub async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    for _ in 0..2000 {
        if condition() {
            return;
        }
        time::sleep(Duration::from_millis(1)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Let other tasks run without waiting for anything in particular.
pub async fn settle() {
    for _ in 0..20 {
        time::sleep(Duration::from_millis(1)).await;
    }
}
