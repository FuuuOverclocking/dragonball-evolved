//! End-to-end: what actually lands in the log file.
//!
//! `init` installs a process-global logger exactly once, so this is one test
//! walking through a sequence of states rather than several independent ones.

use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{env, fs, process, thread};

use logger::{LevelFilter, debug, info_unrestricted, warn};
use logger_backend::{Config, Format, Target};

/// All the flooding goes through here so that every call shares one expansion
/// site, and therefore one rate limiter.
fn flood(marker: u32) {
    warn!("flooding {marker}");
}

fn read(path: &Path) -> String {
    logger_backend::flush();
    fs::read_to_string(path).expect("read back the log")
}

/// Reset the file between sections. The sink holds the descriptor open, so the
/// file is truncated in place rather than replaced.
fn truncate(path: &Path) {
    logger_backend::flush();
    fs::write(path, "").expect("truncate the log");
}

#[test]
fn writes_configures_and_rate_limits() {
    let path: PathBuf = env::temp_dir().join(format!("logger-e2e-{}.log", process::id()));
    let _ = fs::remove_file(&path);

    let guard = logger_backend::init(Config {
        id: Some("e2e-instance".into()),
        level: Some(LevelFilter::Info),
        target: Some(Target::File(path.clone())),
        ..Default::default()
    })
    .expect("init the logger");

    // Defaults: text, thread name, target and file:line, no tid and no id.
    info_unrestricted!("hello {}", "world");
    let first = read(&path);
    let line = first.trim_end();

    let (stamp, rest) = line.split_once(' ').expect("a timestamp");
    assert!(stamp.starts_with("20"), "{line}");
    assert!(rest.starts_with("INFO ["), "{line}");
    assert!(
        rest.contains("] hello world, target=logging, at "),
        "{line}"
    );
    assert!(
        rest.contains("tests/logging.rs:"),
        "origin is missing: {line}"
    );
    // The record belongs to the thread that logged, not to the writer thread.
    assert!(!rest.contains("[logger]"), "{line}");
    assert!(
        !rest.contains("e2e-instance"),
        "id is off by default: {line}"
    );

    // A level below the filter never reaches the file.
    truncate(&path);
    debug!("invisible");
    info_unrestricted!("visible");
    assert_eq!(read(&path).lines().count(), 1);

    // Rate limiting: one callsite, 10 per 5 s, so 5 of these 15 are dropped.
    truncate(&path);
    for marker in 0..15 {
        flood(marker);
    }
    let flooded = read(&path);
    assert_eq!(flooded.lines().count(), 10, "{flooded}");
    assert!(flooded.contains("flooding 9"), "{flooded}");
    assert!(!flooded.contains("flooding 10"), "{flooded}");

    // A token is worth 500 ms. The next pass through that same callsite owes a
    // report for everything it suppressed.
    truncate(&path);
    thread::sleep(Duration::from_millis(600));
    flood(99);
    let resumed = read(&path);
    assert_eq!(resumed.lines().count(), 2, "{resumed}");
    assert!(
        resumed.contains("rate limiting suppressed 5 messages from this callsite"),
        "{resumed}"
    );
    assert!(resumed.contains("flooding 99"), "{resumed}");
    // The report has to point at the flooding callsite, not at the logger crate.
    assert!(resumed.contains("tests/logging.rs:"), "{resumed}");

    // Reconfigure: JSON, every field on.
    truncate(&path);
    logger_backend::update(Config {
        format: Some(Format::Json),
        show_tid: Some(true),
        show_id: Some(true),
        ..Default::default()
    })
    .expect("switch to json");

    info_unrestricted!("structured");
    let json = read(&path);
    let json = json.trim_end();

    assert!(json.starts_with(r#"{"ts":"20"#), "{json}");
    assert!(json.contains(r#","level":"INFO""#), "{json}");
    assert!(json.contains(r#","id":"e2e-instance""#), "{json}");
    assert!(json.contains(r#","target":"logging""#), "{json}");
    assert!(json.contains(r#","source":"#), "{json}");
    assert!(json.contains(r#"tests/logging.rs:"#), "{json}");
    assert!(json.ends_with(r#","msg":"structured"}"#), "{json}");

    let tid = json
        .split(r#","tid":"#)
        .nth(1)
        .and_then(|rest| rest.split(',').next())
        .expect("a tid field");
    assert!(tid.parse::<u32>().is_ok(), "tid is not a number: {json}");

    // Turning the fields off has to remove them again.
    truncate(&path);
    logger_backend::update(Config {
        format: Some(Format::Text),
        show_thread_name: Some(false),
        show_target: Some(false),
        show_file_line: Some(false),
        show_tid: Some(false),
        show_id: Some(false),
        ..Default::default()
    })
    .expect("strip the fields");

    info_unrestricted!("bare");
    let bare = read(&path);
    let (_, rest) = bare.trim_end().split_once(' ').unwrap();
    assert_eq!(rest, "INFO bare", "{bare}");

    // The instance id trails the line now instead of sitting in the brackets.
    truncate(&path);
    logger_backend::update(Config {
        show_thread_name: Some(true),
        show_tid: Some(true),
        show_id: Some(true),
        ..Default::default()
    })
    .expect("put the tail fields back");

    info_unrestricted!("tagged");
    let tagged = read(&path);
    let (_, rest) = tagged.trim_end().split_once(' ').unwrap();
    assert_eq!(
        rest,
        format!(
            "INFO [{}:{}] tagged, id=e2e-instance",
            own_comm(),
            own_tid()
        ),
        "{tagged}"
    );

    // Dropping the guard flushes, so this read needs no flush of its own.
    truncate(&path);
    info_unrestricted!("last words");
    drop(guard);
    assert!(
        fs::read_to_string(&path).unwrap().contains("last words"),
        "the guard did not flush"
    );

    let _ = fs::remove_file(&path);
}

fn own_tid() -> u32 {
    rustix::thread::gettid().as_raw_nonzero().get() as u32
}

/// The kernel's `comm` for this thread, which is what the writer renders.
fn own_comm() -> String {
    fs::read_to_string(format!("/proc/self/task/{}/comm", own_tid()))
        .expect("read comm")
        .trim_end()
        .to_owned()
}
