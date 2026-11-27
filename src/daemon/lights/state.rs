//! What each zone is set to, and the color it should have at a given time.

use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

use super::fade::{Transition, caps_color, effect_color};
use super::history::History;
use crate::action::{Action, Change};
use crate::color::Color;
use crate::daemon::config::{CapsLock, Effects};
use crate::setting::{Effect, Pattern, Period, Setting};
use crate::zone::{PerZone, Zone};

/// What each zone is set to. Saving, undo and redo all work on scenes. Transitions and timed
/// effects aren't part of one, so they're never saved.
pub(crate) type Scene = PerZone<Setting>;

/// Something that covers a zone's setting for a while and ends on its own.
#[derive(Debug, Clone, PartialEq)]
enum Overlay {
    Transition(Transition),
    Timed { effect: Effect, until: Instant },
}

impl Overlay {
    fn end(&self) -> Instant {
        match self {
            Overlay::Transition(transition) => transition.end(),
            Overlay::Timed { until, .. } => *until,
        }
    }
}

/// What a change to the state affected, which is what the caller has to act on.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Changed {
    /// The zones whose color may be different now.
    pub(crate) zones: Vec<Zone>,
    /// Whether the scene is different, so that the state file needs saving.
    pub(crate) scene: bool,
}

/// Everything that decides the keyboard's colors: the scene, the overlays on top of it, caps lock
/// over everything while it's on, and the undo history. It writes no files and reads no clock.
/// Every method that depends on the time takes the time as an argument.
#[derive(Debug)]
pub(crate) struct State {
    caps_settings: CapsLock,
    effects: Effects,
    /// The color of every zone when there is nothing left to undo.
    white: Color,
    /// The moment that every effect counts from.
    epoch: Instant,
    scene: Scene,
    overlays: PerZone<Option<Overlay>>,
    /// When caps lock was turned on, if it's on.
    caps_since: Option<Instant>,
    history: History,
}

impl State {
    pub(crate) fn new(
        scene: Scene,
        epoch: Instant,
        white: Color,
        undo_depth: NonZeroUsize,
        caps_settings: CapsLock,
        effects: Effects,
    ) -> Self {
        State {
            caps_settings,
            effects,
            white,
            epoch,
            scene,
            overlays: PerZone::from_fn(|_| None),
            caps_since: None,
            history: History::new(undo_depth),
        }
    }

    // What each zone has

    pub(crate) fn scene(&self) -> &Scene {
        &self.scene
    }

    pub(crate) fn effects(&self) -> &Effects {
        &self.effects
    }

    /// The color that `zone` should have at `now`.
    pub(crate) fn color_at(&self, zone: Zone, now: Instant) -> Color {
        match (self.caps_since, zone) {
            (Some(since), Zone::Left) => {
                caps_color(&self.caps_settings, now.saturating_duration_since(since))
            }
            (Some(_), _) => self.caps_settings.other,
            (None, _) => self.color_below_caps(zone, now),
        }
    }

    fn color_below_caps(&self, zone: Zone, now: Instant) -> Color {
        match &self.overlays[zone] {
            Some(Overlay::Transition(transition)) => transition.color_at(now),
            Some(Overlay::Timed { effect, .. }) => self.effect_color(effect, zone, now),
            None => self.scene_color_at(zone, now),
        }
    }

    /// The color that `zone`'s setting gives at `now`, ignoring overlays and caps lock.
    pub(crate) fn scene_color_at(&self, zone: Zone, now: Instant) -> Color {
        match &self.scene[zone] {
            Setting::Color(color) => *color,
            Setting::Effect(effect) => self.effect_color(effect, zone, now),
        }
    }

    fn effect_color(&self, effect: &Effect, zone: Zone, at: Instant) -> Color {
        effect_color(
            effect,
            zone,
            at.saturating_duration_since(self.epoch),
            &self.effects,
        )
    }

    /// The zones whose color changes as time passes.
    pub(crate) fn fading(&self) -> Vec<Zone> {
        if self.caps_since.is_some() {
            return vec![Zone::Left];
        }
        let fades = |&zone: &Zone| {
            self.overlays[zone].is_some() || matches!(self.scene[zone], Setting::Effect(_))
        };
        Zone::ALL.into_iter().filter(fades).collect()
    }

    /// What `zone` is set to now. For a zone in a transition, it's the color that the transition
    /// ends on. For a zone with a timed effect, it's that effect.
    pub(crate) fn setting(&self, zone: Zone) -> Setting {
        match &self.overlays[zone] {
            Some(Overlay::Timed { effect, .. }) => Setting::Effect(effect.clone()),
            _ => self.scene[zone].clone(),
        }
    }

    /// How long `zone`'s timed effect has left, if it has one.
    pub(crate) fn time_left(&self, zone: Zone, now: Instant) -> Option<Duration> {
        match &self.overlays[zone] {
            Some(Overlay::Timed { until, .. }) => Some(until.saturating_duration_since(now)),
            _ => None,
        }
    }

    pub(crate) fn history(&self) -> &impl std::fmt::Display {
        &self.history
    }

    // Changes

    /// Runs `actions`. With `new_step`, the scene as it is before them is saved for undo. Without
    /// it, they belong to the undo step of the actions before them.
    pub(crate) fn run(&mut self, actions: &[Action], new_step: bool, now: Instant) -> Changed {
        if new_step {
            self.history.remember(&self.scene);
        }
        let mut changed = Changed::default();
        for action in actions {
            for zone in action.target.zones() {
                changed.scene |= self.apply(zone, &action.change, now);
                if !changed.zones.contains(&zone) {
                    changed.zones.push(zone);
                }
            }
        }
        changed
    }

    /// Applies `change` to `zone`, and returns whether that changed the scene.
    fn apply(&mut self, zone: Zone, change: &Change, now: Instant) -> bool {
        match change {
            Change::Color { color, fade, curve } => {
                let from = self.color_below_caps(zone, now);
                self.scene[zone] = Setting::Color(*color);
                self.overlays[zone] = (!fade.get().is_zero()).then(|| {
                    Overlay::Transition(Transition {
                        from,
                        to: *color,
                        start: now,
                        length: fade.get(),
                        curve: *curve,
                    })
                });
                true
            }
            Change::Effect {
                pattern,
                period,
                time_limit,
            } => {
                let period = period.unwrap_or_else(|| self.default_period(pattern));
                let effect = Effect {
                    pattern: pattern.clone(),
                    period,
                };
                if let Some(time_limit) = time_limit {
                    let until = now + time_limit.get();
                    self.overlays[zone] = Some(Overlay::Timed { effect, until });
                    false
                } else {
                    self.scene[zone] = Setting::Effect(effect);
                    self.overlays[zone] = None;
                    true
                }
            }
        }
    }

    fn default_period(&self, pattern: &Pattern) -> Period {
        match pattern {
            Pattern::Cycle(_) => self.effects.cycle.delay,
            Pattern::Rainbow { .. } => self.effects.rainbow.delay,
            Pattern::Pulse { .. } => self.effects.pulse.delay,
        }
    }

    /// Ends the transitions and timed effects whose time is up at `now`. The zone of a timed
    /// effect is set to the color the effect had when its time was up.
    pub(crate) fn expire(&mut self, now: Instant) -> Changed {
        let mut ended = Vec::new();
        for zone in Zone::ALL {
            let Some(overlay) = &self.overlays[zone] else {
                continue;
            };
            if now < overlay.end() {
                continue;
            }
            if let Overlay::Timed { effect, until } = overlay {
                self.scene[zone] = Setting::Color(self.effect_color(effect, zone, *until));
            }
            self.overlays[zone] = None;
            ended.push(zone);
        }
        Changed {
            scene: !ended.is_empty(),
            zones: ended,
        }
    }

    pub(crate) fn set_caps_lock(&mut self, on: bool, now: Instant) -> Changed {
        if on == self.caps_since.is_some() {
            return Changed::default();
        }
        self.caps_since = on.then_some(now);
        Changed {
            zones: Zone::ALL.to_vec(),
            scene: false,
        }
    }

    // Undo and redo

    /// Goes back to the scene before the last undo step. With nothing left to undo, the whole
    /// keyboard turns white.
    pub(crate) fn undo(&mut self) -> Changed {
        let oldest = Scene::filled(&Setting::Color(self.white));
        let scene = self.history.undo(&self.scene, oldest);
        self.restore(scene)
    }

    /// Goes forward again to the scene that the last undo left, or returns `None` when there is
    /// nothing to redo.
    pub(crate) fn redo(&mut self) -> Option<Changed> {
        let scene = self.history.redo(&self.scene)?;
        Some(self.restore(scene))
    }

    fn restore(&mut self, scene: Scene) -> Changed {
        self.scene = scene;
        self.overlays = PerZone::from_fn(|_| None);
        Changed {
            zones: Zone::ALL.to_vec(),
            scene: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::config::Config;
    use crate::testing::Rig;
    use crate::zone::Zone::{Center, Extra, Left, Right};

    fn color(text: &str) -> Color {
        text.parse().unwrap()
    }

    fn mix(from: &str, to: &str, fraction: f64) -> String {
        color(from).mix(color(to), fraction).to_string()
    }

    // Colors and transitions

    #[test]
    fn a_new_state_has_the_scene_it_was_given() {
        assert_eq!(Rig::new().colors(), ["FFFFFF"; 4]);
    }

    #[test]
    fn a_color_is_set_on_the_target_and_nowhere_else() {
        let mut rig = Rig::new();
        let changed = rig.kbdrgb("color -z left FF0000");
        assert_eq!(rig.colors(), ["FF0000", "FFFFFF", "FFFFFF", "FFFFFF"]);
        assert_eq!(
            changed,
            Changed {
                zones: vec![Left],
                scene: true
            }
        );
    }

    #[test]
    fn zones_sets_all_four_zones_as_one_change() {
        let mut rig = Rig::new();
        let changed = rig.kbdrgb("zones 010101 020202");
        assert_eq!(rig.colors(), ["010101", "020202", "FFFFFF", "FFFFFF"]);
        assert_eq!(changed.zones, Zone::ALL);
    }

    #[test]
    fn a_transition_fades_from_the_current_color_over_its_time() {
        let mut rig = Rig::new();
        rig.kbdrgb("color 000000");
        rig.kbdrgb("color -z left -t 2 FFFFFF");
        assert_eq!(rig.colors(), ["000000"; 4]);
        rig.wait(1.0);
        assert_eq!(rig.color(Left), "808080");
        rig.wait(1.05);
        assert_eq!(rig.color(Left), "FFFFFF");
        assert_eq!(rig.state.fading(), []);
    }

    #[test]
    fn the_new_color_is_the_zones_setting_as_soon_as_the_transition_starts() {
        let mut rig = Rig::new();
        rig.kbdrgb("color 000000");
        let changed = rig.kbdrgb("color -z left -t 2 FFFFFF");
        assert!(changed.scene);
        assert_eq!(rig.setting(Left), "FFFFFF");
        assert_eq!(rig.state.scene()[Left], Setting::Color(Color::WHITE));
    }

    #[test]
    fn zones_fade_with_their_own_times() {
        let mut rig = Rig::new();
        rig.kbdrgb("color 000000");
        rig.kbdrgb("color -z left -t 2 FFFFFF");
        rig.wait(1.0);
        rig.kbdrgb("color -z right -t 0.5 FF0000");
        rig.wait(0.5);
        assert_eq!(rig.color(Left), "BFBFBF");
        assert_eq!(rig.color(Right), "FF0000");
    }

    #[test]
    fn a_transition_out_of_an_effect_starts_from_the_effects_current_color() {
        let mut rig = Rig::new();
        rig.kbdrgb("cycle -z center 0000FF 00FF00");
        rig.wait(0.3);
        let frame = rig.color(Center);
        rig.kbdrgb("color -z center -t 1 FF0000");
        assert_eq!(rig.color(Center), frame);
        rig.wait(1.05);
        assert_eq!(rig.color(Center), "FF0000");
        assert_eq!(rig.state.fading(), []);
    }

    #[test]
    fn a_color_set_at_once_cuts_a_transition_short() {
        let mut rig = Rig::new();
        rig.kbdrgb("color -z left -t 1 00FF00");
        rig.kbdrgb("color -z left 0000FF");
        assert_eq!(rig.color(Left), "0000FF");
        assert_eq!(rig.state.fading(), []);
    }

    #[test]
    fn a_transition_follows_its_curve() {
        let mut rig = Rig::new();
        rig.kbdrgb("color 000000");
        rig.kbdrgb("color -z left -t 2 -e ease-in FFFFFF");
        rig.wait(1.0);
        assert_eq!(
            rig.color(Left),
            mix("000000", "FFFFFF", crate::ease::Ease::EASE_IN.at(0.5))
        );
        rig.wait(1.05);
        assert_eq!(rig.color(Left), "FFFFFF");
    }

    // Effects

    #[test]
    fn a_cycle_fades_between_its_colors_on_its_zone() {
        let mut rig = Rig::new();
        rig.kbdrgb("color 8800FF");
        rig.kbdrgb("cycle -z center AA00AA 00FF00");
        assert_eq!(rig.color(Center), "AA00AA");
        assert_eq!(rig.state.fading(), [Center]);
        rig.wait(0.5);
        assert_eq!(rig.color(Center), mix("AA00AA", "00FF00", 0.5));
        rig.wait(0.5);
        assert_eq!(rig.color(Center), "00FF00");
        rig.wait(1.0);
        assert_eq!(rig.color(Center), "AA00AA");
        assert_eq!(rig.color(Left), "8800FF");
    }

    #[test]
    fn a_cycle_started_later_is_in_step_with_an_equal_one() {
        let mut rig = Rig::new();
        rig.kbdrgb("cycle -z center AA00AA 00FF00");
        rig.wait(2.37);
        rig.kbdrgb("cycle -z right AA00AA 00FF00");
        for _ in 0..3 {
            assert_eq!(rig.color(Right), rig.color(Center));
            rig.wait(0.7);
        }
    }

    #[test]
    fn cycles_with_different_times_go_at_their_own_pace() {
        let mut rig = Rig::new();
        rig.kbdrgb("cycle -z center AA00AA 00FF00");
        rig.kbdrgb("cycle -z left -t 3 AA00AA 00FF00");
        rig.wait(5.3);
        assert_eq!(rig.color(Center), mix("00FF00", "AA00AA", 0.3));
        assert_eq!(rig.color(Left), mix("00FF00", "AA00AA", 2.3 / 3.0));
    }

    #[test]
    fn a_zones_new_color_ends_only_that_zones_effect() {
        let mut rig = Rig::new();
        rig.kbdrgb("cycle AA00AA 00FF00");
        rig.kbdrgb("color -z center 0000FF");
        assert_eq!(rig.color(Center), "0000FF");
        assert_eq!(rig.state.fading(), [Left, Right, Extra]);
        rig.kbdrgb("color FFFFFF");
        assert_eq!(rig.state.fading(), []);
        assert_eq!(rig.colors(), ["FFFFFF"; 4]);
    }

    #[test]
    fn a_rainbow_on_one_zone_has_that_zones_place_among_the_colors() {
        let mut rig = Rig::new();
        rig.kbdrgb("rainbow");
        rig.wait(0.2);
        let all = rig.colors();
        rig.kbdrgb("color FFFFFF");
        rig.kbdrgb("rainbow -z center");
        assert_eq!(rig.color(Center), all[1]);
        assert_eq!(rig.state.fading(), [Center]);
    }

    #[test]
    fn an_effect_without_a_time_uses_the_default_from_the_settings() {
        let mut rig = Rig::new();
        rig.kbdrgb("zones 'AA00AA 00FF00' 'pulse 00FFAA' rainbow 123456");
        assert_eq!(
            rig.settings(),
            [
                "AA00AA 00FF00 1",
                "pulse 00FFAA 0.5",
                "rainbow 0.6",
                "123456"
            ]
        );
    }

    #[test]
    fn a_setting_prints_as_the_dialog_reads_it() {
        let mut rig = Rig::new();
        for (command, text) in [
            ("cycle -z left -t 2 AA00AA 00FF00", "AA00AA 00FF00 2"),
            ("rainbow -z left -t 0.3", "rainbow 0.3"),
            (
                "cycle -z left -t 2 -e ease-in AA00AA 00FF00",
                "AA00AA 00FF00 2 ease-in",
            ),
            (
                "pulse -z left -e 0.4,0,0.2,1 00FFAA",
                "pulse 00FFAA 0.5 0.4,0,0.2,1",
            ),
            (
                "cycle -z left FF0000 FFFF00:ease-in 0000FF:0.9,0,0.1,1",
                "FF0000 FFFF00:ease-in 0000FF:0.9,0,0.1,1 1",
            ),
            (
                "cycle -z left -e ease-out FF0000:ease-out 0000FF",
                "FF0000 0000FF 1 ease-out",
            ),
            ("color -z left -t 3 112233", "112233"),
        ] {
            rig.kbdrgb(command);
            assert_eq!(rig.setting(Left), text, "after {command}");
        }
    }

    // Timed effects

    #[test]
    fn a_timed_effect_runs_on_top_of_the_zones_setting() {
        let mut rig = Rig::new();
        let changed = rig.kbdrgb("pulse -z extra -t 1 -d 3 00ff88");
        assert!(!changed.scene);
        assert_eq!(rig.state.scene()[Extra], Setting::Color(Color::WHITE));
        assert_eq!(rig.setting(Extra), "pulse 00FF88 1");
        assert_eq!(
            rig.state.time_left(Extra, rig.now),
            Some(Duration::from_secs(3))
        );
        assert_eq!(rig.state.time_left(Left, rig.now), None);
        rig.wait(0.5);
        let dim = mix("000000", "00FF88", 0.08);
        assert_eq!(rig.color(Extra), mix("00FF88", &dim, 0.5));
        assert_eq!(rig.color(Left), "FFFFFF");
    }

    #[test]
    fn a_timed_effect_ends_on_the_color_it_had_when_its_time_was_up() {
        let mut rig = Rig::new();
        rig.kbdrgb("pulse -z extra -t 1 -d 3 00ff88");
        rig.wait(2.9);
        assert_eq!(rig.state.fading(), [Extra]);
        let ended = rig.wait(0.3);
        assert_eq!(
            ended,
            Changed {
                zones: vec![Extra],
                scene: true
            }
        );
        assert_eq!(rig.state.fading(), []);
        // After 3 periods, a pulse is on its dim color.
        let dim = mix("000000", "00FF88", 0.08);
        assert_eq!(rig.color(Extra), dim);
        assert_eq!(rig.state.scene()[Extra], Setting::Color(color(&dim)));
    }

    #[test]
    fn an_endless_effect_started_on_top_of_a_timed_one_keeps_going() {
        let mut rig = Rig::new();
        rig.kbdrgb("rainbow -d 2");
        rig.wait(1.0);
        rig.kbdrgb("cycle -z center AA00AA 00FF00");
        rig.wait(1.5);
        assert_eq!(rig.state.fading(), [Center]);
    }

    // Caps lock

    fn caps_rig() -> Rig {
        let mut config = Config::default();
        config.caps.colors = vec![color("FF0000"), color("FFFFFF")];
        config.caps.delay = Duration::from_millis(700);
        config.caps.ease = crate::ease::Ease::LINEAR;
        Rig::with(&config)
    }

    #[test]
    fn caps_lock_fades_the_left_zone_and_turns_the_others_white() {
        let mut rig = caps_rig();
        rig.kbdrgb("color 8800FF");
        let changed = rig.state.set_caps_lock(true, rig.now);
        assert_eq!(
            changed,
            Changed {
                zones: Zone::ALL.to_vec(),
                scene: false
            }
        );
        assert_eq!(rig.colors(), ["FF0000", "FFFFFF", "FFFFFF", "FFFFFF"]);
        assert_eq!(rig.state.fading(), [Left]);
        rig.wait(0.35);
        assert_eq!(rig.color(Left), mix("FF0000", "FFFFFF", 0.5));
        rig.wait(0.35);
        assert_eq!(rig.color(Left), "FFFFFF");
        rig.wait(0.7);
        assert_eq!(rig.color(Left), "FF0000");
    }

    #[test]
    fn colors_set_while_caps_lock_is_on_wait_for_it_to_turn_off() {
        let mut rig = caps_rig();
        rig.state.set_caps_lock(true, rig.now);
        rig.kbdrgb("color -z center 00FF00");
        rig.kbdrgb("color -z left 0000FF");
        assert_eq!(rig.color(Center), "FFFFFF");
        assert_eq!(rig.setting(Center), "00FF00");
        rig.wait(0.35);
        assert_eq!(rig.color(Left), mix("FF0000", "FFFFFF", 0.5));
        rig.state.set_caps_lock(false, rig.now);
        assert_eq!(rig.colors(), ["0000FF", "00FF00", "FFFFFF", "FFFFFF"]);
        assert_eq!(rig.state.fading(), []);
    }

    #[test]
    fn turning_caps_lock_on_again_does_not_restart_its_fade() {
        let mut rig = caps_rig();
        rig.state.set_caps_lock(true, rig.now);
        rig.wait(0.35);
        let changed = rig.state.set_caps_lock(true, rig.now);
        assert_eq!(changed, Changed::default());
        assert_eq!(rig.color(Left), mix("FF0000", "FFFFFF", 0.5));
    }

    #[test]
    fn effects_carry_on_under_caps_lock_and_show_again_when_it_turns_off() {
        let mut rig = caps_rig();
        rig.kbdrgb("cycle -z left AA00AA 00FF00");
        rig.kbdrgb("cycle -z center AA00AA 00FF00");
        rig.state.set_caps_lock(true, rig.now);
        rig.wait(3.25);
        assert_eq!(rig.color(Center), "FFFFFF");
        rig.state.set_caps_lock(false, rig.now);
        assert_eq!(rig.color(Left), mix("00FF00", "AA00AA", 0.25));
        assert_eq!(rig.color(Center), rig.color(Left));
    }

    #[test]
    fn a_timed_effect_still_ends_while_caps_lock_is_on() {
        let mut rig = caps_rig();
        rig.kbdrgb("pulse -z right -d 1 00FFAA");
        rig.state.set_caps_lock(true, rig.now);
        let ended = rig.wait(3.0);
        assert_eq!(ended.zones, [Right]);
        assert_eq!(rig.color(Right), "FFFFFF");
        rig.state.set_caps_lock(false, rig.now);
        assert_eq!(
            rig.state.scene()[Right],
            Setting::Color(color(&rig.color(Right)))
        );
        assert_eq!(rig.state.fading(), []);
    }

    // Undo and redo

    #[test]
    fn with_nothing_to_undo_the_whole_keyboard_turns_white() {
        let mut rig = Rig::new();
        rig.kbdrgb("color FF0000");
        rig.state.undo();
        rig.state.undo();
        assert_eq!(rig.colors(), ["FFFFFF"; 4]);
    }

    #[test]
    fn undo_goes_back_one_command_at_a_time() {
        let mut rig = Rig::new();
        rig.kbdrgb("color FF0000");
        rig.kbdrgb("zones 00FF00 0000FF FFFF00 FF00AA");
        rig.kbdrgb("color -z center 0000AA");
        assert_eq!(
            rig.state.undo(),
            Changed {
                zones: Zone::ALL.to_vec(),
                scene: true
            }
        );
        assert_eq!(rig.colors(), ["00FF00", "0000FF", "FFFF00", "FF00AA"]);
        rig.state.undo();
        assert_eq!(rig.colors(), ["FF0000"; 4]);
        rig.state.undo();
        assert_eq!(rig.colors(), ["FFFFFF"; 4]);
    }

    #[test]
    fn undo_ends_an_effect_and_brings_back_the_one_before_it_in_step() {
        let mut rig = Rig::new();
        rig.kbdrgb("rainbow -z right");
        rig.kbdrgb("cycle -z right 112233 445566");
        rig.wait(7.3);
        rig.state.undo();
        assert_eq!(rig.setting(Right), "rainbow 0.6");
        let mut fresh = Rig::new();
        fresh.kbdrgb("rainbow -z right");
        fresh.wait(7.3);
        assert_eq!(rig.color(Right), fresh.color(Right));
        rig.state.undo();
        assert_eq!(rig.state.fading(), []);
    }

    #[test]
    fn undo_during_a_transition_keeps_the_color_the_transition_was_going_to() {
        let mut rig = Rig::new();
        rig.kbdrgb("color -z center -t 2 AA00AA");
        rig.kbdrgb("cycle -z extra 112233 445566");
        rig.wait(0.3);
        rig.state.undo();
        assert_eq!(rig.color(Center), "AA00AA");
        assert_eq!(rig.color(Extra), "FFFFFF");
        assert_eq!(rig.state.fading(), []);
    }

    #[test]
    fn undo_never_brings_back_a_timed_effect() {
        let mut rig = Rig::new();
        rig.kbdrgb("color 00FF00");
        rig.kbdrgb("pulse -z left -d 5 00FFAA");
        rig.kbdrgb("color -z center 0000AA");
        rig.kbdrgb("pulse -z left -d 100 FF0000");
        rig.kbdrgb("color -z center 0000BB");
        rig.state.undo();
        assert_eq!(rig.colors(), ["00FF00", "0000AA", "00FF00", "00FF00"]);
        assert_eq!(rig.state.fading(), []);
        rig.state.undo();
        assert_eq!(rig.colors(), ["00FF00"; 4]);
    }

    #[test]
    fn once_a_timed_effect_has_ended_undo_keeps_its_last_color() {
        let mut rig = Rig::new();
        rig.kbdrgb("pulse -z left -d 100 FF0000");
        rig.wait(101.0);
        let last = rig.color(Left);
        rig.kbdrgb("color -z center 0000BB");
        rig.kbdrgb("color -z center 0000CC");
        rig.state.undo();
        assert_eq!(rig.color(Left), last);
        assert_eq!(rig.color(Center), "0000BB");
    }

    #[test]
    fn redo_goes_forward_to_what_undo_left() {
        let mut rig = Rig::new();
        rig.kbdrgb("color 111111");
        rig.kbdrgb("cycle -z left 112233 445566");
        rig.kbdrgb("color -z right 222222");
        rig.state.undo();
        rig.state.undo();
        assert_eq!(rig.colors(), ["111111"; 4]);
        rig.wait(7.5);
        assert!(rig.state.redo().is_some());
        assert_eq!(rig.setting(Left), "112233 445566 1");
        assert_eq!(rig.color(Left), mix("445566", "112233", 0.5));
        assert_eq!(rig.color(Right), "111111");
        rig.state.redo();
        assert_eq!(rig.color(Right), "222222");
        assert_eq!(rig.state.redo(), None);
        assert_eq!(rig.color(Right), "222222");
    }

    #[test]
    fn a_new_change_ends_redo() {
        let mut rig = Rig::new();
        rig.kbdrgb("color FF0000");
        rig.state.undo();
        rig.kbdrgb("color 333333");
        assert_eq!(rig.state.redo(), None);
        assert_eq!(rig.colors(), ["333333"; 4]);
        rig.state.undo();
        assert_eq!(rig.colors(), ["FFFFFF"; 4]);
    }

    #[test]
    fn actions_that_do_not_start_a_new_step_are_undone_with_the_step_before_them() {
        let mut rig = Rig::new();
        rig.kbdrgb("color FF0000");
        let now = rig.now;
        let green = crate::text::read("00FF00", Left.into()).unwrap();
        let blue = crate::text::read("0000FF", Center.into()).unwrap();
        rig.state.run(&[green], true, now);
        rig.state.run(&[blue], false, now);
        assert_eq!(rig.colors(), ["00FF00", "0000FF", "FF0000", "FF0000"]);
        rig.state.undo();
        assert_eq!(rig.colors(), ["FF0000"; 4]);
    }

    #[test]
    fn setting_the_same_scene_twice_is_one_undo_step() {
        let mut rig = Rig::new();
        rig.kbdrgb("color 444444");
        rig.kbdrgb("color FF0000");
        rig.kbdrgb("color FF0000");
        rig.kbdrgb("color 00FF00");
        rig.state.undo();
        assert_eq!(rig.colors(), ["FF0000"; 4]);
        rig.state.undo();
        assert_eq!(rig.colors(), ["444444"; 4]);
    }

    #[test]
    fn undo_goes_back_through_as_many_changes_as_the_undo_depth() {
        let config = Config {
            undo_depth: NonZeroUsize::new(5).unwrap(),
            ..Config::default()
        };
        let mut rig = Rig::with(&config);
        for i in 0..15 {
            rig.kbdrgb(&format!("color {i:06X}"));
        }
        for _ in 0..5 {
            rig.state.undo();
        }
        assert_eq!(rig.colors(), ["000009"; 4]);
        rig.state.undo();
        assert_eq!(rig.colors(), ["FFFFFF"; 4]);
    }

    #[test]
    fn the_history_says_how_much_there_is_to_undo_and_redo() {
        let mut rig = Rig::new();
        rig.kbdrgb("color FF0000");
        rig.kbdrgb("color 00FF00");
        rig.state.undo();
        assert_eq!(rig.state.history().to_string(), "1 to undo, 1 to redo");
    }
}
