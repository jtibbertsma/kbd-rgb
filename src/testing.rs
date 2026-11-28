//! Helpers for tests.

use std::collections::VecDeque;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::action::{Action, Change};
use crate::cli;
use crate::color::Color;
use crate::daemon::config::Config;
use crate::daemon::desktop::{Answer, Desktop, Window};
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
pub(crate) fn lights<W: LedWriter>(
    config: &Config,
    leds: W,
    state_file: &Path,
    now: Instant,
) -> Lights<W> {
    let store = Store::new(state_file);
    let white = config.palette.colors[0];
    let scene = store.load(white);
    let state = State::new(
        scene,
        now,
        white,
        config.undo_depth,
        config.caps.clone(),
        config.effects(),
    );
    let output = Output::new(leds, config.frame, config.visible_step);
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

/// A desktop whose boxes are answered from a script, and which records what it was asked to show.
#[derive(Debug, Default)]
pub(crate) struct FakeDesktop {
    state: Mutex<FakeDesktopState>,
}

#[derive(Debug, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is a separate switch that a test sets"
)]
struct FakeDesktopState {
    answers: VecDeque<Answer>,
    answer_when_closed: bool,
    errors_fail: bool,
    notifications_hidden: bool,
    prompts: Vec<String>,
    entries: Vec<String>,
    errors: Vec<String>,
    notifications: Vec<(String, String)>,
    saw_window_close: bool,
}

impl FakeDesktop {
    /// Adds the answer to the next box that asks for text. A box with no answer left is cancelled.
    pub(crate) fn answer(&self, answer: Answer) {
        self.state.lock().unwrap().answers.push_back(answer);
    }

    /// Makes boxes that ask for text stay open until their window is closed.
    pub(crate) fn answer_when_closed(&self) {
        self.state.lock().unwrap().answer_when_closed = true;
    }

    /// Makes error boxes close without the user closing them.
    pub(crate) fn fail_errors(&self) {
        self.state.lock().unwrap().errors_fail = true;
    }

    /// Makes notifications fail to show, as they do when nobody is logged in, or show again.
    pub(crate) fn hide_notifications(&self, hidden: bool) {
        self.state.lock().unwrap().notifications_hidden = hidden;
    }

    /// The prompt of each box that asked for text.
    pub(crate) fn prompts(&self) -> Vec<String> {
        self.state.lock().unwrap().prompts.clone()
    }

    /// The text that each box that asked for text started with.
    pub(crate) fn entries(&self) -> Vec<String> {
        self.state.lock().unwrap().entries.clone()
    }

    pub(crate) fn errors(&self) -> Vec<String> {
        self.state.lock().unwrap().errors.clone()
    }

    /// The notifications that were shown, each as its summary and its body.
    pub(crate) fn notifications(&self) -> Vec<(String, String)> {
        self.state.lock().unwrap().notifications.clone()
    }

    /// Waits until `count` boxes have asked for text.
    pub(crate) fn wait_for_entries(&self, count: usize) {
        wait_for(|| self.state.lock().unwrap().entries.len() >= count);
    }

    /// Waits until a box that was told to stay open has seen its window close.
    pub(crate) fn wait_until_closed(&self) {
        wait_for(|| self.state.lock().unwrap().saw_window_close);
    }
}

impl Desktop for FakeDesktop {
    fn notify(&self, summary: &str, body: &str) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.notifications_hidden {
            return false;
        }
        state
            .notifications
            .push((summary.to_owned(), body.to_owned()));
        true
    }

    fn ask(&self, prompt: &str, initial: &str, window: &Window) -> Answer {
        let stay_open = {
            let mut state = self.state.lock().unwrap();
            state.prompts.push(prompt.to_owned());
            state.entries.push(initial.to_owned());
            state.answer_when_closed
        };
        if stay_open {
            wait_for(|| window.is_closed());
            self.state.lock().unwrap().saw_window_close = true;
            return Answer::Failed;
        }
        self.state
            .lock()
            .unwrap()
            .answers
            .pop_front()
            .unwrap_or(Answer::Cancelled)
    }

    fn show_error(&self, message: &str, _window: &Window) -> bool {
        let mut state = self.state.lock().unwrap();
        state.errors.push(message.to_owned());
        !state.errors_fail
    }
}

/// Waits until `done` returns true, and panics if that takes longer than 5 seconds.
pub(crate) fn wait_for(done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done() {
        assert!(
            Instant::now() < deadline,
            "waited 5 seconds for something that didn't happen"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}
