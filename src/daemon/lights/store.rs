//! The state file, which saves the scene so that it comes back after a restart.

use std::path::PathBuf;
use std::{fs, io};

use serde::Deserialize;
use serde_json::{Map, Value};

use super::state::Scene;
use crate::color::Color;
use crate::setting::Setting;
use crate::zone::Zone;

/// The state file. It holds each zone's setting under the zone's name.
#[derive(Debug)]
pub(crate) struct Store {
    path: PathBuf,
}

impl Store {
    pub(crate) fn new(path: impl Into<PathBuf>) -> Self {
        Store { path: path.into() }
    }

    /// Reads the saved scene. A zone that the file doesn't have, or has in a form that can't be
    /// read, gets `white`.
    pub(crate) fn load(&self, white: Color) -> Scene {
        let mut scene = Scene::filled(&Setting::Color(white));
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
            // Each zone is read on its own, so one that can't be read doesn't lose the others.
            if let Some(setting) = saved
                .get(zone.name())
                .and_then(|setting| Setting::deserialize(setting).ok())
            {
                scene[zone] = setting;
            }
        }
        scene
    }

    /// Saves `scene`.
    pub(crate) fn save(&self, scene: &Scene) -> io::Result<()> {
        let mut saved = Map::new();
        for (zone, setting) in scene.iter() {
            saved.insert(zone.name().to_owned(), serde_json::to_value(setting)?);
        }

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
    use serde_json::json;

    use super::*;
    use crate::testing::Rig;

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
            self.store.save(rig.state.scene()).unwrap();
        }

        fn contents(&self) -> Value {
            serde_json::from_str(&fs::read_to_string(&self.store.path).unwrap()).unwrap()
        }

        /// What each zone is set to after loading, as the color dialog writes it.
        fn load(&self) -> [String; 4] {
            let scene = self.store.load(Color::WHITE);
            Zone::ALL.map(|zone| crate::text::describe(&scene[zone]))
        }
    }

    #[test]
    fn each_zone_is_saved_under_its_name() {
        let saved = Saved::new();
        let mut rig = Rig::new();
        rig.kbdrgb("zones 112233 445566 778899 AABBCC");
        saved.save(&rig);
        assert_eq!(
            saved.contents(),
            json!({
                "left": {"Color": "112233"},
                "center": {"Color": "445566"},
                "right": {"Color": "778899"},
                "extra": {"Color": "AABBCC"}
            })
        );
        assert_eq!(saved.load(), ["112233", "445566", "778899", "AABBCC"]);
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
    fn a_timed_effect_is_not_saved() {
        let saved = Saved::new();
        let mut rig = Rig::new();
        rig.kbdrgb("pulse -z extra -d 3 00FF88");
        saved.save(&rig);
        assert_eq!(saved.contents()["extra"], json!({"Color": "FFFFFF"}));
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
        let saved = Saved::holding(&json!({"left": {"Color": "112233"}}));
        assert_eq!(saved.load(), ["112233", "FFFFFF", "FFFFFF", "FFFFFF"]);
    }

    #[rstest]
    #[case(json!("112233"))]
    #[case(json!({"Color": "zzz"}))]
    #[case(json!({"Effect": {"pattern": {"Cycle": [{"color": "AA00AA", "curve": "linear"}]}, "period": 1.0}}))]
    #[case(json!({"Effect": {"pattern": {"Rainbow": {"ease": "linear"}}, "period": 0}}))]
    #[case(json!({"Effect": {"pattern": {"Pulse": {"color": "AA00AA", "ease": "wobbly"}}, "period": 1.0}}))]
    #[case(json!({"Blink": "AA00AA"}))]
    fn a_zone_that_cannot_be_read_is_white_and_the_others_still_load(#[case] setting: Value) {
        let saved = Saved::holding(&json!({"left": setting, "center": {"Color": "123456"}}));
        assert_eq!(saved.load(), ["FFFFFF", "123456", "FFFFFF", "FFFFFF"]);
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
