//! What `kbdrgb` sends to the daemon over its Unix socket, and what the daemon answers.
//!
//! `kbdrgb` connects once for each command. It sends one [`Request`] as a line of JSON, and the
//! daemon replies with one [`Response`] as a line of JSON.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::action::Action;
use crate::setting::Setting;
use crate::zone::PerZone;

/// The socket that the daemon listens on unless its settings name another one.
pub(crate) const DEFAULT_SOCKET: &str = "/run/kbd-rgb/socket";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum Request {
    /// Runs the actions as one undo step.
    Run(Vec<Action>),
    Status,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[expect(
    clippy::large_enum_variant,
    reason = "a response is made once and sent, so its size in memory doesn't matter"
)]
pub(crate) enum Response {
    /// The actions have been run.
    Done,
    Status(PerZone<ZoneStatus>),
    /// The daemon didn't do what was asked, and this says why.
    Rejected(String),
}

/// What a zone is set to, for `kbdrgb status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ZoneStatus {
    pub(crate) setting: Setting,
    /// How long the zone's timed effect has left, if it has one.
    pub(crate) time_left: Option<Duration>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ReadError {
    #[error("command too long")]
    TooLong,
    #[error("bad request")]
    Malformed(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Reads one line of JSON from `stream`. A line longer than `max_bytes` is an error, so that a
/// client can't make the reader hold an endless line in memory.
pub(crate) fn read<T: for<'de> Deserialize<'de>>(
    stream: impl Read,
    max_bytes: usize,
) -> Result<T, ReadError> {
    let limit = u64::try_from(max_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut line = String::new();
    BufReader::new(stream.take(limit)).read_line(&mut line)?;
    if line.len() > max_bytes {
        return Err(ReadError::TooLong);
    }
    Ok(serde_json::from_str(&line)?)
}

/// Writes `message` to `stream` as one line of JSON.
pub(crate) fn write<T: Serialize>(mut stream: impl Write, message: &T) -> io::Result<()> {
    let mut line = serde_json::to_string(message)?;
    line.push('\n');
    stream.write_all(line.as_bytes())
}

/// Sends `request` to the daemon that listens on `socket`, and waits up to `timeout` for its
/// answer.
pub(crate) fn send(
    socket: &Path,
    request: &Request,
    timeout: Duration,
) -> Result<Response, ReadError> {
    let stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    write(&stream, request)?;
    // No response is longer than a megabyte.
    read(&stream, 1 << 20)
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::*;
    use crate::cli;
    use crate::zone::Zone;

    fn request(line: &str) -> Request {
        match cli::parse(shlex::split(line).unwrap()) {
            Ok(cli::Invocation::Ask(request)) => request,
            other => panic!("{line:?} gave {other:?}"),
        }
    }

    fn through_json<T: Serialize + for<'de> Deserialize<'de>>(message: &T) -> T {
        let mut line = Vec::new();
        write(&mut line, message).unwrap();
        let (last, rest) = line.split_last().unwrap();
        assert!(
            *last == b'\n' && !rest.contains(&b'\n'),
            "the message is one line"
        );
        read(line.as_slice(), 65536).unwrap()
    }

    #[rstest]
    #[case("status")]
    #[case("color -z left -t 2 -e ease-in FF8800")]
    #[case("cycle -t 2 -d 30 FF0000 FFFF00:ease-in 0000FF:0.9,0,0.1,1")]
    #[case("rainbow -z extra -e ease-out")]
    #[case("zones 'pulse 00FFAA 1.5' rainbow 112233")]
    fn a_request_is_the_same_after_the_trip_through_json(#[case] line: &str) {
        let request = request(line);
        assert_eq!(through_json(&request), request);
    }

    #[test]
    fn a_response_is_the_same_after_the_trip_through_json() {
        let Request::Run(actions) = request("zones 'pulse 00FFAA 1.5' 112233") else {
            panic!()
        };
        let status = PerZone::from_fn(|zone| ZoneStatus {
            setting: crate::testing::setting_of(&actions[zone.index()]),
            time_left: (zone == Zone::Left).then_some(Duration::from_millis(1500)),
        });
        for response in [
            Response::Done,
            Response::Rejected("no".to_owned()),
            Response::Status(status),
        ] {
            assert_eq!(through_json(&response), response);
        }
    }

    /// The socket is writable by the user, so the daemon can't assume that a request came from
    /// `kbdrgb`. Reading a request checks every value in it.
    #[rstest]
    #[case(json!({"Run": [{"target": "top", "change": {"Color": {"color": "FF0000", "fade": 0.0, "curve": "linear"}}}]}))]
    #[case(json!({"Run": [{"target": "all", "change": {"Color": {"color": "zzz", "fade": 0.0, "curve": "linear"}}}]}))]
    #[case(json!({"Run": [{"target": "all", "change": {"Color": {"color": "FF0000", "fade": 61.0, "curve": "linear"}}}]}))]
    #[case(json!({"Run": [{"target": "all", "change": {"Color": {"color": "FF0000", "fade": -1.0, "curve": "linear"}}}]}))]
    #[case(json!({"Run": [{"target": "all", "change": {"Color": {"color": "FF0000", "fade": 0.0, "curve": "2,0,0,1"}}}]}))]
    #[case(json!({"Run": [{"target": "all", "change": {"Effect": {"pattern": {"Rainbow": {"ease": "linear"}}, "period": 0.01, "time_limit": null}}}]}))]
    #[case(json!({"Run": [{"target": "all", "change": {"Effect": {"pattern": {"Rainbow": {"ease": "linear"}}, "period": null, "time_limit": 1e9}}}]}))]
    #[case(json!({"Run": [{"target": "all", "change": {"Effect": {"pattern": {"Cycle": [{"color": "FF0000", "curve": "linear"}]}, "period": null, "time_limit": null}}}]}))]
    #[case(json!({"Explode": []}))]
    #[case(json!("hello"))]
    fn a_request_with_a_value_that_is_not_allowed_is_malformed(#[case] request: serde_json::Value) {
        let line = format!("{request}\n");
        let result: Result<Request, ReadError> = read(line.as_bytes(), 65536);
        assert!(
            matches!(result, Err(ReadError::Malformed(_))),
            "{request} gave {result:?}"
        );
    }

    #[test]
    fn the_same_request_with_allowed_values_is_read() {
        let request = json!({"Run": [{"target": "all", "change": {"Color": {"color": "FF0000", "fade": 2.0, "curve": "ease-in"}}}]});
        let line = format!("{request}\n");
        let read: Request = read(line.as_bytes(), 65536).unwrap();
        assert_eq!(read, self::request("color -t 2 -e ease-in FF0000"));
    }

    #[test]
    fn a_line_longer_than_the_limit_is_too_long() {
        let line = format!("{}\n", "x".repeat(100));
        let result: Result<Request, ReadError> = read(line.as_bytes(), 50);
        assert!(matches!(result, Err(ReadError::TooLong)), "{result:?}");
    }

    #[test]
    fn text_that_is_not_json_is_malformed() {
        let result: Result<Request, ReadError> = read("color FF0000\n".as_bytes(), 65536);
        assert_eq!(result.unwrap_err().to_string(), "bad request");
    }
}
