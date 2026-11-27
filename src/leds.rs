//! The color files of System76's `system76` driver, which set the keyboard's colors.

use std::path::PathBuf;
use std::{fs, io};

use crate::color::Color;
use crate::zone::Zone;

/// The LED directory that the driver creates.
pub(crate) const DEFAULT_DIRECTORY: &str = "/sys/class/leds/system76::kbd_backlight";

/// Something that gives a zone a color.
pub(crate) trait LedWriter {
    fn write(&mut self, zone: Zone, color: Color) -> io::Result<()>;
}

/// The driver's LED directory. It has a `color_left`, `color_center`, `color_right` and
/// `color_extra` file, which each hold a color as `RRGGBB`, and a `brightness` file, which holds a
/// number from 0 to 255.
#[derive(Debug, Clone)]
pub(crate) struct LedFiles {
    directory: PathBuf,
}

impl LedFiles {
    pub(crate) fn new(directory: impl Into<PathBuf>) -> Self {
        LedFiles {
            directory: directory.into(),
        }
    }

    pub(crate) fn color_file(&self, zone: Zone) -> PathBuf {
        self.directory.join(format!("color_{zone}"))
    }

    /// The color that `zone` has now, as the driver prints it.
    pub(crate) fn color(&self, zone: Zone) -> io::Result<String> {
        Ok(fs::read_to_string(self.color_file(zone))?.trim().to_owned())
    }

    pub(crate) fn brightness(&self) -> io::Result<String> {
        Ok(fs::read_to_string(self.directory.join("brightness"))?
            .trim()
            .to_owned())
    }

    pub(crate) fn set_brightness(&self, level: u8) -> io::Result<()> {
        fs::write(self.directory.join("brightness"), level.to_string())
    }
}

impl LedWriter for LedFiles {
    fn write(&mut self, zone: Zone, color: Color) -> io::Result<()> {
        fs::write(self.color_file(zone), color.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        for zone in Zone::ALL {
            fs::write(directory.path().join(format!("color_{zone}")), "FFFFFF\n").unwrap();
        }
        fs::write(directory.path().join("brightness"), "255\n").unwrap();
        directory
    }

    #[test]
    fn a_color_is_written_to_its_zones_file_as_hex() {
        let directory = directory();
        let mut leds = LedFiles::new(directory.path());
        leds.write(Zone::Extra, "ff8800".parse().unwrap()).unwrap();
        assert_eq!(
            fs::read_to_string(directory.path().join("color_extra")).unwrap(),
            "FF8800"
        );
        assert_eq!(leds.color(Zone::Extra).unwrap(), "FF8800");
        assert_eq!(leds.color(Zone::Left).unwrap(), "FFFFFF");
    }

    #[test]
    fn brightness_is_read_and_written_as_a_number() {
        let directory = directory();
        let leds = LedFiles::new(directory.path());
        assert_eq!(leds.brightness().unwrap(), "255");
        leds.set_brightness(100).unwrap();
        assert_eq!(
            fs::read_to_string(directory.path().join("brightness")).unwrap(),
            "100"
        );
    }

    #[test]
    fn a_missing_directory_is_an_error() {
        let mut leds = LedFiles::new("/no/such/directory");
        assert!(leds.write(Zone::Left, Color::WHITE).is_err());
        assert!(leds.brightness().is_err());
    }
}
