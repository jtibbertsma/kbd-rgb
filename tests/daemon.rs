//! Runs the real `kbd-rgb-daemon` binary as far as it gets without a keyboard to grab: checking
//! its settings, and failing to start.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

/// A temporary directory with a settings file for the daemon.
struct Settings {
    directory: tempfile::TempDir,
}

impl Settings {
    /// A settings file with `text`, after lines that keep the daemon's files inside the temporary
    /// directory and name a keyboard that doesn't exist.
    fn new(text: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let settings = Settings { directory };
        let top = format!(
            "device = \"{}\"\n",
            settings.path("no-such-keyboard").display()
        );
        let system = format!(
            "[system]\nproblem-file = \"{}\"\n",
            settings.problem_file().display()
        );
        fs::write(settings.file(), format!("{top}{text}\n{system}")).unwrap();
        settings
    }

    fn path(&self, name: &str) -> PathBuf {
        self.directory.path().join(name)
    }

    fn file(&self) -> PathBuf {
        self.path("config.toml")
    }

    fn problem_file(&self) -> PathBuf {
        self.path("problem.json")
    }

    fn daemon(&self, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_kbd-rgb-daemon"))
            .arg("--config")
            .arg(self.file())
            .args(arguments)
            // Without this, the daemon would write its log lines in the journal's form.
            .env_remove("JOURNAL_STREAM")
            .output()
            .unwrap()
    }

    fn problem(&self) -> Option<serde_json::Value> {
        let text = fs::read_to_string(self.problem_file()).ok()?;
        Some(serde_json::from_str(&text).unwrap())
    }
}

fn text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).unwrap()
}

#[test]
fn check_accepts_settings_that_work_and_starts_nothing() {
    let settings = Settings::new("[caps]\ndelay = 0.5");
    let output = settings.daemon(&["--check"]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert!(text(&output.stdout).starts_with("The settings in "));
    assert_eq!(
        settings.problem(),
        None,
        "a check doesn't look for the keyboard"
    );
}

#[test]
fn check_accepts_a_settings_file_that_does_not_exist() {
    let directory = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_kbd-rgb-daemon"))
        .arg("--check")
        .arg("--config")
        .arg(directory.path().join("missing.toml"))
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(text(&output.stdout).contains("so the defaults apply"));
}

#[test]
fn check_says_what_is_wrong_with_the_settings_and_on_which_line() {
    let settings = Settings::new("[caps]\ndelay = 0");
    let output = settings.daemon(&["--check"]);
    assert_eq!(output.status.code(), Some(2));
    let error = text(&output.stderr);
    assert!(error.starts_with("error: "), "{error}");
    assert!(error.contains("config.toml"), "{error}");
    assert!(error.contains("line 3"), "{error}");
    assert!(error.contains("0 must be more than 0."), "{error}");
    assert_eq!(settings.problem(), None, "a check doesn't record a problem");
}

#[test]
fn starting_without_the_keyboard_fails_and_records_the_problem() {
    let settings = Settings::new("");
    let output = settings.daemon(&[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        text(&output.stderr).contains("keyboard not found"),
        "{}",
        text(&output.stderr)
    );
    let problem = settings.problem().unwrap();
    assert!(problem["key"].as_str().unwrap().starts_with("not-found:"));
    assert_eq!(problem["count"], 1);
    assert_eq!(problem["notified"], false);

    settings.daemon(&[]);
    assert_eq!(
        settings.problem().unwrap()["count"],
        2,
        "failed starts in a row are counted"
    );
}

#[test]
fn help_lists_the_two_options() {
    let output = Command::new(env!("CARGO_BIN_EXE_kbd-rgb-daemon"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = text(&output.stdout);
    assert!(
        help.contains("--config") && help.contains("--check"),
        "{help}"
    );
}
