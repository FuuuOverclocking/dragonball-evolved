use std::backtrace::Backtrace;
use std::os::fd::RawFd;
use std::{io, panic};

use anyhow::{Context, Result};
use logger::{error_unrestricted, info_unrestricted};
use logger_backend::FlushGuard;
use rustix::event::{EventfdFlags, eventfd};
use rustix::io::{Errno, fcntl_dupfd_cloexec};
use rustix::process::{Resource, getrlimit, umask};
use tokio::select;
use tokio::signal::unix::{SignalKind, signal};
use vmm_sys_util::terminal::Terminal;

use crate::config::Config;

// Size the fd table is pre-expanded to, big enough for most use cases.
const FDTABLE_SIZE: u64 = 4096;

pub fn init(cfg: &Config) -> Result<(FlushGuard, impl Future<Output = ()> + Send + 'static)> {
    // Ensure all created files (e.g. sockets) are only accessible by this user.
    umask(0o077.into());

    // Pre-expand fd table. This has to happen before the process spawns its first
    // thread, the log thread included, or the kernel stops taking the shortcut
    // described below.
    expand_fdtable().context("pre-expand fd table")?;

    // Setup logger. This also installs the crash log handlers.
    let mut logger_config: logger_backend::Config = cfg.dragonball.logger().clone().into();
    logger_config.id = Some(cfg.dragonball.id().to_owned());
    let flush_guard = logger_backend::init(logger_config).context("setup logger")?;

    // Setup panic hook.
    setup_panic_hook();

    // Install signal handlers and return a future that will resolve
    // when a SIGINT or SIGTERM is received.
    Ok((flush_guard, setup_signal_handlers()?))
}

fn setup_panic_hook() {
    panic::set_hook(Box::new(move |info| {
        error_unrestricted!("Panic occurred: {info}");

        if let Err(e) = io::stdin().lock().set_canon_mode() {
            error_unrestricted!("Failed to reset stdin to canonical mode: {e}");
        }

        let bt = Backtrace::force_capture();
        error_unrestricted!("{bt}");

        // A panic can be the last thing this process does, and the records are
        // still sitting in the writer thread's queue.
        logger_backend::flush();
    }));
}

// This is a best-effort solution to the latency induced by the RCU
// synchronization that happens in the kernel whenever the file descriptor table
// fills up.
// The table has initially 64 entries on amd64 and every time it fills up, a new
// table is created, double the size of the current one, and the entries are
// copied to the new table. The filesystem code that does this uses
// synchronize_rcu() to ensure all preexisting RCU read-side critical sections
// have completed:
//
//     https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/fs/file.c?h=v6.9.1#n162
//
// Rust programs that create lots of file handles or use
// {File,EventFd}::try_clone() to share them are impacted by this issue. This
// behavior is quite noticeable in the snapshot restore scenario, the latency is
// a big chunk of the total time required to start cloud-hypervisor and restore
// the snapshot.
//
// The kernel has an optimization in code, where it doesn't call
// synchronize_rcu() if there is only one thread in the process. We can take
// advantage of this optimization by expanding the descriptor table at
// application start, when it has only one thread.
//
// The code tries to resize the table to an adequate size for most use cases,
// 4096, this way we avoid any expansion that might take place later.
pub fn expand_fdtable() -> Result<()> {
    // A `None` soft limit means unlimited.
    let table_size = getrlimit(Resource::Nofile)
        .current
        .map_or(FDTABLE_SIZE, |soft_limit| soft_limit.min(FDTABLE_SIZE));

    // The first 3 handles are stdin, stdout, stderr. We don't want to touch
    // any of them.
    if table_size <= 3 {
        return Ok(());
    }

    let dummy_evt = eventfd(0, EventfdFlags::CLOEXEC).context("create dummy eventfd")?;

    // Duplicating onto the last slot of the desired table is what makes the
    // kernel grow the table; the duplicate is closed right away, the enlarged
    // table stays. `EMFILE` means no slot that high is free, i.e. the table
    // already has the size we want.
    match fcntl_dupfd_cloexec(&dummy_evt, table_size as RawFd - 1) {
        Ok(_) | Err(Errno::MFILE) => Ok(()),
        Err(e) => Err(e).context("duplicate dummy eventfd"),
    }
}

pub fn setup_signal_handlers() -> Result<impl Future<Output = ()> + Send + 'static> {
    // The fatal signals (SIGSEGV/SIGBUS/SIGILL/SIGFPE/SIGABRT/SIGSYS) belong to
    // the crash log, installed by `logger_backend::init`.
    // TODO: Handle SIGXFSZ/SIGXCPU/SIGHUP.
    // TODO: Ignore SIGPIPE.

    // Handle `SIGINT`, `SIGTERM`. These are process-directed and recoverable, so
    // they go through tokio: its handler only writes to a self-pipe and the
    // shutdown itself runs on the runtime.
    let mut interrupt = signal(SignalKind::interrupt()).context("register SIGINT handler")?;
    let mut terminate = signal(SignalKind::terminate()).context("register SIGTERM handler")?;

    Ok(async move {
        let received = select! {
            _ = interrupt.recv() => "SIGINT",
            _ = terminate.recv() => "SIGTERM",
        };
        info_unrestricted!("Received {received}, shutting down");
    })
}
