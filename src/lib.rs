//! RGB keyboard control for System76 laptops. See README.md for how to use it.

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
