//! The Menu-key combos.

use std::collections::HashSet;
use std::time::Instant;

use evdev::KeyCode;

use super::lights::Command;
use crate::action::{Action, Change, FadeTime};
use crate::color::Color;
use crate::ease::Ease;
use crate::setting::{Cycle, Pattern, Period, Step};
use crate::zone::{Target, Zone};

/// What a key did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Press {
    Down,
    Up,
    /// The kernel sends this again and again while a key is held.
    Repeat,
}

impl Press {
    /// Reads the value of a key event.
    pub(crate) fn from_value(value: i32) -> Self {
        match value {
            0 => Press::Up,
            1 => Press::Down,
            _ => Press::Repeat,
        }
    }
}

/// Something for the main loop to do in answer to a key event.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Output {
    /// Pass the event on to the desktop.
    Forward,
    /// Press and release this key on the virtual keyboard.
    Tap(KeyCode),
    /// Send this command to the lights.
    Lights(Command),
    /// Open the color dialog.
    OpenDialog(Target),
}

/// The zone key that was pressed while Menu is held.
#[derive(Debug)]
struct Selection {
    target: Target,
    /// The zone key, or Space.
    key: KeyCode,
    /// Whether that key is still down. While it is, digits add colors to a cycle.
    held: bool,
    /// Each color picked while the key was down, with when its digit was pressed.
    picks: Vec<(Color, Instant)>,
    /// Whether pressing the key again opens the dialog. Picking a cycle turns this off.
    reopen: bool,
}

/// What has happened since the Menu key went down.
#[derive(Debug, Default)]
struct Hold {
    /// Whether another key was pressed. If none was, letting go of Menu taps the real Menu key.
    used: bool,
    /// Whether a change has started this hold's undo step. Everything until Menu is let go is one
    /// undo step.
    stepped: bool,
    selection: Option<Selection>,
}

/// Turns key events into what to do about them. It does nothing itself: [`Menu::key`] returns the
/// outputs, and the main loop carries them out.
#[derive(Debug)]
pub(crate) struct Menu {
    /// The key to hold for the combos.
    mod_key: KeyCode,
    palette: [Color; 10],
    /// `Some` while the Menu key is down.
    hold: Option<Hold>,
    /// Keys whose press wasn't passed on, so their repeats and release aren't passed on either.
    swallowed: HashSet<KeyCode>,
    /// The Shift keys that are down, for Menu+Shift+Backspace.
    shifts: HashSet<KeyCode>,
}

impl Menu {
    pub(crate) fn new(mod_key: KeyCode, palette: [Color; 10]) -> Self {
        Menu {
            mod_key,
            palette,
            hold: None,
            swallowed: HashSet::new(),
            shifts: HashSet::new(),
        }
    }

    /// Takes one key event, which happened at `now`, and returns what to do about it.
    pub(crate) fn key(&mut self, key: KeyCode, press: Press, now: Instant) -> Vec<Output> {
        // The Shift keys are tracked whether or not the desktop gets them.
        if matches!(key, KeyCode::KEY_LEFTSHIFT | KeyCode::KEY_RIGHTSHIFT) {
            if press == Press::Up {
                self.shifts.remove(&key);
            } else {
                self.shifts.insert(key);
            }
        }

        if key == self.mod_key {
            return self.mod_key(press);
        }

        if self.swallowed.contains(&key) {
            if press != Press::Up {
                return vec![];
            }
            self.swallowed.remove(&key);
            return match self
                .selection()
                .filter(|selection| selection.held && selection.key == key)
            {
                Some(selection) => {
                    selection.held = false;
                    finish_cycle(selection).into_iter().collect()
                }
                None => vec![],
            };
        }

        let shifted = !self.shifts.is_empty();
        let palette = self.palette;
        let Some(hold) = self.hold.as_mut().filter(|_| press == Press::Down) else {
            return vec![Output::Forward];
        };
        hold.used = true;
        self.swallowed.insert(key);

        if key == KeyCode::KEY_BACKSPACE {
            hold.selection = None;
            hold.stepped = false; // A change after this is a new undo step.
            return vec![Output::Lights(if shifted {
                Command::Redo
            } else {
                Command::Undo
            })];
        }
        if let Some(digit) = digit(key) {
            // Digits only ever set colors. They never select a zone.
            let color = palette[digit];
            let selection = hold.selection.as_mut();
            // With no zone selected, a digit colors the whole keyboard.
            let target = selection
                .as_ref()
                .map_or(Target::All, |selection| selection.target);
            if let Some(selection) = selection.filter(|selection| selection.held) {
                selection.picks.push((color, now));
            }
            let change = Change::Color {
                color,
                fade: FadeTime::default(),
                curve: Ease::LINEAR,
            };
            let new_step = !std::mem::replace(&mut hold.stepped, true);
            let actions = vec![Action { target, change }];
            return vec![Output::Lights(Command::Run { actions, new_step })];
        }
        if let Some(selection) = hold
            .selection
            .take_if(|selection| selection.reopen && selection.key == key)
        {
            return vec![Output::OpenDialog(selection.target)];
        }
        if let Some(target) = zone_of(key) {
            // A new zone key may arrive while the last one is still held.
            let finished = hold.selection.as_mut().and_then(finish_cycle);
            log::debug!("selected {target}");
            hold.selection = Some(Selection {
                target,
                key,
                held: true,
                picks: vec![],
                reopen: true,
            });
            return finished.into_iter().collect();
        }
        log::debug!("unmapped key: {key:?}");
        vec![]
    }

    fn mod_key(&mut self, press: Press) -> Vec<Output> {
        match press {
            Press::Down => {
                self.hold = Some(Hold::default());
                vec![]
            }
            Press::Up => {
                let Some(mut hold) = self.hold.take() else {
                    return vec![];
                };
                if !hold.used {
                    // A tap of Menu on its own is the real Menu key.
                    return vec![Output::Tap(self.mod_key)];
                }
                // Menu may be let go before the zone key. What was picked still becomes a cycle.
                hold.selection
                    .as_mut()
                    .and_then(finish_cycle)
                    .into_iter()
                    .collect()
            }
            Press::Repeat => vec![],
        }
    }

    fn selection(&mut self) -> Option<&mut Selection> {
        self.hold.as_mut()?.selection.as_mut()
    }
}

/// Turns the colors picked while the zone key was held into a cycle. The time between the first
/// two picks is how long each color lasts. Fewer than two picks make no cycle.
fn finish_cycle(selection: &mut Selection) -> Option<Output> {
    let picks = std::mem::take(&mut selection.picks);
    let &[(_, first), (_, second), ..] = &picks[..] else {
        return None;
    };
    let steps = picks.iter().take(Cycle::MAX_STEPS);
    let steps = steps
        .map(|&(color, _)| Step {
            color,
            curve: Ease::LINEAR,
        })
        .collect();
    let pattern = Pattern::Cycle(Cycle::new(steps).ok()?);
    let period = Period::clamped(second.saturating_duration_since(first));
    // Pressing the zone key again starts over. It doesn't open the dialog.
    selection.reopen = false;
    let change = Change::Effect {
        pattern,
        period: Some(period),
        time_limit: None,
    };
    let actions = vec![Action {
        target: selection.target,
        change,
    }];
    // The digits that picked the colors started the undo step.
    Some(Output::Lights(Command::Run {
        actions,
        new_step: false,
    }))
}

/// The palette color that a top-row digit key stands for.
fn digit(key: KeyCode) -> Option<usize> {
    let digit = match key {
        KeyCode::KEY_0 => 0,
        KeyCode::KEY_1 => 1,
        KeyCode::KEY_2 => 2,
        KeyCode::KEY_3 => 3,
        KeyCode::KEY_4 => 4,
        KeyCode::KEY_5 => 5,
        KeyCode::KEY_6 => 6,
        KeyCode::KEY_7 => 7,
        KeyCode::KEY_8 => 8,
        KeyCode::KEY_9 => 9,
        _ => return None,
    };
    Some(digit)
}

/// The zone that a key is in on the Serval WS serw14, or the whole keyboard for Space. Zones
/// blend into each other, so this only needs to be close.
fn zone_of(key: KeyCode) -> Option<Target> {
    use KeyCode as K;
    let zone = match key {
        K::KEY_SPACE => return Some(Target::All),
        K::KEY_ESC
        | K::KEY_F1
        | K::KEY_F2
        | K::KEY_F3
        | K::KEY_F4
        | K::KEY_GRAVE
        | K::KEY_TAB
        | K::KEY_Q
        | K::KEY_W
        | K::KEY_E
        | K::KEY_R
        | K::KEY_CAPSLOCK
        | K::KEY_A
        | K::KEY_S
        | K::KEY_D
        | K::KEY_F
        | K::KEY_LEFTSHIFT
        | K::KEY_Z
        | K::KEY_X
        | K::KEY_C
        | K::KEY_V
        | K::KEY_B
        | K::KEY_LEFTCTRL
        | K::KEY_LEFTMETA
        | K::KEY_LEFTALT
        | K::KEY_102ND => Zone::Left,
        K::KEY_F5
        | K::KEY_F6
        | K::KEY_F7
        | K::KEY_F8
        | K::KEY_F9
        | K::KEY_T
        | K::KEY_Y
        | K::KEY_U
        | K::KEY_I
        | K::KEY_G
        | K::KEY_H
        | K::KEY_J
        | K::KEY_K
        | K::KEY_N
        | K::KEY_M
        | K::KEY_COMMA => Zone::Center,
        K::KEY_F10
        | K::KEY_F11
        | K::KEY_F12
        | K::KEY_SYSRQ
        | K::KEY_PRINT
        | K::KEY_INSERT
        | K::KEY_DELETE
        | K::KEY_MINUS
        | K::KEY_EQUAL
        | K::KEY_O
        | K::KEY_P
        | K::KEY_LEFTBRACE
        | K::KEY_RIGHTBRACE
        | K::KEY_BACKSLASH
        | K::KEY_L
        | K::KEY_SEMICOLON
        | K::KEY_APOSTROPHE
        | K::KEY_ENTER
        | K::KEY_DOT
        | K::KEY_SLASH
        | K::KEY_RIGHTSHIFT
        | K::KEY_RIGHTALT
        | K::KEY_COMPOSE
        | K::KEY_RIGHTCTRL
        | K::KEY_UP
        | K::KEY_DOWN
        | K::KEY_LEFT
        | K::KEY_RIGHT => Zone::Right,
        K::KEY_HOME
        | K::KEY_END
        | K::KEY_PAGEUP
        | K::KEY_PAGEDOWN
        | K::KEY_NUMLOCK
        | K::KEY_KPSLASH
        | K::KEY_KPASTERISK
        | K::KEY_KPMINUS
        | K::KEY_KPPLUS
        | K::KEY_KPENTER
        | K::KEY_KPDOT
        | K::KEY_KP0
        | K::KEY_KP1
        | K::KEY_KP2
        | K::KEY_KP3
        | K::KEY_KP4
        | K::KEY_KP5
        | K::KEY_KP6
        | K::KEY_KP7
        | K::KEY_KP8
        | K::KEY_KP9 => Zone::Extra,
        _ => return None,
    };
    Some(Target::Zone(zone))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::daemon::config::Config;

    /// A menu, and a time that tests move forward.
    struct Keys {
        menu: Menu,
        now: Instant,
    }

    impl Keys {
        fn new() -> Self {
            let config = Config::default();
            Keys {
                menu: Menu::new(config.mod_key.0, config.palette.colors),
                now: Instant::now(),
            }
        }

        /// Feeds key events to the menu and returns every output, as text. In `events`, `Q+` is Q
        /// going down, `Q-` is Q coming up, `Q` is both, and `Q*` is a repeat. `M` is the Menu key,
        /// and `~1.5` waits 1.5 seconds.
        fn press(&mut self, events: &str) -> Vec<String> {
            let mut outputs = Vec::new();
            for event in events.split_whitespace() {
                if let Some(seconds) = event.strip_prefix('~') {
                    self.now += Duration::from_secs_f64(seconds.parse().unwrap());
                    continue;
                }
                let (name, presses): (&str, &[Press]) = match event.split_at(event.len() - 1) {
                    (name, "+") => (name, &[Press::Down]),
                    (name, "-") => (name, &[Press::Up]),
                    (name, "*") => (name, &[Press::Repeat]),
                    _ => (event, &[Press::Down, Press::Up]),
                };
                let name = if name == "M" { "COMPOSE" } else { name };
                let key: KeyCode = format!("KEY_{name}").parse().unwrap();
                for &press in presses {
                    outputs.extend(self.menu.key(key, press, self.now).iter().map(describe));
                }
            }
            outputs
        }
    }

    fn describe(output: &Output) -> String {
        match output {
            Output::Forward => "forward".to_owned(),
            Output::Tap(key) => format!("tap {key:?}"),
            Output::OpenDialog(target) => format!("dialog {target}"),
            Output::Lights(Command::Undo) => "undo".to_owned(),
            Output::Lights(Command::Redo) => "redo".to_owned(),
            Output::Lights(Command::CapsLock(on)) => format!("caps lock {on}"),
            Output::Lights(Command::Run { actions, new_step }) => {
                let actions: Vec<String> = actions.iter().map(Action::to_string).collect();
                format!(
                    "{}{}",
                    if *new_step { "new step: " } else { "" },
                    actions.join("; ")
                )
            }
        }
    }

    // Keys that aren't part of a combo

    #[test]
    fn typing_is_passed_on() {
        assert_eq!(Keys::new().press("A+ A* A- 2 BACKSPACE"), ["forward"; 7]);
    }

    #[test]
    fn a_tap_of_menu_on_its_own_is_the_real_menu_key() {
        assert_eq!(Keys::new().press("M+ M* M-"), ["tap KEY_COMPOSE"]);
    }

    #[test]
    fn nothing_is_passed_on_while_menu_is_held() {
        assert_eq!(Keys::new().press("M+ F13+ F13* F13- M-"), [] as [&str; 0]);
    }

    #[test]
    fn a_key_that_is_released_after_menu_is_still_not_passed_on() {
        let mut keys = Keys::new();
        assert_eq!(keys.press("M+ W+ M- W* W-"), [] as [&str; 0]);
        assert_eq!(keys.press("W"), ["forward", "forward"]);
    }

    #[test]
    fn a_key_that_was_down_before_menu_is_released_as_usual() {
        let outputs = Keys::new().press("X+ M+ X- M-");
        assert_eq!(outputs, ["forward", "forward", "tap KEY_COMPOSE"]);
    }

    // Colors

    #[test]
    fn a_digit_with_no_zone_selected_colors_the_whole_keyboard() {
        assert_eq!(Keys::new().press("M+ 3"), ["new step: color -z all FFFF00"]);
    }

    #[test]
    fn a_zone_key_selects_its_zone_for_the_digits_that_follow() {
        let mut keys = Keys::new();
        assert_eq!(keys.press("M+ Q 3"), ["new step: color -z left FFFF00"]);
        assert_eq!(
            keys.press("7 9"),
            ["color -z left 0000FF", "color -z left FF00AA"]
        );
        assert_eq!(keys.press("K 1"), ["color -z center FF0000"]);
        assert_eq!(keys.press("ENTER 0"), ["color -z right FFFFFF"]);
        assert_eq!(keys.press("SPACE 8"), ["color -z all 8800FF"]);
    }

    #[test]
    fn a_numpad_digit_selects_the_numpad_and_a_top_row_digit_colors_it() {
        assert_eq!(
            Keys::new().press("M+ KP6 6"),
            ["new step: color -z extra 0088FF"]
        );
    }

    #[test]
    fn letting_go_of_menu_clears_the_selection() {
        let outputs = Keys::new().press("M+ Q 3 M- M+ 5");
        assert_eq!(
            outputs,
            [
                "new step: color -z left FFFF00",
                "new step: color -z all 00FFAA"
            ]
        );
    }

    #[test]
    fn everything_in_one_hold_of_menu_is_one_undo_step() {
        let outputs = Keys::new().press("M+ 2 Q 4 K 7 M-");
        assert_eq!(
            outputs,
            [
                "new step: color -z all FF6600",
                "color -z left 00FF00",
                "color -z center 0000FF"
            ]
        );
    }

    #[test]
    fn a_key_with_no_zone_does_nothing() {
        assert_eq!(
            Keys::new().press("M+ Q F13 3"),
            ["new step: color -z left FFFF00"]
        );
    }

    // The dialog

    #[test]
    fn the_same_zone_key_twice_opens_the_dialog_for_that_zone() {
        let mut keys = Keys::new();
        assert_eq!(keys.press("M+ KP6 KP6"), ["dialog extra"]);
        assert_eq!(keys.press("ENTER ENTER"), ["dialog right"]);
        assert_eq!(
            keys.press("Q 4 Q"),
            ["new step: color -z left 00FF00", "dialog left"]
        );
    }

    #[test]
    fn space_twice_opens_the_dialog_for_the_whole_keyboard() {
        assert_eq!(Keys::new().press("M+ SPACE SPACE"), ["dialog all"]);
    }

    #[test]
    fn opening_the_dialog_clears_the_selection() {
        let outputs = Keys::new().press("M+ Q Q 1");
        assert_eq!(outputs, ["dialog left", "new step: color -z all FF0000"]);
    }

    // Undo and redo

    #[test]
    fn backspace_undoes_and_shift_backspace_redoes() {
        let mut keys = Keys::new();
        assert_eq!(keys.press("M+ BACKSPACE"), ["undo"]);
        assert_eq!(keys.press("LEFTSHIFT+ BACKSPACE LEFTSHIFT-"), ["redo"]);
        assert_eq!(keys.press("BACKSPACE"), ["undo"]);
    }

    #[test]
    fn shift_held_before_menu_works_too_and_still_reaches_the_desktop() {
        let outputs = Keys::new().press("RIGHTSHIFT+ M+ BACKSPACE M- RIGHTSHIFT-");
        assert_eq!(outputs, ["forward", "redo", "forward"]);
    }

    #[test]
    fn undo_clears_the_selection_and_ends_the_undo_step() {
        let outputs = Keys::new().press("M+ Q 9 BACKSPACE 3");
        assert_eq!(
            outputs,
            [
                "new step: color -z left FF00AA",
                "undo",
                "new step: color -z all FFFF00"
            ]
        );
    }

    // Cycles

    #[test]
    fn digits_pressed_while_the_zone_key_is_held_become_a_cycle_when_it_is_let_go() {
        let mut keys = Keys::new();
        let outputs = keys.press("M+ Q+ 1 ~1.5 3");
        assert_eq!(
            outputs,
            ["new step: color -z left FF0000", "color -z left FFFF00"]
        );
        let outputs = keys.press("~0.2 7 Q-");
        assert_eq!(
            outputs,
            [
                "color -z left 0000FF",
                "cycle -z left -t 1.5 FF0000 FFFF00 0000FF"
            ]
        );
    }

    #[test]
    fn after_a_cycle_the_zone_key_starts_over_and_does_not_open_the_dialog() {
        let mut keys = Keys::new();
        keys.press("M+ Q+ 1 ~1.5 3 Q-");
        let outputs = keys.press("Q+ 4 ~0.5 5 Q-");
        assert_eq!(
            outputs,
            [
                "color -z left 00FF00",
                "color -z left 00FFAA",
                "cycle -z left -t 0.5 00FF00 00FFAA"
            ]
        );
    }

    #[test]
    fn digits_pressed_after_the_zone_key_is_let_go_only_set_colors() {
        let outputs = Keys::new().press("M+ Q 1 ~1 3 Q");
        assert_eq!(
            outputs,
            [
                "new step: color -z left FF0000",
                "color -z left FFFF00",
                "dialog left"
            ]
        );
    }

    #[test]
    fn one_digit_while_the_zone_key_is_held_only_sets_the_color() {
        let outputs = Keys::new().press("M+ Q+ 6 Q- Q");
        assert_eq!(outputs, ["new step: color -z left 0088FF", "dialog left"]);
    }

    #[test]
    fn holding_space_cycles_the_whole_keyboard() {
        let outputs = Keys::new().press("M+ SPACE+ 2 ~2 8 SPACE-");
        assert_eq!(outputs.last().unwrap(), "cycle -z all -t 2 FF6600 8800FF");
    }

    #[test]
    fn another_zone_key_finishes_the_cycle_of_the_one_that_is_held() {
        let outputs = Keys::new().press("M+ K+ 1 ~0.5 2 L+ K-");
        assert_eq!(
            outputs.last().unwrap(),
            "cycle -z center -t 0.5 FF0000 FF6600"
        );
        assert_eq!(outputs.len(), 3);
    }

    #[test]
    fn a_cycles_time_is_at_least_a_tenth_of_a_second_and_at_most_a_minute() {
        let mut keys = Keys::new();
        let fast = keys.press("M+ K+ 1 ~0.01 2 K-");
        assert_eq!(fast.last().unwrap(), "cycle -z center -t 0.1 FF0000 FF6600");
        let slow = keys.press("L+ 4 ~100 5 L-");
        assert_eq!(slow.last().unwrap(), "cycle -z right -t 60 00FF00 00FFAA");
    }

    #[test]
    fn backspace_while_the_zone_key_is_held_drops_the_picked_colors() {
        let outputs = Keys::new().press("M+ E+ 1 ~1 2 BACKSPACE E-");
        assert_eq!(
            outputs,
            [
                "new step: color -z left FF0000",
                "color -z left FF6600",
                "undo"
            ]
        );
    }

    #[test]
    fn a_cycle_has_at_most_16_colors() {
        let mut keys = Keys::new();
        keys.press("M+ E+");
        for digit in (0..10).chain(0..8) {
            keys.press(&format!("~0.25 {digit}"));
        }
        let outputs = keys.press("E-");
        let cycle = outputs.last().unwrap();
        assert_eq!(
            cycle.split(' ').filter(|word| word.len() == 6).count(),
            16,
            "{cycle}"
        );
        assert!(
            cycle.starts_with("cycle -z left -t 0.25 FFFFFF FF0000"),
            "{cycle}"
        );
    }

    #[test]
    fn letting_go_of_menu_before_the_zone_key_still_starts_the_cycle() {
        let outputs = Keys::new().press("M+ Z+ 1 ~1 3 M- Z-");
        assert_eq!(outputs.last().unwrap(), "cycle -z left -t 1 FF0000 FFFF00");
        assert_eq!(outputs.len(), 3);
    }

    // The key table

    #[test]
    fn keys_are_in_the_zone_they_sit_in_on_the_keyboard() {
        for (key, zone) in [
            (KeyCode::KEY_ESC, Zone::Left),
            (KeyCode::KEY_B, Zone::Left),
            (KeyCode::KEY_T, Zone::Center),
            (KeyCode::KEY_COMMA, Zone::Center),
            (KeyCode::KEY_O, Zone::Right),
            (KeyCode::KEY_RIGHT, Zone::Right),
            (KeyCode::KEY_HOME, Zone::Extra),
            (KeyCode::KEY_KP0, Zone::Extra),
        ] {
            assert_eq!(zone_of(key), Some(Target::Zone(zone)), "{key:?}");
        }
        assert_eq!(zone_of(KeyCode::KEY_SPACE), Some(Target::All));
        assert_eq!(zone_of(KeyCode::KEY_1), None);
        assert_eq!(zone_of(KeyCode::KEY_BACKSPACE), None);
    }

    #[test]
    fn another_key_can_be_the_one_to_hold() {
        let config = Config::default();
        let mut menu = Menu::new(KeyCode::KEY_RIGHTMETA, config.palette.colors);
        let now = Instant::now();
        assert_eq!(menu.key(KeyCode::KEY_RIGHTMETA, Press::Down, now), []);
        let outputs = menu.key(KeyCode::KEY_3, Press::Down, now);
        assert_eq!(
            outputs.iter().map(describe).collect::<Vec<_>>(),
            ["new step: color -z all FFFF00"]
        );
        // With another key to hold, Menu is a zone key like its neighbors.
        assert_eq!(menu.key(KeyCode::KEY_COMPOSE, Press::Down, now), []);
        let outputs = menu.key(KeyCode::KEY_1, Press::Down, now);
        assert_eq!(
            outputs.iter().map(describe).collect::<Vec<_>>(),
            ["color -z right FF0000"]
        );
    }
}
