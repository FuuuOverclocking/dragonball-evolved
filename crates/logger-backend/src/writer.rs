//! The writer thread: everything that decides what a line looks like.
//!
//! The thread owns the settings outright. Records and configuration changes
//! arrive on the same queue, so a change applies to exactly the records queued
//! after it and no lock is needed on either side.

use std::collections::HashMap;
use std::collections::hash_map::Entry as MapEntry;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write as _};
use std::process;
use std::sync::mpsc::{Receiver, TryRecvError};

use log::Level;

use crate::buf::Buf;
use crate::entry::Entry;
use crate::time::{self, Timestamp};
use crate::{Format, instance_id};

/// Where rendered lines go.
#[derive(Debug)]
pub enum Sink {
    Stderr,
    File(BufWriter<File>),
}

impl Sink {
    fn write_line(&mut self, line: &[u8]) -> io::Result<()> {
        match self {
            // Left unbuffered so panic messages and the crash log cannot land
            // in the middle of a half-written line.
            Sink::Stderr => io::stderr().write_all(line),
            Sink::File(file) => file.write_all(line),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Sink::Stderr => io::stderr().flush(),
            Sink::File(file) => file.flush(),
        }
    }
}

/// Which optional fields a line carries.
#[derive(Debug, Clone, Copy)]
pub struct Fields {
    pub show_tid: bool,
    pub show_thread_name: bool,
    pub show_target: bool,
    pub show_file_line: bool,
    pub show_id: bool,
}

impl Default for Fields {
    fn default() -> Self {
        Self {
            show_tid: false,
            show_thread_name: true,
            show_target: true,
            show_file_line: true,
            show_id: false,
        }
    }
}

/// What the writer thread owns.
#[derive(Debug)]
pub struct Settings {
    pub sink: Sink,
    pub format: Format,
    pub fields: Fields,
}

/// A configuration change. `None` leaves the current value alone.
#[derive(Debug, Default)]
pub struct Update {
    pub sink: Option<Sink>,
    pub format: Option<Format>,
    pub show_tid: Option<bool>,
    pub show_thread_name: Option<bool>,
    pub show_target: Option<bool>,
    pub show_file_line: Option<bool>,
    pub show_id: Option<bool>,
}

impl Settings {
    pub fn apply(&mut self, update: Update) {
        if let Some(sink) = update.sink {
            // Whatever the old sink still holds belongs to the old target.
            let _ = self.sink.flush();
            self.sink = sink;
        }
        if let Some(format) = update.format {
            self.format = format;
        }
        if let Some(show) = update.show_tid {
            self.fields.show_tid = show;
        }
        if let Some(show) = update.show_thread_name {
            self.fields.show_thread_name = show;
        }
        if let Some(show) = update.show_target {
            self.fields.show_target = show;
        }
        if let Some(show) = update.show_file_line {
            self.fields.show_file_line = show;
        }
        if let Some(show) = update.show_id {
            self.fields.show_id = show;
        }
    }
}

/// What the writer thread accepts.
#[derive(Debug)]
pub enum Msg {
    Entry(Entry),
    Update(Update),
    Flush(oneshot::Sender<()>),
}

pub fn run(rx: Receiver<Msg>, mut settings: Settings) {
    let mut names = ThreadNames::default();
    let mut line = Vec::with_capacity(512);

    loop {
        // Flushing only once the queue runs dry batches writes under load while
        // still putting an idle process's last line on disk immediately.
        let msg = match rx.try_recv() {
            Ok(msg) => msg,
            Err(TryRecvError::Empty) => {
                let _ = settings.sink.flush();
                match rx.recv() {
                    Ok(msg) => msg,
                    Err(_) => return,
                }
            }
            Err(TryRecvError::Disconnected) => return,
        };

        match msg {
            Msg::Entry(entry) => write_entry(&entry, &mut settings, &mut names, &mut line),
            Msg::Update(update) => settings.apply(update),
            Msg::Flush(ack) => {
                let _ = settings.sink.flush();
                let _ = ack.send(());
            }
        }
    }
}

/// One line's worth of fields, so the overflow notice can go through the same
/// renderer as a real record.
#[derive(Debug, Clone, Copy)]
struct View<'a> {
    timestamp: Timestamp,
    level: Level,
    tid: u32,
    target: &'a str,
    file: &'a str,
    line: u32,
    message: &'a str,
}

fn write_entry(
    entry: &Entry,
    settings: &mut Settings,
    names: &mut ThreadNames,
    line: &mut Vec<u8>,
) {
    let view = View {
        timestamp: entry.timestamp,
        level: entry.level,
        tid: entry.tid,
        target: entry.target(),
        file: entry.file(),
        line: entry.line,
        message: entry.message(),
    };

    if entry.dropped > 0 {
        let mut notice = Buf::<64>::new();
        notice.push_bytes(b"log queue full, dropped ");
        notice.push_u64(entry.dropped);
        notice.push_bytes(b" records");

        emit(
            &View {
                level: Level::Warn,
                target: "logger_backend",
                file: "",
                line: 0,
                message: notice.as_str(),
                ..view
            },
            settings,
            names,
            line,
        );
    }

    emit(&view, settings, names, line);
}

fn emit(view: &View<'_>, settings: &mut Settings, names: &mut ThreadNames, line: &mut Vec<u8>) {
    line.clear();
    match settings.format {
        Format::Text => render_text(line, view, &settings.fields, names),
        Format::Json => render_json(line, view, &settings.fields, names),
    }
    line.push(b'\n');

    // A failed write has nowhere to be reported: the report would go through the
    // sink that just failed.
    let _ = settings.sink.write_line(line);
}

fn render_text(out: &mut Vec<u8>, view: &View<'_>, fields: &Fields, names: &mut ThreadNames) {
    out.extend_from_slice(time::render(view.timestamp).as_bytes());
    out.push(b' ');
    out.extend_from_slice(level_name(view.level).as_bytes());

    if fields.show_thread_name || fields.show_tid {
        out.extend_from_slice(b" [");
        let mut first = true;
        if fields.show_thread_name {
            separate(out, &mut first);
            out.extend_from_slice(names.name(view.tid).as_bytes());
        }
        if fields.show_tid {
            separate(out, &mut first);
            push_u64(out, u64::from(view.tid));
        }
        out.push(b']');
    }

    // The message leads; everything else trails it as `key=value`, so a line
    // reads as prose and the fixed-width part stays on the left.
    out.push(b' ');
    out.extend_from_slice(view.message.as_bytes());

    if fields.show_target && !view.target.is_empty() {
        out.extend_from_slice(b", target=");
        out.extend_from_slice(view.target.as_bytes());
    }
    if fields.show_file_line && !view.file.is_empty() {
        out.extend_from_slice(b", at ");
        out.extend_from_slice(view.file.as_bytes());
        out.push(b':');
        push_u64(out, u64::from(view.line));
    }
    if let Some(id) = fields.show_id.then(instance_id).flatten() {
        out.extend_from_slice(b", id=");
        out.extend_from_slice(id.as_bytes());
    }
}

fn render_json(out: &mut Vec<u8>, view: &View<'_>, fields: &Fields, names: &mut ThreadNames) {
    out.extend_from_slice(br#"{"ts":"#);
    push_json_str(out, time::render(view.timestamp).as_str());
    out.extend_from_slice(br#","level":"#);
    push_json_str(out, level_name(view.level));

    if let Some(id) = fields.show_id.then(instance_id).flatten() {
        out.extend_from_slice(br#","id":"#);
        push_json_str(out, id);
    }
    if fields.show_thread_name {
        out.extend_from_slice(br#","thread":"#);
        push_json_str(out, names.name(view.tid));
    }
    if fields.show_tid {
        out.extend_from_slice(br#","tid":"#);
        push_u64(out, u64::from(view.tid));
    }
    if fields.show_target && !view.target.is_empty() {
        out.extend_from_slice(br#","target":"#);
        push_json_str(out, view.target);
    }
    if fields.show_file_line && !view.file.is_empty() {
        // One `file:line` string rather than two fields: that is what a reader
        // pastes into an editor, and it matches the text format.
        out.extend_from_slice(br#","source":""#);
        push_json_escaped(out, view.file);
        out.push(b':');
        push_u64(out, u64::from(view.line));
        out.push(b'"');
    }

    out.extend_from_slice(br#","msg":"#);
    push_json_str(out, view.message);
    out.push(b'}');
}

fn separate(out: &mut Vec<u8>, first: &mut bool) {
    if !*first {
        out.push(b':');
    }
    *first = false;
}

fn push_u64(out: &mut Vec<u8>, value: u64) {
    let mut digits = Buf::<20>::new();
    digits.push_u64(value);
    out.extend_from_slice(digits.as_bytes());
}

fn push_json_str(out: &mut Vec<u8>, value: &str) {
    out.push(b'"');
    push_json_escaped(out, value);
    out.push(b'"');
}

/// The body of a JSON string, without the quotes, so a caller can compose one
/// out of several pieces.
fn push_json_escaped(out: &mut Vec<u8>, value: &str) {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    // Bytes above 0x1f pass through untouched, which keeps UTF-8 intact.
    for &byte in value.as_bytes() {
        match byte {
            b'"' => out.extend_from_slice(br#"\""#),
            b'\\' => out.extend_from_slice(br"\\"),
            b'\n' => out.extend_from_slice(br"\n"),
            b'\r' => out.extend_from_slice(br"\r"),
            b'\t' => out.extend_from_slice(br"\t"),
            0x00..=0x1f => {
                out.extend_from_slice(br"\u00");
                out.push(HEX[usize::from(byte >> 4)]);
                out.push(HEX[usize::from(byte & 0xf)]);
            }
            _ => out.push(byte),
        }
    }
}

/// Every name is four characters, so the level column lines up on its own.
///
/// A plain match over a `Copy` enum, which is why the crash handler can use it.
pub fn level_name(level: Level) -> &'static str {
    match level {
        Level::Error => "ERRO",
        Level::Warn => "WARN",
        Level::Info => "INFO",
        Level::Debug => "DEBG",
        Level::Trace => "TRAC",
    }
}

/// tid to thread name, read from the kernel's `comm`.
///
/// `comm` holds 15 bytes, so longer names arrive truncated. Misses are not
/// cached: an entry can reach the writer after its thread exited, and a name
/// that could not be read must not stick to a tid that is still alive.
#[derive(Debug)]
pub struct ThreadNames {
    names: HashMap<u32, Box<str>>,
    /// The thread-group leader's tid, which is the process id.
    main: u32,
}

impl Default for ThreadNames {
    fn default() -> Self {
        Self {
            names: HashMap::new(),
            main: process::id(),
        }
    }
}

impl ThreadNames {
    fn name(&mut self, tid: u32) -> &str {
        // The leader's `comm` is the process name, which every tool from `pkill`
        // to journald reads, so it is not ours to rename. Reporting `main` here
        // costs one comparison and agrees with `Thread::name()`.
        if tid == self.main {
            return "main";
        }

        if let MapEntry::Vacant(slot) = self.names.entry(tid) {
            let Some(comm) = read_comm(tid) else {
                return "-";
            };
            slot.insert(comm);
        }
        &self.names[&tid]
    }
}

fn read_comm(tid: u32) -> Option<Box<str>> {
    let mut path = String::with_capacity(32);
    let _ = write!(path, "/proc/self/task/{tid}/comm");

    let comm = fs::read_to_string(path).ok()?;
    let comm = comm.trim_end();
    (!comm.is_empty()).then(|| comm.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view<'a>(message: &'a str, target: &'a str, file: &'a str) -> View<'a> {
        View {
            timestamp: Timestamp::now(),
            level: Level::Warn,
            tid: crate::entry::tid(),
            target,
            file,
            line: 108,
            message,
        }
    }

    fn text(fields: Fields, view: &View<'_>) -> String {
        let mut out = Vec::new();
        render_text(&mut out, view, &fields, &mut ThreadNames::default());
        String::from_utf8(out).unwrap()
    }

    fn json(fields: Fields, view: &View<'_>) -> String {
        let mut out = Vec::new();
        render_json(&mut out, view, &fields, &mut ThreadNames::default());
        String::from_utf8(out).unwrap()
    }

    fn all_off() -> Fields {
        Fields {
            show_tid: false,
            show_thread_name: false,
            show_target: false,
            show_file_line: false,
            show_id: false,
        }
    }

    #[test]
    fn text_drops_every_optional_field() {
        let line = text(all_off(), &view("boom", "vmm::block", "block.rs"));

        // Timestamp, level, message. Nothing else.
        let (stamp, rest) = line.split_once(' ').unwrap();
        assert!(stamp.starts_with("20"), "{line}");
        assert_eq!(rest, "WARN boom");
    }

    #[test]
    fn text_shows_the_fields_that_are_on() {
        let fields = Fields {
            show_tid: true,
            show_thread_name: false,
            show_target: true,
            show_file_line: true,
            ..all_off()
        };
        let line = text(fields, &view("boom", "vmm::block", "block.rs"));
        let tid = crate::entry::tid();

        assert!(
            line.ends_with(&format!("[{tid}] boom, target=vmm::block, at block.rs:108")),
            "{line}"
        );
    }

    #[test]
    fn text_omits_empty_target_and_file() {
        let fields = Fields {
            show_target: true,
            show_file_line: true,
            ..all_off()
        };
        let line = text(fields, &view("boom", "", ""));

        assert!(line.ends_with("WARN boom"), "{line}");
    }

    #[test]
    fn json_escapes_the_message() {
        let line = json(all_off(), &view("a\"b\\c\nd\te\r\u{1}f", "", ""));

        assert!(
            line.ends_with(r#","msg":"a\"b\\c\nd\te\r\u0001f"}"#),
            "{line}"
        );
    }

    #[test]
    fn json_keeps_multibyte_text_intact() {
        let line = json(all_off(), &view("设备失败", "", ""));

        assert!(line.ends_with(r#","msg":"设备失败"}"#), "{line}");
    }

    #[test]
    fn json_numbers_are_unquoted() {
        let fields = Fields {
            show_tid: true,
            show_file_line: true,
            ..all_off()
        };
        let line = json(fields, &view("boom", "", "block.rs"));
        let tid = crate::entry::tid();

        assert!(line.contains(&format!(r#","tid":{tid},"#)), "{line}");
        assert!(line.contains(r#","source":"block.rs:108""#), "{line}");
    }

    #[test]
    fn a_dropped_count_is_reported_before_the_record() {
        let path = std::env::temp_dir().join(format!(
            "logger-dropped-{}-{}.log",
            std::process::id(),
            crate::entry::tid()
        ));
        let file = File::create(&path).unwrap();

        let mut settings = Settings {
            sink: Sink::File(BufWriter::new(file)),
            format: Format::Text,
            fields: all_off(),
        };
        let entry = Entry::new(
            &log::Record::builder()
                .metadata(
                    log::MetadataBuilder::new()
                        .level(Level::Info)
                        .target("t")
                        .build(),
                )
                .args(format_args!("survivor"))
                .build(),
            7,
        );

        write_entry(
            &entry,
            &mut settings,
            &mut ThreadNames::default(),
            &mut Vec::new(),
        );
        settings.sink.flush().unwrap();

        let written = fs::read_to_string(&path).unwrap();
        let mut lines = written.lines();
        assert!(
            lines
                .next()
                .unwrap()
                .ends_with("WARN log queue full, dropped 7 records"),
            "{written}"
        );
        assert!(
            lines.next().unwrap().ends_with("INFO survivor"),
            "{written}"
        );
        assert_eq!(lines.next(), None, "{written}");

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn thread_name_comes_from_comm() {
        let mut names = ThreadNames::default();
        let gone = std::thread::Builder::new()
            .name("logger-test".into())
            .spawn(crate::entry::tid)
            .unwrap()
            .join()
            .unwrap();

        // The harness runs tests off the leader, so this one reads its `comm`.
        let own = crate::entry::tid();
        assert_ne!(own, process::id(), "expected a non-leader thread");
        let comm = read_comm(own).unwrap();
        assert_eq!(names.name(own), comm.as_ref());

        // That thread is gone, so its `comm` is too.
        assert_eq!(names.name(gone), "-");
    }

    #[test]
    fn the_leader_is_called_main() {
        let mut names = ThreadNames::default();
        let leader = process::id();

        // `comm` for the leader is the executable name, not `main`.
        assert_ne!(read_comm(leader).unwrap().as_ref(), "main");
        assert_eq!(names.name(leader), "main");
    }

    #[test]
    fn long_thread_names_arrive_truncated() {
        let comm = std::thread::Builder::new()
            .name("this-name-is-far-too-long".into())
            .spawn(|| read_comm(crate::entry::tid()))
            .unwrap()
            .join()
            .unwrap();

        assert_eq!(comm.unwrap().as_ref(), "this-name-is-fa");
    }
}
