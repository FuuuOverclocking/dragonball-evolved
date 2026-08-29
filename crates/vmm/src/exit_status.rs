use logger::{Level, log_unrestricted};

/// Why the vmm thread stopped.
#[derive(Debug, Clone)]
pub enum VmmExitStatus {
    /// The vmm shut down as asked.
    Ok,
    /// The vmm stopped because of an error it could not recover from.
    Error(String),
}

impl VmmExitStatus {
    pub fn print_log(&self) {
        let log_level = match self {
            VmmExitStatus::Ok => Level::Info,
            VmmExitStatus::Error(_) => Level::Error,
        };
        log_unrestricted!(log_level, "vmm exit with status {self:?}");
    }
}
