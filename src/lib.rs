//! RGB keyboard control for System76 laptops. See README.md for how to use it.
//!
//! The package builds two programs, and this library holds the code of both:
//!
//! - `kbd-rgb-daemon` runs in the background as root and controls the keyboard's lights. Its code
//!   is in [`daemon`].
//! - `kbdrgb` is the command that sets colors and effects. Its code is in [`cli`].
//!
//! The other modules are what the two programs share: zones, colors, easing curves, settings,
//! actions, the text syntax of the color dialog, the requests that `kbdrgb` sends to the daemon,
//! and the driver's color files.

mod action;
pub mod cli;
mod color;
pub mod daemon;
mod ease;
mod leds;
mod request;
mod seconds;
mod setting;
#[cfg(test)]
mod testing;
mod text;
mod zone;
