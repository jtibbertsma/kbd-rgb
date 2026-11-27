//! RGB keyboard control for System76 laptops. See README.md for how to use it.
// Later commits add the code that uses these modules. This line goes away when the binaries exist.
#![allow(dead_code)]

mod action;
mod cli;
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
