//! Helpers for tests.

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::action::{Action, Change};
use crate::cli;
use crate::color::Color;
use crate::daemon::config::Config;
use crate::daemon::lights::Lights;
use crate::daemon::lights::output::Output;
use crate::daemon::lights::state::{Changed, Scene, State};
use crate::daemon::lights::store::Store;
use crate::leds::LedWriter;
use crate::request::Request;
use crate::setting::{Effect, Period, Setting};
use crate::text;
use crate::zone::Zone;

/// A state and a time. Tests run `kbdrgb` command lines on the state, move the time forward, and
/// look at the colors.
pub(crate) struct Rig {
    pub(crate) state: State,
    pub(crate) now: Instant,
}

impl Rig {
    /// A rig with the default settings and a white keyboard.
    pub(crate) fn new() -> Self {
        Rig::with(&Config::default())
    }

    pub(crate) fn with(config: &Config) -> Self {
        let now = Instant::now();
        let white = config.palette.colors[0];
        let state = State::new(
            Scene::filled(&Setting::Color(white)),
            now,
            white,
            config.undo_depth,
            config.caps.clone(),
            config.effects(),
        );
        Rig { state, now }
    }

    /// Runs the arguments of a `kbdrgb` command line as one undo step. For example,
    /// `rig.kbdrgb("color -z left FF0000")`.
    pub(crate) fn kbdrgb(&mut self, line: &str) -> Changed {
        self.state.run(&actions(line), true, self.now)
    }

    /// Moves the time forward, and ends the transitions and timed effects whose time is up.
    pub(crate) fn wait(&mut self, seconds: f64) -> Changed {
        self.now += Duration::from_secs_f64(seconds);
        self.state.expire(self.now)
    }

    pub(crate) fn color(&self, zone: Zone) -> String {
        self.state.color_at(zone, self.now).to_string()
    }

    pub(crate) fn colors(&self) -> [String; 4] {
        Zone::ALL.map(|zone| self.color(zone))
    }

    /// What `zone` is set to, as the color dialog writes it.
    pub(crate) fn setting(&self, zone: Zone) -> String {
        text::describe(&self.state.setting(zone))
    }

    pub(crate) fn settings(&self) -> [String; 4] {
        Zone::ALL.map(|zone| self.setting(zone))
    }
}

/// Lights that write to `leds` and save to `state_file`, with the scene that the file holds.
pub(crate) fn lights(
    config: &Config,
    leds: &FakeLeds,
    state_file: &Path,
    now: Instant,
) -> Lights<FakeLeds> {
    let store = Store::new(state_file);
    let scene = store.load(&config.palette.colors, &config.effects());
    let white = config.palette.colors[0];
    let state = State::new(
        scene,
        now,
        white,
        config.undo_depth,
        config.caps.clone(),
        config.effects(),
    );
    let output = Output::new(leds.clone(), config.frame, config.visible_step);
    Lights::new(state, output, store, now)
}

/// The actions that the arguments of a `kbdrgb` command line ask for.
pub(crate) fn actions(line: &str) -> Vec<Action> {
    let arguments = shlex::split(line).expect("the quotes in the line match");
    match cli::parse(arguments) {
        Ok(cli::Invocation::Ask(Request::Run(actions))) => actions,
        other => panic!("{line:?} doesn't run actions: {other:?}"),
    }
}

/// The setting that `action` gives a zone. An effect without a time gets one second.
pub(crate) fn setting_of(action: &Action) -> Setting {
    match &action.change {
        Change::Color { color, .. } => Setting::Color(*color),
        Change::Effect {
            pattern, period, ..
        } => Setting::Effect(Effect {
            pattern: pattern.clone(),
            period: period.unwrap_or(Period::clamped(Duration::from_secs(1))),
        }),
    }
}

/// An LED writer that records each write, for tests to look at.
#[derive(Debug, Clone, Default)]
pub(crate) struct FakeLeds {
    record: Arc<Mutex<Record>>,
    failing: Arc<AtomicBool>,
}

#[derive(Debug, Default)]
struct Record {
    writes: Vec<(Zone, Color)>,
    last: [Option<Color>; 4],
}

impl FakeLeds {
    /// The writes since the last call, oldest first.
    pub(crate) fn take(&self) -> Vec<(Zone, Color)> {
        std::mem::take(&mut self.record.lock().unwrap().writes)
    }

    /// The last color written to each zone, as text. A zone that was never written gives an empty
    /// string.
    pub(crate) fn colors(&self) -> [String; 4] {
        let last = self.record.lock().unwrap().last;
        last.map(|color| color.map(|color| color.to_string()).unwrap_or_default())
    }

    /// Makes every write fail, or work again.
    pub(crate) fn fail(&self, failing: bool) {
        self.failing.store(failing, Ordering::Relaxed);
    }
}

impl LedWriter for FakeLeds {
    fn write(&mut self, zone: Zone, color: Color) -> io::Result<()> {
        if self.failing.load(Ordering::Relaxed) {
            return Err(io::Error::other("the fake LEDs are set to fail"));
        }
        let mut record = self.record.lock().unwrap();
        record.writes.push((zone, color));
        record.last[zone.index()] = Some(color);
        Ok(())
    }
}
