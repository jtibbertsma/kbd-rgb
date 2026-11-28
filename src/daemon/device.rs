//! The keyboard that the daemon grabs, and the virtual keyboard that it passes keys on through.

use std::fs::File;
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Path, PathBuf};

use evdev::InputEvent;
use input_linux::sys::input_event;
use input_linux::{EventKind, InputId, UInputHandle};

/// A keyboard whose events the daemon reads. Polling its file descriptor says when there are
/// events to read.
pub(crate) trait Keyboard: AsFd {
    /// Reads the events that have arrived. It fails when the keyboard has gone away.
    fn read(&mut self) -> io::Result<Vec<InputEvent>>;
}

/// The keyboard that the desktop sees in place of the grabbed one.
pub(crate) trait VirtualKeyboard: AsFd {
    /// Sends events to the desktop.
    fn write(&mut self, events: &[InputEvent]) -> io::Result<()>;

    /// Reads the events that the desktop sent to the keyboard, which are changes to its LEDs.
    fn read(&mut self) -> io::Result<Vec<InputEvent>>;
}

/// A keyboard that only the daemon gets events from. Dropping it gives the keyboard back.
#[derive(Debug)]
pub(crate) struct Grabbed {
    device: evdev::Device,
}

impl Grabbed {
    pub(crate) fn grab(mut device: evdev::Device) -> io::Result<Self> {
        device.grab()?;
        Ok(Grabbed { device })
    }
}

impl Drop for Grabbed {
    fn drop(&mut self) {
        // This fails when the keyboard has gone away, and then there is nothing to give back.
        let _ = self.device.ungrab();
    }
}

impl AsFd for Grabbed {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.device.as_fd()
    }
}

impl Keyboard for Grabbed {
    fn read(&mut self) -> io::Result<Vec<InputEvent>> {
        Ok(self.device.fetch_events()?.collect())
    }
}

/// What an input device can do, as the event codes it supports.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Features {
    keys: Vec<u16>,
    relative_axes: Vec<u16>,
    scancodes: Vec<u16>,
    switches: Vec<u16>,
    leds: Vec<u16>,
    sounds: Vec<u16>,
}

impl Features {
    pub(crate) fn of(keyboard: &evdev::Device) -> Self {
        fn codes<T: evdev::EvdevEnum>(
            supported: Option<&evdev::AttributeSetRef<T>>,
            code: impl Fn(T) -> u16,
        ) -> Vec<u16> {
            supported
                .map(|supported| supported.iter().map(code).collect())
                .unwrap_or_default()
        }
        Features {
            keys: codes(keyboard.supported_keys(), evdev::KeyCode::code),
            relative_axes: codes(keyboard.supported_relative_axes(), |axis| axis.0),
            scancodes: codes(keyboard.misc_properties(), |kind| kind.0),
            switches: codes(keyboard.supported_switches(), |switch| switch.0),
            leds: codes(keyboard.supported_leds(), |led| led.0),
            sounds: codes(keyboard.supported_sounds(), |sound| sound.0),
        }
    }
}

/// A virtual keyboard made with the kernel's uinput. Dropping it removes the keyboard.
pub(crate) struct Uinput {
    handle: UInputHandle<File>,
}

impl Uinput {
    /// Creates a virtual keyboard named `name` that can do what `features` lists.
    ///
    /// The LEDs matter most. The desktop turns a keyboard's caps lock LED on and off, and the
    /// daemon grabs the real keyboard, so the desktop can only reach this one. Reading the LED
    /// changes here is how the daemon knows that caps lock is on.
    pub(crate) fn create(name: &str, features: &Features) -> io::Result<Self> {
        /// Declares one kind of event and each of its codes. A code that this version of
        /// input-linux doesn't know is left out.
        fn declare<T: TryFrom<u16>>(
            handle: &UInputHandle<File>,
            kind: EventKind,
            codes: &[u16],
            declare: impl Fn(&UInputHandle<File>, T) -> io::Result<()>,
        ) -> io::Result<()> {
            if codes.is_empty() {
                return Ok(());
            }
            handle.set_evbit(kind)?;
            codes
                .iter()
                .filter_map(|&code| T::try_from(code).ok())
                .try_for_each(|code| declare(handle, code))
        }

        let handle = UInputHandle::new(File::options().read(true).write(true).open("/dev/uinput")?);
        declare(
            &handle,
            EventKind::Key,
            &features.keys,
            UInputHandle::set_keybit,
        )?;
        declare(
            &handle,
            EventKind::Relative,
            &features.relative_axes,
            UInputHandle::set_relbit,
        )?;
        declare(
            &handle,
            EventKind::Misc,
            &features.scancodes,
            UInputHandle::set_mscbit,
        )?;
        declare(
            &handle,
            EventKind::Switch,
            &features.switches,
            UInputHandle::set_swbit,
        )?;
        declare(
            &handle,
            EventKind::Led,
            &features.leds,
            UInputHandle::set_ledbit,
        )?;
        declare(
            &handle,
            EventKind::Sound,
            &features.sounds,
            UInputHandle::set_sndbit,
        )?;
        // Bus 3 is USB. The vendor, product, and version numbers have no meaning.
        let id = InputId {
            bustype: 3,
            vendor: 1,
            product: 1,
            version: 1,
        };
        handle.create(&id, name.as_bytes(), 0, &[])?;
        Ok(Uinput { handle })
    }

    /// The virtual keyboard's device file, for the log.
    pub(crate) fn path(&self) -> io::Result<PathBuf> {
        self.handle.evdev_path()
    }
}

impl Drop for Uinput {
    fn drop(&mut self) {
        // Closing the file removes the keyboard too, so a failure here changes nothing.
        let _ = self.handle.dev_destroy();
    }
}

impl AsFd for Uinput {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.handle.as_inner().as_fd()
    }
}

impl VirtualKeyboard for Uinput {
    fn write(&mut self, events: &[InputEvent]) -> io::Result<()> {
        let events: Vec<input_event> = events.iter().map(|&event| event.into()).collect();
        self.handle.write(&events)?;
        Ok(())
    }

    fn read(&mut self) -> io::Result<Vec<InputEvent>> {
        let mut events = [InputEvent::new(0, 0, 0).into(); 16];
        let count = self.handle.read(&mut events)?;
        Ok(events[..count].iter().map(|&event| event.into()).collect())
    }
}

/// Finds the keyboard that `device` names: a device file's path if it starts with `/`, and a
/// device's name otherwise. Returns the device file's path, or `None` if the keyboard isn't there.
pub(crate) fn find_keyboard(device: &str) -> Option<PathBuf> {
    if device.starts_with('/') {
        return Path::new(device).canonicalize().ok();
    }
    evdev::enumerate()
        .find(|(_, found)| found.name() == Some(device))
        .map(|(path, _)| path)
}

/// Whether caps lock is on, read from the caps lock LED files that match `pattern`. It's only used
/// at startup. After that, changes arrive as LED events.
pub(crate) fn caps_lock_on(pattern: &str) -> bool {
    let Ok(leds) = glob::glob(pattern) else {
        return false;
    };
    leds.filter_map(Result::ok)
        .filter_map(|led| std::fs::read_to_string(led).ok())
        .any(|brightness| brightness.trim() != "0")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn a_keyboard_given_as_a_path_is_found_if_the_path_exists() {
        let directory = tempfile::tempdir().unwrap();
        let device = directory.path().join("event-kbd");
        fs::write(&device, "").unwrap();
        let link = directory.path().join("by-path");
        std::os::unix::fs::symlink(&device, &link).unwrap();
        assert_eq!(
            find_keyboard(link.to_str().unwrap()),
            Some(device.canonicalize().unwrap())
        );
        assert_eq!(
            find_keyboard(directory.path().join("missing").to_str().unwrap()),
            None
        );
    }

    #[test]
    fn a_keyboard_given_as_a_name_that_no_device_has_is_not_found() {
        assert_eq!(find_keyboard("no keyboard has this name"), None);
    }

    #[test]
    fn caps_lock_is_on_if_any_caps_lock_led_is_lit() {
        let directory = tempfile::tempdir().unwrap();
        let pattern = format!("{}/input*::capslock", directory.path().display());
        assert!(!caps_lock_on(&pattern), "no LEDs");
        fs::write(directory.path().join("input3::capslock"), "0\n").unwrap();
        fs::write(directory.path().join("input7::capslock"), "0\n").unwrap();
        assert!(!caps_lock_on(&pattern), "both LEDs off");
        fs::write(directory.path().join("input7::capslock"), "1\n").unwrap();
        assert!(caps_lock_on(&pattern), "one LED on");
    }

    /// The name of a virtual keyboard that only this test makes.
    const TEST_NAME: &str = "kbd-rgb test keyboard";

    /// The lines that `/proc/bus/input/devices` has for the device named `name`.
    fn kernel_lines(name: &str) -> Option<String> {
        let devices = fs::read_to_string("/proc/bus/input/devices").unwrap();
        devices
            .split("\n\n")
            .find(|device| device.contains(&format!("N: Name=\"{name}\"")))
            .map(str::to_owned)
    }

    /// This test creates a real virtual keyboard for a moment, which sends no keys. It needs
    /// permission to open `/dev/uinput`, so it only runs when asked for:
    /// `cargo test -- --ignored`.
    #[test]
    #[ignore = "creates a real input device, which needs access to /dev/uinput"]
    fn the_virtual_keyboard_has_the_features_it_was_given_and_goes_away_when_dropped() {
        let features = Features {
            keys: vec![
                evdev::KeyCode::KEY_A.code(),
                evdev::KeyCode::KEY_COMPOSE.code(),
            ],
            scancodes: vec![evdev::MiscCode::MSC_SCAN.0],
            leds: vec![0, 1, 2],
            ..Features::default()
        };
        let mut keyboard = Uinput::create(TEST_NAME, &features).unwrap();
        let lines = kernel_lines(TEST_NAME).expect("the kernel lists the keyboard");
        // Keys, scancodes, and LEDs, with no autorepeat.
        assert!(
            lines.contains("I: Bus=0003 Vendor=0001 Product=0001 Version=0001"),
            "{lines}"
        );
        assert!(lines.contains("B: EV=20013"), "{lines}");
        assert!(lines.contains("B: MSC=10"), "{lines}");
        assert!(lines.contains("B: LED=7"), "{lines}");
        assert!(keyboard.path().unwrap().starts_with("/dev/input/"));
        // A report on its own presses nothing.
        keyboard
            .write(&[InputEvent::new(evdev::EventType::SYNCHRONIZATION.0, 0, 0)])
            .unwrap();
        drop(keyboard);
        assert_eq!(kernel_lines(TEST_NAME), None);
    }
}
