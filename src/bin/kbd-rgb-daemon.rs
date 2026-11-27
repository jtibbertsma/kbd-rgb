//! The kbd-rgb daemon, which the `kbd-rgb` systemd service runs as root.

use std::process::ExitCode;

fn main() -> ExitCode {
    kbd_rgb::daemon::main()
}
