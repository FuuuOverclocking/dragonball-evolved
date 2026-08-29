//! The crash log, exercised by making a child process actually die.
//!
//! The handler ends in the default disposition, so the only honest way to test it
//! is to re-exec this binary and let the child go down for real.

use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs, process};

use logger::info_unrestricted;
use logger_backend::{Config, CrashTarget, Target};

/// Set on the child: where the normal log goes.
const LOG_ENV: &str = "LOGGER_CRASH_LOG";
/// Set on the child when the crash log should go somewhere of its own.
const CRASH_ENV: &str = "LOGGER_CRASH_OWN";

#[test]
fn crash_child() {
    let Ok(log) = env::var(LOG_ENV) else {
        // The parent's run of this test has nothing to do.
        return;
    };
    let own_crash = env::var(CRASH_ENV).ok();

    logger_backend::init(Config {
        id: Some("crash-instance".into()),
        target: Some(Target::File(log.into())),
        crash_target: own_crash.map(|path| CrashTarget::Own(Target::File(path.into()))),
        ..Default::default()
    })
    .expect("init the logger");

    info_unrestricted!("a normal record");
    logger_backend::flush();

    // A real fault rather than a `raise`: only a kernel-raised SIGSEGV carries a
    // fault address, and that is the half of the record worth checking. The
    // address goes through `black_box` so the compiler can neither fold the read
    // away nor flag it as a literal null dereference.
    let unmapped = std::hint::black_box(0usize) as *const u8;
    // SAFETY: deliberately reading an unmapped page in a process that exists to
    // die. This fault is the thing under test.
    let _ = unsafe { std::ptr::read_volatile(unmapped) };

    unreachable!("the crash handler must not let the process carry on");
}

/// Run the child and return what it wrote to each path.
fn crash(log: &Path, own_crash: Option<&Path>) -> (String, Option<String>) {
    let _ = fs::remove_file(log);
    if let Some(path) = own_crash {
        let _ = fs::remove_file(path);
    }

    let mut child = Command::new(env::current_exe().expect("our own path"));
    child.args(["crash_child", "--exact"]).env(LOG_ENV, log);
    if let Some(path) = own_crash {
        child.env(CRASH_ENV, path);
    }
    let finished = child.output().expect("run the child");

    assert_eq!(
        finished.status.signal(),
        Some(libc::SIGSEGV),
        "the child should have died from the signal it raised: {finished:?}"
    );

    (
        fs::read_to_string(log).expect("read the log"),
        own_crash.map(|path| fs::read_to_string(path).expect("read the crash log")),
    )
}

fn assert_crash_line(line: &str) {
    let (stamp, rest) = line.split_once(' ').expect("a timestamp");
    assert!(stamp.starts_with("20"), "{line}");
    // libtest names the thread after the test, and PR_GET_NAME reads that name
    // directly from the crashing thread.
    assert!(rest.starts_with("ERRO [crash_child:"), "{line}");
    // SEGV_MAPERR, plus the address of the page that was not there.
    assert!(
        rest.contains("] crashed on SIGSEGV, code=1, addr=0x0,"),
        "{line}"
    );
    assert!(rest.ends_with(", id=crash-instance"), "{line}");
}

#[test]
fn crash_record_follows_the_log_by_default() {
    if env::var_os(LOG_ENV).is_some() {
        // This is the child; the other test does the crashing.
        return;
    }

    let log: PathBuf = env::temp_dir().join(format!("logger-crash-{}.log", process::id()));
    let (written, _) = crash(&log, None);

    let mut lines = written.lines();
    assert!(
        lines
            .next()
            .expect("the normal record")
            .contains("a normal record"),
        "{written}"
    );
    assert_crash_line(lines.next().expect("the crash record"));
    assert_eq!(lines.next(), None, "{written}");

    let _ = fs::remove_file(&log);
}

#[test]
fn an_own_crash_target_keeps_the_two_apart() {
    if env::var_os(LOG_ENV).is_some() {
        return;
    }

    let log: PathBuf = env::temp_dir().join(format!("logger-split-{}.log", process::id()));
    let own: PathBuf = env::temp_dir().join(format!("logger-split-{}.crash", process::id()));
    let (written, crashed) = crash(&log, Some(&own));

    // The normal log keeps the normal record and nothing else.
    assert!(written.contains("a normal record"), "{written}");
    assert!(
        !written.contains("crashed on"),
        "the crash record leaked into the log: {written}"
    );

    // The crash file holds the crash record and nothing else.
    let crashed = crashed.expect("a crash file");
    assert!(
        !crashed.contains("a normal record"),
        "a normal record leaked into the crash log: {crashed}"
    );
    let mut lines = crashed.lines();
    assert_crash_line(lines.next().expect("the crash record"));
    assert_eq!(lines.next(), None, "{crashed}");

    let _ = fs::remove_file(&log);
    let _ = fs::remove_file(&own);
}
