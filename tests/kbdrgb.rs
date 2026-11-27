//! Runs the real `kbdrgb` binary. The library's own tests cover what `kbdrgb` does in detail.
//! These check that the binary is wired up: its arguments, its output, and its exit status.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;

/// A keyboard's LED directory in a temporary directory, and a place for a socket.
struct Keyboard {
    directory: tempfile::TempDir,
}

impl Keyboard {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        for zone in ["left", "center", "right", "extra"] {
            fs::write(directory.path().join(format!("color_{zone}")), "FFFFFF").unwrap();
        }
        fs::write(directory.path().join("brightness"), "255\n").unwrap();
        Keyboard { directory }
    }

    fn socket(&self) -> PathBuf {
        self.directory.path().join("socket")
    }

    fn kbdrgb(&self, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_kbdrgb"))
            .args(arguments)
            .env("KBDRGB_LED", self.directory.path())
            .env("KBDRGB_SOCKET", self.socket())
            .output()
            .unwrap()
    }

    fn color(&self, zone: &str) -> String {
        fs::read_to_string(self.directory.path().join(format!("color_{zone}"))).unwrap()
    }
}

fn text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).unwrap()
}

/// Stands in for the daemon: answers one connection on `socket` with `answer`, and returns the
/// line that `kbdrgb` sent.
fn answer_once(socket: &Path, answer: &'static str) -> thread::JoinHandle<String> {
    let listener = UnixListener::bind(socket).unwrap();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = String::new();
        BufReader::new(&stream).read_line(&mut request).unwrap();
        stream.write_all(answer.as_bytes()).unwrap();
        request
    })
}

#[test]
fn help_lists_the_commands_and_exits_with_success() {
    let keyboard = Keyboard::new();
    for arguments in [&["--help"][..], &["-h"], &["help"]] {
        let output = keyboard.kbdrgb(arguments);
        assert!(output.status.success(), "{arguments:?}");
        let help = text(&output.stdout);
        for command in [
            "color",
            "zones",
            "cycle",
            "rainbow",
            "pulse",
            "brightness",
            "status",
        ] {
            assert!(
                help.contains(command),
                "{arguments:?}: {command} is missing from:\n{help}"
            );
        }
    }
    let output = keyboard.kbdrgb(&["cycle", "--help"]);
    assert!(output.status.success());
    for option in ["--zone", "--time", "--for", "--ease"] {
        assert!(text(&output.stdout).contains(option), "{option}");
    }
}

#[test]
fn without_the_daemon_a_color_is_written_to_the_keyboard() {
    let keyboard = Keyboard::new();
    let output = keyboard.kbdrgb(&["color", "abcdef", "-z", "right"]);
    assert!(output.status.success());
    assert_eq!(text(&output.stdout), "");
    assert_eq!(text(&output.stderr), "");
    assert_eq!(keyboard.color("right"), "ABCDEF");
    assert_eq!(keyboard.color("left"), "FFFFFF");
}

#[test]
fn without_the_daemon_no_command_prints_the_status() {
    let keyboard = Keyboard::new();
    let output = keyboard.kbdrgb(&[]);
    assert!(output.status.success());
    assert_eq!(
        text(&output.stdout),
        "brightness: 255\n\
         left:   FFFFFF\n\
         center: FFFFFF\n\
         right:  FFFFFF\n\
         extra:  FFFFFF\n\
         kbdrgb zones FFFFFF FFFFFF FFFFFF FFFFFF\n"
    );
}

#[test]
fn an_error_goes_to_standard_error_and_fails() {
    let keyboard = Keyboard::new();
    let output = keyboard.kbdrgb(&["cycle", "AA00AA"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        text(&output.stderr),
        "kbdrgb: cycle requires 2 to 16 colors.\n"
    );

    let output = keyboard.kbdrgb(&["rainbow"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(text(&output.stderr).starts_with("kbdrgb: rainbow needs the daemon"));

    let output = keyboard.kbdrgb(&["bogus"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(text(&output.stderr).contains("unrecognized subcommand 'bogus'"));
    assert_eq!(keyboard.color("left"), "FFFFFF");
}

#[test]
fn with_a_daemon_the_request_goes_over_the_socket_as_one_line_of_json() {
    let keyboard = Keyboard::new();
    let daemon = answer_once(&keyboard.socket(), "\"Done\"\n");
    let output = keyboard.kbdrgb(&["color", "-z", "left", "-t", "2", "FF0000"]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let request: serde_json::Value = serde_json::from_str(&daemon.join().unwrap()).unwrap();
    let expected = serde_json::json!({"Run": [{
        "target": "left",
        "change": {"Color": {"color": "FF0000", "fade": 2.0, "curve": "linear"}},
    }]});
    assert_eq!(request, expected);
    // The daemon sets the color. kbdrgb only writes it when there is no daemon.
    assert_eq!(keyboard.color("left"), "FFFFFF");
}

#[test]
fn the_daemons_reason_for_rejecting_a_request_is_the_error() {
    let keyboard = Keyboard::new();
    let daemon = answer_once(&keyboard.socket(), "{\"Rejected\":\"command too long\"}\n");
    let output = keyboard.kbdrgb(&["rainbow"]);
    daemon.join().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stderr), "kbdrgb: command too long\n");
}
