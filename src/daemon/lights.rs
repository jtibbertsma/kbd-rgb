//! The keyboard's lights, and the thread that owns them.
//!
//! One thread, named `lights`, owns the [`Lights`]. Every other thread holds a [`LightsHandle`]
//! and sends it messages, so nothing is shared and nothing needs a lock. The thread waits for the
//! next message or for the next fade frame, whichever comes first.

mod fade;
mod frames;
mod history;
pub(crate) mod output;
pub(crate) mod state;
pub(crate) mod store;

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Instant;
use std::{fmt, io};

use self::output::Output;
use self::state::{Changed, State};
use self::store::Store;
use crate::action::Action;
use crate::leds::LedWriter;
use crate::request::ZoneStatus;
use crate::setting::Setting;
use crate::text;
use crate::zone::{PerZone, Zone};

/// A change to the lights that another thread asks for.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Command {
    /// Runs `actions`. With `new_step`, they start a new undo step. Without it, they belong to the
    /// step before them, so that everything in one hold of Menu is undone together.
    Run {
        actions: Vec<Action>,
        new_step: bool,
    },
    Undo,
    Redo,
    /// Caps lock was turned on or off.
    CapsLock(bool),
}

enum Message {
    Command(Command),
    /// Asks what a zone is set to.
    Setting {
        zone: Zone,
        reply: Sender<Setting>,
    },
    Status {
        reply: Sender<PerZone<ZoneStatus>>,
    },
    /// Asks for a reply once every message before this one has been carried out.
    Sync {
        reply: Sender<()>,
    },
}

/// What a thread uses to change the lights and to ask about them. Cloning it gives another thread
/// its own.
#[derive(Debug, Clone)]
pub(crate) struct LightsHandle {
    sender: Sender<Message>,
}

impl LightsHandle {
    /// Asks for `command` and returns at once. The lights thread carries commands out in the
    /// order they were sent.
    pub(crate) fn send(&self, command: Command) {
        if self.sender.send(Message::Command(command)).is_err() {
            log::error!("the lights thread has stopped");
        }
    }

    /// What `zone` is set to, or `None` if the lights thread has stopped.
    pub(crate) fn setting(&self, zone: Zone) -> Option<Setting> {
        self.ask(|reply| Message::Setting { zone, reply })
    }

    pub(crate) fn status(&self) -> Option<PerZone<ZoneStatus>> {
        self.ask(|reply| Message::Status { reply })
    }

    /// Waits until every command sent before this call has been carried out.
    pub(crate) fn sync(&self) {
        self.ask(|reply| Message::Sync { reply });
    }

    fn ask<T>(&self, message: impl FnOnce(Sender<T>) -> Message) -> Option<T> {
        let (reply, answer) = mpsc::channel();
        self.sender.send(message(reply)).ok()?;
        answer.recv().ok()
    }
}

/// The lights. The state decides what color each zone should have, the output writes colors to the
/// keyboard, and the store saves the scene. The output asks the state for colors, and the state
/// never calls the output.
#[derive(Debug)]
pub(crate) struct Lights<W> {
    state: State,
    output: Output<W>,
    store: Store,
}

impl<W: LedWriter> Lights<W> {
    /// Puts the state's colors on the keyboard.
    pub(crate) fn new(state: State, output: Output<W>, store: Store, now: Instant) -> Self {
        let mut lights = Lights {
            state,
            output,
            store,
        };
        lights.draw(&Zone::ALL, now);
        lights
    }

    fn carry_out(&mut self, command: Command, now: Instant) {
        let changed = match command {
            Command::Run { actions, new_step } => {
                for action in &actions {
                    log::info!("{action}");
                }
                self.state.run(&actions, new_step, now)
            }
            Command::Undo => {
                let changed = self.state.undo();
                log::info!("undo: {}", self.scene_text());
                changed
            }
            Command::Redo => {
                let Some(changed) = self.state.redo() else {
                    log::info!("redo: nothing to redo");
                    return;
                };
                log::info!("redo: {}", self.scene_text());
                changed
            }
            Command::CapsLock(on) => self.state.set_caps_lock(on, now),
        };
        self.draw(&changed.zones, now);
        self.save_if_changed(&changed);
    }

    /// Draws a fade frame. It first ends the transitions and timed effects whose time is up, and
    /// their last colors are written right away.
    fn tick(&mut self, now: Instant) {
        let ended = self.state.expire(now);
        for &zone in &ended.zones {
            self.output.show(zone, self.state.color_at(zone, now), now);
        }
        let fading = self.state.fading();
        let state = &self.state;
        self.output
            .frame(&fading, |zone| state.color_at(zone, now), now);
        self.output.schedule(&fading, now);
        if !ended.zones.is_empty() {
            let colors: Vec<String> = ended
                .zones
                .iter()
                .map(|&zone| format!("{zone}={}", self.zone_text(zone)))
                .collect();
            log::info!("ended: {}", colors.join(" "));
        }
        self.save_if_changed(&ended);
    }

    fn draw(&mut self, zones: &[Zone], now: Instant) {
        for &zone in zones {
            self.output.draw(zone, self.state.color_at(zone, now), now);
        }
        self.output.schedule(&self.state.fading(), now);
    }

    fn save_if_changed(&self, changed: &Changed) {
        if !changed.scene {
            return;
        }
        if let Err(error) = self.store.save(self.state.scene()) {
            log::warn!("save failed: {error}");
        }
    }

    fn next_due(&self) -> Option<Instant> {
        self.output.next_due()
    }

    fn status(&self, now: Instant) -> PerZone<ZoneStatus> {
        PerZone::from_fn(|zone| ZoneStatus {
            setting: self.state.setting(zone),
            time_left: self.state.time_left(zone, now),
        })
    }

    fn zone_text(&self, zone: Zone) -> String {
        text::describe(&self.state.scene()[zone])
    }

    fn scene_text(&self) -> impl fmt::Display {
        let zones: Vec<String> = Zone::ALL
            .iter()
            .map(|&zone| format!("{zone}={}", self.zone_text(zone)))
            .collect();
        format!("{} ({})", zones.join(" "), self.state.history())
    }

    /// Carries out the messages from `inbox` and draws fade frames, until every [`LightsHandle`]
    /// is gone.
    fn serve(mut self, inbox: &Receiver<Message>) {
        loop {
            let message = match self.next_due() {
                Some(due) => {
                    match inbox.recv_timeout(due.saturating_duration_since(Instant::now())) {
                        Ok(message) => Some(message),
                        Err(RecvTimeoutError::Timeout) => None,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
                None => match inbox.recv() {
                    Ok(message) => Some(message),
                    Err(_) => return,
                },
            };
            let now = Instant::now();
            match message {
                None => self.tick(now),
                Some(Message::Command(command)) => self.carry_out(command, now),
                Some(Message::Setting { zone, reply }) => answer(&reply, self.state.setting(zone)),
                Some(Message::Status { reply }) => answer(&reply, self.status(now)),
                Some(Message::Sync { reply }) => answer(&reply, ()),
            }
        }
    }
}

/// Sends the answer to a question. Sending fails only if the thread that asked has gone away, and
/// then nobody is waiting for the answer.
fn answer<T>(reply: &Sender<T>, answer: T) {
    let _ = reply.send(answer);
}

/// Starts the `lights` thread, which owns `lights` from here on. The thread ends when the handle
/// and all of its clones have been dropped.
pub(crate) fn spawn<W>(lights: Lights<W>) -> io::Result<(LightsHandle, JoinHandle<()>)>
where
    W: LedWriter + Send + 'static,
{
    let (sender, inbox) = mpsc::channel();
    let thread = thread::Builder::new()
        .name("lights".to_owned())
        .spawn(move || lights.serve(&inbox))?;
    Ok((LightsHandle { sender }, thread))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::Duration;

    use serde_json::{Value, json};

    use super::*;
    use crate::daemon::config::Config;
    use crate::testing::{FakeLeds, actions, lights};
    use crate::zone::Zone::{Center, Left, Right};

    /// Lights that write to fake LEDs and save to a temporary directory.
    struct Fixture {
        lights: Lights<FakeLeds>,
        leds: FakeLeds,
        now: Instant,
        state_file: PathBuf,
        _directory: tempfile::TempDir,
    }

    fn run(line: &str) -> Command {
        Command::Run {
            actions: actions(line),
            new_step: true,
        }
    }

    impl Fixture {
        fn new() -> Self {
            Fixture::with(&Config::default())
        }

        fn with(config: &Config) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let state_file = directory.path().join("state.json");
            let leds = FakeLeds::default();
            let now = Instant::now();
            let lights = lights(config, leds.clone(), &state_file, now);
            Fixture {
                lights,
                leds,
                now,
                state_file,
                _directory: directory,
            }
        }

        fn kbdrgb(&mut self, line: &str) {
            self.lights.carry_out(run(line), self.now);
        }

        /// Draws frames as the thread does, for `seconds`, and returns each write with the time it
        /// happened at, counted from the start of the call.
        fn run_for(&mut self, seconds: f64) -> Vec<(Duration, Zone)> {
            let start = self.now;
            let end = start + Duration::from_secs_f64(seconds);
            let mut writes = Vec::new();
            self.leds.take();
            while let Some(due) = self.lights.next_due().filter(|&due| due < end) {
                self.now = self.now.max(due);
                self.lights.tick(self.now);
                let at = self.now - start;
                writes.extend(self.leds.take().into_iter().map(|(zone, _)| (at, zone)));
            }
            self.now = end;
            writes
        }

        fn saved(&self) -> Value {
            serde_json::from_str(&fs::read_to_string(&self.state_file).unwrap()).unwrap()
        }
    }

    fn writes_to(writes: &[(Duration, Zone)], zone: Zone) -> usize {
        writes
            .iter()
            .filter(|(_, written)| *written == zone)
            .count()
    }

    // Changes

    #[test]
    fn new_lights_put_the_saved_scene_on_the_keyboard_without_saving_it_again() {
        let fixture = Fixture::new();
        assert_eq!(fixture.leds.colors(), ["FFFFFF"; 4]);
        assert!(!fixture.state_file.exists());
    }

    #[test]
    fn a_command_writes_the_zones_it_changed_and_saves_the_scene() {
        let mut fixture = Fixture::new();
        fixture.leds.take();
        fixture.kbdrgb("color -z right 778899");
        assert_eq!(fixture.leds.take(), [(Right, "778899".parse().unwrap())]);
        assert_eq!(fixture.saved()["right"], json!({"Color": "778899"}));
    }

    #[test]
    fn the_scene_comes_back_after_a_restart_with_its_effects_in_step() {
        let mut fixture = Fixture::new();
        fixture.kbdrgb("zones 112233 'AA00AA 00FF00'");
        fixture.now += Duration::from_millis(250);
        let leds = FakeLeds::default();
        let restarted = lights(
            &Config::default(),
            leds.clone(),
            &fixture.state_file,
            fixture.now,
        );
        assert_eq!(leds.colors()[0], "112233");
        assert_eq!(
            restarted.state.setting(Center),
            fixture.lights.state.setting(Center)
        );
    }

    #[test]
    fn the_undo_history_does_not_come_back_after_a_restart() {
        let mut fixture = Fixture::new();
        fixture.kbdrgb("color 111111");
        fixture.kbdrgb("color 444444");
        let leds = FakeLeds::default();
        let mut restarted = lights(
            &Config::default(),
            leds.clone(),
            &fixture.state_file,
            fixture.now,
        );
        assert_eq!(leds.colors(), ["444444"; 4]);
        restarted.carry_out(Command::Undo, fixture.now);
        assert_eq!(leds.colors(), ["FFFFFF"; 4]);
        restarted.carry_out(Command::Redo, fixture.now);
        assert_eq!(leds.colors(), ["444444"; 4]);
    }

    #[test]
    fn undo_and_redo_write_every_zone_and_save() {
        let mut fixture = Fixture::new();
        fixture.kbdrgb("color FF0000");
        fixture.lights.carry_out(Command::Undo, fixture.now);
        assert_eq!(fixture.leds.colors(), ["FFFFFF"; 4]);
        assert_eq!(fixture.saved()["left"], json!({"Color": "FFFFFF"}));
        fixture.lights.carry_out(Command::Redo, fixture.now);
        assert_eq!(fixture.leds.colors(), ["FF0000"; 4]);
        assert_eq!(fixture.saved()["left"], json!({"Color": "FF0000"}));
    }

    #[test]
    fn redo_with_nothing_to_redo_writes_nothing() {
        let mut fixture = Fixture::new();
        fixture.leds.take();
        fixture.lights.carry_out(Command::Redo, fixture.now);
        assert_eq!(fixture.leds.take(), []);
    }

    #[test]
    fn caps_lock_writes_every_zone_and_does_not_save() {
        let mut fixture = Fixture::new();
        fixture
            .lights
            .carry_out(Command::CapsLock(true), fixture.now);
        assert_eq!(
            fixture.leds.colors(),
            ["FF0000", "FFFFFF", "FFFFFF", "FFFFFF"]
        );
        assert!(fixture.lights.next_due().is_some());
        assert!(!fixture.state_file.exists());
        fixture.kbdrgb("color 8800FF");
        fixture
            .lights
            .carry_out(Command::CapsLock(false), fixture.now);
        assert_eq!(fixture.leds.colors(), ["8800FF"; 4]);
        assert_eq!(fixture.lights.next_due(), None);
    }

    #[test]
    fn a_timed_effect_ends_on_its_last_color_and_that_color_is_saved() {
        let mut fixture = Fixture::new();
        fixture.kbdrgb("pulse -z extra -t 1 -d 3 00ff88");
        assert!(
            !fixture.state_file.exists(),
            "a timed effect doesn't change the scene"
        );
        fixture.run_for(3.5);
        let dim = "00140B";
        assert_eq!(fixture.leds.colors()[3], dim);
        assert_eq!(fixture.saved()["extra"], json!({"Color": dim}));
        assert_eq!(fixture.lights.next_due(), None);
    }

    #[test]
    fn status_gives_each_zones_setting_and_the_time_a_timed_effect_has_left() {
        let mut fixture = Fixture::new();
        fixture.kbdrgb("cycle -z left -e ease-in 00FF00 0000FF");
        fixture.kbdrgb("pulse -z center -t 2 -d 30 8800FF");
        fixture.kbdrgb("rainbow -z right");
        fixture.kbdrgb("color -z extra -t 5 FF00AA");
        fixture
            .lights
            .carry_out(Command::CapsLock(true), fixture.now);
        let status = fixture.lights.status(fixture.now);
        let texts = Zone::ALL.map(|zone| text::describe(&status[zone].setting));
        assert_eq!(
            texts,
            [
                "00FF00 0000FF 1 ease-in",
                "pulse 8800FF 2",
                "rainbow 0.6",
                "FF00AA"
            ]
        );
        assert_eq!(status[Center].time_left, Some(Duration::from_secs(30)));
        assert_eq!(status[Left].time_left, None);
    }

    // Frames

    #[test]
    fn nothing_is_due_while_nothing_is_fading() {
        let mut fixture = Fixture::new();
        assert_eq!(fixture.lights.next_due(), None);
        fixture.kbdrgb("cycle AA00AA 00FF00");
        assert!(fixture.lights.next_due().is_some());
        fixture.kbdrgb("color 445566");
        assert_eq!(fixture.lights.next_due(), None);
        assert_eq!(fixture.run_for(3.0), []);
    }

    #[test]
    fn a_tick_with_nothing_fading_writes_nothing() {
        let mut fixture = Fixture::new();
        fixture.kbdrgb("color 0000FF");
        fixture.leds.take();
        fixture.lights.tick(fixture.now + Duration::from_secs(1000));
        assert_eq!(fixture.leds.take(), []);
        assert_eq!(fixture.lights.next_due(), None);
    }

    #[test]
    fn four_fading_zones_share_30_writes_a_second() {
        let mut fixture = Fixture::new();
        fixture.kbdrgb("rainbow");
        let writes = fixture.run_for(10.0);
        assert!(
            (295..=305).contains(&writes.len()),
            "{} writes in 10 seconds",
            writes.len()
        );
        let closest = writes
            .windows(2)
            .map(|pair| pair[1].0.saturating_sub(pair[0].0))
            .min()
            .unwrap();
        assert!(
            closest >= Duration::from_nanos(1_000_000_000 / 30),
            "two writes {closest:?} apart"
        );
        let counts = Zone::ALL.map(|zone| writes_to(&writes, zone));
        assert!(
            counts.iter().max().unwrap() - counts.iter().min().unwrap() <= 1,
            "{counts:?}"
        );
        for turn in writes.windows(4) {
            let mut zones: Vec<usize> = turn.iter().map(|(_, zone)| zone.index()).collect();
            zones.sort_unstable();
            assert_eq!(zones, [0, 1, 2, 3], "the zones take turns");
        }
    }

    #[test]
    fn up_to_three_fading_zones_are_each_drawn_once_per_frame() {
        for count in 1..=3 {
            let mut fixture = Fixture::new();
            for zone in &Zone::ALL[..count] {
                fixture.kbdrgb(&format!("cycle -z {zone} AA00AA 00FF00"));
            }
            let writes = fixture.run_for(10.0);
            for zone in Zone::ALL {
                let expected = if zone.index() < count {
                    90..=110
                } else {
                    0..=0
                };
                let writes = writes_to(&writes, zone);
                assert!(
                    expected.contains(&writes),
                    "{count} fading: {writes} writes to {zone}"
                );
            }
        }
    }

    #[test]
    fn after_a_stall_frames_carry_on_from_now() {
        let mut fixture = Fixture::new();
        fixture.kbdrgb("cycle AA00AA 00FF00");
        fixture.run_for(1.0);
        fixture.now += Duration::from_secs(1000);
        fixture.lights.tick(fixture.now);
        let due = fixture.lights.next_due().unwrap();
        assert!(due + Duration::from_millis(1) > fixture.now);
        assert!(due <= fixture.now + Duration::from_nanos(1_000_000_000 / 30));
    }

    #[test]
    fn a_slow_fade_skips_frames_that_barely_change() {
        let mut fixture = Fixture::new();
        fixture.kbdrgb("cycle -z left -t 20 -e ease-in-out 00FF00 0000FF");
        let writes = fixture.run_for(40.0);
        assert!(writes.len() < 200, "{} writes in 40 seconds", writes.len());
        assert!(writes.len() > 20, "{} writes in 40 seconds", writes.len());
    }

    #[test]
    fn a_fade_that_skipped_frames_still_ends_exactly_on_its_color() {
        let mut fixture = Fixture::new();
        fixture.kbdrgb("color -z left -t 20 000000");
        fixture.run_for(25.0);
        assert_eq!(fixture.leds.colors()[0], "000000");
        assert_eq!(fixture.lights.next_due(), None);
    }

    #[test]
    fn a_new_color_stops_the_frames_of_the_effect_it_replaced() {
        let mut fixture = Fixture::new();
        fixture.kbdrgb("cycle -z center AA00AA 00FF00");
        fixture.run_for(0.55);
        fixture.kbdrgb("color 445566");
        assert_eq!(fixture.run_for(3.0), []);
    }

    // The thread

    fn spawned(config: &Config) -> (LightsHandle, JoinHandle<()>, FakeLeds, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let leds = FakeLeds::default();
        let lights = lights(
            config,
            leds.clone(),
            &directory.path().join("state.json"),
            Instant::now(),
        );
        let (handle, thread) = spawn(lights).unwrap();
        (handle, thread, leds, directory)
    }

    #[test]
    fn the_thread_carries_out_commands_in_the_order_they_were_sent() {
        let (handle, thread, leds, _directory) = spawned(&Config::default());
        handle.send(run("color FF0000"));
        handle.send(run("color -z left 00FF00"));
        handle.send(Command::Undo);
        handle.sync();
        assert_eq!(leds.colors(), ["FF0000"; 4]);
        drop(handle);
        thread.join().unwrap();
    }

    #[test]
    fn the_thread_answers_questions_about_the_lights() {
        let (handle, _thread, _leds, _directory) = spawned(&Config::default());
        handle.send(run("pulse -z right -d 30 00FFAA"));
        let setting = handle.setting(Right).unwrap();
        assert_eq!(text::describe(&setting), "pulse 00FFAA 0.5");
        let status = handle.status().unwrap();
        assert_eq!(status[Right].setting, setting);
        assert!(status[Right].time_left.unwrap() > Duration::from_secs(29));
        assert_eq!(status[Left].time_left, None);
    }

    #[test]
    fn the_thread_draws_fade_frames_as_time_passes() {
        let config = Config {
            frame: Duration::from_millis(5),
            ..Config::default()
        };
        let (handle, _thread, leds, _directory) = spawned(&config);
        handle.send(run("cycle -z left -t 0.1 FF0000 0000FF"));
        handle.sync();
        leds.take();
        thread::sleep(Duration::from_millis(200));
        let frames = leds.take();
        assert!(frames.len() >= 5, "{} frames in 200 ms", frames.len());
        assert!(frames.iter().all(|(zone, _)| *zone == Left));
    }

    #[test]
    fn the_thread_ends_when_every_handle_is_gone() {
        let (handle, thread, _leds, _directory) = spawned(&Config::default());
        let clone = handle.clone();
        drop(handle);
        assert!(!thread.is_finished());
        drop(clone);
        thread.join().unwrap();
    }

    #[test]
    fn a_handle_whose_thread_has_stopped_gives_no_answers() {
        let (sender, inbox) = mpsc::channel();
        drop(inbox);
        let handle = LightsHandle { sender };
        handle.send(Command::Undo);
        assert_eq!(handle.setting(Left), None);
        assert_eq!(handle.status(), None);
    }
}
