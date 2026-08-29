//! Logging backend: the `log::Log` implementation, the writer thread and the
//! crash log.
//!
//! Only the bin crate touches this crate. Library crates log through `logger`,
//! which knows nothing about where records end up.
//!
//! Normal records and crash records take separate paths. A normal record is
//! rendered on the caller's thread only as far as its message, then queued; the
//! writer thread owns the settings and does the field assembly. A crash record
//! cannot wait for a thread or take a lock, so it is written straight to a
//! reserved descriptor from the signal handler. See `docs/logger-design.md`.

mod buf;
mod crash;
mod entry;
mod time;
mod writer;

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Mutex, OnceLock};
use std::thread;

use log::{LevelFilter, Log, Metadata, Record};

use crate::entry::Entry;
use crate::writer::{Fields, Msg, Settings, Sink, Update};

/// Queue depth. Deep enough to absorb a burst, shallow enough that a stalled
/// writer costs a few hundred kilobytes rather than the host's memory.
const QUEUE_DEPTH: usize = 8192;

static FRONTEND: Frontend = Frontend;
static SENDER: OnceLock<SyncSender<Msg>> = OnceLock::new();
static INSTANCE_ID: OnceLock<String> = OnceLock::new();

/// Records the queue refused since the last one it accepted.
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// Whether the crash log tracks the normal target. Only [`split`] reads or writes
/// it; the handler still knows nothing but its own descriptor.
static CRASH_FOLLOWS_LOG: AtomicBool = AtomicBool::new(true);

/// A duplicate of the current log target, so `SameAsLog` can point the crash log
/// at it without a target change in the same call. The writer thread owns the
/// original and may drop it at any time, hence a copy rather than a raw number.
static LOG_FD: Mutex<Option<OwnedFd>> = Mutex::new(None);

/// How a line is laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    #[default]
    Text,
    Json,
}

/// Where lines go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Stderr,
    File(PathBuf),
}

/// Where crash records go.
///
/// The default is [`CrashTarget::SameAsLog`]: under a supervisor stderr is often
/// `/dev/null`, and a crash record that lands there is a crash record nobody
/// reads.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CrashTarget {
    /// Track whatever the normal log target is.
    #[default]
    SameAsLog,
    /// A target of its own, independent of the normal log.
    Own(Target),
}

/// A logger configuration. `None` leaves the current value alone.
#[derive(Debug, Default)]
pub struct Config {
    /// Instance id. Only [`init`] honours it; it cannot be changed later.
    pub id: Option<String>,
    pub level: Option<LevelFilter>,
    pub target: Option<Target>,
    pub crash_target: Option<CrashTarget>,
    pub format: Option<Format>,
    pub show_tid: Option<bool>,
    pub show_thread_name: Option<bool>,
    pub show_target: Option<bool>,
    pub show_file_line: Option<bool>,
    pub show_id: Option<bool>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("logger is already initialised")]
    AlreadyInitialised,

    #[error("logger is not initialised")]
    NotInitialised,

    #[error("open log target {path}")]
    OpenTarget {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("spawn the writer thread")]
    Spawn(#[source] io::Error),

    #[error("install the crash log")]
    Crash(#[source] io::Error),
}

/// Install the global logger and start the writer thread.
///
/// Returns a guard whose drop flushes; hold it for as long as the process should
/// keep logging.
pub fn init(mut config: Config) -> Result<FlushGuard, Error> {
    // Both have to be on record before a handler can run.
    time::capture_utc_offset();
    if let Some(id) = config.id.take() {
        INSTANCE_ID.set(id).map_err(|_| Error::AlreadyInitialised)?;
    }

    crash::init().map_err(Error::Crash)?;

    let (level, update) = split(config)?;
    let mut settings = Settings {
        sink: Sink::Stderr,
        format: Format::default(),
        fields: Fields::default(),
    };
    settings.apply(update);

    let (sender, receiver) = mpsc::sync_channel(QUEUE_DEPTH);
    thread::Builder::new()
        .name("logger".into())
        .spawn(move || writer::run(receiver, settings))
        .map_err(Error::Spawn)?;
    SENDER.set(sender).map_err(|_| Error::AlreadyInitialised)?;

    log::set_logger(&FRONTEND).map_err(|_| Error::AlreadyInitialised)?;
    log::set_max_level(level.unwrap_or(LevelFilter::Info));

    Ok(FlushGuard)
}

/// Reconfigure a running logger. `Config::id` is ignored.
///
/// The change reaches the writer through the record queue, so it applies to
/// exactly the records logged after this call returns.
pub fn update(config: Config) -> Result<(), Error> {
    let (level, update) = split(config)?;
    let sender = SENDER.get().ok_or(Error::NotInitialised)?;

    // Control messages block rather than drop: they are rare, and losing one
    // would leave the configuration silently stale.
    sender
        .send(Msg::Update(update))
        .map_err(|_| Error::NotInitialised)?;

    // The level is the one setting the frontend reads, so it lives in `log`'s
    // global rather than in the writer thread.
    if let Some(level) = level {
        log::set_max_level(level);
    }

    Ok(())
}

/// Wait until everything logged so far has reached the target.
///
/// Does nothing if the logger was never initialised.
pub fn flush() {
    let Some(sender) = SENDER.get() else {
        return;
    };

    let (ack, done) = oneshot::channel();
    if sender.send(Msg::Flush(ack)).is_ok() {
        let _ = done.recv();
    }
}

/// Flushes on drop.
///
/// [`flush`] is an idempotent barrier, so hold as many guards as convenient —
/// one in `main`, one per test, one per thread.
#[derive(Debug)]
pub struct FlushGuard;

impl Drop for FlushGuard {
    fn drop(&mut self) {
        flush();
    }
}

pub(crate) fn instance_id() -> Option<&'static str> {
    INSTANCE_ID.get().map(String::as_str)
}

/// Turn a [`Config`] into the level the frontend needs and the update the writer
/// thread needs. Opening a target happens here, on the caller's thread, so the
/// caller is the one who sees the error.
fn split(config: Config) -> Result<(Option<LevelFilter>, Update), Error> {
    // Open every file before touching a descriptor, so a path that cannot be
    // opened leaves the logger exactly as it was.
    let log_file = match &config.target {
        Some(Target::File(path)) => Some(open(path)?),
        Some(Target::Stderr) | None => None,
    };
    let crash_file = match &config.crash_target {
        Some(CrashTarget::Own(Target::File(path))) => Some(open(path)?),
        _ => None,
    };

    // Settle where the crash log points before the log target moves, so that a
    // call setting both does not briefly aim the crash log at the wrong file.
    match &config.crash_target {
        Some(CrashTarget::Own(_)) => {
            CRASH_FOLLOWS_LOG.store(false, Ordering::Relaxed);
            let fd = match &crash_file {
                Some(file) => file.as_fd(),
                // `Own(Stderr)` is the only variant without a file.
                None => rustix::stdio::stderr(),
            };
            crash::set_target(fd).map_err(Error::Crash)?;
        }
        Some(CrashTarget::SameAsLog) => {
            CRASH_FOLLOWS_LOG.store(true, Ordering::Relaxed);
            // Re-couple right away. A `target` further down overrides this.
            let log_fd = LOG_FD.lock().unwrap_or_else(|e| e.into_inner());
            let fd = match log_fd.as_ref() {
                Some(fd) => fd.as_fd(),
                None => rustix::stdio::stderr(),
            };
            crash::set_target(fd).map_err(Error::Crash)?;
        }
        None => {}
    }

    let follows = CRASH_FOLLOWS_LOG.load(Ordering::Relaxed);
    let sink = match (&config.target, log_file) {
        (Some(Target::File(_)), Some(file)) => {
            remember_log_fd(file.as_fd())?;
            if follows {
                crash::set_target(file.as_fd()).map_err(Error::Crash)?;
            }
            Some(Sink::File(BufWriter::new(file)))
        }
        (Some(Target::Stderr), _) => {
            remember_log_fd(rustix::stdio::stderr())?;
            if follows {
                crash::set_target(rustix::stdio::stderr()).map_err(Error::Crash)?;
            }
            Some(Sink::Stderr)
        }
        _ => None,
    };

    let update = Update {
        sink,
        format: config.format,
        show_tid: config.show_tid,
        show_thread_name: config.show_thread_name,
        show_target: config.show_target,
        show_file_line: config.show_file_line,
        show_id: config.show_id,
    };

    Ok((config.level, update))
}

fn remember_log_fd(fd: BorrowedFd<'_>) -> Result<(), Error> {
    let copy = rustix::io::fcntl_dupfd_cloexec(fd, 0).map_err(|e| Error::Crash(e.into()))?;
    *LOG_FD.lock().unwrap_or_else(|e| e.into_inner()) = Some(copy);

    Ok(())
}

fn open(path: &Path) -> Result<File, Error> {
    OpenOptions::new()
        .create(true)
        .append(true)
        // A named pipe with no reader on the other end would otherwise block
        // startup until one shows up.
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|source| Error::OpenTarget {
            path: path.to_path_buf(),
            source,
        })
}

#[derive(Debug)]
struct Frontend;

impl Log for Frontend {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &Record<'_>) {
        let Some(sender) = SENDER.get() else {
            return;
        };

        // Claim the outstanding count so it rides out with this record.
        let dropped = DROPPED.swap(0, Ordering::Relaxed);
        if sender
            .try_send(Msg::Entry(Entry::new(record, dropped)))
            .is_err()
        {
            // Hand back what this record claimed, and count itself.
            DROPPED.fetch_add(dropped + 1, Ordering::Relaxed);
        }
    }

    fn flush(&self) {
        flush();
    }
}
