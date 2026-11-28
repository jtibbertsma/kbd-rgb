//! The `kbdrgb` command.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::{self, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;
use std::str::FromStr;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

use crate::action::{Action, Change, Draft, Kind};
use crate::color::Color;
use crate::ease::Ease;
use crate::leds::{self, LedFiles, LedWriter};
use crate::request::{self, ReadError, Request, Response, ZoneStatus};
use crate::seconds::parse_seconds;
use crate::text;
use crate::zone::{PerZone, Target, Zone};

/// Control the keyboard's colors on a System76 laptop.
#[derive(Debug, Parser)]
#[command(name = "kbdrgb", after_help = AFTER_HELP)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

const AFTER_HELP: &str = "\
With no command, kbdrgb runs status.

For color, zones, and cycle, a color can have its own curve for fading into it, which takes the \
place of --ease for that step: kbdrgb cycle FF0000 FFFF00:ease-in 0000FF:0.9,0,0.1,1

Effects run until their zone gets a new color, and each zone has its own effect and time. Effects \
with the same colors and --time stay in step, however far apart you start them.

Without the kbd-rgb daemon running, kbdrgb can only set plain colors, set the brightness, and \
print the colors.";

#[derive(Debug, Subcommand)]
enum Command {
    /// Set a color
    Color {
        /// The color as RRGGBB, or RRGGBB:EASE to give the fade its curve
        #[arg(value_name = "COLOR")]
        step: StepArgument,
        #[command(flatten)]
        zone: ZoneOption,
        #[command(flatten)]
        fade: FadeOptions,
    },
    /// Set each zone's color or effect
    Zones {
        /// Up to four settings, for left, center, right, and the numpad. Each one is written as in
        /// the color dialog: 00FF00, 00FF00,0000FF (a cycle), pulse,00FFAA, rainbow, or
        /// "00FF00 0000FF 2" (2 seconds per color). Zones left out are white
        #[arg(value_name = "SETTING", required = true)]
        settings: Vec<String>,
        #[command(flatten)]
        fade: FadeOptions,
    },
    /// Fade between 2 to 16 colors, over and over
    Cycle {
        /// Each color as RRGGBB, or RRGGBB:EASE to give the fade into it its own curve
        #[arg(value_name = "COLOR")]
        steps: Vec<StepArgument>,
        #[command(flatten)]
        zone: ZoneOption,
        #[command(flatten)]
        fade: FadeOptions,
        #[command(flatten)]
        time_limit: ForOption,
    },
    /// Fade through the rainbow's colors, each zone further along them than the last
    Rainbow {
        #[command(flatten)]
        zone: ZoneOption,
        #[command(flatten)]
        fade: FadeOptions,
        #[command(flatten)]
        time_limit: ForOption,
    },
    /// Fade between a color and a dim copy of it
    Pulse {
        /// The color as RRGGBB
        #[arg(value_name = "COLOR")]
        step: StepArgument,
        #[command(flatten)]
        zone: ZoneOption,
        #[command(flatten)]
        fade: FadeOptions,
        #[command(flatten)]
        time_limit: ForOption,
    },
    /// Set the backlight brightness of the whole keyboard
    Brightness {
        /// From 0 to 255
        level: u8,
    },
    /// Print the brightness, each zone's color or effect, and a zones command that sets them all
    /// again
    Status,
}

#[derive(Debug, Args)]
struct ZoneOption {
    /// left, center, right, extra (the numpad), or all
    #[arg(short, long, value_name = "ZONE", default_value = "all")]
    zone: Target,
}

#[derive(Debug, Args)]
struct FadeOptions {
    /// Seconds to fade from one color to the next
    #[arg(short, long, value_name = "SECONDS", value_parser = seconds)]
    time: Option<Duration>,
    /// How each fade speeds up and slows down: linear, ease, ease-in, ease-out, ease-in-out, or a
    /// cubic bezier X1,Y1,X2,Y2 with each number from 0 to 1
    #[arg(short, long, value_name = "EASE")]
    ease: Option<Ease>,
}

#[derive(Debug, Args)]
struct ForOption {
    /// Stop after this many seconds, and stay on the color it stopped on
    #[arg(short = 'd', long = "for", value_name = "SECONDS", value_parser = seconds)]
    time_limit: Option<Duration>,
}

fn seconds(text: &str) -> Result<Duration, String> {
    parse_seconds(text)
        .ok_or_else(|| format!("\"{text}\" isn't a number of seconds like 2 or 0.5."))
}

/// `COLOR` or `COLOR:CURVE`, where `CURVE` is for fading into `COLOR`.
#[derive(Debug, Clone, Copy)]
struct StepArgument(Color, Option<Ease>);

impl FromStr for StepArgument {
    type Err = text::TextError;

    fn from_str(word: &str) -> Result<Self, Self::Err> {
        match word.split_once(':') {
            Some((color, curve)) => Ok(StepArgument(color.parse()?, Some(curve.parse()?))),
            None => Ok(StepArgument(word.parse()?, None)),
        }
    }
}

/// What a `kbdrgb` command line asks for.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Invocation {
    /// A request for the daemon.
    Ask(Request),
    /// A new brightness, which `kbdrgb` writes itself.
    Brightness(u8),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum UsageError {
    /// The arguments don't fit the command. This is also how clap hands back `--help`'s text.
    #[error(transparent)]
    Arguments(#[from] clap::Error),
    #[error(
        "zones requires 1 to 4 colors or effects, for left, center, right and the numpad. Zones \
         left out are white."
    )]
    TooManyZones,
    #[error("{zone} zone: {problem}")]
    Zone {
        zone: Zone,
        problem: text::TextError,
    },
    #[error(transparent)]
    Action(#[from] crate::action::ActionError),
}

/// Reads the arguments that follow `kbdrgb` on its command line.
pub(crate) fn parse<I, T>(arguments: I) -> Result<Invocation, UsageError>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let program = std::iter::once(OsString::from("kbdrgb"));
    let cli = Cli::try_parse_from(program.chain(arguments.into_iter().map(Into::into)))?;
    let draft =
        |kind, steps: &[StepArgument], fade: &FadeOptions, time_limit: Option<Duration>| Draft {
            kind,
            steps: steps
                .iter()
                .map(|&StepArgument(color, curve)| (color, curve))
                .collect(),
            time: fade.time,
            ease: fade.ease,
            time_limit,
        };
    let action = match cli.command.unwrap_or(Command::Status) {
        Command::Status => return Ok(Invocation::Ask(Request::Status)),
        Command::Brightness { level } => return Ok(Invocation::Brightness(level)),
        Command::Zones { settings, fade } => {
            return Ok(Invocation::Ask(Request::Run(zone_actions(
                &settings, &fade,
            )?)));
        }
        Command::Color { step, zone, fade } => {
            draft(Kind::Color, &[step], &fade, None).build(zone.zone)
        }
        Command::Cycle {
            steps,
            zone,
            fade,
            time_limit,
        } => draft(Kind::Cycle, &steps, &fade, time_limit.time_limit).build(zone.zone),
        Command::Rainbow {
            zone,
            fade,
            time_limit,
        } => draft(Kind::Rainbow, &[], &fade, time_limit.time_limit).build(zone.zone),
        Command::Pulse {
            step,
            zone,
            fade,
            time_limit,
        } => draft(Kind::Pulse, &[step], &fade, time_limit.time_limit).build(zone.zone),
    };
    Ok(Invocation::Ask(Request::Run(vec![action?])))
}

/// Turns the arguments of `kbdrgb zones` into one action for each zone. Each string in `settings`
/// is one zone's color or effect, read like text typed in the color dialog. Zones without a string
/// are set to white. `fade` applies to any zone whose text doesn't give its own time or curve.
fn zone_actions(settings: &[String], fade: &FadeOptions) -> Result<Vec<Action>, UsageError> {
    if settings.len() > Zone::ALL.len() {
        return Err(UsageError::TooManyZones);
    }
    let white = Color::WHITE.to_string();
    Zone::ALL
        .into_iter()
        .enumerate()
        .map(|(index, zone)| {
            let text = settings.get(index).unwrap_or(&white);
            let mut draft = text::parse(text)?;
            draft.time = draft.time.or(fade.time);
            // A plain color that is set at once can't use a curve, so the curve that `-e` gives
            // every zone only goes to the zones that fade.
            let fades = draft.kind != Kind::Color || draft.time.is_some_and(|time| !time.is_zero());
            if fades {
                draft.ease = draft.ease.or(fade.ease);
            }
            Ok(draft.build(Target::Zone(zone))?)
        })
        .zip(Zone::ALL)
        .map(|(action, zone)| action.map_err(|problem| UsageError::Zone { zone, problem }))
        .collect()
}

/// Runs `kbdrgb` with the arguments it was started with.
#[must_use]
pub fn main() -> ExitCode {
    let invocation = match parse(std::env::args_os().skip(1)) {
        Ok(invocation) => invocation,
        // clap prints its own errors and `--help`, and picks the exit status for each.
        Err(UsageError::Arguments(error)) => error.exit(),
        Err(error) => return fail(&error),
    };
    match carry_out(invocation, &Paths::from_environment()) {
        Ok(output) => {
            // A reader that has gone away, as in `kbdrgb status | head -1`, is not an error.
            let _ = io::stdout().write_all(output.as_bytes());
            ExitCode::SUCCESS
        }
        Err(failure) => fail(&failure),
    }
}

fn fail(error: &dyn std::error::Error) -> ExitCode {
    let _ = writeln!(io::stderr(), "kbdrgb: {error}");
    ExitCode::FAILURE
}

/// Where `kbdrgb` finds the keyboard and the daemon.
#[derive(Debug, Clone)]
struct Paths {
    led: PathBuf,
    socket: PathBuf,
}

impl Paths {
    /// The usual paths, unless `KBDRGB_LED` or `KBDRGB_SOCKET` names another one. The tests set
    /// those.
    fn from_environment() -> Self {
        let path = |variable, default: &str| {
            std::env::var_os(variable).map_or_else(|| default.into(), PathBuf::from)
        };
        Paths {
            led: path("KBDRGB_LED", leds::DEFAULT_DIRECTORY),
            socket: path("KBDRGB_SOCKET", request::DEFAULT_SOCKET),
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum Failure {
    /// The daemon's reason for not doing what was asked.
    #[error("{0}")]
    Rejected(String),
    #[error("no answer from the kbd-rgb daemon")]
    NoAnswer,
    #[error(
        "{what} needs the daemon, and the kbd-rgb daemon isn't running (sudo systemctl start kbd-rgb)"
    )]
    NeedsDaemon { what: &'static str },
    #[error("can't use the keyboard's files in {}: {source}", directory.display())]
    Leds {
        directory: PathBuf,
        source: io::Error,
    },
}

impl Failure {
    fn leds(files: &LedFiles, source: io::Error) -> Self {
        Failure::Leds {
            directory: files.directory().to_owned(),
            source,
        }
    }
}

/// How long `kbdrgb` waits for the daemon's answer.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(2);

/// Does what the command line asked for, and returns what to print.
fn carry_out(invocation: Invocation, paths: &Paths) -> Result<String, Failure> {
    let mut files = LedFiles::new(&paths.led);
    let request = match invocation {
        Invocation::Brightness(level) => {
            files
                .set_brightness(level)
                .map_err(|source| Failure::leds(&files, source))?;
            return Ok(String::new());
        }
        Invocation::Ask(request) => request,
    };
    match request::send(&paths.socket, &request, ANSWER_TIMEOUT) {
        Ok(Response::Done) => Ok(String::new()),
        Ok(Response::Status(status)) => Ok(status_text(&files, &status)),
        Ok(Response::Rejected(reason)) => Err(Failure::Rejected(reason)),
        Err(ReadError::Malformed(_) | ReadError::TooLong) => Err(Failure::NoAnswer),
        // The daemon can't be reached, so it isn't running, and kbdrgb does what it can itself.
        Err(ReadError::Io(_)) => match request {
            Request::Run(actions) => set_plain_colors(&mut files, &actions).map(|()| String::new()),
            Request::Status => leds_status(&files),
        },
    }
}

/// Without the daemon, writes plain colors straight to the keyboard. Anything that changes over
/// time needs the daemon. Every action is checked before any color is written.
fn set_plain_colors(files: &mut LedFiles, actions: &[Action]) -> Result<(), Failure> {
    let mut colors = Vec::new();
    for action in actions {
        let color = match &action.change {
            Change::Color { color, fade, .. } if fade.get().is_zero() => *color,
            Change::Color { .. } => return Err(Failure::NeedsDaemon { what: "a fade" }),
            Change::Effect { pattern, .. } => {
                return Err(Failure::NeedsDaemon {
                    what: pattern.name(),
                });
            }
        };
        colors.extend(action.target.zones().map(|zone| (zone, color)));
    }
    for (zone, color) in colors {
        files
            .write(zone, color)
            .map_err(|source| Failure::leds(files, source))?;
    }
    Ok(())
}

/// The first line of `status`.
fn brightness_line(files: &LedFiles) -> String {
    match files.brightness() {
        Ok(brightness) => format!("brightness: {brightness}\n"),
        Err(error) => format!("brightness: unknown ({error})\n"),
    }
}

/// What `status` prints with the daemon: each zone's setting as the color dialog writes it, and a
/// `kbdrgb zones` command that sets them all again.
fn status_text(files: &LedFiles, status: &PerZone<ZoneStatus>) -> String {
    let mut text = brightness_line(files);
    let mut settings = Vec::new();
    for (zone, status) in status.iter() {
        let setting = text::describe(&status.setting);
        // Writing to a `String` can't fail.
        let _ = write!(text, "{:8}{setting}", format!("{zone}:"));
        if let Some(time_left) = status.time_left {
            let _ = write!(text, "  (for {:.0}s more)", time_left.as_secs_f64());
        }
        text.push('\n');
        settings.push(setting);
    }
    let _ = writeln!(text, "kbdrgb zones {}", quoted(&settings));
    text
}

/// What `status` prints without the daemon: the colors that the keyboard has now.
fn leds_status(files: &LedFiles) -> Result<String, Failure> {
    let mut text = brightness_line(files);
    let mut colors = Vec::new();
    for zone in Zone::ALL {
        let color = files
            .color(zone)
            .map_err(|source| Failure::leds(files, source))?;
        let _ = writeln!(text, "{:8}{color}", format!("{zone}:"));
        colors.push(color);
    }
    let _ = writeln!(text, "kbdrgb zones {}", quoted(&colors));
    Ok(text)
}

/// `words` as arguments for a shell.
fn quoted(words: &[String]) -> String {
    // Quoting only fails for a word with a NUL byte in it, which no setting has.
    shlex::try_join(words.iter().map(String::as_str)).unwrap_or_else(|_| words.join(" "))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn actions(line: &str) -> Vec<String> {
        match parse(line.split(' ')) {
            Ok(Invocation::Ask(Request::Run(actions))) => {
                actions.iter().map(Action::to_string).collect()
            }
            other => panic!("{line:?} gave {other:?}"),
        }
    }

    #[rstest]
    #[case("color ff0000", "color -z all FF0000")]
    #[case("color #00FF00 -z left -t 2", "color -z left -t 2 00FF00")]
    #[case("color -z all -t 2.5 #0000FF", "color -z all -t 2.5 0000FF")]
    #[case("color -t 0 112233", "color -z all 112233")]
    #[case(
        "color 112233 -t 2 --ease=0.4,0,0.2,1",
        "color -z all -t 2 -e 0.4,0,0.2,1 112233"
    )]
    #[case("color -t 2 FF0000:ease-out", "color -z all -t 2 -e ease-out FF0000")]
    #[case("cycle AA00AA 00FF00 --zone center", "cycle -z center AA00AA 00FF00")]
    #[case(
        "cycle AA00AA 00FF00 0000FF -t 2 -d 30",
        "cycle -z all -t 2 -d 30 AA00AA 00FF00 0000FF"
    )]
    #[case(
        "cycle -z center -t 0.1 -d 0.1 AA00AA 00FF00",
        "cycle -z center -t 0.1 -d 0.1 AA00AA 00FF00"
    )]
    #[case(
        "cycle -e 1,0,0,1 AA00AA 00FF00",
        "cycle -z all -e 1,0,0,1 AA00AA 00FF00"
    )]
    #[case(
        "cycle ff0000 FFFF00:ease-in #0000FF:0.9,0,0.1,1",
        "cycle -z all FF0000 FFFF00:ease-in 0000FF:0.9,0,0.1,1"
    )]
    #[case(
        "cycle -e ease-in AA00AA:linear 00FF00",
        "cycle -z all AA00AA 00FF00:ease-in"
    )]
    #[case("rainbow", "rainbow -z all")]
    #[case("rainbow -z extra --for 20", "rainbow -z extra -d 20")]
    #[case("rainbow -z extra -t 60 -d 86400", "rainbow -z extra -t 60 -d 86400")]
    #[case("pulse 00ffaa --zone=right --time 1.5", "pulse -z right -t 1.5 00FFAA")]
    #[case("pulse 00FFAA -e ease-in-out", "pulse -z all -e ease-in-out 00FFAA")]
    fn a_command_asks_for_one_action(#[case] line: &str, #[case] action: &str) {
        assert_eq!(actions(line), [action]);
    }

    #[test]
    fn an_action_reads_back_from_what_it_prints() {
        for line in [
            "color -z left -t 2 -e ease-in FF8800",
            "cycle -z all -t 2 -d 30 FF0000 FFFF00:ease-in",
        ] {
            assert_eq!(actions(line), [line]);
        }
    }

    #[test]
    fn zones_asks_for_one_action_per_zone_and_fills_the_rest_with_white() {
        assert_eq!(
            actions("zones 111111 222222 --time=0.5"),
            [
                "color -z left -t 0.5 111111",
                "color -z center -t 0.5 222222",
                "color -z right -t 0.5 FFFFFF",
                "color -z extra -t 0.5 FFFFFF"
            ]
        );
    }

    #[test]
    fn zones_reads_each_setting_like_the_color_dialog() {
        let request = parse([
            "zones",
            "00FF00,0000FF",
            "0000FF 00FF00",
            "pulse,8800FF",
            "RAINBOW",
            "-t",
            "2",
        ]);
        let Ok(Invocation::Ask(Request::Run(actions))) = request else {
            panic!("{request:?}")
        };
        let actions: Vec<String> = actions.iter().map(Action::to_string).collect();
        assert_eq!(
            actions,
            [
                "cycle -z left -t 2 00FF00 0000FF",
                "cycle -z center -t 2 0000FF 00FF00",
                "pulse -z right -t 2 8800FF",
                "rainbow -z extra -t 2"
            ]
        );
    }

    #[test]
    fn zones_options_apply_to_the_zones_without_their_own_time_or_curve() {
        let request = parse([
            "zones",
            "FF0000 00FF00 3 ease-in",
            "112233:ease-in",
            "-t",
            "1",
            "-e",
            "ease-out",
        ]);
        let Ok(Invocation::Ask(Request::Run(actions))) = request else {
            panic!("{request:?}")
        };
        let actions: Vec<String> = actions.iter().map(Action::to_string).collect();
        assert_eq!(
            actions,
            [
                "cycle -z left -t 3 -e ease-in FF0000 00FF00",
                "color -z center -t 1 -e ease-in 112233",
                "color -z right -t 1 -e ease-out FFFFFF",
                "color -z extra -t 1 -e ease-out FFFFFF"
            ]
        );
    }

    #[test]
    fn the_curve_of_zones_goes_only_to_the_zones_that_fade() {
        assert_eq!(
            actions("zones FF0000 00FF00,0000FF -e ease-in"),
            [
                "color -z left FF0000",
                "cycle -z center -e ease-in 00FF00 0000FF",
                "color -z right FFFFFF",
                "color -z extra FFFFFF"
            ]
        );
    }

    #[rstest]
    #[case(&["color", "FF8800", "-e", "ease-in"], "A curve needs a time to fade over, such as -t 2.")]
    #[case(&["color", "FF8800:ease-in"], "A curve needs a time to fade over, such as -t 2.")]
    #[case(&["color", "FF8800", "-t", "0", "-e", "ease-in"], "A curve needs a time to fade over, such as -t 2.")]
    #[case(&["zones", "FF8800:ease-in"], "left zone: A curve needs a time to fade over, such as -t 2.")]
    fn a_color_with_a_curve_needs_a_time(#[case] arguments: &[&str], #[case] problem: &str) {
        let error = parse(arguments.iter().copied()).unwrap_err();
        assert_eq!(error.to_string(), problem);
    }

    #[test]
    fn no_command_means_status() {
        assert_eq!(
            parse::<_, &str>([]).unwrap(),
            Invocation::Ask(Request::Status)
        );
        assert_eq!(parse(["status"]).unwrap(), Invocation::Ask(Request::Status));
    }

    #[test]
    fn brightness_takes_a_level() {
        assert_eq!(
            parse(["brightness", "128"]).unwrap(),
            Invocation::Brightness(128)
        );
    }

    #[rstest]
    #[case(&["nope"])]
    #[case(&["sync"])]
    #[case(&["color"])]
    #[case(&["color", "FF0000", "00FF00"])]
    #[case(&["color", "FF0000", "-d", "3"])]
    #[case(&["color", "-z", "top", "FF0000"])]
    #[case(&["color", "-t", "61", "FF0000"])]
    #[case(&["color", "-t", "-1", "FF0000"])]
    #[case(&["color", "-t", "abc", "FF0000"])]
    #[case(&["color", "-t", "1e3", "FF0000"])]
    #[case(&["color", "-t", "nan", "FF0000"])]
    #[case(&["color", "color=FF0000"])]
    #[case(&["color", "FF0000", "-q"])]
    #[case(&["color", "FF0000\nFF0000"])]
    #[case(&["color", "FF0000 00FF00"])]
    #[case(&["cycle", "AA00AA"])]
    #[case(&["cycle", "AA00AA", "zzz"])]
    #[case(&["cycle", "AA00AA", "00FF00", "-t"])]
    #[case(&["cycle", "-t", "0", "AA00AA", "00FF00"])]
    #[case(&["cycle", "-t", "0.05", "AA00AA", "00FF00"])]
    #[case(&["cycle", "-d", "0", "AA00AA", "00FF00"])]
    #[case(&["cycle", "AA00AA", "00FF00", "-e", "bouncy"])]
    #[case(&["cycle", "FF0000:bouncy", "00FF00"])]
    #[case(&["cycle", "FF0000:", "00FF00"])]
    #[case(&["cycle", "FF0000:0.4,0,0.2,2", "00FF00"])]
    #[case(&["cycle", "zzz:ease-in", "00FF00"])]
    #[case(&["rainbow", "FF0000"])]
    #[case(&["rainbow", "-z"])]
    #[case(&["rainbow", "-t", "0"])]
    #[case(&["rainbow", "-t", "61"])]
    #[case(&["rainbow", "-d", "0.05"])]
    #[case(&["rainbow", "-d", "86401"])]
    #[case(&["pulse"])]
    #[case(&["pulse", "zzzzzz"])]
    #[case(&["pulse", "FF0000", "00FF00"])]
    #[case(&["pulse", "FF0000", "-z", "top"])]
    #[case(&["pulse", "FF0000", "-e", "0.4,0,0.2"])]
    #[case(&["pulse", "FF0000", "-e", "0.4,0,0.2,1.5"])]
    #[case(&["pulse", "FF0000", "-e", "0.4,0,0.2,1,"])]
    #[case(&["pulse", "FF0000", "-e", "0.4,-1,0.2,1"])]
    #[case(&["pulse", "-e", "FF0000"])]
    #[case(&["pulse", "FF0000:ease-in"])]
    #[case(&["zones"])]
    #[case(&["zones", ""])]
    #[case(&["zones", "-z", "left", "FF0000"])]
    #[case(&["zones", "1", "2", "3", "4", "5"])]
    #[case(&["zones", "FF0000,zzz"])]
    #[case(&["zones", "pulse,FF0000,00FF00"])]
    #[case(&["zones", "FF0000,00FF00", "-t", "0"])]
    #[case(&["zones", "FF0000", "-d", "5"])]
    #[case(&["brightness"])]
    #[case(&["brightness", "300"])]
    #[case(&["brightness", "9", "-e", "ease"])]
    #[case(&["status", "-z", "left"])]
    fn a_command_that_cannot_work_is_rejected(#[case] arguments: &[&str]) {
        assert!(
            parse(arguments.iter().copied()).is_err(),
            "{arguments:?} was accepted"
        );
    }

    #[rstest]
    #[case(&["cycle", "AA00AA"], "cycle requires 2 to 16 colors.")]
    #[case(&["pulse", "FF0000:ease-in"], "pulse uses one curve for every step")]
    #[case(&["zones", "FF0000", "rainbow,FF0000"], "center zone: rainbow doesn't have color arguments.")]
    #[case(&["zones", "1", "2", "3", "4", "5"], "zones requires 1 to 4 colors or effects")]
    #[case(&["color", "-t", "abc", "FF0000"], "\"abc\" isn't a number of seconds like 2 or 0.5.")]
    #[case(&["color", "-z", "top", "FF0000"], "\"top\" isn't a zone.")]
    #[case(&["cycle", "AA00AA", "zzz"], "\"zzz\" isn't a color.")]
    #[case(&["pulse", "FF0000", "-e", "bouncy"], "\"bouncy\" isn't an easing curve.")]
    fn the_rejection_says_what_is_wrong(#[case] arguments: &[&str], #[case] reason: &str) {
        let error = parse(arguments.iter().copied()).unwrap_err().to_string();
        assert!(error.contains(reason), "{arguments:?} gave {error}");
    }

    #[test]
    fn a_cycle_of_17_colors_is_rejected() {
        let arguments = std::iter::once("cycle").chain(std::iter::repeat_n("AA00AA", 17));
        let error = parse(arguments).unwrap_err().to_string();
        assert_eq!(error, "cycle requires 2 to 16 colors.");
    }

    #[rstest]
    #[case(&["-h"])]
    #[case(&["--help"])]
    #[case(&["help"])]
    #[case(&["cycle", "-h"])]
    fn help_lists_the_options(#[case] arguments: &[&str]) {
        let Err(UsageError::Arguments(help)) = parse(arguments.iter().copied()) else {
            panic!("{arguments:?} didn't give the help text");
        };
        assert_eq!(help.kind(), clap::error::ErrorKind::DisplayHelp);
        assert!(help.to_string().contains("--zone") || help.to_string().contains("cycle"));
    }

    // Carrying a command out

    /// A keyboard's LED directory in a temporary directory, and optionally a daemon that writes
    /// to it and listens on a socket next to it.
    struct Fixture {
        paths: Paths,
        directory: tempfile::TempDir,
        daemon: Option<crate::daemon::server::Socket>,
    }

    impl Fixture {
        fn without_daemon() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let led = directory.path().join("led");
            std::fs::create_dir(&led).unwrap();
            for zone in Zone::ALL {
                std::fs::write(led.join(format!("color_{zone}")), "FFFFFF").unwrap();
            }
            std::fs::write(led.join("brightness"), "255\n").unwrap();
            let paths = Paths {
                led,
                socket: directory.path().join("run").join("socket"),
            };
            Fixture {
                paths,
                directory,
                daemon: None,
            }
        }

        fn with_daemon() -> Self {
            use crate::daemon::server::{Limits, Socket};
            let mut fixture = Fixture::without_daemon();
            let files = LedFiles::new(&fixture.paths.led);
            let state_file = fixture.directory.path().join("state.json");
            let config = crate::daemon::config::Config::default();
            let lights = crate::testing::lights(
                &config,
                files.clone(),
                &state_file,
                std::time::Instant::now(),
            );
            let (lights, _thread) = crate::daemon::lights::spawn(lights).unwrap();
            let socket =
                Socket::bind(&fixture.paths.socket, &files.color_file(Zone::Left)).unwrap();
            let limits = Limits {
                max_request: config.system.max_request,
                timeout: config.system.request_timeout,
            };
            socket.serve(lights, limits).unwrap();
            fixture.daemon = Some(socket);
            fixture
        }

        /// Runs a `kbdrgb` command line, and returns what it prints or the error it reports.
        fn kbdrgb(&self, line: &str) -> Result<String, String> {
            let arguments = shlex::split(line).unwrap();
            let invocation = parse(arguments).map_err(|error| error.to_string())?;
            carry_out(invocation, &self.paths).map_err(|failure| failure.to_string())
        }

        /// The colors in the LED files.
        fn colors(&self) -> [String; 4] {
            let files = LedFiles::new(&self.paths.led);
            Zone::ALL.map(|zone| files.color(zone).unwrap())
        }

        fn saved(&self) -> serde_json::Value {
            let state_file = self.directory.path().join("state.json");
            serde_json::from_str(&std::fs::read_to_string(state_file).unwrap()).unwrap()
        }
    }

    #[test]
    fn with_the_daemon_a_color_goes_through_the_daemon_which_writes_and_saves_it() {
        let fixture = Fixture::with_daemon();
        assert_eq!(
            fixture.kbdrgb("zones 112233 445566 778899 AABBCC"),
            Ok(String::new())
        );
        assert_eq!(fixture.colors(), ["112233", "445566", "778899", "AABBCC"]);
        assert_eq!(fixture.saved()["right"], "778899");
    }

    #[test]
    fn with_the_daemon_an_effect_starts() {
        let fixture = Fixture::with_daemon();
        assert_eq!(
            fixture.kbdrgb("cycle AA00AA 00ff00 -z center"),
            Ok(String::new())
        );
        assert_eq!(
            fixture.saved()["fades"]["center"]["colors"],
            serde_json::json!(["AA00AA", "00FF00"])
        );
        assert_eq!(fixture.kbdrgb("color 445566"), Ok(String::new()));
        assert_eq!(fixture.saved()["fades"], serde_json::json!({}));
        assert_eq!(fixture.colors(), ["445566"; 4]);
    }

    #[test]
    fn status_prints_each_zones_setting_and_a_command_that_sets_them_again() {
        let fixture = Fixture::with_daemon();
        fixture
            .kbdrgb("cycle -z left -e ease-in 00FF00 0000FF")
            .unwrap();
        fixture.kbdrgb("pulse -z center -t 2 -d 30 8800FF").unwrap();
        fixture.kbdrgb("rainbow -z right").unwrap();
        fixture.kbdrgb("color -z extra -t 5 FF00AA").unwrap();
        let status = fixture.kbdrgb("status").unwrap();
        assert_eq!(
            status,
            "brightness: 255\n\
             left:   00FF00 0000FF 1 ease-in\n\
             center: pulse 8800FF 2  (for 30s more)\n\
             right:  rainbow 0.6\n\
             extra:  FF00AA\n\
             kbdrgb zones '00FF00 0000FF 1 ease-in' 'pulse 8800FF 2' 'rainbow 0.6' FF00AA\n"
        );
        assert_eq!(fixture.kbdrgb(""), Ok(status), "no command means status");
    }

    #[test]
    fn the_command_that_status_prints_sets_the_same_settings_again() {
        let fixture = Fixture::with_daemon();
        fixture
            .kbdrgb("zones 'FF0000 FFFF00:ease-in 0000FF 2' 'pulse 00FFAA' rainbow 123456")
            .unwrap();
        let before = fixture.kbdrgb("status").unwrap();
        let command = before
            .lines()
            .last()
            .unwrap()
            .strip_prefix("kbdrgb ")
            .unwrap()
            .to_owned();
        fixture.kbdrgb("color FFFFFF").unwrap();
        fixture.kbdrgb(&command).unwrap();
        assert_eq!(fixture.kbdrgb("status").unwrap(), before);
    }

    #[test]
    fn a_command_that_cannot_work_is_reported_and_changes_nothing() {
        let fixture = Fixture::with_daemon();
        let error = fixture.kbdrgb("cycle AA00AA").unwrap_err();
        assert_eq!(error, "cycle requires 2 to 16 colors.");
        assert_eq!(fixture.colors(), ["FFFFFF"; 4]);
    }

    #[test]
    fn brightness_is_written_by_kbdrgb_itself() {
        for fixture in [Fixture::with_daemon(), Fixture::without_daemon()] {
            assert_eq!(fixture.kbdrgb("brightness 100"), Ok(String::new()));
            assert_eq!(
                std::fs::read_to_string(fixture.paths.led.join("brightness")).unwrap(),
                "100"
            );
        }
    }

    /// The ways that the daemon can be missing: no socket file, a socket file that a daemon left
    /// behind, and a socket that this user may not use.
    fn fixtures_without_a_daemon() -> Vec<(&'static str, Fixture)> {
        use std::os::unix::fs::PermissionsExt;
        let no_socket = Fixture::without_daemon();
        let nobody_listening = Fixture::without_daemon();
        std::fs::create_dir_all(nobody_listening.paths.socket.parent().unwrap()).unwrap();
        drop(std::os::unix::net::UnixListener::bind(&nobody_listening.paths.socket).unwrap());
        let not_allowed = Fixture::with_daemon();
        std::fs::set_permissions(
            &not_allowed.paths.socket,
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        vec![
            ("no socket", no_socket),
            ("nobody listening", nobody_listening),
            ("not allowed", not_allowed),
        ]
    }

    #[test]
    fn without_the_daemon_kbdrgb_writes_plain_colors_itself() {
        for (case, fixture) in fixtures_without_a_daemon() {
            let started = std::time::Instant::now();
            assert_eq!(fixture.kbdrgb("color ABCDEF"), Ok(String::new()), "{case}");
            assert_eq!(fixture.colors(), ["ABCDEF"; 4], "{case}");
            assert!(started.elapsed() < Duration::from_secs(2), "{case}");
            assert_eq!(
                fixture.kbdrgb("color 123123 -z right"),
                Ok(String::new()),
                "{case}"
            );
            assert_eq!(
                fixture.colors(),
                ["ABCDEF", "ABCDEF", "123123", "ABCDEF"],
                "{case}"
            );
            assert_eq!(
                fixture.kbdrgb("zones 010101 020202"),
                Ok(String::new()),
                "{case}"
            );
            assert_eq!(
                fixture.colors(),
                ["010101", "020202", "FFFFFF", "FFFFFF"],
                "{case}"
            );
        }
    }

    #[test]
    fn without_the_daemon_anything_that_changes_over_time_says_that_it_needs_the_daemon() {
        for (case, fixture) in fixtures_without_a_daemon() {
            for command in [
                "rainbow",
                "cycle AA00AA 00FF00",
                "pulse FF0000",
                "color FF0000 -t 1",
                "color FF0000:ease-in -t 1",
                "zones FF0000 00FF00,0000FF",
                "zones Rainbow",
                "zones 'pulse 00FF00'",
            ] {
                let error = fixture.kbdrgb(command).unwrap_err();
                assert!(
                    error.contains("needs the daemon"),
                    "{case}: {command}: {error}"
                );
                assert!(
                    error.contains("sudo systemctl start kbd-rgb"),
                    "{case}: {command}: {error}"
                );
            }
            assert_eq!(
                fixture.colors(),
                ["FFFFFF"; 4],
                "{case}: nothing was written"
            );
        }
    }

    #[test]
    fn without_the_daemon_every_color_is_checked_before_any_is_written() {
        let fixture = Fixture::without_daemon();
        assert!(fixture.kbdrgb("zones 999999 nope").is_err());
        assert!(fixture.kbdrgb("zones 999999 888888,777777").is_err());
        assert!(fixture.kbdrgb("color 999999 -z top").is_err());
        assert!(fixture.kbdrgb("color 999999 888888").is_err());
        assert_eq!(fixture.colors(), ["FFFFFF"; 4]);
    }

    #[test]
    fn without_the_daemon_status_prints_the_colors_that_the_keyboard_has() {
        let fixture = Fixture::without_daemon();
        fixture.kbdrgb("zones 010101 020202").unwrap();
        fixture.kbdrgb("brightness 100").unwrap();
        assert_eq!(
            fixture.kbdrgb("status").unwrap(),
            "brightness: 100\n\
             left:   010101\n\
             center: 020202\n\
             right:  FFFFFF\n\
             extra:  FFFFFF\n\
             kbdrgb zones 010101 020202 FFFFFF FFFFFF\n"
        );
    }

    #[test]
    fn a_keyboard_without_color_files_is_reported() {
        let fixture = Fixture::without_daemon();
        std::fs::remove_dir_all(&fixture.paths.led).unwrap();
        let error = fixture.kbdrgb("color FF0000").unwrap_err();
        assert!(
            error.starts_with("can't use the keyboard's files in "),
            "{error}"
        );
    }

    #[test]
    fn every_kbdrgb_command_in_the_readme_is_one_that_kbdrgb_accepts() {
        let readme = include_str!("../README.md");
        let commands: Vec<&str> = readme
            .lines()
            .filter_map(|line| line.strip_prefix("kbdrgb "))
            .map(|line| line.split("  #").next().unwrap().trim())
            .collect();
        assert!(
            commands.len() >= 12,
            "the README has {} commands",
            commands.len()
        );
        for command in commands {
            let arguments = shlex::split(command).unwrap();
            assert!(parse(arguments).is_ok(), "kbdrgb {command}");
        }
    }
}
