//! The Unix socket that `kbdrgb` sends its requests to.

use std::num::NonZeroUsize;
use std::os::unix::fs::{MetadataExt, PermissionsExt, chown};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use std::{fs, io};

use super::lights::{Command, LightsHandle};
use crate::request::{self, ReadError, Request, Response};

/// The daemon's socket file. Dropping it removes the file, and without the file `kbdrgb` knows
/// that the daemon isn't running.
#[derive(Debug)]
pub(crate) struct Socket {
    listener: UnixListener,
    path: PathBuf,
}

impl Socket {
    /// Creates the socket at `path`, replacing a file that a daemon left there.
    ///
    /// `kbdrgb` needs write permission on the socket to connect. The udev rule gives the color
    /// files the user's own group, so the socket gets the group of `color_file` too. The same user
    /// can then run `kbdrgb` without root.
    pub(crate) fn bind(path: &Path, color_file: &Path) -> io::Result<Self> {
        if let Some(directory) = path.parent() {
            fs::create_dir_all(directory)?;
        }
        match fs::remove_file(path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        let socket = Socket {
            listener: UnixListener::bind(path)?,
            path: path.to_owned(),
        };
        let allow_group = || {
            chown(path, None, Some(fs::metadata(color_file)?.gid()))?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o660))
        };
        if let Err(error) = allow_group() {
            log::warn!("socket permissions: {error}");
        }
        Ok(socket)
    }

    /// Starts the `commands` thread, which answers one request at a time until the daemon exits.
    pub(crate) fn serve(&self, lights: LightsHandle, limits: Limits) -> io::Result<JoinHandle<()>> {
        let listener = self.listener.try_clone()?;
        thread::Builder::new()
            .name("commands".to_owned())
            .spawn(move || {
                for connection in listener.incoming() {
                    match connection {
                        Ok(stream) => {
                            if let Err(error) = answer(&stream, &lights, limits) {
                                log::warn!("kbdrgb request failed: {error}");
                            }
                        }
                        Err(error) => {
                            log::warn!("accept failed: {error}");
                            thread::sleep(Duration::from_secs(1));
                        }
                    }
                }
            })
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        // Nothing can be done about a file that can't be removed, and `kbdrgb` copes with one.
        let _ = fs::remove_file(&self.path);
    }
}

/// How much one `kbdrgb` connection can ask of the daemon.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    /// The longest request, in bytes.
    pub(crate) max_request: NonZeroUsize,
    /// How long `kbdrgb` gets to send its request.
    pub(crate) timeout: Duration,
}

fn answer(stream: &UnixStream, lights: &LightsHandle, limits: Limits) -> io::Result<()> {
    stream.set_read_timeout(Some(limits.timeout))?;
    stream.set_write_timeout(Some(limits.timeout))?;
    let response = match request::read(stream, limits.max_request.get()) {
        Ok(request) => carry_out(request, lights),
        Err(ReadError::Io(error)) => return Err(error),
        Err(error) => Response::Rejected(error.to_string()),
    };
    request::write(stream, &response)
}

fn carry_out(request: Request, lights: &LightsHandle) -> Response {
    match request {
        Request::Run(actions) => {
            lights.send(Command::Run {
                actions,
                new_step: true,
            });
            // The answer goes out after the colors do, so that a script can rely on them.
            lights.sync();
            Response::Done
        }
        Request::Status => match lights.status() {
            Some(status) => Response::Status(status),
            None => Response::Rejected("the daemon's lights have stopped".to_owned()),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::time::Instant;

    use super::*;
    use crate::daemon::config::Config;
    use crate::testing::{FakeLeds, actions, lights};
    use crate::text;
    use crate::zone::Zone;

    /// A server on a socket in a temporary directory, with lights that write to fake LEDs.
    struct Fixture {
        socket: Socket,
        leds: FakeLeds,
        directory: tempfile::TempDir,
    }

    const TIMEOUT: Duration = Duration::from_secs(2);

    impl Fixture {
        fn new() -> Self {
            Fixture::with(Limits {
                max_request: NonZeroUsize::new(65536).unwrap(),
                timeout: TIMEOUT,
            })
        }

        fn with(limits: Limits) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let color_file = directory.path().join("color_left");
            fs::write(&color_file, "FFFFFF").unwrap();
            let leds = FakeLeds::default();
            let state_file = directory.path().join("state.json");
            let lights = lights(&Config::default(), &leds, &state_file, Instant::now());
            let (handle, _thread) = crate::daemon::lights::spawn(lights).unwrap();
            let socket =
                Socket::bind(&directory.path().join("run").join("socket"), &color_file).unwrap();
            socket.serve(handle, limits).unwrap();
            Fixture {
                socket,
                leds,
                directory,
            }
        }

        fn send(&self, request: &Request) -> Response {
            request::send(&self.socket.path, request, TIMEOUT).unwrap()
        }

        fn send_text(&self, text: &str) -> Response {
            let mut stream = UnixStream::connect(&self.socket.path).unwrap();
            stream.write_all(text.as_bytes()).unwrap();
            request::read(&stream, 1 << 20).unwrap()
        }
    }

    #[test]
    fn a_run_request_is_answered_once_the_colors_are_written() {
        let fixture = Fixture::new();
        let response = fixture.send(&Request::Run(actions("zones 112233 445566 778899 AABBCC")));
        assert_eq!(response, Response::Done);
        assert_eq!(
            fixture.leds.colors(),
            ["112233", "445566", "778899", "AABBCC"]
        );
    }

    #[test]
    fn one_request_is_one_undo_step() {
        let fixture = Fixture::new();
        fixture.send(&Request::Run(actions("color FF0000")));
        fixture.send(&Request::Run(actions("zones 112233 445566 778899 AABBCC")));
        let Response::Status(status) = fixture.send(&Request::Status) else {
            panic!()
        };
        assert_eq!(text::describe(&status[Zone::Right].setting), "778899");
    }

    #[test]
    fn a_status_request_is_answered_with_each_zones_setting() {
        let fixture = Fixture::new();
        fixture.send(&Request::Run(actions("pulse -z center -d 30 8800FF")));
        let Response::Status(status) = fixture.send(&Request::Status) else {
            panic!()
        };
        assert_eq!(
            text::describe(&status[Zone::Center].setting),
            "pulse 8800FF 0.5"
        );
        assert!(status[Zone::Center].time_left.is_some());
        assert_eq!(text::describe(&status[Zone::Left].setting), "FFFFFF");
    }

    #[test]
    fn a_request_that_is_not_valid_is_rejected_and_changes_nothing() {
        let fixture = Fixture::new();
        fixture.leds.take();
        assert_eq!(
            fixture.send_text("color FF0000\n"),
            Response::Rejected("bad request".to_owned())
        );
        assert_eq!(
            fixture.send_text("{\"Run\": 3}\n"),
            Response::Rejected("bad request".to_owned())
        );
        assert_eq!(fixture.leds.take(), []);
    }

    #[test]
    fn a_request_longer_than_the_limit_is_rejected() {
        let limits = Limits {
            max_request: NonZeroUsize::new(64).unwrap(),
            timeout: TIMEOUT,
        };
        let fixture = Fixture::with(limits);
        let response = fixture.send(&Request::Run(actions("cycle FF0000 00FF00 0000FF FFFF00")));
        assert_eq!(response, Response::Rejected("command too long".to_owned()));
    }

    #[test]
    fn a_client_that_sends_nothing_does_not_stop_the_next_one() {
        let limits = Limits {
            max_request: NonZeroUsize::new(65536).unwrap(),
            timeout: Duration::from_millis(50),
        };
        let fixture = Fixture::with(limits);
        let silent = UnixStream::connect(&fixture.socket.path).unwrap();
        assert_eq!(
            fixture.send(&Request::Run(actions("color 123456"))),
            Response::Done
        );
        drop(silent);
    }

    #[test]
    fn the_socket_is_writable_by_the_color_files_group() {
        let fixture = Fixture::new();
        let socket = fs::metadata(&fixture.socket.path).unwrap();
        let color_file = fs::metadata(fixture.directory.path().join("color_left")).unwrap();
        assert_eq!(socket.permissions().mode() & 0o777, 0o660);
        assert_eq!(socket.gid(), color_file.gid());
    }

    #[test]
    fn binding_replaces_a_socket_file_that_a_daemon_left_behind() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("socket");
        let color_file = directory.path().join("color_left");
        fs::write(&color_file, "FFFFFF").unwrap();
        // Forgetting the socket skips its `Drop`, as a daemon that was killed does.
        std::mem::forget(Socket::bind(&path, &color_file).unwrap());
        assert!(path.exists());
        assert!(Socket::bind(&path, &color_file).is_ok());
    }

    #[test]
    fn dropping_the_socket_removes_its_file() {
        let fixture = Fixture::new();
        let path = fixture.socket.path.clone();
        assert!(path.exists());
        drop(fixture);
        assert!(!path.exists());
    }
}
