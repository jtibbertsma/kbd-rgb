//! The color dialog, where the user types a color or effect for a zone.

use std::sync::Arc;
use std::{io, thread};

use super::config::Palette;
use super::desktop::{Answer, Desktop, Window};
use super::lights::{Command, LightsHandle};
use crate::text;
use crate::zone::Target;

/// An open color dialog. Dropping the handle closes the dialog, so storing the handle of a new
/// dialog in place of an old one closes the old one. The old one may be hidden behind other
/// windows, and only one dialog should be open at a time.
#[derive(Debug)]
pub(crate) struct DialogHandle {
    window: Arc<Window>,
}

impl Drop for DialogHandle {
    fn drop(&mut self) {
        self.window.close();
    }
}

/// Opens the color dialog for `target` in a `dialog` thread of its own, which waits for the user
/// and sends what they typed to the lights.
pub(crate) fn open(
    target: Target,
    lights: LightsHandle,
    desktop: Arc<dyn Desktop>,
    palette: Palette,
) -> io::Result<DialogHandle> {
    let window = Arc::new(Window::default());
    let handle = DialogHandle {
        window: Arc::clone(&window),
    };
    thread::Builder::new()
        .name("dialog".to_owned())
        .spawn(move || ask(target, &lights, desktop.as_ref(), &window, &palette))?;
    Ok(handle)
}

/// Asks the user for a color or effect for `target`, and runs it. Text that can't be read is
/// answered with the reason, and the user gets to correct what they typed.
fn ask(
    target: Target,
    lights: &LightsHandle,
    desktop: &dyn Desktop,
    window: &Window,
    palette: &Palette,
) {
    let label = match target {
        Target::All => "the whole keyboard".to_owned(),
        Target::Zone(zone) => format!("the {zone} zone"),
    };
    let prompt = format!("Color or effect for {label}:\n\n{}", palette_text(palette));
    // The dialog for the whole keyboard starts from the left zone's setting.
    let Some(zone) = target.zones().next() else {
        return;
    };
    let Some(setting) = lights.setting(zone) else {
        return;
    };
    let mut entry = text::describe(&setting);
    loop {
        let Answer::Text(typed) = desktop.ask(&prompt, &entry, window) else {
            return;
        };
        match text::read(&typed, target) {
            Ok(action) => {
                if !window.is_closed() {
                    lights.send(Command::Run {
                        actions: vec![action],
                        new_step: true,
                    });
                }
                return;
            }
            Err(problem) => {
                if !desktop.show_error(&problem.to_string(), window) {
                    return;
                }
                typed.trim().clone_into(&mut entry);
            }
        }
    }
}

/// The text above the dialog's entry box: the palette, one color per line. zenity shows this text
/// in a proportional font, so it can't be split into columns.
fn palette_text(palette: &Palette) -> String {
    let lines = palette.colors.iter().zip(&palette.names).enumerate();
    let lines: Vec<String> = lines
        .map(|(number, (color, name))| format!("{number}   {color}   {name}"))
        .collect();
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::daemon::config::Config;
    use crate::daemon::lights;
    use crate::testing::{FakeDesktop, FakeLeds, actions};
    use crate::zone::Zone::{Left, Right};

    /// A lights thread, and a fake desktop whose boxes are answered from a script.
    struct Fixture {
        lights: LightsHandle,
        desktop: Arc<FakeDesktop>,
        _directory: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let state_file = directory.path().join("state.json");
            let lights = crate::testing::lights(
                &Config::default(),
                FakeLeds::default(),
                &state_file,
                Instant::now(),
            );
            let (lights, _thread) = lights::spawn(lights).unwrap();
            Fixture {
                lights,
                desktop: Arc::new(FakeDesktop::default()),
                _directory: directory,
            }
        }

        fn kbdrgb(&self, line: &str) {
            self.lights.send(Command::Run {
                actions: actions(line),
                new_step: true,
            });
        }

        /// Runs the dialog for `target` to its end, on this thread.
        fn ask(&self, target: impl Into<Target>) {
            let palette = Config::default().palette;
            ask(
                target.into(),
                &self.lights,
                self.desktop.as_ref(),
                &Window::default(),
                &palette,
            );
        }

        fn setting(&self, zone: crate::zone::Zone) -> String {
            text::describe(&self.lights.setting(zone).unwrap())
        }
    }

    #[test]
    fn the_dialog_starts_from_the_zones_setting_and_runs_what_is_typed() {
        let fixture = Fixture::new();
        fixture.kbdrgb("color -z right 445566");
        fixture
            .desktop
            .answer(Answer::Text("FF0000 0000FF 2\n".to_owned()));
        fixture.ask(Right);
        assert_eq!(fixture.desktop.entries(), ["445566"]);
        assert_eq!(fixture.setting(Right), "FF0000 0000FF 2");
        assert_eq!(fixture.setting(Left), "FFFFFF");
    }

    #[test]
    fn text_that_cannot_be_read_shows_why_and_asks_again_from_what_was_typed() {
        let fixture = Fixture::new();
        for typed in ["<i>zzz", " FF0000 0000FF 61 ", "FF0000 0000FF 2"] {
            fixture.desktop.answer(Answer::Text(typed.to_owned()));
        }
        fixture.ask(Right);
        assert_eq!(
            fixture.desktop.entries(),
            ["FFFFFF", "<i>zzz", "FF0000 0000FF 61"]
        );
        let errors = fixture.desktop.errors();
        assert_eq!(errors.len(), 2);
        assert!(errors[0].contains("\"<i>zzz\" isn't a color"), "{errors:?}");
        assert!(errors[1].contains("60 seconds"), "{errors:?}");
        assert_eq!(fixture.setting(Right), "FF0000 0000FF 2");
    }

    #[test]
    fn cancelling_changes_nothing() {
        let fixture = Fixture::new();
        fixture.kbdrgb("cycle -z right -t 2 FF0000 0000FF");
        fixture.desktop.answer(Answer::Cancelled);
        fixture.ask(Right);
        assert_eq!(fixture.setting(Right), "FF0000 0000FF 2");
    }

    #[test]
    fn an_error_box_that_the_user_did_not_close_ends_the_dialog() {
        let fixture = Fixture::new();
        fixture.desktop.answer(Answer::Text("zzz".to_owned()));
        fixture.desktop.fail_errors();
        fixture.ask(Right);
        assert_eq!(fixture.desktop.entries().len(), 1);
    }

    #[test]
    fn the_dialog_for_the_whole_keyboard_starts_from_the_left_zone_and_sets_every_zone() {
        let fixture = Fixture::new();
        fixture.kbdrgb("zones 112233 445566");
        fixture.desktop.answer(Answer::Text("00FF00".to_owned()));
        fixture.ask(Target::All);
        assert_eq!(fixture.desktop.entries(), ["112233"]);
        assert!(
            fixture.desktop.prompts()[0].starts_with("Color or effect for the whole keyboard:")
        );
        assert_eq!(fixture.setting(Left), "00FF00");
        assert_eq!(fixture.setting(Right), "00FF00");
    }

    #[test]
    fn the_dialog_names_its_zone_and_lists_the_palette_in_ascii() {
        let fixture = Fixture::new();
        fixture.desktop.answer(Answer::Cancelled);
        fixture.ask(Right);
        let prompt = &fixture.desktop.prompts()[0];
        assert!(prompt.starts_with("Color or effect for the right zone:\n\n0   FFFFFF   white\n"));
        assert!(prompt.ends_with("\n9   FF00AA   pink"));
        // Outside a UTF-8 locale, zenity rejects text that isn't ASCII.
        assert!(prompt.is_ascii());
    }

    #[test]
    fn dropping_the_handle_closes_a_dialog_that_is_still_open() {
        let fixture = Fixture::new();
        fixture.desktop.answer_when_closed();
        let desktop: Arc<dyn Desktop> = fixture.desktop.clone();
        let handle = open(
            Right.into(),
            fixture.lights.clone(),
            desktop,
            Config::default().palette,
        )
        .unwrap();
        fixture.desktop.wait_for_entries(1);
        drop(handle);
        fixture.desktop.wait_until_closed();
        assert_eq!(fixture.setting(Right), "FFFFFF");
    }

    #[test]
    fn what_was_typed_in_a_dialog_that_has_been_closed_is_not_run() {
        let fixture = Fixture::new();
        fixture.desktop.answer(Answer::Text("00FF00".to_owned()));
        let window = Window::default();
        window.close();
        let palette = Config::default().palette;
        ask(
            Right.into(),
            &fixture.lights,
            fixture.desktop.as_ref(),
            &window,
            &palette,
        );
        fixture.lights.sync();
        assert_eq!(fixture.setting(Right), "FFFFFF");
    }
}
