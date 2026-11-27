//! Startup problems, and telling the desktop user about them.
//!
//! When the daemon can't start, it exits and systemd starts it again a few seconds later. The
//! problem file counts those failed starts across the restarts.

use std::num::NonZeroU32;
use std::path::Path;
use std::{fs, io};

use serde::{Deserialize, Serialize};

use super::desktop::Desktop;

/// Why the daemon couldn't start, or had to stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Problem {
    /// The settings file can't be used. It holds what is wrong with the file.
    BadSettings(String),
    /// The keyboard to grab isn't there. It holds the `device` setting.
    KeyboardNotFound(String),
    /// Something else went wrong. It holds the error.
    Failed(String),
}

impl Problem {
    /// Tells one problem from another, so that the same problem coming back is counted and a
    /// different one starts a new count.
    fn key(&self) -> String {
        match self {
            Problem::BadSettings(what) => format!("settings:{what}"),
            Problem::KeyboardNotFound(device) => format!("not-found:{device}"),
            Problem::Failed(error) => format!("failed:{error}"),
        }
    }

    fn message(&self) -> String {
        match self {
            Problem::BadSettings(what) => {
                format!("The kbd-rgb daemon's settings file doesn't work. {what}")
            }
            Problem::KeyboardNotFound(device) => {
                format!("The kbd-rgb daemon can't find the keyboard \"{device}\".")
            }
            Problem::Failed(error) => format!("The kbd-rgb daemon stopped: {error}."),
        }
    }
}

/// What the problem file holds.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Record {
    key: String,
    /// The number of failed starts in a row with this problem.
    count: u32,
    /// Whether the desktop user has been notified.
    notified: bool,
}

/// A problem file that can't be read is the same as no problem file.
fn load(file: &Path) -> Option<Record> {
    serde_json::from_str(&fs::read_to_string(file).ok()?).ok()
}

fn save(file: &Path, record: &Record) -> io::Result<()> {
    if let Some(directory) = file.parent() {
        fs::create_dir_all(directory)?;
    }
    fs::write(file, serde_json::to_string(record)?)
}

/// Records a failed start in `file`. Once the same problem has caused `report_after` failed starts
/// in a row, the desktop user gets one notification about it, so a brief hiccup doesn't notify
/// anyone. If the notification couldn't be shown, for example because nobody was logged in, the
/// next failed start tries again.
pub(crate) fn report(
    problem: &Problem,
    file: &Path,
    report_after: NonZeroU32,
    desktop: &dyn Desktop,
) {
    let key = problem.key();
    let mut record = load(file)
        .filter(|record| record.key == key)
        .unwrap_or_default();
    record.key = key;
    record.count += 1;
    if !record.notified && record.count >= report_after.get() {
        let body = format!(
            "{} The kbd-rgb service keeps retrying; see journalctl -u kbd-rgb.",
            problem.message()
        );
        record.notified = desktop.notify("Keyboard colors stopped working", &body);
    }
    if let Err(error) = save(file, &record) {
        log::warn!("can't record the problem: {error}");
    }
}

/// Clears the problem in `file`. Call it once the daemon has run for a while. If the desktop user
/// was told about the problem, they are told that it's over.
pub(crate) fn recovered(file: &Path, desktop: &dyn Desktop) {
    let Some(record) = load(file) else { return };
    // The file may be gone already, which is what this wants.
    let _ = fs::remove_file(file);
    if record.notified {
        desktop.notify(
            "Keyboard colors are working again",
            "The kbd-rgb service is running.",
        );
    }
    log::info!("recovered from: {}", record.key);
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::testing::FakeDesktop;

    struct Fixture {
        file: PathBuf,
        desktop: FakeDesktop,
        _directory: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let file = directory.path().join("run").join("problem.json");
            Fixture {
                file,
                desktop: FakeDesktop::default(),
                _directory: directory,
            }
        }

        fn report(&self, problem: &Problem, times: usize) {
            for _ in 0..times {
                report(
                    problem,
                    &self.file,
                    NonZeroU32::new(3).unwrap(),
                    &self.desktop,
                );
            }
        }

        fn recovered(&self) {
            recovered(&self.file, &self.desktop);
        }

        fn notified(&self) -> usize {
            self.desktop.notifications().len()
        }
    }

    fn not_found(device: &str) -> Problem {
        Problem::KeyboardNotFound(device.to_owned())
    }

    #[test]
    fn the_user_is_notified_on_the_third_failed_start_in_a_row() {
        let fixture = Fixture::new();
        fixture.report(&not_found("x"), 2);
        assert_eq!(fixture.notified(), 0);
        fixture.report(&not_found("x"), 1);
        let notifications = fixture.desktop.notifications();
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].0, "Keyboard colors stopped working");
        assert_eq!(
            notifications[0].1,
            "The kbd-rgb daemon can't find the keyboard \"x\". The kbd-rgb service keeps \
             retrying; see journalctl -u kbd-rgb."
        );
    }

    #[test]
    fn a_problem_is_notified_once() {
        let fixture = Fixture::new();
        fixture.report(&not_found("x"), 10);
        assert_eq!(fixture.notified(), 1);
    }

    #[test]
    fn a_different_problem_starts_a_new_count() {
        let fixture = Fixture::new();
        fixture.report(&Problem::Failed("device gone".to_owned()), 2);
        fixture.report(&not_found("y"), 2);
        assert_eq!(fixture.notified(), 0);
        fixture.report(&not_found("y"), 1);
        assert_eq!(fixture.notified(), 1);
    }

    #[test]
    fn recovery_from_a_notified_problem_is_notified_and_clears_the_file() {
        let fixture = Fixture::new();
        fixture.report(&not_found("x"), 3);
        fixture.recovered();
        let notifications = fixture.desktop.notifications();
        assert_eq!(notifications.len(), 2);
        assert_eq!(notifications[1].0, "Keyboard colors are working again");
        assert!(!fixture.file.exists());
        fixture.recovered();
        assert_eq!(fixture.notified(), 2);
    }

    #[test]
    fn a_problem_that_was_never_notified_clears_without_a_notification() {
        let fixture = Fixture::new();
        fixture.report(&Problem::Failed("x".to_owned()), 1);
        assert!(fixture.file.exists());
        fixture.recovered();
        assert!(!fixture.file.exists());
        assert_eq!(fixture.notified(), 0);
    }

    #[test]
    fn with_no_problem_recovery_does_nothing() {
        let fixture = Fixture::new();
        fixture.recovered();
        assert_eq!(fixture.notified(), 0);
    }

    #[test]
    fn a_notification_that_could_not_be_shown_is_tried_again_on_the_next_failed_start() {
        let fixture = Fixture::new();
        fixture.desktop.hide_notifications(true);
        fixture.report(&not_found("z"), 4);
        assert_eq!(fixture.notified(), 0);
        fixture.desktop.hide_notifications(false);
        fixture.report(&not_found("z"), 1);
        assert_eq!(fixture.notified(), 1);
    }

    #[test]
    fn each_kind_of_problem_has_its_own_message() {
        let bad = Problem::BadSettings("caps.delay must be more than 0.".to_owned());
        assert_eq!(
            bad.message(),
            "The kbd-rgb daemon's settings file doesn't work. caps.delay must be more than 0."
        );
        let failed = Problem::Failed("can't grab the keyboard: device busy".to_owned());
        assert_eq!(
            failed.message(),
            "The kbd-rgb daemon stopped: can't grab the keyboard: device busy."
        );
    }

    #[test]
    fn a_problem_file_that_cannot_be_read_counts_from_the_start() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.file.parent().unwrap()).unwrap();
        fs::write(&fixture.file, "not json").unwrap();
        fixture.report(&not_found("x"), 2);
        assert_eq!(fixture.notified(), 0);
        fixture.report(&not_found("x"), 1);
        assert_eq!(fixture.notified(), 1);
    }
}
