//! The state file, which saves the scene so that it comes back after a restart.

use std::path::PathBuf;
use std::{fs, io};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::state::Scene;
use crate::color::Color;
use crate::daemon::config::Effects;
use crate::ease::Ease;
use crate::setting::{Cycle, Effect, Pattern, Period, Setting, Step};
use crate::zone::Zone;

/// An effect as the state file holds it, under its zone's name in `fades`.
///
/// The Python version of the daemon reads and writes the same file. It has no `kind`, and it
/// needs `colors`, `period`, `offset`, and `ease` to run the effect. So a rainbow and a pulse are
/// saved with their colors too, and a file without `kind` still loads.
#[derive(Debug, Serialize, Deserialize)]
struct SavedEffect {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kind: Option<Kind>,
    colors: Vec<Color>,
    period: Period,
    /// How far along its colors the effect starts. The Rust version works this out from the zone,
    /// so it only writes it.
    #[serde(default)]
    offset: f64,
    /// One curve for every step, or one curve into each color. A file without it means linear.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ease: Option<SavedEase>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Cycle,
    Rainbow,
    Pulse,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum SavedEase {
    One(Ease),
    Each(Vec<Ease>),
}

impl SavedEffect {
    fn of(effect: &Effect, zone: Zone, settings: &Effects) -> Self {
        let (kind, steps, offset) = match &effect.pattern {
            Pattern::Cycle(cycle) => (Kind::Cycle, cycle.steps().to_vec(), 0.0),
            Pattern::Rainbow { ease } => {
                let colors = &settings.rainbow.colors;
                let steps = colors
                    .iter()
                    .map(|&color| Step {
                        color,
                        curve: *ease,
                    })
                    .collect();
                #[expect(clippy::cast_precision_loss, reason = "both numbers are small")]
                let offset = (zone.index() as f64 * settings.rainbow.lag) % colors.len() as f64;
                (Kind::Rainbow, steps, offset)
            }
            Pattern::Pulse { color, ease } => {
                let dim = Color::BLACK.mix(*color, settings.pulse.dim);
                let steps = [*color, dim]
                    .map(|color| Step {
                        color,
                        curve: *ease,
                    })
                    .to_vec();
                (Kind::Pulse, steps, 0.0)
            }
        };
        let first = steps[0].curve;
        let ease = if steps.iter().all(|step| step.curve == Ease::LINEAR) {
            None
        } else if steps.iter().all(|step| step.curve == first) {
            Some(SavedEase::One(first))
        } else {
            Some(SavedEase::Each(
                steps.iter().map(|step| step.curve).collect(),
            ))
        };
        SavedEffect {
            kind: Some(kind),
            colors: steps.iter().map(|step| step.color).collect(),
            period: effect.period,
            offset,
            ease,
        }
    }

    /// The effect that this was saved from, or `None` if it isn't valid.
    fn into_effect(self, settings: &Effects) -> Option<Effect> {
        let count = self.colors.len();
        let curves = match self.ease {
            None => vec![Ease::LINEAR; count],
            Some(SavedEase::One(curve)) => vec![curve; count],
            Some(SavedEase::Each(curves)) if curves.len() == 1 => vec![curves[0]; count],
            Some(SavedEase::Each(curves)) if curves.len() == count => curves,
            Some(SavedEase::Each(_)) => return None,
        };
        let steps: Vec<Step> = self
            .colors
            .iter()
            .zip(&curves)
            .map(|(&color, &curve)| Step { color, curve })
            .collect();
        let cycle = Cycle::new(steps).ok()?;
        let kind = self.kind.unwrap_or_else(|| guess_kind(&cycle, settings));
        let ease = cycle.steps()[0].curve;
        let pattern = match kind {
            Kind::Cycle => Pattern::Cycle(cycle),
            Kind::Rainbow => Pattern::Rainbow { ease },
            Kind::Pulse => Pattern::Pulse {
                color: cycle.steps()[0].color,
                ease,
            },
        };
        Some(Effect {
            pattern,
            period: self.period,
        })
    }
}

/// Works out which kind of effect a file from the Python version holds, from its colors.
fn guess_kind(cycle: &Cycle, settings: &Effects) -> Kind {
    let colors: Vec<Color> = cycle.steps().iter().map(|step| step.color).collect();
    if cycle.common_curve().is_none() {
        Kind::Cycle
    } else if colors == settings.rainbow.colors {
        Kind::Rainbow
    } else if colors.len() == 2 && colors[1] == Color::BLACK.mix(colors[0], settings.pulse.dim) {
        Kind::Pulse
    } else {
        Kind::Cycle
    }
}

/// The state file.
#[derive(Debug)]
pub(crate) struct Store {
    path: PathBuf,
}

impl Store {
    pub(crate) fn new(path: impl Into<PathBuf>) -> Self {
        Store { path: path.into() }
    }

    /// Reads the saved scene. A zone that the file doesn't have, or has in a form that can't be
    /// read, gets the palette's first color.
    pub(crate) fn load(&self, palette: &[Color; 10], settings: &Effects) -> Scene {
        let mut scene = Scene::filled(&Setting::Color(palette[0]));
        let saved: Value =
            match fs::read_to_string(&self.path).map(|text| serde_json::from_str(&text)) {
                Ok(Ok(saved)) => saved,
                Ok(Err(error)) => {
                    log::warn!("can't read {}: {error}", self.path.display());
                    return scene;
                }
                Err(_) => return scene,
            };
        for zone in Zone::ALL {
            match saved.get(zone.name()) {
                // The first versions saved each zone's place in the palette.
                Some(Value::Number(index)) => {
                    if let Some(index) = index.as_i64() {
                        let index = usize::try_from(index.rem_euclid(10)).unwrap_or(0);
                        scene[zone] = Setting::Color(palette[index]);
                    }
                }
                Some(Value::String(color)) => {
                    if let Ok(color) = color.parse() {
                        scene[zone] = Setting::Color(color);
                    }
                }
                _ => {}
            }
            let effect = saved
                .get("fades")
                .and_then(|fades| fades.get(zone.name()))
                .and_then(|effect| SavedEffect::deserialize(effect).ok())
                .and_then(|effect| effect.into_effect(settings));
            if let Some(effect) = effect {
                scene[zone] = Setting::Effect(effect);
            }
        }
        scene
    }

    /// Saves `scene`. A zone with an effect is saved with the effect and with `color_now`, which
    /// is the color the effect gives at this moment.
    pub(crate) fn save(
        &self,
        scene: &Scene,
        color_now: impl Fn(Zone) -> Color,
        settings: &Effects,
    ) -> io::Result<()> {
        let mut saved = Map::new();
        let mut fades = Map::new();
        for (zone, setting) in scene.iter() {
            saved.insert(zone.name().to_owned(), json!(color_now(zone)));
            if let Setting::Effect(effect) = setting {
                let effect = SavedEffect::of(effect, zone, settings);
                fades.insert(zone.name().to_owned(), serde_json::to_value(effect)?);
            }
        }
        saved.insert("fades".to_owned(), Value::Object(fades));

        if let Some(directory) = self.path.parent() {
            fs::create_dir_all(directory)?;
        }
        // Writing a new file and renaming it over the old one means that the state file is never
        // half written.
        let mut temporary = self.path.clone().into_os_string();
        temporary.push(".tmp");
        fs::write(&temporary, Value::Object(saved).to_string())?;
        fs::rename(&temporary, &self.path)
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::daemon::config::Config;
    use crate::testing::Rig;
    use crate::zone::Zone::Center;

    struct Saved {
        // Dropping the directory deletes it, so it lives as long as the store.
        _directory: tempfile::TempDir,
        store: Store,
    }

    impl Saved {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let store = Store::new(directory.path().join("state").join("state.json"));
            Saved {
                _directory: directory,
                store,
            }
        }

        fn holding(contents: &Value) -> Self {
            let saved = Saved::new();
            fs::create_dir_all(saved.store.path.parent().unwrap()).unwrap();
            fs::write(&saved.store.path, contents.to_string()).unwrap();
            saved
        }

        fn save(&self, rig: &Rig) {
            let color_now = |zone| rig.state.scene_color_at(zone, rig.now);
            self.store
                .save(rig.state.scene(), color_now, &Effects::default())
                .unwrap();
        }

        fn contents(&self) -> Value {
            serde_json::from_str(&fs::read_to_string(&self.store.path).unwrap()).unwrap()
        }

        /// What each zone is set to after loading, as the color dialog writes it.
        fn load(&self) -> [String; 4] {
            let config = Config::default();
            let scene = self.store.load(&config.palette.colors, &config.effects());
            Zone::ALL.map(|zone| crate::text::describe(&scene[zone]))
        }
    }

    #[test]
    fn colors_are_saved_as_hex_with_an_empty_fades_table() {
        let saved = Saved::new();
        let mut rig = Rig::new();
        rig.kbdrgb("zones 112233 445566 778899 AABBCC");
        saved.save(&rig);
        assert_eq!(
            saved.contents(),
            json!({"left": "112233", "center": "445566", "right": "778899", "extra": "AABBCC", "fades": {}})
        );
        assert_eq!(saved.load(), ["112233", "445566", "778899", "AABBCC"]);
    }

    #[test]
    fn a_cycle_is_saved_in_the_form_that_the_python_version_reads() {
        let saved = Saved::new();
        let mut rig = Rig::new();
        rig.kbdrgb("cycle -z center AA00AA 00FF00");
        rig.wait(0.5);
        saved.save(&rig);
        let contents = saved.contents();
        assert_eq!(
            contents["fades"],
            json!({"center": {"kind": "cycle", "colors": ["AA00AA", "00FF00"], "period": 1.0, "offset": 0.0}})
        );
        assert_eq!(contents["center"], json!(rig.color(Center)));
    }

    #[rstest]
    #[case("cycle -z left -t 2 AA00AA 00FF00", "AA00AA 00FF00 2")]
    #[case("cycle -z left -e ease-in AA00AA 00FF00", "AA00AA 00FF00 1 ease-in")]
    #[case(
        "cycle -z left FF0000 FFFF00:ease-in 0000FF:0.9,0,0.1,1",
        "FF0000 FFFF00:ease-in 0000FF:0.9,0,0.1,1 1"
    )]
    #[case("rainbow -z left -t 0.3", "rainbow 0.3")]
    #[case("rainbow -z left -e ease-out", "rainbow 0.6 ease-out")]
    #[case("pulse -z left 00FFAA", "pulse 00FFAA 0.5")]
    #[case("pulse -z left -t 2 -e ease 00FFAA", "pulse 00FFAA 2 ease")]
    fn an_effect_loads_as_the_effect_that_was_saved(#[case] command: &str, #[case] text: &str) {
        let saved = Saved::new();
        let mut rig = Rig::new();
        rig.kbdrgb(command);
        saved.save(&rig);
        assert_eq!(saved.load()[0], text);
    }

    #[test]
    fn an_eased_effect_is_saved_with_one_curve_or_with_one_for_each_color() {
        let saved = Saved::new();
        let mut rig = Rig::new();
        rig.kbdrgb("cycle -z left -e ease-in AA00AA 00FF00");
        rig.kbdrgb("cycle -z right FF0000 FFFF00:ease-in 0000FF:0.9,0,0.1,1");
        saved.save(&rig);
        let fades = &saved.contents()["fades"];
        assert_eq!(fades["left"]["ease"], json!("ease-in"));
        assert_eq!(
            fades["right"]["ease"],
            json!(["linear", "ease-in", "0.9,0,0.1,1"])
        );
    }

    #[test]
    fn a_rainbow_is_saved_with_each_zones_place_among_its_colors() {
        let saved = Saved::new();
        let mut rig = Rig::new();
        rig.kbdrgb("rainbow");
        saved.save(&rig);
        let fades = &saved.contents()["fades"];
        assert_eq!(fades["left"]["kind"], json!("rainbow"));
        assert_eq!(fades["left"]["colors"].as_array().unwrap().len(), 6);
        assert_eq!(fades["left"]["offset"], json!(0.0));
        assert_eq!(fades["extra"]["offset"], json!(2.0));
    }

    #[test]
    fn a_timed_effect_is_not_saved() {
        let saved = Saved::new();
        let mut rig = Rig::new();
        rig.kbdrgb("pulse -z extra -d 3 00FF88");
        saved.save(&rig);
        assert_eq!(saved.contents()["fades"], json!({}));
        assert_eq!(saved.contents()["extra"], json!("FFFFFF"));
    }

    #[test]
    fn a_missing_file_gives_a_white_keyboard() {
        assert_eq!(Saved::new().load(), ["FFFFFF"; 4]);
    }

    #[test]
    fn a_file_that_is_not_json_gives_a_white_keyboard() {
        let saved = Saved::new();
        fs::create_dir_all(saved.store.path.parent().unwrap()).unwrap();
        fs::write(&saved.store.path, "{\"left\": ").unwrap();
        assert_eq!(saved.load(), ["FFFFFF"; 4]);
    }

    #[test]
    fn a_zone_that_the_file_does_not_have_is_white() {
        let saved = Saved::holding(&json!({"left": "112233", "right": "nonsense"}));
        assert_eq!(saved.load(), ["112233", "FFFFFF", "FFFFFF", "FFFFFF"]);
    }

    #[test]
    fn a_place_in_the_palette_from_the_first_versions_loads_as_that_color() {
        let saved = Saved::holding(&json!({"left": 7, "center": 0, "right": 12, "extra": 0}));
        assert_eq!(saved.load(), ["0000FF", "FFFFFF", "FF6600", "FFFFFF"]);
    }

    #[test]
    fn a_file_from_the_python_version_loads_with_the_kind_worked_out_from_its_colors() {
        let rainbow = ["FF0000", "FFFF00", "00FF00", "00FFFF", "0000FF", "FF00FF"];
        let saved = Saved::holding(&json!({
            "left": "FFFFFF", "center": "FFFFFF", "right": "FFFFFF", "extra": "FFFFFF",
            "fades": {
                "left": {"colors": ["AA00AA", "00FF00"], "period": 2.0, "offset": 0.0},
                "center": {"colors": rainbow, "period": 0.6, "offset": 2.0 / 3.0},
                "right": {"colors": ["00FFAA", "00140E"], "period": 0.5, "offset": 0.0, "ease": "ease-in"},
                "extra": {"colors": rainbow, "period": 1, "ease": ["linear", "ease-in", "linear", "linear", "linear", "linear"]},
            }
        }));
        assert_eq!(
            saved.load(),
            [
                "AA00AA 00FF00 2",
                "rainbow 0.6",
                "pulse 00FFAA 0.5 ease-in",
                "FF0000 FFFF00:ease-in 00FF00 00FFFF 0000FF FF00FF 1"
            ]
        );
    }

    #[rstest]
    #[case(json!({"colors": ["AA00AA"], "period": 1.0}))]
    #[case(json!({"colors": ["AA00AA", "zzz"], "period": 1.0}))]
    #[case(json!({"colors": ["AA00AA", "00FF00"], "period": 0}))]
    #[case(json!({"colors": ["AA00AA", "00FF00"], "period": "x"}))]
    #[case(json!({"colors": ["AA00AA", "00FF00"], "period": 1.0, "ease": "wobbly"}))]
    #[case(json!({"colors": ["AA00AA", "00FF00", "0000FF"], "period": 1.0, "ease": ["ease-in", "linear"]}))]
    #[case(json!({"period": 1.0}))]
    #[case(json!("junk"))]
    fn an_effect_that_cannot_be_read_is_ignored(#[case] effect: Value) {
        let saved = Saved::holding(&json!({"left": "123456", "fades": {"left": effect}}));
        assert_eq!(saved.load(), ["123456", "FFFFFF", "FFFFFF", "FFFFFF"]);
    }

    #[test]
    fn an_effect_for_something_that_is_not_a_zone_is_ignored() {
        let effect = json!({"colors": ["AA00AA", "00FF00"], "period": 1.0});
        let saved = Saved::holding(&json!({"left": "123456", "fades": {"nowhere": effect}}));
        assert_eq!(saved.load(), ["123456", "FFFFFF", "FFFFFF", "FFFFFF"]);
    }

    #[test]
    fn a_fades_entry_that_is_not_a_table_does_not_stop_the_colors_from_loading() {
        let saved = Saved::holding(&json!({"left": "123456", "fades": ["junk"]}));
        assert_eq!(saved.load(), ["123456", "FFFFFF", "FFFFFF", "FFFFFF"]);
    }

    #[test]
    fn saving_replaces_the_file_and_leaves_no_temporary_file() {
        let saved = Saved::new();
        let mut rig = Rig::new();
        rig.kbdrgb("color 111111");
        saved.save(&rig);
        rig.kbdrgb("color 222222");
        saved.save(&rig);
        assert_eq!(saved.load(), ["222222"; 4]);
        let files: Vec<_> = fs::read_dir(saved.store.path.parent().unwrap())
            .unwrap()
            .collect();
        assert_eq!(files.len(), 1);
    }
}
