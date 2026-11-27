//! The `kbdrgb` command.

use std::ffi::OsString;
use std::str::FromStr;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

use crate::action::{Action, Draft, Kind};
use crate::color::Color;
use crate::ease::Ease;
use crate::request::Request;
use crate::seconds::parse_seconds;
use crate::text;
use crate::zone::{Target, Zone};

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
            draft.ease = draft.ease.or(fade.ease);
            Ok(draft.build(Target::Zone(zone))?)
        })
        .zip(Zone::ALL)
        .map(|(action, zone)| action.map_err(|problem| UsageError::Zone { zone, problem }))
        .collect()
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
}
