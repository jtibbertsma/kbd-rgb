//! Helpers for tests.

use std::time::{Duration, Instant};

use crate::cli;
use crate::daemon::config::Config;
use crate::daemon::lights::state::{Changed, Scene, State};
use crate::setting::Setting;
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
        let arguments = shlex::split(line).expect("the quotes in the line match");
        match cli::parse(arguments) {
            Ok(cli::Request::Run(actions)) => self.state.run(&actions, true, self.now),
            other => panic!("{line:?} doesn't run actions: {other:?}"),
        }
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
