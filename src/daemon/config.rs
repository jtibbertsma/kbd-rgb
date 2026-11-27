//! The daemon's settings, which come from a TOML file.

use std::num::{NonZeroU32, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{fmt, fs, io};

use evdev::KeyCode;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer};

use crate::color::Color;
use crate::ease::Ease;
use crate::seconds::format_decimal;
use crate::setting::Period;
use crate::{leds, request};

/// Where the daemon looks for its settings unless `--config` names another file.
pub(crate) const DEFAULT_PATH: &str = "/etc/kbd-rgb/config.toml";

/// Every setting. A group of settings is a table in the file, and each part of the daemon is
/// given only the groups it uses. `config.toml` in the repository explains each setting.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub(crate) struct Config {
    pub(crate) mod_key: ModKey,
    pub(crate) device: String,
    pub(crate) undo_depth: NonZeroUsize,
    #[serde(deserialize_with = "positive_seconds")]
    pub(crate) frame: Duration,
    pub(crate) visible_step: NonZeroU32,
    pub(crate) palette: Palette,
    pub(crate) caps: CapsLock,
    pub(crate) cycle: CycleSettings,
    pub(crate) rainbow: RainbowSettings,
    pub(crate) pulse: PulseSettings,
    pub(crate) system: System,
}

/// The colors for Menu+0 to Menu+9.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub(crate) struct Palette {
    #[serde(deserialize_with = "ten_colors")]
    pub(crate) colors: [Color; 10],
    #[serde(deserialize_with = "ten_names")]
    pub(crate) names: [String; 10],
}

/// What the keyboard does while caps lock is on.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub(crate) struct CapsLock {
    #[serde(deserialize_with = "at_least_one_color")]
    pub(crate) colors: Vec<Color>,
    #[serde(deserialize_with = "positive_seconds")]
    pub(crate) delay: Duration,
    pub(crate) ease: Ease,
    pub(crate) other: Color,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub(crate) struct CycleSettings {
    pub(crate) delay: Period,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub(crate) struct RainbowSettings {
    #[serde(deserialize_with = "at_least_two_colors")]
    pub(crate) colors: Vec<Color>,
    pub(crate) delay: Period,
    #[serde(deserialize_with = "not_negative")]
    pub(crate) lag: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub(crate) struct PulseSettings {
    pub(crate) delay: Period,
    #[serde(deserialize_with = "from_zero_to_one")]
    pub(crate) dim: f64,
}

/// Devices, files, and limits, which rarely need changing.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub(crate) struct System {
    pub(crate) virtual_name: String,
    pub(crate) led: PathBuf,
    pub(crate) caps_glob: String,
    pub(crate) state_file: PathBuf,
    pub(crate) socket: PathBuf,
    pub(crate) max_request: NonZeroUsize,
    #[serde(deserialize_with = "positive_seconds")]
    pub(crate) request_timeout: Duration,
    pub(crate) problem_file: PathBuf,
    pub(crate) report_after: NonZeroU32,
    #[serde(deserialize_with = "positive_seconds")]
    pub(crate) stable_after: Duration,
}

/// The defaults that the three effects use when an action gives no time, and what a rainbow and a
/// pulse look like.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Effects {
    pub(crate) cycle: CycleSettings,
    pub(crate) rainbow: RainbowSettings,
    pub(crate) pulse: PulseSettings,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ConfigError {
    #[error("can't read {}: {source}", path.display())]
    Read { path: PathBuf, source: io::Error },
    #[error("{}: {source}", path.display())]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("{}: device can't be the daemon's own virtual keyboard.", path.display())]
    OwnVirtualKeyboard { path: PathBuf },
}

impl Config {
    /// Reads the settings file at `path`. A file that doesn't exist gives the defaults.
    pub(crate) fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        let config: Config = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_owned(),
            source,
        })?;
        if config.device == config.system.virtual_name {
            return Err(ConfigError::OwnVirtualKeyboard {
                path: path.to_owned(),
            });
        }
        Ok(config)
    }

    pub(crate) fn effects(&self) -> Effects {
        Effects {
            cycle: self.cycle.clone(),
            rainbow: self.rainbow.clone(),
            pulse: self.pulse.clone(),
        }
    }
}

fn colors<const N: usize>(colors: [&str; N]) -> [Color; N] {
    colors.map(|color| color.parse().expect("the default colors are valid"))
}

fn period(milliseconds: u64) -> Period {
    Period::new(Duration::from_millis(milliseconds)).expect("the default delays are valid")
}

impl Default for Config {
    fn default() -> Self {
        Config {
            mod_key: ModKey(KeyCode::KEY_COMPOSE),
            device: "/dev/input/by-path/platform-i8042-serio-0-event-kbd".to_owned(),
            undo_depth: NonZeroUsize::new(100).expect("100 isn't 0"),
            frame: Duration::from_millis(100),
            visible_step: NonZeroU32::new(32).expect("32 isn't 0"),
            palette: Palette::default(),
            caps: CapsLock::default(),
            cycle: CycleSettings::default(),
            rainbow: RainbowSettings::default(),
            pulse: PulseSettings::default(),
            system: System::default(),
        }
    }
}

impl Default for Palette {
    fn default() -> Self {
        let names = [
            "white", "red", "orange", "yellow", "green", "teal", "sky blue", "blue", "purple",
            "pink",
        ];
        Palette {
            colors: colors([
                "FFFFFF", "FF0000", "FF6600", "FFFF00", "00FF00", "00FFAA", "0088FF", "0000FF",
                "8800FF", "FF00AA",
            ]),
            names: names.map(str::to_owned),
        }
    }
}

impl Default for CapsLock {
    fn default() -> Self {
        let colors = colors([
            "FF0000", "FF0000", "FF0000", "FF0000", "FF0000", "FFFFFF", "FFFFFF",
        ]);
        CapsLock {
            colors: colors.to_vec(),
            delay: Duration::from_millis(200),
            ease: Ease::new(0.0, 0.0, 0.2, 1.0).expect("the default curve is valid"),
            other: Color::WHITE,
        }
    }
}

impl Default for CycleSettings {
    fn default() -> Self {
        CycleSettings {
            delay: period(1000),
        }
    }
}

impl Default for RainbowSettings {
    fn default() -> Self {
        RainbowSettings {
            colors: colors(["FF0000", "FFFF00", "00FF00", "00FFFF", "0000FF", "FF00FF"]).to_vec(),
            delay: period(600),
            lag: 2.0 / 3.0,
        }
    }
}

impl Default for PulseSettings {
    fn default() -> Self {
        PulseSettings {
            delay: period(500),
            dim: 0.08,
        }
    }
}

impl Default for System {
    fn default() -> Self {
        System {
            virtual_name: "kbd-rgb virtual keyboard".to_owned(),
            led: leds::DEFAULT_DIRECTORY.into(),
            caps_glob: "/sys/class/leds/input*::capslock/brightness".to_owned(),
            state_file: "/var/lib/kbd-rgb/state.json".into(),
            socket: request::DEFAULT_SOCKET.into(),
            max_request: NonZeroUsize::new(65536).expect("65536 isn't 0"),
            request_timeout: Duration::from_secs(1),
            problem_file: "/run/kbd-rgb/problem.json".into(),
            report_after: NonZeroU32::new(3).expect("3 isn't 0"),
            stable_after: Duration::from_secs(30),
        }
    }
}

/// The key to hold for the color combos. In the settings file it's a key's name from the kernel,
/// with or without `KEY_` and in any case, such as `COMPOSE` for `KEY_COMPOSE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub(crate) struct ModKey(pub(crate) KeyCode);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("\"{0}\" isn't a key name like COMPOSE.")]
pub(crate) struct NotAKey(String);

impl TryFrom<String> for ModKey {
    type Error = NotAKey;

    fn try_from(word: String) -> Result<Self, Self::Error> {
        let name = word.to_uppercase();
        let name = name.strip_prefix("KEY_").unwrap_or(&name);
        format!("KEY_{name}")
            .parse()
            .map(ModKey)
            .map_err(|_| NotAKey(word))
    }
}

impl fmt::Display for ModKey {
    /// The inverse of reading it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = format!("{:?}", self.0);
        f.write_str(name.strip_prefix("KEY_").unwrap_or(&name))
    }
}

fn positive_seconds<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
    let seconds = f64::deserialize(deserializer)?;
    Duration::try_from_secs_f64(seconds)
        .ok()
        .filter(|time| !time.is_zero())
        .ok_or_else(|| {
            D::Error::custom(format!("{} must be more than 0.", format_decimal(seconds)))
        })
}

fn not_negative<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
    let number = f64::deserialize(deserializer)?;
    if number >= 0.0 && number.is_finite() {
        Ok(number)
    } else {
        Err(D::Error::custom(format!(
            "{} must be 0 or more.",
            format_decimal(number)
        )))
    }
}

fn from_zero_to_one<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
    let number = f64::deserialize(deserializer)?;
    if (0.0..=1.0).contains(&number) {
        Ok(number)
    } else {
        Err(D::Error::custom(format!(
            "{} must be from 0 to 1.",
            format_decimal(number)
        )))
    }
}

fn ten_colors<'de, D: Deserializer<'de>>(deserializer: D) -> Result<[Color; 10], D::Error> {
    Vec::deserialize(deserializer)?
        .try_into()
        .map_err(|_| D::Error::custom("The palette requires 10 colors, for Menu+0 to Menu+9."))
}

fn ten_names<'de, D: Deserializer<'de>>(deserializer: D) -> Result<[String; 10], D::Error> {
    let names: [String; 10] = Vec::deserialize(deserializer)?.try_into().map_err(|_| {
        D::Error::custom("The palette requires 10 names, one for each palette color.")
    })?;
    if names.iter().any(|name| name.trim().is_empty()) {
        return Err(D::Error::custom("A palette name can't be empty."));
    }
    Ok(names)
}

fn at_least_one_color<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<Color>, D::Error> {
    let colors = Vec::deserialize(deserializer)?;
    if colors.is_empty() {
        return Err(D::Error::custom("Caps lock requires at least 1 color."));
    }
    Ok(colors)
}

fn at_least_two_colors<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<Color>, D::Error> {
    let colors = Vec::deserialize(deserializer)?;
    if colors.len() < 2 {
        return Err(D::Error::custom("The rainbow requires at least 2 colors."));
    }
    Ok(colors)
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn read(text: &str) -> Result<Config, String> {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, text).unwrap();
        Config::load(&path).map_err(|error| error.to_string())
    }

    #[test]
    fn a_missing_file_gives_the_defaults() {
        let directory = tempfile::tempdir().unwrap();
        let config = Config::load(&directory.path().join("no-such-file.toml")).unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn an_empty_file_gives_the_defaults() {
        assert_eq!(read(""), Ok(Config::default()));
    }

    #[test]
    fn the_repositorys_config_toml_has_every_setting_commented_out() {
        let example = include_str!("../../config.toml");
        assert_eq!(read(example), Ok(Config::default()));
    }

    /// A commented-out setting is a line that starts with `#` and then the setting's name, with no
    /// space between them. Lines of explanation have a space after the `#`.
    #[test]
    fn the_repositorys_config_toml_has_the_defaults_from_the_code() {
        let example = include_str!("../../config.toml");
        let uncommented: Vec<&str> = example
            .lines()
            .map(|line| match line.strip_prefix('#') {
                Some(setting) if setting.starts_with(|c: char| c.is_ascii_lowercase()) => setting,
                _ => line,
            })
            .collect();
        let settings = uncommented
            .iter()
            .filter(|line| line.contains(" = "))
            .count();
        assert_eq!(settings, 27, "every setting is in the file");
        assert_eq!(read(&uncommented.join("\n")), Ok(Config::default()));
    }

    #[test]
    fn a_setting_changes_only_that_setting() {
        let config = read("device = \"keyd virtual keyboard\"\n[caps]\ndelay = 0.25").unwrap();
        let expected = Config {
            device: "keyd virtual keyboard".to_owned(),
            caps: CapsLock {
                delay: Duration::from_millis(250),
                ..CapsLock::default()
            },
            ..Config::default()
        };
        assert_eq!(config, expected);
    }

    #[test]
    fn colors_curves_and_key_names_are_read_as_the_dialog_and_kbdrgb_read_them() {
        let config = read(
            "mod-key = \"rightalt\"\n\
             [caps]\n\
             colors = [\"ff0000\", \"#00FF00\"]\n\
             ease = \"0.4,0,0.2,1\"",
        )
        .unwrap();
        assert_eq!(config.mod_key, ModKey(KeyCode::KEY_RIGHTALT));
        assert_eq!(config.caps.colors, colors(["FF0000", "00FF00"]));
        assert_eq!(config.caps.ease, Ease::new(0.4, 0.0, 0.2, 1.0).unwrap());
    }

    #[test]
    fn a_whole_number_works_as_a_time() {
        assert_eq!(
            read("[cycle]\ndelay = 2").unwrap().cycle.delay,
            period(2000)
        );
    }

    #[rstest]
    #[case("COMPOSE", KeyCode::KEY_COMPOSE)]
    #[case("key_rightmeta", KeyCode::KEY_RIGHTMETA)]
    fn a_mod_key_prints_the_name_it_reads(#[case] name: &str, #[case] key: KeyCode) {
        let mod_key = ModKey::try_from(name.to_owned()).unwrap();
        assert_eq!(mod_key, ModKey(key));
        assert_eq!(ModKey::try_from(mod_key.to_string()), Ok(mod_key));
    }

    #[rstest]
    #[case("[palette]\ncolors = [\"FF0000\"]", "The palette requires 10 colors")]
    #[case("[palette]\nnames = [\"a\", \"b\"]", "The palette requires 10 names")]
    #[case(
        "[palette]\nnames = [\"a\", \"\", \"c\", \"d\", \"e\", \"f\", \"g\", \"h\", \"i\", \"j\"]",
        "A palette name can't be empty."
    )]
    #[case("[caps]\ndelay = 0", "0 must be more than 0.")]
    #[case("[caps]\ndelay = -1", "-1 must be more than 0.")]
    #[case("[caps]\ncolors = []", "Caps lock requires at least 1 color.")]
    #[case("[caps]\ncolors = [\"FF0000\", \"zzz\"]", "\"zzz\" isn't a color")]
    #[case("[caps]\nease = \"bouncy\"", "\"bouncy\" isn't an easing curve")]
    #[case("[cycle]\ndelay = 0.05", "The time must be from 0.1 to 60 seconds.")]
    #[case(
        "[rainbow]\ncolors = [\"FF0000\"]",
        "The rainbow requires at least 2 colors."
    )]
    #[case("[rainbow]\nlag = -0.5", "-0.5 must be 0 or more.")]
    #[case("[pulse]\ndim = 2", "2 must be from 0 to 1.")]
    #[case("undo-depth = 0", "expected a nonzero usize")]
    #[case("undo-depth = -1", "expected a nonzero usize")]
    #[case("mod-key = \"nope\"", "\"nope\" isn't a key name like COMPOSE.")]
    #[case(
        "device = \"kbd-rgb virtual keyboard\"",
        "device can't be the daemon's own virtual keyboard"
    )]
    #[case("bogus = 1", "unknown field `bogus`")]
    #[case("process-name = \"kbd\"", "unknown field `process-name`")]
    #[case("[caps]\ndelays = 1", "unknown field `delays`")]
    #[case("frame = ", "TOML parse error")]
    fn a_bad_setting_is_rejected_with_the_reason(#[case] text: &str, #[case] reason: &str) {
        let error = read(text).unwrap_err();
        assert!(error.contains(reason), "{text:?} gave {error}");
    }

    #[test]
    fn the_error_names_the_file_and_the_line() {
        let error = read("device = \"x\"\n\n[caps]\ndelay = 0").unwrap_err();
        assert!(error.contains("config.toml"), "{error}");
        assert!(error.contains("line 4"), "{error}");
    }
}
