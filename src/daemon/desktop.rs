//! The desktop session of the user who is logged in, where notifications and the color dialog
//! appear. The daemon is root and the desktop belongs to that user, so it starts programs as them.

use std::io::{self, Read};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::{Pid, User};
use wait_timeout::ChildExt;

/// What the daemon shows the user outside the keyboard itself.
pub(crate) trait Desktop: Send + Sync {
    /// Shows a desktop notification to the active session's user. Returns whether it was shown.
    fn notify(&self, summary: &str, body: &str) -> bool;

    /// Opens a box that shows `prompt` and asks for a line of text, which starts as `initial`.
    /// It returns when the box closes.
    fn ask(&self, prompt: &str, initial: &str, window: &Window) -> Answer;

    /// Opens a box that shows `message` as an error, and returns whether the user closed it.
    fn show_error(&self, message: &str, window: &Window) -> bool;
}

/// How a box that asks for text was closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Answer {
    Text(String),
    Cancelled,
    /// The box couldn't be shown, or something other than the user closed it.
    Failed,
}

/// The box that a dialog has on screen. The dialog's thread opens boxes through it, and the
/// dialog's handle closes it from another thread.
#[derive(Debug, Default)]
pub(crate) struct Window {
    state: Mutex<WindowState>,
}

#[derive(Debug, Default)]
struct WindowState {
    closed: bool,
    /// The process of the box that is on screen now. While this is set, the process hasn't been
    /// waited for, so its ID still belongs to it and is safe to send a signal to.
    process: Option<Pid>,
}

/// What a finished process left behind.
struct Finished {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

impl Window {
    /// The lock is only held for a few lines that can't panic, so a poisoned lock still holds a
    /// state that makes sense.
    fn state(&self) -> MutexGuard<'_, WindowState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Closes the box that is on screen, and stops any more boxes from opening.
    pub(crate) fn close(&self) {
        let mut state = self.state();
        state.closed = true;
        if let Some(process) = state.process {
            // The process may have exited already, which needs no action.
            let _ = kill(process, Signal::SIGTERM);
        }
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.state().closed
    }

    /// Runs `command` as the window's box and waits for it to exit. Returns `None` if the window
    /// was closed before the box could open.
    fn run(&self, command: &mut Command) -> io::Result<Option<Finished>> {
        let mut child = {
            let mut state = self.state();
            if state.closed {
                return Ok(None);
            }
            let child = command
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            state.process = i32::try_from(child.id()).ok().map(Pid::from_raw);
            child
        };
        // Reading to the end waits for the process to exit without collecting its exit status, so
        // its ID stays its own until `process` has been cleared below. The two streams are read
        // at the same time, so that the process can't block on one while this waits on the other.
        let (mut stdout, mut stderr) = (String::new(), String::new());
        let (out, err) = (child.stdout.take(), child.stderr.take());
        thread::scope(|scope| {
            scope.spawn(|| err.map(|mut err| err.read_to_string(&mut stderr)));
            out.map(|mut out| out.read_to_string(&mut stdout));
        });
        let mut state = self.state();
        state.process = None;
        let status = child.wait()?;
        Ok(Some(Finished {
            status,
            stdout,
            stderr,
        }))
    }
}

/// The desktop session that is active on the machine's seat.
#[derive(Debug, Default)]
pub(crate) struct Session;

impl Session {
    /// The command that runs `program` as the session's user in their session, or `None` if
    /// nobody is logged in.
    fn command(program: &str, arguments: &[String]) -> Option<Command> {
        let Some(user) = session_user(&loginctl, |name| User::from_name(name).ok().flatten())
        else {
            log::info!("no active desktop session found");
            return None;
        };
        let runtime = format!("/run/user/{}", user.uid);
        let mut command = Command::new("runuser");
        command.args(as_user(&user, wayland_display(&runtime).as_deref()));
        command.arg(program).args(arguments);
        Some(command)
    }

    fn zenity(arguments: &[String], window: &Window) -> Option<Finished> {
        let mut command = Session::command("zenity", arguments)?;
        match window.run(&mut command) {
            Ok(Some(finished)) => {
                // 1 means that the user cancelled or closed the box.
                if !matches!(finished.status.code(), Some(0 | 1)) && !window.is_closed() {
                    let stderr = finished.stderr.trim();
                    log::warn!("zenity failed ({}): {stderr}", finished.status);
                }
                Some(finished)
            }
            Ok(None) => None,
            Err(error) => {
                log::warn!("can't run zenity: {error}");
                None
            }
        }
    }
}

impl Desktop for Session {
    fn notify(&self, summary: &str, body: &str) -> bool {
        let Some(mut command) = Session::command("gdbus", &notify_arguments(summary, body)) else {
            return false;
        };
        match run_within(&mut command, Duration::from_secs(10)) {
            Ok(shown) => shown.is_some(),
            Err(error) => {
                log::warn!("desktop notification failed: {error}");
                false
            }
        }
    }

    fn ask(&self, prompt: &str, initial: &str, window: &Window) -> Answer {
        match Session::zenity(&entry_arguments(prompt, initial), window) {
            Some(finished) => match finished.status.code() {
                Some(0) => Answer::Text(finished.stdout),
                Some(1) => Answer::Cancelled,
                _ => Answer::Failed,
            },
            None => Answer::Failed,
        }
    }

    fn show_error(&self, message: &str, window: &Window) -> bool {
        Session::zenity(&error_arguments(message), window)
            .is_some_and(|finished| matches!(finished.status.code(), Some(0 | 1)))
    }
}

/// Runs `command` and returns what it printed, or `None` if it failed or took longer than
/// `timeout`.
fn run_within(command: &mut Command, timeout: Duration) -> io::Result<Option<String>> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let Some(status) = child.wait_timeout(timeout)? else {
        child.kill()?;
        child.wait()?;
        return Ok(None);
    };
    let mut printed = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        stdout.read_to_string(&mut printed)?;
    }
    Ok(status.success().then_some(printed))
}

/// Asks `loginctl` for one value, and returns it without the newline.
fn loginctl(arguments: &[&str]) -> Option<String> {
    let mut command = Command::new("loginctl");
    command.args(arguments).arg("--value");
    let printed = run_within(&mut command, Duration::from_secs(5)).ok()??;
    Some(printed.trim().to_owned())
}

/// The user of the session that is active on seat0. `loginctl` runs `loginctl` with the given
/// arguments, and `lookup` finds a user by name. Both are arguments so that tests can stand in
/// for them.
fn session_user(
    loginctl: &dyn Fn(&[&str]) -> Option<String>,
    lookup: impl Fn(&str) -> Option<User>,
) -> Option<User> {
    let session = loginctl(&["show-seat", "seat0", "-p", "ActiveSession"])?;
    if session.is_empty() {
        return None;
    }
    let name = loginctl(&["show-session", &session, "-p", "Name"])?;
    lookup(&name)
}

/// The name of the session's Wayland socket in `runtime`, which is the user's `/run/user`
/// directory.
fn wayland_display(runtime: &str) -> Option<String> {
    let pattern = format!("{}/wayland-*", glob::Pattern::escape(runtime));
    let mut sockets: Vec<String> = glob::glob(&pattern)
        .ok()?
        .filter_map(Result::ok)
        .filter(|path| path.extension().is_none_or(|extension| extension != "lock"))
        .filter_map(|path| Some(path.file_name()?.to_str()?.to_owned()))
        .collect();
    sockets.sort();
    sockets.into_iter().next()
}

/// The arguments to `runuser` that run a program as `user` in `user`'s desktop session. The
/// program gets an empty environment plus the variables for reaching that session's Wayland
/// display and D-Bus.
fn as_user(user: &User, wayland_display: Option<&str>) -> Vec<String> {
    let runtime = format!("/run/user/{}", user.uid);
    let mut arguments: Vec<String> = ["-u", &user.name, "--", "env", "-i"]
        .map(str::to_owned)
        .into();
    arguments.extend([
        format!("XDG_RUNTIME_DIR={runtime}"),
        format!("DBUS_SESSION_BUS_ADDRESS=unix:path={runtime}/bus"),
        format!("HOME={}", user.dir.display()),
        format!("USER={}", user.name),
        "PATH=/usr/bin:/bin".to_owned(),
        // Without a UTF-8 locale, zenity rejects arguments that aren't ASCII.
        "LANG=C.UTF-8".to_owned(),
    ]);
    arguments.extend(wayland_display.map(|display| format!("WAYLAND_DISPLAY={display}")));
    arguments
}

fn notify_arguments(summary: &str, body: &str) -> Vec<String> {
    let quoted = |text: &str| format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"));
    let arguments = [
        "call",
        "--session",
        "--dest",
        "org.freedesktop.Notifications",
        "--object-path",
        "/org/freedesktop/Notifications",
        "--method",
        "org.freedesktop.Notifications.Notify",
        // After "--", the -1 at the end isn't taken for an option.
        "--",
        "'kbd-rgb'",
        "0",
        "'input-keyboard'",
        &quoted(summary),
        &quoted(body),
        "[]",
        "{}",
        "-1",
    ];
    arguments.map(str::to_owned).into()
}

const TITLE: &str = "--title=Keyboard color";

fn entry_arguments(prompt: &str, initial: &str) -> Vec<String> {
    vec![
        "--entry".to_owned(),
        TITLE.to_owned(),
        format!("--text={prompt}"),
        format!("--entry-text={initial}"),
    ]
}

/// zenity reads the text of an error box as Pango markup, so the characters that mean something
/// in markup are escaped.
fn error_arguments(message: &str) -> Vec<String> {
    let escaped = message
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    vec![
        "--error".to_owned(),
        TITLE.to_owned(),
        format!("--text={escaped}"),
    ]
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Instant;

    use nix::unistd::{Gid, Uid};

    use super::*;

    fn user(name: &str) -> User {
        User {
            name: name.to_owned(),
            passwd: std::ffi::CString::default(),
            uid: Uid::from_raw(1000),
            gid: Gid::from_raw(1000),
            gecos: std::ffi::CString::default(),
            dir: PathBuf::from("/home").join(name),
            shell: PathBuf::from("/bin/sh"),
        }
    }

    fn known(name: &str) -> Option<User> {
        (name == "someone").then(|| user(name))
    }

    #[test]
    fn the_session_user_is_the_user_of_seat0s_active_session() {
        let loginctl = |arguments: &[&str]| match arguments {
            ["show-seat", "seat0", "-p", "ActiveSession"] => Some("3".to_owned()),
            ["show-session", "3", "-p", "Name"] => Some("someone".to_owned()),
            _ => None,
        };
        assert_eq!(session_user(&loginctl, known), Some(user("someone")));
    }

    #[test]
    fn without_an_active_session_there_is_no_session_user() {
        let loginctl = |_: &[&str]| Some(String::new());
        assert_eq!(session_user(&loginctl, known), None);
    }

    #[test]
    fn a_session_whose_user_does_not_exist_gives_no_session_user() {
        let loginctl = |arguments: &[&str]| match arguments[0] {
            "show-seat" => Some("3".to_owned()),
            _ => Some("no-such-user".to_owned()),
        };
        assert_eq!(session_user(&loginctl, known), None);
    }

    #[test]
    fn when_loginctl_fails_there_is_no_session_user() {
        assert_eq!(session_user(&|_: &[&str]| None, known), None);
    }

    #[test]
    fn a_program_runs_as_the_user_with_a_clean_environment_and_a_utf8_locale() {
        let arguments = as_user(&user("someone"), Some("wayland-0"));
        assert_eq!(arguments[..5], ["-u", "someone", "--", "env", "-i"]);
        for variable in [
            "XDG_RUNTIME_DIR=/run/user/1000",
            "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus",
            "HOME=/home/someone",
            "LANG=C.UTF-8",
            "WAYLAND_DISPLAY=wayland-0",
        ] {
            assert!(
                arguments.contains(&variable.to_owned()),
                "{variable} in {arguments:?}"
            );
        }
    }

    #[test]
    fn without_a_wayland_socket_the_program_gets_no_wayland_display() {
        let arguments = as_user(&user("someone"), None);
        assert!(
            !arguments
                .iter()
                .any(|argument| argument.starts_with("WAYLAND_DISPLAY"))
        );
    }

    #[test]
    fn the_wayland_display_is_the_first_socket_that_is_not_a_lock_file() {
        let runtime = tempfile::tempdir().unwrap();
        for name in ["wayland-1", "wayland-0.lock", "wayland-0", "bus"] {
            std::fs::write(runtime.path().join(name), "").unwrap();
        }
        let display = wayland_display(runtime.path().to_str().unwrap());
        assert_eq!(display.as_deref(), Some("wayland-0"));
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(wayland_display(empty.path().to_str().unwrap()), None);
    }

    #[test]
    fn a_notifications_text_is_quoted_for_gdbus() {
        let arguments = notify_arguments("It's back", "a\\b");
        assert!(arguments.contains(&"'It\\'s back'".to_owned()));
        assert!(arguments.contains(&"'a\\\\b'".to_owned()));
        assert_eq!(arguments.last().unwrap(), "-1");
    }

    #[test]
    fn an_error_boxs_text_has_its_markup_escaped() {
        let arguments = error_arguments("\"<i>zzz\" isn't a color & more");
        assert_eq!(
            arguments[2],
            "--text=\"&lt;i&gt;zzz\" isn't a color &amp; more"
        );
    }

    #[test]
    fn an_entry_box_starts_with_the_text_it_was_given() {
        let arguments = entry_arguments("Color:", "<i>zzz");
        assert_eq!(
            arguments,
            ["--entry", TITLE, "--text=Color:", "--entry-text=<i>zzz"]
        );
    }

    fn shell(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn a_window_gives_what_its_box_printed_and_how_it_exited() {
        let window = Window::default();
        let finished = window
            .run(&mut shell("echo typed; echo oops >&2; exit 1"))
            .unwrap()
            .unwrap();
        assert_eq!(finished.stdout, "typed\n");
        assert_eq!(finished.stderr, "oops\n");
        assert_eq!(finished.status.code(), Some(1));
    }

    #[test]
    fn closing_a_window_ends_the_box_that_is_on_screen() {
        let window = Arc::new(Window::default());
        let started = Instant::now();
        let closer = {
            let window = Arc::clone(&window);
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(100));
                window.close();
            })
        };
        let finished = window.run(&mut shell("exec sleep 30")).unwrap().unwrap();
        closer.join().unwrap();
        assert_eq!(
            finished.status.code(),
            None,
            "the box was ended by a signal"
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(window.is_closed());
    }

    #[test]
    fn a_closed_window_opens_no_more_boxes() {
        let window = Window::default();
        window.close();
        assert!(window.run(&mut shell("echo typed")).unwrap().is_none());
    }

    #[test]
    fn run_within_gives_up_on_a_program_that_takes_too_long() {
        let started = Instant::now();
        let printed = run_within(&mut shell("exec sleep 30"), Duration::from_millis(100)).unwrap();
        assert_eq!(printed, None);
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(
            run_within(&mut shell("echo 3"), Duration::from_secs(5))
                .unwrap()
                .as_deref(),
            Some("3\n")
        );
        assert_eq!(
            run_within(&mut shell("exit 1"), Duration::from_secs(5)).unwrap(),
            None
        );
    }
}
