//! The daemon's log, which goes to standard error for systemd's journal to collect.

use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;

use log::{Level, LevelFilter, Log, Metadata, Record};

/// Writes each log line to standard error. Under systemd, each line starts with its level in the
/// form that the journal reads, such as `<4>` for a warning, so that
/// `journalctl -u kbd-rgb -p warning` can pick lines by level.
struct Journal {
    under_systemd: bool,
}

impl Log for Journal {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= Level::Info
    }

    fn log(&self, record: &Record<'_>) {
        if self.enabled(record.metadata()) {
            let text = lines(
                record.level(),
                &record.args().to_string(),
                self.under_systemd,
            );
            // A log line that can't be written is lost. It must never stop the daemon.
            let _ = io::stderr().lock().write_all(text.as_bytes());
        }
    }

    fn flush(&self) {}
}

/// `message` as log lines. The journal reads the level from the start of each line, so every line
/// of a message that has several gets one.
fn lines(level: Level, message: &str, under_systemd: bool) -> String {
    let prefix = match (under_systemd, level) {
        (true, Level::Error) => "<3>",
        (true, Level::Warn) => "<4>",
        (true, Level::Info) => "<6>",
        (true, Level::Debug | Level::Trace) => "<7>",
        (false, Level::Error) => "error: ",
        (false, Level::Warn) => "warning: ",
        (false, _) => "",
    };
    let mut text = String::new();
    for (number, line) in message.lines().enumerate() {
        // In a terminal, the level only needs saying once.
        if under_systemd || number == 0 {
            text.push_str(prefix);
        }
        text.push_str(line);
        text.push('\n');
    }
    text
}

/// Whether standard error goes to the journal. systemd sets `JOURNAL_STREAM` to the device and
/// inode numbers of the stream that it connected to the journal. Programs that a service starts
/// inherit the variable, so it only means something if the numbers are those of standard error.
fn stderr_is_the_journal() -> bool {
    let Some(stream) = std::env::var_os("JOURNAL_STREAM") else {
        return false;
    };
    let Ok(stderr) = std::fs::metadata("/dev/stderr") else {
        return false;
    };
    stream.to_str() == Some(&format!("{}:{}", stderr.dev(), stderr.ino()))
}

/// Starts logging.
pub(super) fn start() {
    let journal = Journal {
        under_systemd: stderr_is_the_journal(),
    };
    if log::set_boxed_logger(Box::new(journal)).is_ok() {
        log::set_max_level(LevelFilter::Info);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_systemd_each_line_starts_with_its_level_for_the_journal() {
        assert_eq!(
            lines(Level::Info, "grabbed the keyboard", true),
            "<6>grabbed the keyboard\n"
        );
        assert_eq!(lines(Level::Warn, "save failed", true), "<4>save failed\n");
        assert_eq!(
            lines(Level::Error, "line 1\nline 2", true),
            "<3>line 1\n<3>line 2\n"
        );
    }

    #[test]
    fn in_a_terminal_warnings_and_errors_say_what_they_are() {
        assert_eq!(
            lines(Level::Info, "grabbed the keyboard", false),
            "grabbed the keyboard\n"
        );
        assert_eq!(
            lines(Level::Warn, "save failed", false),
            "warning: save failed\n"
        );
        assert_eq!(
            lines(Level::Error, "no keyboard", false),
            "error: no keyboard\n"
        );
    }
}
