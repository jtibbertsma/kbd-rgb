//! The `kbdrgb` command, which sets the keyboard's colors and effects.

use std::process::ExitCode;

fn main() -> ExitCode {
    kbd_rgb::cli::main()
}
