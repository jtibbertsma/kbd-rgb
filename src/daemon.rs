//! The daemon, which runs in the background as root and controls the keyboard lights on a
//! System76 laptop, through the color files of System76's `system76` driver.
//!
//! To catch the Menu-key combos, the daemon grabs a keyboard: the built-in one by default, or
//! another device named by the `device` setting, such as a key remapper's virtual keyboard. Keys
//! that aren't part of a combo go on to the desktop through the daemon's own virtual keyboard.
//!
//! Since typing on that keyboard passes through the daemon, a hang in the daemon would stall
//! typing. So the main thread, which forwards the keys, never waits on anything slow. It decides
//! what a key means with `menu::Menu`, which only returns what to do, and it hands the slow work
//! to other threads:
//!
//! - The `lights` thread owns the lights. It applies changes, writes colors to the keyboard, and
//!   draws fade frames. The `lights` module has it.
//! - The `commands` thread answers the `kbdrgb` command. The `server` module has it.
//! - A `dialog` thread shows the color dialog and waits for the user to type in it. The `dialog`
//!   module has it.
//!
//! The colors can be changed in three ways: Menu-key combos, the `kbdrgb` command, and the color
//! dialog. Each of them produces an `Action` and sends it to the `lights` thread.

pub(crate) mod config;
pub(crate) mod desktop;
pub(crate) mod device;
pub(crate) mod dialog;
mod journal;
pub(crate) mod lights;
pub(crate) mod menu;
pub(crate) mod problem;
pub(crate) mod server;

use std::io::Write;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::Parser;
use evdev::{EventSummary, EventType, InputEvent, KeyCode, LedCode};
use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

use self::config::{Config, Palette, System};
use self::desktop::{Desktop, Session};
use self::device::{Features, Grabbed, Keyboard, Uinput, VirtualKeyboard};
use self::dialog::DialogHandle;
use self::lights::output::Output;
use self::lights::state::State;
use self::lights::store::Store;
use self::lights::{Command, Lights, LightsHandle};
use self::menu::{Menu, Press};
use self::problem::Problem;
use self::server::{Limits, Socket};
use crate::leds::LedFiles;
use crate::zone::{Target, Zone};

/// Control the keyboard's colors on a System76 laptop. The kbd-rgb service runs this as root.
#[derive(Debug, Parser)]
#[command(name = "kbd-rgb-daemon")]
struct Arguments {
    /// The settings file
    #[arg(long, value_name = "PATH", default_value = config::DEFAULT_PATH)]
    config: PathBuf,
    /// Check the settings file and exit
    #[arg(long)]
    check: bool,
}

/// Runs the daemon until it is told to stop or can't go on.
#[must_use]
pub fn main() -> ExitCode {
    let arguments = Arguments::parse();
    journal::start();
    let desktop: Arc<dyn Desktop> = Arc::new(Session);

    let config = match Config::load(&arguments.config) {
        Ok(config) => config,
        Err(error) => {
            log::error!("{error}");
            if !arguments.check {
                // The file that failed to load may be the one that says where problems go.
                report(
                    &Problem::BadSettings(error.summary()),
                    &System::default(),
                    desktop.as_ref(),
                );
            }
            return ExitCode::from(2);
        }
    };
    if arguments.check {
        let path = arguments.config.display();
        let found = if arguments.config.exists() {
            format!("The settings in {path} work.")
        } else {
            format!("There is no {path}, so the defaults apply.")
        };
        // Nothing can be done if standard output is closed.
        let _ = writeln!(std::io::stdout(), "{found}");
        return ExitCode::SUCCESS;
    }

    let Some(keyboard) = device::find_keyboard(&config.device) else {
        log::error!(
            "keyboard not found: {:?} (systemd will retry)",
            config.device
        );
        let problem = Problem::KeyboardNotFound(config.device.clone());
        report(&problem, &config.system, desktop.as_ref());
        return ExitCode::FAILURE;
    };
    match run(&config, &keyboard, &desktop) {
        Ok(()) => {
            log::info!("stopped");
            ExitCode::SUCCESS
        }
        Err(error) => {
            log::error!("stopped: {error:#}");
            report(
                &Problem::Failed(format!("{error:#}")),
                &config.system,
                desktop.as_ref(),
            );
            ExitCode::FAILURE
        }
    }
}

fn report(problem: &Problem, system: &System, desktop: &dyn Desktop) {
    problem::report(problem, &system.problem_file, system.report_after, desktop);
}

/// Sets everything up, then forwards keys until a signal asks the daemon to stop. Everything is
/// taken down again when this returns, in the reverse order: each value gives back what it holds
/// when it is dropped.
fn run(config: &Config, keyboard_path: &Path, desktop: &Arc<dyn Desktop>) -> anyhow::Result<()> {
    let system = &config.system;
    let now = Instant::now();
    let leds = LedFiles::new(&system.led);
    // From here on, a signal asks the main loop to stop, so that everything below is taken down
    // in order.
    let stop = stop_signals(&[signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT])
        .context("can't watch for signals")?;

    let store = Store::new(&system.state_file);
    let scene = store.load(&config.palette.colors, &config.effects());
    let white = config.palette.colors[0];
    let mut state = State::new(
        scene,
        clock_zero(),
        white,
        config.undo_depth,
        config.caps.clone(),
        config.effects(),
    );
    state.set_caps_lock(device::caps_lock_on(&system.caps_glob), now);
    let output = Output::new(leds.clone(), config.frame, config.visible_step);
    let (lights, _lights_thread) = lights::spawn(Lights::new(state, output, store, now))
        .context("can't start the lights thread")?;

    let socket = Socket::bind(&system.socket, &leds.color_file(Zone::Left))
        .with_context(|| format!("can't create the socket {}", system.socket.display()))?;
    let limits = Limits {
        max_request: system.max_request,
        timeout: system.request_timeout,
    };
    socket
        .serve(lights.clone(), limits)
        .context("can't start the commands thread")?;

    let device = evdev::Device::open(keyboard_path)
        .with_context(|| format!("can't open the keyboard {}", keyboard_path.display()))?;
    let name = device.name().unwrap_or("unnamed").to_owned();
    let virtual_keyboard = Uinput::create(&system.virtual_name, &Features::of(&device))
        .context("can't create the virtual keyboard")?;
    // A key that is held when the daemon starts, such as Enter from the command that started it,
    // gets time to be released. Its release would otherwise come from a different keyboard than
    // its press did, as the desktop sees it.
    thread::sleep(Duration::from_millis(500));
    let keyboard = Grabbed::grab(device)
        .with_context(|| format!("can't grab the keyboard {}", keyboard_path.display()))?;
    let forwarding = virtual_keyboard
        .path()
        .map_or_else(|_| "?".into(), |path| path.display().to_string());
    log::info!(
        "grabbed {} ({name}); forwarding through {forwarding}",
        keyboard_path.display()
    );

    let mut forwarder = Forwarder {
        keyboard,
        virtual_keyboard,
        menu: Menu::new(config.mod_key.0, config.palette.colors),
        lights,
        desktop: Arc::clone(desktop),
        palette: config.palette.clone(),
        dialog: None,
    };
    let recovered = || problem::recovered(&system.problem_file, desktop.as_ref());
    forwarder.run(stop.as_fd(), system.stable_after, recovered)
}

/// The moment that the monotonic clock counts from, which is when the machine started. Effects
/// count from here, so an effect that was saved carries on in step when the daemon restarts.
fn clock_zero() -> Instant {
    // `Instant` is a reading of the same clock, so taking the clock's reading away from the
    // present gives the clock's zero. If either step fails, the daemon's start serves instead,
    // and effects still stay in step with each other.
    let now = Instant::now();
    nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC)
        .ok()
        .and_then(|reading| now.checked_sub(Duration::from(reading)))
        .unwrap_or(now)
}

/// A stream that has something to read once the daemon gets one of `signals`, so that the main
/// loop can wait for a signal the same way it waits for keys.
fn stop_signals(signals: &[i32]) -> std::io::Result<UnixStream> {
    let (stop, signalled) = UnixStream::pair()?;
    for &signal in signals {
        signal_hook::low_level::pipe::register(signal, signalled.try_clone()?)?;
    }
    Ok(stop)
}

/// The main loop and what it works with.
struct Forwarder<K, V> {
    keyboard: K,
    virtual_keyboard: V,
    menu: Menu,
    lights: LightsHandle,
    desktop: Arc<dyn Desktop>,
    palette: Palette,
    /// The color dialog that is open. Storing a new one here closes the old one.
    dialog: Option<DialogHandle>,
}

impl<K: Keyboard, V: VirtualKeyboard> Forwarder<K, V> {
    /// Forwards keys until `stop` has something to read. Once this has run for `stable_after`, it
    /// calls `on_stable`, which is when a problem from an earlier start is taken to be over.
    fn run(
        &mut self,
        stop: BorrowedFd<'_>,
        stable_after: Duration,
        on_stable: impl FnOnce(),
    ) -> anyhow::Result<()> {
        let mut stable = Some((Instant::now() + stable_after, on_stable));
        loop {
            let timeout = match &stable {
                // The extra millisecond makes up for the timeout being rounded down.
                Some((at, _)) => {
                    let wait =
                        at.saturating_duration_since(Instant::now()) + Duration::from_millis(1);
                    PollTimeout::try_from(wait).unwrap_or(PollTimeout::MAX)
                }
                None => PollTimeout::NONE,
            };
            let mut waiting = [self.keyboard.as_fd(), self.virtual_keyboard.as_fd(), stop]
                .map(|fd| PollFd::new(fd, PollFlags::POLLIN));
            match poll(&mut waiting, timeout) {
                Ok(_) => {}
                Err(Errno::EINTR) => continue,
                Err(error) => return Err(error).context("can't wait for key events"),
            }
            // An error on a device also counts, so that reading it reports the error.
            let [keys, leds, stopping] = waiting.map(|fd| fd.any().unwrap_or(true));
            if stopping {
                return Ok(());
            }
            if let Some((_, on_stable)) = stable.take_if(|(at, _)| Instant::now() >= *at) {
                on_stable();
            }
            if keys {
                self.forward_keys(Instant::now())?;
            }
            if leds {
                self.read_leds()?;
            }
        }
    }

    /// Reads the keyboard's events and passes on the ones that aren't part of a Menu combo.
    fn forward_keys(&mut self, now: Instant) -> anyhow::Result<()> {
        let events = self.keyboard.read().context("can't read the keyboard")?;
        let mut forwarded = Vec::with_capacity(events.len());
        for event in events {
            match event.destructure() {
                // A raw scancode comes with each key event, and the desktop only uses the key event.
                EventSummary::Misc(..) => {}
                EventSummary::Key(_, key, value) => {
                    for output in self.menu.key(key, Press::from_value(value), now) {
                        match output {
                            menu::Output::Forward => forwarded.push(event),
                            menu::Output::Tap(key) => forwarded.extend(tap(key)),
                            menu::Output::Lights(command) => self.lights.send(command),
                            menu::Output::OpenDialog(target) => self.open_dialog(target),
                        }
                    }
                }
                _ => forwarded.push(event),
            }
        }
        if !forwarded.is_empty() {
            self.virtual_keyboard
                .write(&forwarded)
                .context("can't write to the virtual keyboard")?;
        }
        Ok(())
    }

    /// Reads what the desktop sent to the virtual keyboard, which is how the daemon learns that
    /// caps lock was turned on or off.
    fn read_leds(&mut self) -> anyhow::Result<()> {
        let events = self
            .virtual_keyboard
            .read()
            .context("can't read the virtual keyboard")?;
        for event in events {
            if let EventSummary::Led(_, LedCode::LED_CAPSL, value) = event.destructure() {
                self.lights.send(Command::CapsLock(value != 0));
            }
        }
        Ok(())
    }

    fn open_dialog(&mut self, target: Target) {
        log::info!("color dialog for {target}");
        let (lights, desktop) = (self.lights.clone(), Arc::clone(&self.desktop));
        // Dropping the old handle first closes the old dialog before the new one opens.
        self.dialog = None;
        self.dialog = match dialog::open(target, lights, desktop, self.palette.clone()) {
            Ok(dialog) => Some(dialog),
            Err(error) => {
                log::warn!("can't start the dialog thread: {error}");
                None
            }
        };
    }
}

/// The events of `key` being pressed and released.
fn tap(key: KeyCode) -> [InputEvent; 4] {
    let key_event = |value| InputEvent::new(EventType::KEY.0, key.code(), value);
    let report = InputEvent::new(EventType::SYNCHRONIZATION.0, 0, 0);
    [key_event(1), report, key_event(0), report]
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::Read;
    use std::os::fd::OwnedFd;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::JoinHandle;

    use super::*;
    use crate::testing::{FakeDesktop, FakeLeds, wait_for};

    /// A fake input device. A test pushes events for the daemon to read, and looks at the events
    /// the daemon wrote. It is backed by a real socket, so that polling it really waits.
    #[derive(Clone)]
    struct FakeDevice {
        shared: Arc<Shared>,
    }

    struct Shared {
        wake: UnixStream,
        woken: UnixStream,
        to_read: Mutex<VecDeque<InputEvent>>,
        written: Mutex<Vec<InputEvent>>,
        gone: AtomicBool,
    }

    impl FakeDevice {
        fn new() -> Self {
            let (wake, woken) = UnixStream::pair().unwrap();
            woken.set_nonblocking(true).unwrap();
            let shared = Shared {
                wake,
                woken,
                to_read: Mutex::default(),
                written: Mutex::default(),
                gone: AtomicBool::new(false),
            };
            FakeDevice {
                shared: Arc::new(shared),
            }
        }

        fn push(&self, events: impl IntoIterator<Item = InputEvent>) {
            self.shared.to_read.lock().unwrap().extend(events);
            (&self.shared.wake).write_all(b"x").unwrap();
        }

        /// Makes the next read fail, as reading a device that was unplugged does.
        fn unplug(&self) {
            self.shared.gone.store(true, Ordering::Relaxed);
            (&self.shared.wake).write_all(b"x").unwrap();
        }

        /// The key events that were written, each as the key's name and the event's value.
        fn written_keys(&self) -> Vec<(String, i32)> {
            let written = self.shared.written.lock().unwrap();
            written
                .iter()
                .filter_map(|event| match event.destructure() {
                    EventSummary::Key(_, key, value) => Some((format!("{key:?}"), value)),
                    _ => None,
                })
                .collect()
        }

        fn written_scancodes(&self) -> usize {
            let written = self.shared.written.lock().unwrap();
            written
                .iter()
                .filter(|event| event.event_type() == EventType::MISC)
                .count()
        }

        fn take_events(&self) -> std::io::Result<Vec<InputEvent>> {
            let mut woken = [0; 64];
            let _ = (&self.shared.woken).read(&mut woken);
            if self.shared.gone.load(Ordering::Relaxed) {
                return Err(std::io::Error::other("device gone"));
            }
            Ok(self.shared.to_read.lock().unwrap().drain(..).collect())
        }
    }

    impl AsFd for FakeDevice {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.shared.woken.as_fd()
        }
    }

    impl Keyboard for FakeDevice {
        fn read(&mut self) -> std::io::Result<Vec<InputEvent>> {
            self.take_events()
        }
    }

    impl VirtualKeyboard for FakeDevice {
        fn write(&mut self, events: &[InputEvent]) -> std::io::Result<()> {
            self.shared
                .written
                .lock()
                .unwrap()
                .extend_from_slice(events);
            Ok(())
        }

        fn read(&mut self) -> std::io::Result<Vec<InputEvent>> {
            self.take_events()
        }
    }

    /// One key event as the kernel sends it: a scancode, the key, and a report.
    fn key(key: KeyCode, value: i32) -> [InputEvent; 3] {
        [
            InputEvent::new(EventType::MISC.0, 4, 99),
            InputEvent::new(EventType::KEY.0, key.code(), value),
            InputEvent::new(EventType::SYNCHRONIZATION.0, 0, 0),
        ]
    }

    fn typed(keys: &[KeyCode]) -> Vec<InputEvent> {
        keys.iter()
            .flat_map(|&k| key(k, 1).into_iter().chain(key(k, 0)))
            .collect()
    }

    fn led(led: LedCode, on: bool) -> InputEvent {
        InputEvent::new(EventType::LED.0, led.0, i32::from(on))
    }

    /// The main loop on a thread of its own, with fake devices, fake LEDs, and a fake desktop.
    struct Fixture {
        keyboard: FakeDevice,
        virtual_keyboard: FakeDevice,
        leds: FakeLeds,
        desktop: Arc<FakeDesktop>,
        stable: Arc<AtomicBool>,
        stop: UnixStream,
        main_loop: Option<JoinHandle<anyhow::Result<()>>>,
        _directory: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Fixture::with(&Config::default(), Duration::from_secs(3600))
        }

        fn with(config: &Config, stable_after: Duration) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let leds = FakeLeds::default();
            let state_file = directory.path().join("state.json");
            let lights = crate::testing::lights(config, leds.clone(), &state_file, Instant::now());
            let (lights, _thread) = lights::spawn(lights).unwrap();
            let (keyboard, virtual_keyboard) = (FakeDevice::new(), FakeDevice::new());
            let desktop = Arc::new(FakeDesktop::default());
            let stable = Arc::new(AtomicBool::new(false));
            let (stop, stopped) = UnixStream::pair().unwrap();
            let mut forwarder = Forwarder {
                keyboard: keyboard.clone(),
                virtual_keyboard: virtual_keyboard.clone(),
                menu: Menu::new(config.mod_key.0, config.palette.colors),
                lights,
                desktop: desktop.clone(),
                palette: config.palette.clone(),
                dialog: None,
            };
            let on_stable = {
                let stable = Arc::clone(&stable);
                move || stable.store(true, Ordering::Relaxed)
            };
            let main_loop = thread::spawn(move || {
                let stopped: OwnedFd = stopped.into();
                forwarder.run(stopped.as_fd(), stable_after, on_stable)
            });
            Fixture {
                keyboard,
                virtual_keyboard,
                leds,
                desktop,
                stable,
                stop,
                main_loop: Some(main_loop),
                _directory: directory,
            }
        }

        fn wait_for_keys(&self, expected: &[(&str, i32)]) {
            let expected: Vec<(String, i32)> = expected
                .iter()
                .map(|&(name, value)| (name.to_owned(), value))
                .collect();
            wait_for(|| self.virtual_keyboard.written_keys().len() >= expected.len());
            assert_eq!(self.virtual_keyboard.written_keys(), expected);
        }

        fn wait_for_colors(&self, expected: [&str; 4]) {
            wait_for(|| self.leds.colors() == expected);
        }

        /// Waits for the main loop to return, and gives what it returned.
        fn finish(&mut self) -> anyhow::Result<()> {
            self.main_loop.take().unwrap().join().unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = self.stop.write_all(b"x");
        }
    }

    use KeyCode as K;

    #[test]
    fn keys_are_passed_on_and_combos_and_scancodes_are_not() {
        let fixture = Fixture::new();
        fixture.keyboard.push(typed(&[K::KEY_A]));
        fixture.keyboard.push(key(K::KEY_COMPOSE, 1));
        fixture.keyboard.push(typed(&[K::KEY_Q, K::KEY_3]));
        fixture.keyboard.push(key(K::KEY_COMPOSE, 0));
        fixture.keyboard.push(typed(&[K::KEY_B]));
        fixture.wait_for_keys(&[("KEY_A", 1), ("KEY_A", 0), ("KEY_B", 1), ("KEY_B", 0)]);
        assert_eq!(fixture.virtual_keyboard.written_scancodes(), 0);
        fixture.wait_for_colors(["FFFF00", "FFFFFF", "FFFFFF", "FFFFFF"]);
    }

    #[test]
    fn a_tap_of_menu_reaches_the_desktop_as_a_press_and_a_release() {
        let fixture = Fixture::new();
        fixture.keyboard.push(
            key(K::KEY_COMPOSE, 1)
                .into_iter()
                .chain(key(K::KEY_COMPOSE, 0)),
        );
        fixture.wait_for_keys(&[("KEY_COMPOSE", 1), ("KEY_COMPOSE", 0)]);
    }

    #[test]
    fn the_caps_lock_led_of_the_virtual_keyboard_turns_the_indicator_on_and_off() {
        let fixture = Fixture::new();
        fixture.keyboard.push(key(K::KEY_COMPOSE, 1));
        fixture.keyboard.push(typed(&[K::KEY_Q, K::KEY_3]));
        fixture.wait_for_colors(["FFFF00", "FFFFFF", "FFFFFF", "FFFFFF"]);
        fixture
            .virtual_keyboard
            .push([led(LedCode::LED_CAPSL, true)]);
        wait_for(|| fixture.leds.colors()[0] == "FF0000");
        // Num lock changing doesn't turn the indicator off.
        fixture
            .virtual_keyboard
            .push([led(LedCode::LED_NUML, false)]);
        fixture
            .virtual_keyboard
            .push([led(LedCode::LED_CAPSL, false)]);
        fixture.wait_for_colors(["FFFF00", "FFFFFF", "FFFFFF", "FFFFFF"]);
    }

    #[test]
    fn the_same_zone_key_twice_opens_the_dialog_and_another_dialog_closes_it() {
        let fixture = Fixture::new();
        fixture.desktop.answer_when_closed();
        fixture.keyboard.push(key(K::KEY_COMPOSE, 1));
        fixture.keyboard.push(typed(&[K::KEY_Q, K::KEY_Q]));
        fixture.desktop.wait_for_entries(1);
        assert!(fixture.desktop.prompts()[0].contains("the left zone"));
        fixture.keyboard.push(typed(&[K::KEY_ENTER, K::KEY_ENTER]));
        fixture.desktop.wait_until_closed();
        fixture.desktop.wait_for_entries(2);
        assert!(fixture.desktop.prompts()[1].contains("the right zone"));
    }

    #[test]
    fn a_signal_stops_the_loop_without_an_error() {
        let mut fixture = Fixture::new();
        fixture.stop.write_all(b"x").unwrap();
        assert!(fixture.finish().is_ok());
    }

    #[test]
    fn the_keyboard_going_away_stops_the_loop_with_an_error() {
        let mut fixture = Fixture::new();
        fixture.keyboard.unplug();
        let error = fixture.finish().unwrap_err();
        assert_eq!(format!("{error:#}"), "can't read the keyboard: device gone");
    }

    #[test]
    fn the_loop_says_when_it_has_run_long_enough_to_be_stable() {
        let fixture = Fixture::with(&Config::default(), Duration::from_millis(50));
        assert!(!fixture.stable.load(Ordering::Relaxed));
        wait_for(|| fixture.stable.load(Ordering::Relaxed));
        // Keys still work afterwards.
        fixture.keyboard.push(typed(&[K::KEY_A]));
        fixture.wait_for_keys(&[("KEY_A", 1), ("KEY_A", 0)]);
    }

    #[test]
    fn a_signal_makes_the_stop_stream_readable() {
        // SIGUSR1 stands in for SIGTERM, which the test program needs for itself.
        let mut stop = stop_signals(&[signal_hook::consts::SIGUSR1]).unwrap();
        stop.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        signal_hook::low_level::raise(signal_hook::consts::SIGUSR1).unwrap();
        let mut byte = [0];
        assert_eq!(stop.read(&mut byte).unwrap(), 1);
    }

    #[test]
    fn the_clocks_zero_is_the_same_moment_whenever_it_is_worked_out() {
        let first = clock_zero();
        thread::sleep(Duration::from_millis(20));
        let second = clock_zero();
        let apart = if first > second {
            first - second
        } else {
            second - first
        };
        assert!(apart < Duration::from_millis(5), "{apart:?} apart");
        assert!(first < Instant::now());
    }

    #[test]
    fn the_daemon_has_two_options() {
        let arguments = Arguments::parse_from(["kbd-rgb-daemon"]);
        assert_eq!(arguments.config, Path::new(config::DEFAULT_PATH));
        assert!(!arguments.check);
        let arguments = Arguments::parse_from(["kbd-rgb-daemon", "--check", "--config", "x.toml"]);
        assert_eq!(arguments.config, Path::new("x.toml"));
        assert!(arguments.check);
    }
}
