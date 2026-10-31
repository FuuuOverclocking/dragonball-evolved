pub mod guest;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub struct TempDisk {
    path: PathBuf,
}

impl TempDisk {
    pub fn new(name: &str, contents: &[u8]) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "dbe-block-{}-{}-{name}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, contents).unwrap();
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn contents(&self) -> Vec<u8> {
        fs::read(&self.path).unwrap()
    }
}

impl Drop for TempDisk {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}
