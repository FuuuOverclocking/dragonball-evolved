//! Crash log: the signal path.
//!
//! A fault handler cannot wait for the writer thread, take a lock or allocate,
//! so this path shares nothing with the normal one. It formats into a stack
//! buffer and writes straight to a reserved descriptor.
//!
//! Everything that rustix covers goes through rustix: on this target its
//! backend issues raw syscalls, which is a shorter path than a libc wrapper and
//! never touches `errno`. `sigaction`, `signal` and `raise` stay on libc —
//! rustix only exposes those through `rustix::runtime`, which bypasses the
//! signal bookkeeping that a glibc-linked process depends on.

use std::os::fd::{AsRawFd as _, BorrowedFd, IntoRawFd as _, RawFd};
use std::sync::atomic::{AtomicI32, Ordering};
use std::{io, ptr};

use log::Level;
use rustix::io::Errno;
use rustix::thread::Timespec;

use crate::buf::Buf;
use crate::time::{self, Timestamp};
use crate::{instance_id, writer};

/// Signals that mean the process is already past saving.
const FATAL: [libc::c_int; 6] = [
    libc::SIGSEGV,
    libc::SIGBUS,
    libc::SIGILL,
    libc::SIGFPE,
    libc::SIGABRT,
    libc::SIGSYS,
];

/// How long the writer thread gets to drain before the process dies.
const GRACE: Timespec = Timespec {
    tv_sec: 0,
    tv_nsec: 100_000_000,
};

/// The descriptor the handler writes to. Retargeting `dup3`s onto this same
/// number, so the handler never reads anything but this one atomic.
static CRASH_FD: AtomicI32 = AtomicI32::new(-1);

/// Reserve the descriptor and install the handlers. Call after the UTC offset
/// and the instance id are on record.
pub fn init() -> io::Result<()> {
    // A private duplicate of stderr, so retargeting never disturbs fd 2. It is
    // deliberately leaked: the handler must be able to use it until the process
    // is gone.
    let reserved = rustix::io::fcntl_dupfd_cloexec(rustix::stdio::stderr(), 0)?;
    CRASH_FD.store(reserved.into_raw_fd(), Ordering::Relaxed);

    for signo in FATAL {
        install(signo)?;
    }

    Ok(())
}

/// Point the crash log at `fd`. The handler keeps writing to the same descriptor
/// number, so nothing it touches has to change.
///
/// `rustix::io::dup3` wants to own the target as an `OwnedFd`; this target is a
/// process-global number that a signal handler reads, so it stays raw.
pub fn set_target(fd: BorrowedFd<'_>) -> io::Result<()> {
    let crash_fd = CRASH_FD.load(Ordering::Relaxed);
    if crash_fd < 0 {
        return Ok(());
    }

    // SAFETY: both are descriptors this process owns, and `dup3` only rebinds
    // the target number.
    if unsafe { libc::dup3(fd.as_raw_fd(), crash_fd, libc::O_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

fn install(signo: libc::c_int) -> io::Result<()> {
    // SAFETY: an all-zero `sigaction` is a valid starting point; every field
    // this code depends on is written below.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = handler as *const () as usize;
    // SA_ONSTACK uses the alternate stack std installs per thread, which is the
    // only way to report a stack overflow. Taking SIGSEGV over also takes over
    // std's own "thread has overflowed its stack" message.
    action.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;

    // SAFETY: `action` is fully initialised and `handler` has the signature
    // SA_SIGINFO asks for.
    unsafe {
        libc::sigfillset(&mut action.sa_mask);
        if libc::sigaction(signo, &action, ptr::null_mut()) < 0 {
            return Err(io::Error::last_os_error());
        }
    }

    Ok(())
}

extern "C" fn handler(signo: libc::c_int, info: *mut libc::siginfo_t, _context: *mut libc::c_void) {
    let fd = CRASH_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        let mut line = Buf::<256>::new();

        line.push_bytes(time::render(Timestamp::now()).as_bytes());
        line.push(b' ');
        line.push_bytes(writer::level_name(Level::Error).as_bytes());

        // Kernel lookups, not the thread-local cache: a first touch of lazy TLS
        // can allocate. Linux stores the current thread's name in a fixed buffer.
        let tid = rustix::thread::gettid().as_raw_nonzero().get();
        let mut name_buffer = [0_u8; 16];
        let name = current_thread_name(&mut name_buffer).unwrap_or(b"-");
        line.push_bytes(b" [");
        line.push_bytes(name);
        line.push(b':');
        line.push_u64(tid.unsigned_abs().into());
        line.push(b']');

        line.push_bytes(b" crashed on ");
        match signal_name(signo) {
            Some(name) => line.push_bytes(name.as_bytes()),
            None => {
                line.push_bytes(b"signal ");
                line.push_u64(u64::from(signo.unsigned_abs()));
            }
        }

        if !info.is_null() {
            // SAFETY: the kernel hands an SA_SIGINFO handler a valid siginfo_t
            // that outlives the handler.
            let info = unsafe { &*info };
            line.push_bytes(b", code=");
            // The SI_* codes are negative, and a sign-stripped code is a lie.
            if info.si_code < 0 {
                line.push(b'-');
            }
            line.push_u64(u64::from(info.si_code.unsigned_abs()));

            // `si_addr` only holds a fault address when the kernel raised the
            // signal itself, which a positive `si_code` marks. For a `raise`d or
            // `kill`ed signal the union carries the sender's pid and uid, and
            // printing those as an address is worse than saying nothing.
            if info.si_code > 0 && delivers_fault_address(signo) {
                line.push_bytes(b", addr=0x");
                // SAFETY: reading the fault address out of a valid siginfo_t.
                line.push_hex(unsafe { info.si_addr() } as usize as u64);
            }
        }

        // `OnceLock::get` past initialisation is an acquire load and a read, so it
        // is safe to reach for here.
        if let Some(id) = instance_id() {
            line.push_bytes(b", id=");
            line.push_bytes(id.as_bytes());
        }
        line.push(b'\n');

        write_all(fd, line.as_bytes());
    }

    // Let the writer thread put what it already has on disk.
    let _ = rustix::thread::nanosleep(&GRACE);

    // Hand the signal back to its default disposition so the core dump and the
    // exit status are the ones the kernel would have produced on its own. For a
    // fault signal, returning re-executes the faulting instruction and reaches
    // the same place.
    // SAFETY: both calls take plain values.
    unsafe {
        libc::signal(signo, libc::SIG_DFL);
        libc::raise(signo);
    }
}

fn current_thread_name(buffer: &mut [u8; 16]) -> Option<&[u8]> {
    // SAFETY: PR_GET_NAME writes at most 16 bytes to the supplied writable buffer;
    // the remaining variadic arguments use the unsigned-long ABI prctl expects.
    let result = unsafe {
        libc::prctl(
            libc::PR_GET_NAME,
            buffer.as_mut_ptr() as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        )
    };
    if result < 0 {
        return None;
    }

    let len = buffer
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(buffer.len());
    (len != 0).then_some(&buffer[..len])
}

/// A name for the signals this module installs, so the record does not make the
/// reader look up a number.
fn signal_name(signo: libc::c_int) -> Option<&'static str> {
    Some(match signo {
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGBUS => "SIGBUS",
        libc::SIGILL => "SIGILL",
        libc::SIGFPE => "SIGFPE",
        libc::SIGABRT => "SIGABRT",
        libc::SIGSYS => "SIGSYS",
        _ => return None,
    })
}

/// Whether this signal carries a fault address at all. A positive `si_code` still
/// has to confirm that the kernel, not a `raise`, produced it.
fn delivers_fault_address(signo: libc::c_int) -> bool {
    matches!(
        signo,
        libc::SIGSEGV | libc::SIGBUS | libc::SIGILL | libc::SIGFPE
    )
}

fn write_all(fd: RawFd, mut bytes: &[u8]) {
    // SAFETY: `init` opened this descriptor and nothing ever closes it.
    let fd = unsafe { BorrowedFd::borrow_raw(fd) };

    while !bytes.is_empty() {
        match rustix::io::write(fd, bytes) {
            Ok(0) => return,
            Ok(written) => bytes = &bytes[written..],
            // An interrupted write is worth retrying. Anything else means giving
            // up rather than spinning inside a fault handler.
            Err(Errno::INTR) => {}
            Err(_) => return,
        }
    }
}
